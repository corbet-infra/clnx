//! The seams between the driver logic and the host.
//!
//! Each is a small trait with a real implementation beside it, so every
//! driver module is testable with a fake (`Libvirt`, `Systemd`, `Kube`, `Procs`) or a
//! fixture tree (`Sysfs`, plus the `Kernel` seam for `modprobe`).

pub mod command;
pub mod kernel;
pub mod kube;
pub mod libvirt;
pub mod procs;
pub mod qemu_events;
pub mod sysfs;
pub mod systemd;
pub mod virsh_events;

pub use command::{CommandRunner, ProcessRunner};
pub use kernel::{Kernel, LinuxKernel};
pub use kube::{Kube, KubeClient, Workload, WorkloadKind};
pub use libvirt::{DomainEventSource, Libvirt, NoDomainEvents, RealLibvirt};
pub use procs::{DeviceNode, Holder, LinuxProcs, Procs, makedev};
pub use qemu_events::{GuestShutdown, QemuShutdownEvents, parse_shutdown_line};
pub use sysfs::{LinuxSysfs, Sysfs};
pub use systemd::{Systemd, ZbusSystemd};
pub use virsh_events::VirshEvents;
