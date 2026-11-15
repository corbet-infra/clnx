//! The Kubernetes seam, realised with the `kube` crate.
//!
//! The GPU platform's host tenants are workloads whose pod template requests a
//! GPU resource. Taking one off the card is always two acts: tell Argo CD not
//! to heal the change, then scale the workload to zero. Returning it reverses
//! both in the same order. Workloads are read as dynamic objects so the driver
//! carries no generated Kubernetes types and no cluster-specific import.

use std::collections::BTreeMap;

use kube::Client;
use kube::api::{
    Api, ApiResource, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
};
use serde_json::{Value, json};

use crate::error::{ClnxError, Result};

/// The workload kinds that can own a GPU pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    /// A Deployment.
    Deployment,
    /// A StatefulSet.
    StatefulSet,
}

impl WorkloadKind {
    /// The `apps/v1` group-version-kind of this workload kind.
    #[must_use]
    pub fn gvk(self) -> GroupVersionKind {
        let kind = match self {
            Self::Deployment => "Deployment",
            Self::StatefulSet => "StatefulSet",
        };
        GroupVersionKind::gvk("apps", "v1", kind)
    }
}

/// One workload that requests a GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workload {
    /// The namespace it lives in.
    pub namespace: String,
    /// Its kind.
    pub kind: WorkloadKind,
    /// Its name.
    pub name: String,
    /// The replicas it asks for now.
    pub replicas: u32,
    /// The replicas that are ready now.
    pub ready: u32,
    /// The ladder rung its pod template's priority class names, when it is one.
    pub ladder: Option<cdrv::GpuLadder>,
    /// Whether it asks for compute (a video-engine-only workload does not).
    pub compute: bool,
}

/// The Kubernetes operations the host GPU platform needs.
pub trait Kube {
    /// Every workload that requests a GPU resource, across all namespaces.
    fn gpu_workloads(&self) -> Result<Vec<Workload>>;

    /// Set a workload's replica count.
    fn set_replicas(&self, workload: &Workload, replicas: u32) -> Result<()>;

    /// Set or clear Argo CD's skip-reconcile annotation on the Application
    /// that deploys a namespace. A cluster without that Application is not an
    /// error: there is nothing to pause.
    fn set_skip_reconcile(&self, namespace: &str, skip: bool) -> Result<()>;

    /// The cluster IP of a Service, `None` when it has none (absent, headless).
    fn service_address(&self, namespace: &str, name: &str) -> Result<Option<String>> {
        let _ = (namespace, name);
        Ok(None)
    }

    /// A node's allocatable resources, name to quantity.
    fn node_allocatable(&self, node: &str) -> Result<BTreeMap<String, String>> {
        let _ = node;
        Ok(BTreeMap::new())
    }

    /// The phases of the pods matching a label selector on one node.
    fn pod_phases(&self, namespace: &str, labels: &str, node: &str) -> Result<Vec<String>> {
        let _ = (namespace, labels, node);
        Ok(Vec::new())
    }

    /// Delete the pods matching a label selector on one node (their controller
    /// starts fresh ones); returns how many were deleted.
    fn delete_pods(&self, namespace: &str, labels: &str, node: &str) -> Result<u32> {
        let _ = (namespace, labels, node);
        Err(ClnxError::Refused(
            "this Kubernetes seam cannot delete pods".to_owned(),
        ))
    }
}

/// The real Kubernetes client.
pub struct KubeClient {
    runtime: tokio::runtime::Runtime,
    client: Client,
    argocd_namespace: String,
}

impl std::fmt::Debug for KubeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KubeClient")
            .field("argocd_namespace", &self.argocd_namespace)
            .finish_non_exhaustive()
    }
}

impl KubeClient {
    /// Connect with the in-cluster or kubeconfig configuration and Argo CD in
    /// its conventional `argocd` namespace.
    ///
    /// # Errors
    ///
    /// Fails when no configuration is found or the runtime cannot be created.
    pub fn connect() -> Result<Self> {
        Self::with_argocd_namespace("argocd")
    }

    /// Connect with an explicit Argo CD namespace.
    ///
    /// # Errors
    ///
    /// Fails when no configuration is found or the runtime cannot be created.
    pub fn with_argocd_namespace(argocd_namespace: impl Into<String>) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| ClnxError::backend("tokio", error.to_string()))?;
        let client = runtime
            .block_on(Client::try_default())
            .map_err(kube_error)?;
        Ok(Self {
            runtime,
            client,
            argocd_namespace: argocd_namespace.into(),
        })
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

/// The Kubernetes reads an async caller (the pressure watch) makes on a client
/// it already runs, without a runtime of its own.
pub struct AsyncKube {
    client: Client,
}

impl AsyncKube {
    /// Use an existing client.
    #[must_use]
    pub fn from_client(client: Client) -> Self {
        Self { client }
    }

    /// The cluster IP of a Service, `None` when it has none (absent, headless).
    ///
    /// # Errors
    ///
    /// The API request fails.
    pub async fn service_address(&self, namespace: &str, name: &str) -> Result<Option<String>> {
        service_address(&self.client, namespace, name).await
    }
}

async fn service_address(client: &Client, namespace: &str, name: &str) -> Result<Option<String>> {
    let api: Api<k8s_openapi::api::core::v1::Service> = Api::namespaced(client.clone(), namespace);
    Ok(api
        .get_opt(name)
        .await
        .map_err(kube_error)?
        .and_then(|service| service.spec)
        .and_then(|spec| spec.cluster_ip)
        .filter(|address| !address.is_empty() && address != "None"))
}

impl Kube for KubeClient {
    fn gpu_workloads(&self) -> Result<Vec<Workload>> {
        let mut workloads =
            self.block_on(list_gpu_workloads(&self.client, WorkloadKind::Deployment))?;
        workloads
            .extend(self.block_on(list_gpu_workloads(&self.client, WorkloadKind::StatefulSet))?);
        workloads.sort_by(|a, b| {
            (&a.namespace, a.kind.gvk().kind, &a.name).cmp(&(
                &b.namespace,
                b.kind.gvk().kind,
                &b.name,
            ))
        });
        Ok(workloads)
    }

    fn set_replicas(&self, workload: &Workload, replicas: u32) -> Result<()> {
        self.block_on(set_replicas(&self.client, workload, replicas))
    }

    fn set_skip_reconcile(&self, namespace: &str, skip: bool) -> Result<()> {
        self.block_on(set_skip_reconcile(
            &self.client,
            &self.argocd_namespace,
            namespace,
            skip,
        ))
    }

    fn service_address(&self, namespace: &str, name: &str) -> Result<Option<String>> {
        self.block_on(service_address(&self.client, namespace, name))
    }

    fn node_allocatable(&self, node: &str) -> Result<BTreeMap<String, String>> {
        let api: Api<k8s_openapi::api::core::v1::Node> = Api::all(self.client.clone());
        let node = self.block_on(api.get(node)).map_err(kube_error)?;
        Ok(node
            .status
            .and_then(|status| status.allocatable)
            .unwrap_or_default()
            .into_iter()
            .map(|(name, quantity)| (name, quantity.0))
            .collect())
    }

    fn pod_phases(&self, namespace: &str, labels: &str, node: &str) -> Result<Vec<String>> {
        let api: Api<k8s_openapi::api::core::v1::Pod> =
            Api::namespaced(self.client.clone(), namespace);
        let params = ListParams::default()
            .labels(labels)
            .fields(&format!("spec.nodeName={node}"));
        let list = self.block_on(api.list(&params)).map_err(kube_error)?;
        Ok(list
            .items
            .into_iter()
            .filter_map(|pod| pod.status.and_then(|status| status.phase))
            .collect())
    }

    fn delete_pods(&self, namespace: &str, labels: &str, node: &str) -> Result<u32> {
        let api: Api<k8s_openapi::api::core::v1::Pod> =
            Api::namespaced(self.client.clone(), namespace);
        let params = ListParams::default()
            .labels(labels)
            .fields(&format!("spec.nodeName={node}"));
        let list = self.block_on(api.list(&params)).map_err(kube_error)?;
        let mut deleted = 0_u32;
        for pod in list.items {
            let Some(name) = pod.metadata.name else {
                continue;
            };
            self.block_on(api.delete(&name, &kube::api::DeleteParams::default()))
                .map_err(kube_error)?;
            deleted = deleted.saturating_add(1);
        }
        Ok(deleted)
    }
}

async fn list_gpu_workloads(client: &Client, kind: WorkloadKind) -> Result<Vec<Workload>> {
    let resource = ApiResource::from_gvk(&kind.gvk());
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &resource);
    let list = api.list(&ListParams::default()).await.map_err(kube_error)?;
    let mut workloads = Vec::new();
    for object in list.items {
        if !requests_gpu(&object.data) {
            continue;
        }
        let namespace = object.metadata.namespace.unwrap_or_default();
        let name = object.metadata.name.unwrap_or_default();
        if namespace.is_empty() || name.is_empty() {
            continue;
        }
        let replicas = object
            .data
            .pointer("/spec/replicas")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let ready = object
            .data
            .pointer("/status/readyReplicas")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let ladder = object
            .data
            .pointer("/spec/template/spec/priorityClassName")
            .and_then(Value::as_str)
            .and_then(ladder_of);
        workloads.push(Workload {
            namespace,
            kind,
            name,
            replicas: u32::try_from(replicas).unwrap_or(u32::MAX),
            ready: u32::try_from(ready).unwrap_or(u32::MAX),
            ladder,
            compute: requests_compute(&object.data),
        });
    }
    Ok(workloads)
}

async fn set_replicas(client: &Client, workload: &Workload, replicas: u32) -> Result<()> {
    let resource = ApiResource::from_gvk(&workload.kind.gvk());
    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), &workload.namespace, &resource);
    let patch = json!({ "spec": { "replicas": replicas } });
    api.patch(
        &workload.name,
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .map(|_object| ())
    .map_err(kube_error)
}

async fn set_skip_reconcile(
    client: &Client,
    argocd_namespace: &str,
    namespace: &str,
    skip: bool,
) -> Result<()> {
    let resource = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("argoproj.io", "v1alpha1", "Application"),
        "applications",
    );
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), argocd_namespace, &resource);
    let value = if skip { json!("true") } else { Value::Null };
    let patch = json!({
        "metadata": { "annotations": { "argocd.argoproj.io/skip-reconcile": value } }
    });
    match api
        .patch(namespace, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_object) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
        Err(error) => Err(kube_error(error)),
    }
}

/// Whether a workload's pod template requests a GPU resource.
///
/// Mirrors the proven GPU tool: a `devic.es/*` resource other than `kvm`, or
/// any resource key that names a GPU, in limits or requests of any container
/// or init container.
/// The ladder rung a priority class names (`gpu-desktop`, `gpu-interactive`,
/// `gpu-besteffort`); any other class is not a rung.
fn ladder_of(class: &str) -> Option<cdrv::GpuLadder> {
    serde_json::from_value(Value::String(class.to_owned())).ok()
}

/// Whether a pod template asks for GPU compute: a GPU resource other than the
/// video engine (`devic.es/vcn`) and the KVM device.
fn requests_compute(data: &Value) -> bool {
    resource_keys(data).any(|key| {
        (key.starts_with("devic.es/") && key != "devic.es/kvm" && key != "devic.es/vcn")
            || key.to_ascii_lowercase().contains("gpu")
    })
}

fn resource_keys(data: &Value) -> impl Iterator<Item = String> + '_ {
    let containers = data
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let init_containers = data
        .pointer("/spec/template/spec/initContainers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    containers.chain(init_containers).flat_map(|container| {
        ["limits", "requests"].into_iter().flat_map(move |section| {
            container
                .pointer(&format!("/resources/{section}"))
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|resources| resources.keys().cloned())
        })
    })
}

fn requests_gpu(data: &Value) -> bool {
    let containers = data
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let init_containers = data
        .pointer("/spec/template/spec/initContainers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    containers.chain(init_containers).any(|container| {
        ["limits", "requests"].iter().any(|section| {
            container
                .pointer(&format!("/resources/{section}"))
                .and_then(Value::as_object)
                .is_some_and(|resources| {
                    resources.keys().any(|key| {
                        (key.starts_with("devic.es/") && key != "devic.es/kvm")
                            || key.to_ascii_lowercase().contains("gpu")
                    })
                })
        })
    })
}

fn kube_error(error: kube::Error) -> ClnxError {
    ClnxError::backend("kubernetes", error.to_string())
}
