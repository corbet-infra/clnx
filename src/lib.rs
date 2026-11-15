//! Linux host drivers for the VM/GPU system.
//!
//! `clnx` implements the `cdrv` host traits on Linux and holds no decisions:
//! the conductor decides admission and ownership; this crate carries them out
//! and reports facts.
//!
//! - [`instances`] defines and runs libvirt domains, rendered from the one
//!   source ([`InstanceSpec`](cdrv::InstanceSpec)), stops them cleanly and
//!   translates lifecycle events;
//! - [`devices`] reads PCI functions and their live IOMMU groups from sysfs,
//!   refuses passthrough when a group holds an undeclared device, binds and
//!   returns the card in the proven order, and reports BAR sizes;
//! - [`memory`] reports `MemAvailable` plus the evictable ARC and the memory
//!   PSI share, and moves a balloon;
//! - [`gpu`] takes the desktop session and the GPU pods off the card and
//!   moves Sunshine to whichever kernel holds it.
//!
//! Every host boundary is a small trait in [`backends`] (`Libvirt`, `Sysfs`,
//! `Kernel`, `Systemd`, `Kube`, `Procs`), so the logic is tested with fakes and fixture
//! trees. This crate must never decide admission, hold a claim, poll for
//! state, or destroy a guest outside the contract's explicit destroy. The
//! README lists what only a real host can prove and the contract changes this
//! driver needs.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backends;
pub mod devices;
pub mod domain_xml;
pub mod error;
pub mod events;
pub mod gpu;
pub mod instances;
pub mod memory;

mod proc;

pub use backends::{
    CommandRunner, DeviceNode, DomainEventSource, Holder, Kernel, Kube, KubeClient, Libvirt,
    LinuxKernel, LinuxProcs, LinuxSysfs, NoDomainEvents, ProcessRunner, Procs, RealLibvirt, Sysfs,
    Systemd, Workload, WorkloadKind, ZbusSystemd, makedev,
};
pub use devices::{Bar0Size, BarLimit, LinuxDevices, bdf, parse_bdf};
pub use domain_xml::{RenderOptions, is_fixed, render_domain};
pub use error::{ClnxError, Result};
pub use events::{DomainEvent, DomainEventKind, StartDetail, StopDetail, now_timestamp, translate};
pub use gpu::{LinuxGpu, TenantConfig};
pub use instances::LinuxInstances;
pub use memory::{LinuxMemory, SEVERE_PRESSURE_AVG10};
