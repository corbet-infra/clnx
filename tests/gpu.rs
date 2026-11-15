//! Host GPU tenants and Sunshine following the card.

use std::cell::RefCell;
use std::rc::Rc;

use cdrv::traits::HostGpu;
use cdrv::{GpuLadder, InstanceId, KernelOwner};
use clnx::{Kube, LinuxGpu, Systemd, TenantConfig, Workload, WorkloadKind};

type Calls = Rc<RefCell<Vec<String>>>;

fn calls() -> Calls {
    Rc::new(RefCell::new(Vec::new()))
}

struct FakeSystemd {
    calls: Calls,
}

impl Systemd for FakeSystemd {
    fn unit_active(&self, _unit: &str) -> clnx::Result<bool> {
        Ok(true)
    }

    fn start_unit(&self, unit: &str) -> clnx::Result<()> {
        self.calls.borrow_mut().push(format!("start:{unit}"));
        Ok(())
    }

    fn stop_unit(&self, unit: &str) -> clnx::Result<()> {
        self.calls.borrow_mut().push(format!("stop:{unit}"));
        Ok(())
    }

    fn start_user_unit(&self, unit: &str) -> clnx::Result<()> {
        self.calls.borrow_mut().push(format!("start_user:{unit}"));
        Ok(())
    }

    fn stop_user_unit(&self, unit: &str) -> clnx::Result<()> {
        self.calls.borrow_mut().push(format!("stop_user:{unit}"));
        Ok(())
    }
}

struct FakeKube {
    workloads: Vec<Workload>,
    calls: Calls,
}

impl Kube for FakeKube {
    fn gpu_workloads(&self) -> clnx::Result<Vec<Workload>> {
        Ok(self.workloads.clone())
    }

    fn set_replicas(&self, workload: &Workload, replicas: u32) -> clnx::Result<()> {
        self.calls.borrow_mut().push(format!(
            "scale:{}/{}:{replicas}",
            workload.namespace, workload.name
        ));
        Ok(())
    }

    fn set_skip_reconcile(&self, namespace: &str, skip: bool) -> clnx::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("skip:{namespace}:{skip}"));
        Ok(())
    }

    fn delete_pods(&self, namespace: &str, labels: &str, node: &str) -> clnx::Result<u32> {
        self.calls
            .borrow_mut()
            .push(format!("delete:{namespace}:{labels}:{node}"));
        Ok(1)
    }
}

fn workload(namespace: &str, name: &str, replicas: u32) -> Workload {
    Workload {
        namespace: namespace.to_owned(),
        kind: WorkloadKind::Deployment,
        name: name.to_owned(),
        replicas,
        ready: replicas,
        ladder: None,
        compute: true,
    }
}

fn driver(
    workloads: Vec<Workload>,
    config: TenantConfig,
) -> (LinuxGpu<FakeSystemd, FakeKube>, Calls, Calls) {
    let systemd_calls = calls();
    let kube_calls = calls();
    let gpu = LinuxGpu::new(
        FakeSystemd {
            calls: Rc::clone(&systemd_calls),
        },
        FakeKube {
            workloads,
            calls: Rc::clone(&kube_calls),
        },
        config,
    );
    (gpu, systemd_calls, kube_calls)
}

fn default_driver(workloads: Vec<Workload>) -> (LinuxGpu<FakeSystemd, FakeKube>, Calls, Calls) {
    driver(workloads, TenantConfig::default())
}

#[test]
fn a_pod_tenant_is_scaled_to_zero_behind_argo_and_restored_to_its_own_count() {
    let (gpu, _systemd, kube) = default_driver(vec![workload("llm", "inference", 2)]);
    let tenant = "llm/inference";

    gpu.stop_tenant(tenant, GpuLadder::BestEffort).unwrap();
    assert_eq!(
        kube.borrow().clone(),
        vec![
            "skip:llm:true".to_owned(),
            "scale:llm/inference:0".to_owned()
        ]
    );
    kube.borrow_mut().clear();

    gpu.start_tenant(tenant, GpuLadder::BestEffort, 2).unwrap();
    assert_eq!(
        kube.borrow().clone(),
        vec![
            "scale:llm/inference:2".to_owned(),
            "skip:llm:false".to_owned()
        ]
    );
}

#[test]
fn the_desktop_tenant_is_its_session_unit_not_a_pod() {
    let (gpu, systemd, kube) = default_driver(vec![workload("llm", "inference", 1)]);

    gpu.stop_tenant("desktop", GpuLadder::Desktop).unwrap();
    assert_eq!(
        systemd.borrow().clone(),
        vec!["stop:desktop-session.service".to_owned()]
    );
    gpu.start_tenant("desktop", GpuLadder::Desktop, 1).unwrap();
    assert_eq!(
        systemd.borrow().clone(),
        vec![
            "stop:desktop-session.service".to_owned(),
            "start:desktop-session.service".to_owned()
        ]
    );
    assert!(kube.borrow().is_empty());
}

#[test]
fn sunshine_runs_on_the_kernel_that_holds_the_card() {
    let (gpu, systemd, _kube) = default_driver(Vec::new());

    gpu.sunshine_follows(&KernelOwner::Host).unwrap();
    gpu.sunshine_follows(&KernelOwner::Guest(InstanceId::from("desktop-mac")))
        .unwrap();

    assert_eq!(
        systemd.borrow().clone(),
        vec![
            "start_user:sunshine.service".to_owned(),
            "stop_user:sunshine.service".to_owned()
        ]
    );
}

#[test]
fn sunshine_can_be_a_system_unit_instead() {
    let config = TenantConfig {
        sunshine_in_user_manager: false,
        ..TenantConfig::default()
    };
    let (gpu, systemd, _kube) = driver(Vec::new(), config);
    gpu.sunshine_follows(&KernelOwner::Host).unwrap();
    gpu.sunshine_follows(&KernelOwner::Guest(InstanceId::from("desktop-win")))
        .unwrap();
    assert_eq!(
        systemd.borrow().clone(),
        vec![
            "start:sunshine.service".to_owned(),
            "stop:sunshine.service".to_owned()
        ]
    );
}

#[test]
fn sunshine_inside_the_desktop_session_needs_no_unit_call() {
    let config = TenantConfig {
        sunshine_unit: None,
        ..TenantConfig::default()
    };
    let (gpu, systemd, _kube) = driver(Vec::new(), config);
    gpu.sunshine_follows(&KernelOwner::Host).unwrap();
    gpu.sunshine_follows(&KernelOwner::Guest(InstanceId::from("desktop-win")))
        .unwrap();
    assert!(systemd.borrow().is_empty());
}

#[test]
fn an_unknown_tenant_is_refused_without_touching_the_cluster() {
    let (gpu, _systemd, kube) = default_driver(vec![workload("llm", "inference", 1)]);
    let error = gpu
        .stop_tenant("llm/absent", GpuLadder::Interactive)
        .unwrap_err();
    assert!(error.to_string().contains("no GPU workload"));
    assert!(kube.borrow().is_empty());
}

#[test]
fn a_name_in_two_namespaces_must_be_qualified() {
    let (gpu, _systemd, kube) = default_driver(vec![
        workload("llm", "inference", 1),
        workload("vision", "inference", 1),
    ]);
    let error = gpu
        .stop_tenant("inference", GpuLadder::Interactive)
        .unwrap_err();
    assert!(error.to_string().contains("namespace/name"));
    assert!(kube.borrow().is_empty());
}

#[test]
fn tenant_replicas_reads_the_cluster() {
    let (gpu, _systemd, _kube) = default_driver(vec![workload("llm", "inference", 3)]);
    assert_eq!(
        gpu.tenant_replicas("llm/inference", GpuLadder::Interactive)
            .unwrap(),
        3
    );
    assert_eq!(
        gpu.tenant_replicas("desktop", GpuLadder::Desktop).unwrap(),
        1
    );
}

#[test]
fn a_fresh_driver_restores_the_given_count_after_a_crash() {
    // The workload sits at zero and this driver never saw the stop.
    let (gpu, _systemd, kube) = default_driver(vec![workload("llm", "inference", 0)]);
    gpu.start_tenant("llm/inference", GpuLadder::BestEffort, 3)
        .unwrap();
    assert!(
        kube.borrow().iter().any(|c| c.contains('3')),
        "{:?}",
        kube.borrow()
    );
}

#[test]
fn starting_with_zero_replicas_is_refused() {
    let (gpu, _systemd, kube) = default_driver(vec![workload("llm", "inference", 0)]);
    assert!(
        gpu.start_tenant("llm/inference", GpuLadder::BestEffort, 0)
            .is_err()
    );
    assert!(kube.borrow().is_empty());
}

#[test]
fn the_device_plugin_restart_deletes_its_pods_on_this_node() {
    let config = TenantConfig {
        device_plugin: Some(clnx::DevicePlugin {
            namespace: "kube-system".to_owned(),
            labels: "app=example-device-plugin".to_owned(),
            node: "example-node".to_owned(),
        }),
        ..TenantConfig::default()
    };
    let (gpu, _systemd, kube) = driver(Vec::new(), config);
    gpu.restart_device_plugin().unwrap();
    assert_eq!(
        kube.borrow().clone(),
        vec!["delete:kube-system:app=example-device-plugin:example-node".to_owned()]
    );
}

#[test]
fn a_host_without_a_configured_device_plugin_refuses_the_restart() {
    let (gpu, _systemd, kube) = driver(Vec::new(), TenantConfig::default());
    assert!(gpu.restart_device_plugin().is_err());
    assert!(kube.borrow().is_empty());
}
