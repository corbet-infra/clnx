//! Pressure facts for the live GPU ladder, read at events.
//!
//! The conductor decides who leaves the card; this module only reads what it
//! needs and says when to read again. [`watch`] turns event sources into
//! wake-ups: Kubernetes watches on this node's GPU pods and on the node object
//! (a pod that OOM-restarts or turns not-ready, allocatable that changes), and
//! the model broker's own event stream (a model that starts or stops loading).
//! [`observe`] then reads the facts once per wake-up: the card's VRAM and GTT
//! counters in sysfs, the GPU workloads and their readiness, and the broker's
//! stuck models.
//!
//! One source runs on a clock: the [`CardSampler`]. Graphics VRAM spills to
//! system RAM (GTT) without any Kubernetes or broker event, so only reading the
//! counters notices a desktop that thrashes. It reads two local sysfs files
//! every few seconds (the retired pressure-watcher's 6 s cadence, restored
//! 2026-11-17) and wakes the observer only when the reading matters to the
//! ladder: while the full card spills, when a spill ends, and when VRAM crosses
//! the full fraction.

use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use cdrv::{DeviceAddress, GpuLadder};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::api::{ListParams, WatchEvent, WatchParams};
use kube::{Api, Client};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::ChildStdout;

use crate::backends::{Kube, Sysfs, Workload};
use crate::devices::bdf;
use crate::error::{ClnxError, Result};

/// Where the facts and the event sources are, for one host.
#[derive(Debug, Clone, PartialEq)]
pub struct PressureConfig {
    /// This host's Kubernetes node name.
    pub node: String,
    /// The card's GPU function (its sysfs directory carries the counters).
    pub card: DeviceAddress,
    /// The shared model broker, when one runs on the card.
    pub broker: Option<BrokerConfig>,
    /// The device plugin and the resources it must advertise, when guarded.
    pub guard: Option<GuardConfig>,
    /// The card sampler (the desktop's GTT spill), when it runs.
    pub sampler: Option<SamplerConfig>,
}

/// The card sampler's clock and the two thresholds it applies; the thresholds
/// are the conductor's (`vram_full_fraction`, `gtt_delta_bytes`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplerConfig {
    /// Time between two readings (6 s).
    pub interval: Duration,
    /// GTT growth between two readings below which a spill is noise (64 MiB).
    pub gtt_delta_bytes: u64,
    /// The VRAM-used fraction above which the card counts as full (0.85).
    pub vram_full_fraction: f64,
}

/// The registration guard: the resources the GPU device plugin must advertise
/// on this node while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardConfig {
    /// The extended resources (`devic.es/rocm-compute`, ...).
    pub resources: Vec<String>,
    /// The plugin pods' namespace.
    pub namespace: String,
    /// The plugin pods' label selector.
    pub labels: String,
}

/// The model broker (llama-swap) as a Kubernetes Service. Its address is read
/// from the Service at each use, so nothing on the host pins a cluster IP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConfig {
    /// The Service's namespace.
    pub namespace: String,
    /// The Service's name.
    pub name: String,
    /// The Service port of the broker's HTTP API.
    pub port: u16,
}

/// llama-swap's server-sent event stream: a model status change wakes the observer.
pub const BROKER_EVENTS_PATH: &str = "/api/events";
/// llama-swap's status document: every running model and its state.
pub const BROKER_STATUS_PATH: &str = "/running";

/// The URL of one broker endpoint, through the Service's current cluster IP.
fn broker_url<K: Kube>(kube: &K, broker: &BrokerConfig, path: &str) -> Option<String> {
    let address = kube
        .service_address(&broker.namespace, &broker.name)
        .ok()??;
    Some(format!("http://{address}:{}{path}", broker.port))
}

/// What wakes the observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// A GPU pod on this node or the node object changed.
    Kubernetes,
    /// The broker reported a model status change.
    Broker,
    /// The card sampler took a reading that matters to the ladder.
    Card(CardSample),
}

/// The card's memory counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardMemory {
    /// VRAM in use, in bytes.
    pub vram_used: u64,
    /// VRAM in total, in bytes.
    pub vram_total: u64,
    /// GTT (system RAM the driver maps for the card) in use, in bytes.
    pub gtt_used: u64,
}

impl CardMemory {
    /// Whether VRAM in use is above `fraction` of the total.
    #[must_use]
    pub fn vram_full(&self, fraction: f64) -> bool {
        if self.vram_total == 0 {
            return false;
        }
        #[allow(clippy::cast_precision_loss)]
        let used = self.vram_used as f64 / self.vram_total as f64;
        used > fraction
    }
}

/// One reading of the card's counters, as the sampler reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardSample {
    /// The counters now.
    pub memory: CardMemory,
    /// GTT in use at the previous reading, in bytes.
    pub gtt_previous: u64,
    /// Consecutive readings, this one included, in which the card was full and
    /// GTT grew by more than the noise floor; 0 when it does not spill.
    pub spilling: u32,
}

/// The desktop's GTT spill, read on a clock: the pressure-watcher's signal 2.
///
/// A reading spills when VRAM is above the full fraction and GTT grew by more
/// than `gtt_delta_bytes` since the previous reading; consecutive spilling
/// readings are counted (the conductor's grace). [`CardSampler::sample`]
/// returns the reading to report only when it matters: every spilling reading,
/// the first one after a spill, and each crossing of the full fraction.
#[derive(Debug, Clone)]
pub struct CardSampler {
    config: SamplerConfig,
    previous: Option<CardMemory>,
    spilling: u32,
    full: Option<bool>,
}

impl CardSampler {
    /// A sampler that has read nothing yet.
    #[must_use]
    pub fn new(config: SamplerConfig) -> Self {
        Self {
            config,
            previous: None,
            spilling: 0,
            full: None,
        }
    }

    /// Take one reading; `None` means the counters could not be read.
    pub fn sample(&mut self, memory: Option<CardMemory>) -> Option<CardSample> {
        let Some(memory) = memory else {
            // Unreadable counters are no spill: a running spill ends here.
            let ended = self
                .previous
                .filter(|_| self.spilling > 0)
                .map(|last| CardSample {
                    memory: last,
                    gtt_previous: last.gtt_used,
                    spilling: 0,
                });
            self.previous = None;
            self.spilling = 0;
            self.full = None;
            return ended;
        };
        let full = memory.vram_full(self.config.vram_full_fraction);
        let gtt_previous = self.previous.map_or(memory.gtt_used, |last| last.gtt_used);
        let spills = self.previous.is_some()
            && full
            && memory.gtt_used.saturating_sub(gtt_previous) > self.config.gtt_delta_bytes;
        let was_spilling = self.spilling > 0;
        self.spilling = if spills {
            self.spilling.saturating_add(1)
        } else {
            0
        };
        let crossed = self.full.is_some_and(|was_full| was_full != full);
        self.previous = Some(memory);
        self.full = Some(full);
        (spills || was_spilling || crossed).then_some(CardSample {
            memory,
            gtt_previous,
            spilling: self.spilling,
        })
    }
}

/// One compute tenant on the card: a workload with a ladder rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantFacts {
    /// `namespace/name`, the platform's tenant name.
    pub id: String,
    /// Its ladder rung, from the pod template's priority class.
    pub ladder: GpuLadder,
    /// The replicas it asks for.
    pub replicas: u32,
    /// The replicas that are ready.
    pub ready: u32,
}

/// Everything one observation read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressureFacts {
    /// The card's counters; `None` while no host driver holds the card.
    pub memory: Option<CardMemory>,
    /// The compute tenants.
    pub tenants: Vec<TenantFacts>,
    /// Models the broker holds in a state other than `ready`; `None` when the
    /// broker is not configured or did not answer (never a fabricated count).
    pub broker_stuck: Option<u32>,
    /// The guarded resources that read zero (or are absent) on the node while
    /// a device plugin pod runs. Empty when the plugin does not run or the node
    /// could not be read (a failed read is never zero).
    pub zero_resources: Vec<String>,
}

/// The guarded resources that read zero or are absent in a node's allocatable.
#[must_use]
pub fn zero_resources(
    allocatable: &std::collections::BTreeMap<String, String>,
    resources: &[String],
) -> Vec<String> {
    resources
        .iter()
        .filter(|resource| {
            allocatable
                .get(resource.as_str())
                .is_none_or(|quantity| quantity.trim() == "0")
        })
        .cloned()
        .collect()
}

/// Read the card's VRAM and GTT counters (amdgpu's `mem_info_*` attributes).
#[must_use]
pub fn card_memory<S: Sysfs>(sysfs: &S, card: &DeviceAddress) -> Option<CardMemory> {
    let dir = format!("bus/pci/devices/{}", bdf(card));
    let read = |name: &str| -> Option<u64> {
        sysfs
            .read(&format!("{dir}/{name}"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    };
    Some(CardMemory {
        vram_used: read("mem_info_vram_used")?,
        vram_total: read("mem_info_vram_total")?,
        gtt_used: read("mem_info_gtt_used").unwrap_or(0),
    })
}

/// The compute tenants among the GPU workloads: a workload with a ladder rung
/// that asks for compute (a video-engine-only workload never competes for
/// VRAM and is not a tenant).
#[must_use]
pub fn compute_tenants(workloads: &[Workload]) -> Vec<TenantFacts> {
    workloads
        .iter()
        .filter(|workload| workload.compute)
        .filter_map(|workload| {
            Some(TenantFacts {
                id: format!("{}/{}", workload.namespace, workload.name),
                ladder: workload.ladder?,
                replicas: workload.replicas,
                ready: workload.ready,
            })
        })
        .collect()
}

/// The number of broker models whose state is not `ready`, from llama-swap's
/// `/running` document (`{"running": [{"model": ..., "state": ...}]}`).
#[must_use]
pub fn stuck_models(document: &str) -> Option<u32> {
    let value: Value = serde_json::from_str(document).ok()?;
    let running = value.get("running")?.as_array()?;
    let stuck = running
        .iter()
        .filter(|model| model.get("state").and_then(Value::as_str) != Some("ready"))
        .count();
    u32::try_from(stuck).ok()
}

/// Whether one line of the broker's event stream is a model status change
/// (llama-swap sends `data:{"type":"modelStatus",...}`; its log lines, which
/// include the observer's own status requests, do not wake anything).
#[must_use]
pub fn is_model_status_line(line: &str) -> bool {
    let Some(data) = line.strip_prefix("data:") else {
        return false;
    };
    serde_json::from_str::<Value>(data.trim())
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|kind| kind == "modelStatus")
}

/// Read every fact once.
///
/// # Errors
///
/// The GPU workloads cannot be listed. A missing counter or an unanswered
/// broker is a missing fact, not an error.
pub fn observe<S: Sysfs, K: Kube>(
    config: &PressureConfig,
    sysfs: &S,
    kube: &K,
) -> Result<PressureFacts> {
    let tenants = compute_tenants(&kube.gpu_workloads()?);
    let broker_stuck = config
        .broker
        .as_ref()
        .and_then(|broker| broker_url(kube, broker, BROKER_STATUS_PATH))
        .and_then(|url| fetch(&url))
        .and_then(|document| stuck_models(&document));
    let zero = config
        .guard
        .as_ref()
        .map(|guard| guard_zero(config, guard, kube))
        .unwrap_or_default();
    Ok(PressureFacts {
        memory: card_memory(sysfs, &config.card),
        tenants,
        broker_stuck,
        zero_resources: zero,
    })
}

/// The zero guarded resources, counted only while a plugin pod runs; any
/// failed read counts as nothing zero.
fn guard_zero<K: Kube>(config: &PressureConfig, guard: &GuardConfig, kube: &K) -> Vec<String> {
    let running = kube
        .pod_phases(&guard.namespace, &guard.labels, &config.node)
        .is_ok_and(|phases| phases.iter().any(|phase| phase == "Running"));
    if !running {
        return Vec::new();
    }
    kube.node_allocatable(&config.node)
        .map(|allocatable| zero_resources(&allocatable, &guard.resources))
        .unwrap_or_default()
}

/// One short GET through `curl` (the house HTTP adapter: no TLS stack in the
/// crate, the host's trust store).
fn fetch(url: &str) -> Option<String> {
    let output = Command::new("curl")
        .args(["-s", "--max-time", "3", url])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Start the event sources; every event sends one [`Wake`] on `wake`.
///
/// One thread runs every source on its own runtime and client: the
/// Kubernetes watches (this node's GPU pods, the node, and the broker
/// Service's endpoint slices) and the broker's event stream, read from
/// `curl -N`. The stream is (re)connected when the broker's endpoints change,
/// never on a timer: a broker scaled to zero costs nothing until its pods
/// return. A second thread runs the [`CardSampler`] when it is configured.
/// Everything stops when the receiver is gone.
///
/// # Errors
///
/// The Kubernetes client or the runtime cannot be created.
pub fn watch(config: &PressureConfig, wake: &Sender<Wake>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ClnxError::backend("tokio", error.to_string()))?;
    let client = runtime
        .block_on(Client::try_default())
        .map_err(|error| ClnxError::backend("kubernetes", error.to_string()))?;
    let config = config.clone();
    let sender = wake.clone();
    if let Some(sampler) = config.sampler {
        let card = config.card;
        let sender = wake.clone();
        thread::Builder::new()
            .name("clnx-card".to_owned())
            .spawn(move || sample_card(&card, sampler, &sender))
            .map_err(|error| ClnxError::backend("thread", error.to_string()))?;
    }
    thread::Builder::new()
        .name("clnx-pressure".to_owned())
        .spawn(move || runtime.block_on(watch_sources(client, config, sender)))
        .map_err(|error| ClnxError::backend("thread", error.to_string()))?;
    Ok(())
}

/// The card sampler's loop: two local sysfs reads per interval, a wake-up only
/// when the reading matters.
fn sample_card(card: &DeviceAddress, config: SamplerConfig, wake: &Sender<Wake>) {
    let sysfs = crate::backends::LinuxSysfs::system();
    let mut sampler = CardSampler::new(config);
    loop {
        thread::sleep(config.interval);
        if let Some(sample) = sampler.sample(card_memory(&sysfs, card))
            && wake.send(Wake::Card(sample)).is_err()
        {
            return;
        }
    }
}

/// What one source reported.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// A GPU pod or the node changed.
    Kubernetes,
    /// The broker Service's endpoints changed.
    BrokerEndpoints,
}

async fn watch_sources(client: Client, config: PressureConfig, wake: Sender<Wake>) {
    let (sender, mut sources) = tokio::sync::mpsc::unbounded_channel::<Source>();
    let node = config.node.clone();
    tokio::spawn(watch_kind::<Pod>(
        Api::all(client.clone()),
        Some(format!("spec.nodeName={node}")),
        None,
        sender.clone(),
        Source::Kubernetes,
        pod_requests_gpu,
    ));
    tokio::spawn(watch_kind::<Node>(
        Api::all(client.clone()),
        Some(format!("metadata.name={node}")),
        None,
        sender.clone(),
        Source::Kubernetes,
        |_| true,
    ));
    if let Some(broker) = &config.broker {
        tokio::spawn(watch_kind::<EndpointSlice>(
            Api::namespaced(client.clone(), &broker.namespace),
            None,
            Some(format!("kubernetes.io/service-name={}", broker.name)),
            sender.clone(),
            Source::BrokerEndpoints,
            |_| true,
        ));
    }
    drop(sender);
    let kube = crate::backends::kube::AsyncKube::from_client(client);
    let mut stream: Option<(tokio::process::Child, Lines<BufReader<ChildStdout>>)> = None;
    loop {
        tokio::select! {
            source = sources.recv() => {
                let Some(source) = source else { return };
                if wake.send(Wake::Kubernetes).is_err() {
                    return;
                }
                if matches!(source, Source::BrokerEndpoints) && stream.is_none()
                    && let Some(broker) = &config.broker
                {
                    stream = connect_broker(&kube, broker).await;
                }
            }
            line = next_line(&mut stream), if stream.is_some() => {
                match line {
                    Some(line) => {
                        if is_model_status_line(&line) && wake.send(Wake::Broker).is_err() {
                            return;
                        }
                    }
                    None => {
                        // The stream ended (the broker restarted or left): it is
                        // reconnected at the next change of its endpoints.
                        if let Some((mut child, _)) = stream.take() {
                            let _ = child.kill().await;
                        }
                    }
                }
            }
        }
    }
}

/// Watch one kind with the API server's own watch: list once (the start
/// counts as a change), then follow the watch from the last resource version,
/// resuming from it when the server ends a watch (its timeout) and listing
/// again only when that version has expired. A change that passes `matters`
/// sends `source`. A failed request is retried after a pause; nothing is read
/// on a timer.
async fn watch_kind<K>(
    api: Api<K>,
    fields: Option<String>,
    labels: Option<String>,
    sender: tokio::sync::mpsc::UnboundedSender<Source>,
    source: Source,
    matters: fn(&K) -> bool,
) where
    K: kube::Resource + Clone + std::fmt::Debug + serde::de::DeserializeOwned + Send + 'static,
{
    let mut list_params = ListParams::default();
    let mut watch_params = WatchParams::default();
    if let Some(fields) = &fields {
        list_params = list_params.fields(fields);
        watch_params = watch_params.fields(fields);
    }
    if let Some(labels) = &labels {
        list_params = list_params.labels(labels);
        watch_params = watch_params.labels(labels);
    }
    let pause = || tokio::time::sleep(std::time::Duration::from_secs(5));
    let mut version: Option<String> = None;
    loop {
        let resume = match version.take() {
            Some(resume) => resume,
            None => match api.list(&list_params).await {
                Ok(list) => {
                    if sender.send(source).is_err() {
                        return;
                    }
                    list.metadata.resource_version.unwrap_or_default()
                }
                Err(_) => {
                    pause().await;
                    continue;
                }
            },
        };
        let mut events = match api.watch(&watch_params, &resume).await {
            Ok(events) => events.boxed(),
            Err(_) => {
                pause().await;
                continue;
            }
        };
        let mut last = Some(resume);
        while let Some(event) = events.next().await {
            match event {
                Ok(
                    WatchEvent::Added(object)
                    | WatchEvent::Modified(object)
                    | WatchEvent::Deleted(object),
                ) => {
                    last = object.meta().resource_version.clone().or(last);
                    if matters(&object) && sender.send(source).is_err() {
                        return;
                    }
                }
                Ok(WatchEvent::Bookmark(bookmark)) => {
                    last = Some(bookmark.metadata.resource_version);
                }
                // 410 Gone (the version expired) or a broken stream: list again.
                Ok(WatchEvent::Error(_)) | Err(_) => {
                    last = None;
                    break;
                }
            }
        }
        version = last;
    }
}

async fn connect_broker(
    kube: &crate::backends::kube::AsyncKube,
    broker: &BrokerConfig,
) -> Option<(tokio::process::Child, Lines<BufReader<ChildStdout>>)> {
    let address = kube
        .service_address(&broker.namespace, &broker.name)
        .await
        .ok()??;
    let url = format!("http://{address}:{}{BROKER_EVENTS_PATH}", broker.port);
    let mut child = tokio::process::Command::new("curl")
        .args(["-s", "-N", url.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    Some((child, BufReader::new(stdout).lines()))
}

async fn next_line(
    stream: &mut Option<(tokio::process::Child, Lines<BufReader<ChildStdout>>)>,
) -> Option<String> {
    let (_, lines) = stream.as_mut()?;
    lines.next_line().await.ok().flatten()
}

fn pod_requests_gpu(pod: &Pod) -> bool {
    let Some(spec) = &pod.spec else {
        return false;
    };
    spec.containers
        .iter()
        .chain(spec.init_containers.iter().flatten())
        .filter_map(|container| container.resources.as_ref())
        .flat_map(|resources| {
            resources
                .limits
                .iter()
                .flatten()
                .chain(resources.requests.iter().flatten())
                .map(|(key, _)| key.clone())
        })
        .any(|key| key.starts_with("devic.es/rocm") || key.contains("gpu"))
}
