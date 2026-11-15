//! PCI functions, live IOMMU groups and the VFIO bind.
//!
//! The declared set is *which* functions form the card (owned by the hardware
//! layer, supplied by the caller). The group is *read live*: firmware decides
//! it, so passthrough is refused whenever a declared function shares its live
//! IOMMU group with a device outside the declared set. Nothing is inferred
//! from a declaration.
//!
//! The bind order is the one the proven GPU tool uses: audio, USB and i2c
//! functions first, the GPU function last; the return unbinds the GPU first
//! and probes the others back afterwards. A failure is reported and never
//! retried.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use cdrv::traits::{HostDevices, Result as CdrvResult};
use cdrv::{DeviceAddress, DeviceClass, DeviceFacts, DeviceFilter};

use crate::backends::{Kernel, LinuxProcs, Procs, Sysfs};
use crate::error::{ClnxError, Result};

/// The size of a device's first BAR, as sysfs reports it.
///
/// This is a fact, not a policy: a caller decides whether it is a ReBAR-sized
/// window. [`Self::is_large`] names the usual threshold only as a convenience.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar0Size {
    /// The size in bytes; zero when the BAR is unassigned.
    pub bytes: u64,
}

impl Bar0Size {
    /// The size in whole GiB.
    #[must_use]
    pub const fn gib(self) -> u64 {
        self.bytes / (1 << 30)
    }

    /// Whether the BAR is at least 1 GiB, the smallest window called ReBAR.
    #[must_use]
    pub const fn is_large(self) -> bool {
        self.bytes >= (1 << 30)
    }
}

/// The Linux device driver: live sysfs facts plus the passthrough bind.
#[derive(Debug, Clone)]
pub struct LinuxDevices<S: Sysfs, K: Kernel, P: Procs = LinuxProcs> {
    sysfs: S,
    kernel: K,
    procs: P,
    functions: Vec<DeviceAddress>,
    host_driver: String,
    holder_wait: Duration,
}

/// How long the hand-over waits for holders of the card to leave.
pub const DEFAULT_HOLDER_WAIT: Duration = Duration::from_secs(60);

impl<S: Sysfs, K: Kernel> LinuxDevices<S, K> {
    /// A driver for one declared card.
    ///
    /// `functions` is the set the hardware layer says forms the card;
    /// `host_driver` is the driver the card returns to (the GPU driver).
    #[must_use]
    pub fn new(
        sysfs: S,
        kernel: K,
        functions: Vec<DeviceAddress>,
        host_driver: impl Into<String>,
    ) -> Self {
        Self {
            sysfs,
            kernel,
            procs: LinuxProcs::system(),
            functions,
            host_driver: host_driver.into(),
            holder_wait: DEFAULT_HOLDER_WAIT,
        }
    }
}

impl<S: Sysfs, K: Kernel, P: Procs> LinuxDevices<S, K, P> {
    /// Use another process table (a fake in tests).
    #[must_use]
    pub fn with_procs<Q: Procs>(self, procs: Q) -> LinuxDevices<S, K, Q> {
        LinuxDevices {
            sysfs: self.sysfs,
            kernel: self.kernel,
            procs,
            functions: self.functions,
            host_driver: self.host_driver,
            holder_wait: self.holder_wait,
        }
    }

    /// How long the hand-over waits for holders to leave (default 60 s).
    #[must_use]
    pub fn with_holder_wait(mut self, wait: Duration) -> Self {
        self.holder_wait = wait;
        self
    }

    /// The declared function set.
    #[must_use]
    pub fn functions(&self) -> &[DeviceAddress] {
        &self.functions
    }

    /// The BAR0 size of a declared function, as a reported fact.
    ///
    /// # Errors
    ///
    /// Fails when the device is not declared or its resource file is absent.
    pub fn bar0_size(&self, device: &DeviceAddress) -> Result<Bar0Size> {
        self.ensure_declared(device)?;
        let id = bdf(device);
        let bytes = self
            .sysfs
            .file_len(&format!("bus/pci/devices/{id}/resource0"))?;
        Ok(Bar0Size { bytes })
    }

    /// The device nodes of the card: the DRM nodes of every declared
    /// function, plus `/dev/kfd` for the amdgpu driver.
    fn device_nodes(&self) -> Result<Vec<PathBuf>> {
        let mut nodes = Vec::new();
        for device in &self.functions {
            let names = self
                .sysfs
                .list(&format!("bus/pci/devices/{}/drm", bdf(device)))?;
            for name in names {
                if name.starts_with("card") || name.starts_with("renderD") {
                    nodes.push(PathBuf::from("/dev/dri").join(name));
                }
            }
        }
        if self.host_driver == "amdgpu" {
            nodes.push(PathBuf::from("/dev/kfd"));
        }
        Ok(nodes)
    }

    /// Refuse the hand-over while a process holds the card or the host
    /// driver is still in use. Waits once for the holders to leave; kills
    /// nothing.
    fn ensure_released(&self) -> Result<()> {
        let nodes = self.device_nodes()?;
        let mut holders = self.procs.holders(&nodes);
        if !holders.is_empty() {
            let pids: Vec<u32> = holders.iter().map(|holder| holder.pid).collect();
            self.procs.wait_exit(&pids, self.holder_wait);
            holders = self.procs.holders(&nodes);
        }
        if !holders.is_empty() {
            let named: Vec<String> = holders
                .iter()
                .map(|holder| format!("{} {} {}", holder.pid, holder.comm, holder.node.display()))
                .collect();
            return Err(ClnxError::Refused(format!(
                "the card is still held open: {}",
                named.join(", ")
            )));
        }
        let module = self.host_driver.replace('-', "_");
        let count = self.sysfs.read(&format!("module/{module}/refcnt"))?;
        if count.trim() != "0" {
            return Err(ClnxError::Refused(format!(
                "{} is still in use (module use count {})",
                self.host_driver,
                count.trim()
            )));
        }
        Ok(())
    }

    fn ensure_declared(&self, device: &DeviceAddress) -> Result<()> {
        if self.functions.contains(device) {
            Ok(())
        } else {
            Err(ClnxError::Refused(format!(
                "{} is not in the declared function set",
                bdf(device)
            )))
        }
    }

    fn ensure_present(&self) -> Result<()> {
        for device in &self.functions {
            let id = bdf(device);
            if !self.sysfs.exists(&format!("bus/pci/devices/{id}")) {
                return Err(ClnxError::Refused(format!(
                    "declared function {id} is not present"
                )));
            }
        }
        Ok(())
    }

    fn driver(&self, device: &DeviceAddress) -> Result<Option<String>> {
        let id = bdf(device);
        let link = format!("bus/pci/devices/{id}/driver");
        if !self.sysfs.exists(&link) {
            return Ok(None);
        }
        let target = self.sysfs.read_link(&link)?;
        Ok(target.rsplit('/').next().map(ToOwned::to_owned))
    }

    fn iommu_group(&self, device: &DeviceAddress) -> Result<Option<u32>> {
        let id = bdf(device);
        let link = format!("bus/pci/devices/{id}/iommu_group");
        if !self.sysfs.exists(&link) {
            return Ok(None);
        }
        let target = self.sysfs.read_link(&link)?;
        Ok(target
            .rsplit('/')
            .next()
            .and_then(|group| group.parse::<u32>().ok()))
    }

    /// Every device that shares a live IOMMU group with a declared function
    /// but is not itself declared.
    fn intruders(&self) -> Result<Vec<String>> {
        let declared: BTreeSet<String> = self.functions.iter().map(bdf).collect();
        let mut declared_groups = BTreeSet::new();
        for device in &self.functions {
            match self.iommu_group(device)? {
                Some(group) => {
                    declared_groups.insert(group);
                }
                None => {
                    return Err(ClnxError::Refused(format!(
                        "{} has no live IOMMU group; passthrough would share the default group",
                        bdf(device)
                    )));
                }
            }
        }
        let mut intruders = Vec::new();
        for name in self.sysfs.pci_devices()? {
            if declared.contains(&name) {
                continue;
            }
            let Some(address) = parse_bdf(&name) else {
                continue;
            };
            if let Some(group) = self.iommu_group(&address)?
                && declared_groups.contains(&group)
            {
                intruders.push(name);
            }
        }
        Ok(intruders)
    }

    fn bind_one(&self, function: &DeviceAddress, id: &str) -> Result<()> {
        self.sysfs
            .write(&format!("bus/pci/devices/{id}/driver_override"), "vfio-pci")?;
        if self.driver(function)?.as_deref() == Some("vfio-pci") {
            return Ok(());
        }
        if self.driver(function)?.is_some() {
            self.sysfs
                .write(&format!("bus/pci/devices/{id}/driver/unbind"), id)?;
        }
        self.sysfs.write("bus/pci/drivers/vfio-pci/bind", id)
    }

    /// The bind order: every non-zero function ascending, function 0 last.
    fn bind_order(&self) -> Vec<DeviceAddress> {
        let mut order = self.functions.clone();
        order.sort_by_key(|device| {
            (
                device.function == 0,
                device.domain,
                device.bus,
                device.slot,
                device.function,
            )
        });
        order
    }

    /// The return order: function 0 first, then the others ascending.
    fn unbind_order(&self) -> Vec<DeviceAddress> {
        let mut order = self.functions.clone();
        order.sort_by_key(|device| {
            (
                device.function != 0,
                device.domain,
                device.bus,
                device.slot,
                device.function,
            )
        });
        order
    }
}

impl<S: Sysfs, K: Kernel, P: Procs> HostDevices for LinuxDevices<S, K, P> {
    fn enumerate(&self, filter: DeviceFilter) -> CdrvResult<Vec<DeviceFacts>> {
        let mut facts = Vec::new();
        for name in self.sysfs.pci_devices()? {
            let Some(address) = parse_bdf(&name) else {
                continue;
            };
            let class = self
                .sysfs
                .read(&format!("bus/pci/devices/{name}/class"))
                .map(|text| parse_class(&text))
                .unwrap_or(DeviceClass::Other);
            if filter.class.is_some_and(|wanted| wanted != class) {
                continue;
            }
            if filter.name.as_deref().is_some_and(|wanted| wanted != name) {
                continue;
            }
            let vendor = self
                .sysfs
                .read(&format!("bus/pci/devices/{name}/vendor"))
                .ok()
                .map(|value| value.trim().to_owned());
            let bar0_bytes = self
                .sysfs
                .file_len(&format!("bus/pci/devices/{name}/resource0"))
                .ok()
                .filter(|bytes| *bytes > 0);
            facts.push(DeviceFacts {
                name,
                address: Some(address),
                vendor,
                class,
                iommu_group: self.iommu_group(&address)?,
                driver: self.driver(&address)?,
                bar0_bytes,
            });
        }
        Ok(facts)
    }

    fn bind_passthrough(&self, device: &DeviceAddress) -> CdrvResult<()> {
        self.ensure_declared(device)?;
        self.ensure_present()?;
        let intruders = self.intruders()?;
        if !intruders.is_empty() {
            return Err(ClnxError::Refused(format!(
                "the live IOMMU group holds devices outside the declared set: {}",
                intruders.join(", ")
            ))
            .into());
        }
        self.ensure_released()?;
        self.kernel
            .load_module("vfio-pci", &[("disable_idle_d3", "1")])?;
        let parameter = self
            .kernel
            .module_parameter("vfio-pci", "disable_idle_d3")?;
        if parameter != "Y" {
            return Err(ClnxError::Refused(format!(
                "vfio-pci is loaded with disable_idle_d3={parameter}, not 1"
            ))
            .into());
        }
        let mut done: Vec<String> = Vec::new();
        for function in self.bind_order() {
            let id = bdf(&function);
            // A failure stops the bind (never retried) and names the functions
            // already on vfio-pci, so the caller sees the half-bound card.
            self.bind_one(&function, &id)
                .map_err(|error| ClnxError::Backend {
                    backend: "vfio bind",
                    message: format!(
                        "binding {id} failed ({error}); already on vfio-pci: [{}]",
                        done.join(", ")
                    ),
                })?;
            done.push(id);
        }
        Ok(())
    }

    fn unbind_passthrough(&self, device: &DeviceAddress) -> CdrvResult<()> {
        self.ensure_declared(device)?;
        self.ensure_present()?;
        for function in self.unbind_order() {
            let id = bdf(&function);
            if self.driver(&function)?.as_deref() == Some("vfio-pci") {
                self.sysfs
                    .write(&format!("bus/pci/devices/{id}/driver/unbind"), &id)?;
            }
            self.sysfs
                .write(&format!("bus/pci/devices/{id}/driver_override"), "")?;
        }
        self.kernel.load_module(&self.host_driver, &[])?;
        // A repeated return must not fail: a bind of a function the host
        // driver already holds would answer EBUSY.
        if let Some(gpu) = self.functions.iter().find(|device| device.function == 0)
            && self.driver(gpu)?.as_deref() != Some(self.host_driver.as_str())
        {
            self.sysfs.write(
                &format!("bus/pci/drivers/{}/bind", self.host_driver),
                &bdf(gpu),
            )?;
        }
        for function in self.functions.iter().filter(|device| device.function != 0) {
            if self.driver(function)?.is_none() {
                self.sysfs.write("bus/pci/drivers_probe", &bdf(function))?;
            }
        }
        Ok(())
    }

    fn reset(&self, device: &DeviceAddress) -> CdrvResult<()> {
        self.ensure_declared(device)?;
        let id = bdf(device);
        self.sysfs
            .write(&format!("bus/pci/devices/{id}/reset"), "1")?;
        Ok(())
    }
}

/// A PCI address as sysfs names it: `dddd:bb:ss.f`.
#[must_use]
pub fn bdf(device: &DeviceAddress) -> String {
    format!(
        "{:04x}:{:02x}:{:02x}.{:x}",
        device.domain, device.bus, device.slot, device.function
    )
}

/// Parse a sysfs PCI device name.
#[must_use]
pub fn parse_bdf(name: &str) -> Option<DeviceAddress> {
    let (domain, rest) = name.split_once(':')?;
    let (bus, rest) = rest.split_once(':')?;
    let (slot, function) = rest.split_once('.')?;
    Some(DeviceAddress {
        domain: u16::from_str_radix(domain, 16).ok()?,
        bus: u8::from_str_radix(bus, 16).ok()?,
        slot: u8::from_str_radix(slot, 16).ok()?,
        function: u8::from_str_radix(function, 16).ok()?,
    })
}

/// Classify a sysfs `class` value (`0x030000`).
fn parse_class(text: &str) -> DeviceClass {
    let hex = text.trim().trim_start_matches("0x");
    let value = u32::from_str_radix(hex, 16).unwrap_or(0);
    let base = (value >> 16) & 0xff;
    let subclass = (value >> 8) & 0xff;
    match (base, subclass) {
        (0x03, _) => DeviceClass::Gpu,
        (0x04, _) => DeviceClass::Audio,
        (0x0c, 0x03) => DeviceClass::Usb,
        (0x02, _) => DeviceClass::Network,
        _ => DeviceClass::Other,
    }
}
