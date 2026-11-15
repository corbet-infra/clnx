//! The four seams between the driver logic and the host.
//!
//! Each is a small trait with a real implementation beside it, so every
//! driver module is testable with a fake (`Libvirt`, `Systemd`, `Kube`) or a
//! fixture tree (`Sysfs`, plus the `Kernel` seam for `modprobe`).

pub mod kernel;
pub mod kube;
pub mod libvirt;
pub mod sysfs;
pub mod systemd;

pub use kernel::{Kernel, LinuxKernel};
pub use kube::{Kube, KubeClient, Workload, WorkloadKind};
pub use libvirt::{DomainEventSource, Libvirt, NoDomainEvents, RealLibvirt};
pub use sysfs::{LinuxSysfs, Sysfs};
pub use systemd::{Systemd, ZbusSystemd};
