//! The host-side GPU platform: tenants on the card, and Sunshine following it.
//!
//! The card belongs to exactly one kernel. While the host kernel holds it,
//! the desktop session and the GPU pods share it by the ladder: the desktop
//! session is stopped at its unit (archlxc itself keeps running), a pod is
//! scaled to zero behind Argo CD's skip-reconcile. The replicas a tenant had
//! are remembered so returning it restores the platform's shape rather than a
//! default. Sunshine runs on whichever kernel holds the card.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use cdrv::traits::{HostGpu, Result as CdrvResult};
use cdrv::{GpuLadder, KernelOwner};

use crate::backends::{Kube, Systemd, Workload};
use crate::error::{ClnxError, Result};

/// The units and naming a deployment supplies.
///
/// The defaults are synthetic: a public repository carries no host names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantConfig {
    /// The unit that owns the desktop session holding the card.
    pub desktop_unit: String,
    /// The unit that runs Sunshine on the host.
    pub sunshine_unit: String,
    /// Whether Sunshine is a user unit rather than a system unit.
    pub sunshine_in_user_manager: bool,
}

impl Default for TenantConfig {
    fn default() -> Self {
        Self {
            desktop_unit: "desktop-session.service".to_owned(),
            sunshine_unit: "sunshine.service".to_owned(),
            sunshine_in_user_manager: true,
        }
    }
}

/// The Linux host GPU driver over systemd and Kubernetes.
#[derive(Debug)]
pub struct LinuxGpu<S: Systemd, K: Kube> {
    systemd: S,
    kube: K,
    config: TenantConfig,
    stopped: Mutex<HashMap<String, u32>>,
}

impl<S: Systemd, K: Kube> LinuxGpu<S, K> {
    /// A driver with the deployment's unit names.
    #[must_use]
    pub fn new(systemd: S, kube: K, config: TenantConfig) -> Self {
        Self {
            systemd,
            kube,
            config,
            stopped: Mutex::new(HashMap::new()),
        }
    }

    /// The configuration this driver uses.
    #[must_use]
    pub fn config(&self) -> &TenantConfig {
        &self.config
    }

    fn stopped(&self) -> MutexGuard<'_, HashMap<String, u32>> {
        self.stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn workload(&self, tenant: &str) -> Result<Workload> {
        let workloads = self.kube.gpu_workloads()?;
        let matches: Vec<&Workload> = match tenant.split_once('/') {
            Some((namespace, name)) => workloads
                .iter()
                .filter(|workload| workload.namespace == namespace && workload.name == name)
                .collect(),
            None => workloads
                .iter()
                .filter(|workload| workload.name == tenant)
                .collect(),
        };
        match matches.as_slice() {
            [workload] => Ok((**workload).clone()),
            [] => Err(ClnxError::Refused(format!(
                "no GPU workload matches {tenant}"
            ))),
            _ => Err(ClnxError::Refused(format!(
                "{tenant} matches more than one GPU workload; name it namespace/name"
            ))),
        }
    }

    fn sunshine(&self, start: bool) -> Result<()> {
        let unit = self.config.sunshine_unit.as_str();
        if start {
            if self.config.sunshine_in_user_manager {
                self.systemd.start_user_unit(unit)
            } else {
                self.systemd.start_unit(unit)
            }
        } else if self.config.sunshine_in_user_manager {
            self.systemd.stop_user_unit(unit)
        } else {
            self.systemd.stop_unit(unit)
        }
    }
}

impl<S: Systemd, K: Kube> HostGpu for LinuxGpu<S, K> {
    fn stop_tenant(&self, tenant: &str, ladder: GpuLadder) -> CdrvResult<()> {
        match ladder {
            GpuLadder::Desktop => {
                self.systemd.stop_unit(&self.config.desktop_unit)?;
            }
            GpuLadder::Interactive | GpuLadder::BestEffort => {
                let workload = self.workload(tenant)?;
                self.kube.set_skip_reconcile(&workload.namespace, true)?;
                self.stopped().insert(tenant.to_owned(), workload.replicas);
                self.kube.set_replicas(&workload, 0)?;
            }
        }
        Ok(())
    }

    fn start_tenant(&self, tenant: &str, ladder: GpuLadder) -> CdrvResult<()> {
        match ladder {
            GpuLadder::Desktop => {
                self.systemd.start_unit(&self.config.desktop_unit)?;
            }
            GpuLadder::Interactive | GpuLadder::BestEffort => {
                let workload = self.workload(tenant)?;
                let replicas = self.stopped().remove(tenant).unwrap_or(1);
                self.kube.set_replicas(&workload, replicas)?;
                self.kube.set_skip_reconcile(&workload.namespace, false)?;
            }
        }
        Ok(())
    }

    fn sunshine_follows(&self, kernel: &KernelOwner) -> CdrvResult<()> {
        self.sunshine(kernel.is_host())?;
        Ok(())
    }
}
