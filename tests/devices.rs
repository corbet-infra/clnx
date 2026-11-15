//! PCI enumeration, the live IOMMU refusal, the bind order and BAR facts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use cdrv::traits::HostDevices;
use cdrv::{DeviceAddress, DeviceClass, DeviceFilter};
use clnx::{Bar0Size, Kernel, LinuxDevices, LinuxKernel, LinuxSysfs, Sysfs, bdf};
use tempfile::TempDir;

fn function(number: u8) -> DeviceAddress {
    DeviceAddress {
        domain: 0,
        bus: 0x03,
        slot: 0,
        function: number,
    }
}

fn card() -> Vec<DeviceAddress> {
    (0..4).map(function).collect()
}

/// A sysfs tree that records writes and knows drivers by address.
struct RecordingSysfs {
    devices: Vec<String>,
    groups: HashMap<String, Option<u32>>,
    drivers: RefCell<HashMap<String, String>>,
    writes: Rc<RefCell<Vec<(String, String)>>>,
    classes: HashMap<String, &'static str>,
}

impl RecordingSysfs {
    fn card() -> Self {
        let mut groups = HashMap::new();
        let mut drivers = HashMap::new();
        let mut classes = HashMap::new();
        let mut devices = Vec::new();
        for (index, number) in (0..4).enumerate() {
            let name = bdf(&function(number));
            devices.push(name.clone());
            groups.insert(name.clone(), Some(16 + index as u32));
            let driver = ["amdgpu", "snd_hda_intel", "xhci_hcd", "i2c-designware-pci"][index];
            drivers.insert(name, driver.to_owned());
            classes.insert(
                bdf(&function(number)),
                ["0x030000", "0x040300", "0x0c0330", "0x0c8000"][index],
            );
        }
        Self {
            devices,
            groups,
            drivers: RefCell::new(drivers),
            writes: Rc::new(RefCell::new(Vec::new())),
            classes,
        }
    }

    fn with_extra(mut self, name: &str, group: u32, class: &'static str) -> Self {
        self.devices.push(name.to_owned());
        self.groups.insert(name.to_owned(), Some(group));
        self.classes.insert(name.to_owned(), class);
        self
    }

    fn writes(&self) -> Rc<RefCell<Vec<(String, String)>>> {
        Rc::clone(&self.writes)
    }
}

fn device_of(relative: &str) -> Option<String> {
    let parts: Vec<&str> = relative.split('/').collect();
    let index = parts.iter().position(|part| *part == "devices")?;
    parts.get(index + 1).map(|name| (*name).to_owned())
}

impl Sysfs for RecordingSysfs {
    fn pci_devices(&self) -> clnx::Result<Vec<String>> {
        Ok(self.devices.clone())
    }

    fn read(&self, relative: &str) -> clnx::Result<String> {
        if relative.ends_with("/class") {
            let name = device_of(relative).unwrap_or_default();
            if let Some(class) = self.classes.get(&name) {
                return Ok((*class).to_owned());
            }
        }
        if relative.ends_with("/vendor") {
            return Ok("0x1002".to_owned());
        }
        Err(clnx::ClnxError::io(
            relative,
            std::io::ErrorKind::NotFound.into(),
        ))
    }

    fn write(&self, relative: &str, value: &str) -> clnx::Result<()> {
        self.writes
            .borrow_mut()
            .push((relative.to_owned(), value.to_owned()));
        if relative == "bus/pci/drivers/vfio-pci/bind" {
            self.drivers
                .borrow_mut()
                .insert(value.to_owned(), "vfio-pci".to_owned());
        } else if relative.ends_with("/driver/unbind")
            && let Some(name) = device_of(relative)
        {
            self.drivers.borrow_mut().remove(&name);
        }
        Ok(())
    }

    fn read_link(&self, relative: &str) -> clnx::Result<String> {
        if relative.ends_with("/driver")
            && let Some(name) = device_of(relative)
            && let Some(driver) = self.drivers.borrow().get(&name)
        {
            return Ok(format!("../../../bus/pci/drivers/{driver}"));
        }
        if relative.ends_with("/iommu_group")
            && let Some(name) = device_of(relative)
            && let Some(Some(group)) = self.groups.get(&name)
        {
            return Ok(format!("../../../kernel/iommu_groups/{group}"));
        }
        Err(clnx::ClnxError::io(
            relative,
            std::io::ErrorKind::NotFound.into(),
        ))
    }

    fn file_len(&self, relative: &str) -> clnx::Result<u64> {
        Err(clnx::ClnxError::io(
            relative,
            std::io::ErrorKind::NotFound.into(),
        ))
    }

    fn exists(&self, relative: &str) -> bool {
        if relative.ends_with("/driver")
            && let Some(name) = device_of(relative)
        {
            return self.drivers.borrow().contains_key(&name);
        }
        if relative.ends_with("/iommu_group")
            && let Some(name) = device_of(relative)
        {
            return matches!(self.groups.get(&name), Some(Some(_)));
        }
        if let Some(name) = device_of(relative) {
            return self.devices.contains(&name);
        }
        false
    }
}

struct RecordingKernel {
    calls: Rc<RefCell<Vec<String>>>,
    parameter: String,
}

impl RecordingKernel {
    fn new(parameter: &str) -> Self {
        Self {
            calls: Rc::new(RefCell::new(Vec::new())),
            parameter: parameter.to_owned(),
        }
    }
}

impl Kernel for RecordingKernel {
    fn load_module(&self, module: &str, options: &[(&str, &str)]) -> clnx::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("modprobe {module} {options:?}"));
        Ok(())
    }

    fn module_parameter(&self, module: &str, parameter: &str) -> clnx::Result<String> {
        self.calls
            .borrow_mut()
            .push(format!("parameter {module}.{parameter}"));
        Ok(self.parameter.clone())
    }
}

#[test]
fn bind_takes_the_non_gpu_functions_first_and_the_gpu_function_last() {
    let sysfs = RecordingSysfs::card();
    let writes = sysfs.writes();
    let kernel = RecordingKernel::new("Y");
    let loads = Rc::clone(&kernel.calls);
    let driver = LinuxDevices::new(sysfs, kernel, card(), "amdgpu");

    driver.bind_passthrough(&function(0)).unwrap();

    let binds: Vec<String> = writes
        .borrow()
        .iter()
        .filter(|(path, _)| path == "bus/pci/drivers/vfio-pci/bind")
        .map(|(_, value)| value.clone())
        .collect();
    assert_eq!(
        binds,
        vec![
            bdf(&function(1)),
            bdf(&function(2)),
            bdf(&function(3)),
            bdf(&function(0))
        ]
    );
    assert!(loads.borrow().iter().any(|call| call.contains("vfio-pci")));
    assert!(
        loads
            .borrow()
            .iter()
            .any(|call| call.contains("disable_idle_d3"))
    );
    assert!(
        writes
            .borrow()
            .iter()
            .any(|(path, value)| { path.ends_with("/driver_override") && value == "vfio-pci" })
    );
}

#[test]
fn the_return_unbinds_the_gpu_function_first_and_probes_the_others_back() {
    let sysfs = RecordingSysfs::card();
    let writes = sysfs.writes();
    let kernel = RecordingKernel::new("Y");
    let driver = LinuxDevices::new(sysfs, kernel, card(), "amdgpu");

    driver.bind_passthrough(&function(0)).unwrap();
    writes.borrow_mut().clear();
    driver.unbind_passthrough(&function(0)).unwrap();

    let unbinds: Vec<String> = writes
        .borrow()
        .iter()
        .filter(|(path, _)| path.ends_with("/driver/unbind"))
        .map(|(_, value)| value.clone())
        .collect();
    assert_eq!(
        unbinds,
        vec![
            bdf(&function(0)),
            bdf(&function(1)),
            bdf(&function(2)),
            bdf(&function(3))
        ]
    );
    assert!(writes.borrow().iter().any(|(path, value)| {
        path == "bus/pci/drivers/amdgpu/bind" && value == &bdf(&function(0))
    }));
    let probes: Vec<String> = writes
        .borrow()
        .iter()
        .filter(|(path, _)| path == "bus/pci/drivers_probe")
        .map(|(_, value)| value.clone())
        .collect();
    assert_eq!(
        probes,
        vec![bdf(&function(1)), bdf(&function(2)), bdf(&function(3))]
    );
}

#[test]
fn an_undeclared_device_in_a_live_group_refuses_the_bind() {
    let extra = bdf(&DeviceAddress {
        domain: 0,
        bus: 0x03,
        slot: 0x04,
        function: 0,
    });
    let sysfs = RecordingSysfs::card().with_extra(&extra, 16, "0x020000");
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu");

    let error = driver.bind_passthrough(&function(0)).unwrap_err();
    let text = error.to_string();
    assert!(text.contains(&extra), "{text}");
    assert!(text.contains("outside the declared set"), "{text}");
    assert!(
        writes.borrow().is_empty(),
        "nothing may be written before the group is trusted"
    );
}

#[test]
fn a_declared_function_without_an_iommu_group_is_refused() {
    let sysfs = RecordingSysfs::card();
    let missing = bdf(&function(2));
    let mut sysfs = sysfs;
    sysfs.groups.insert(missing, None);
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu");

    let error = driver.bind_passthrough(&function(0)).unwrap_err();
    assert!(error.to_string().contains("no live IOMMU group"));
}

#[test]
fn a_device_outside_the_declared_set_is_refused() {
    let sysfs = RecordingSysfs::card();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu");

    let error = driver
        .bind_passthrough(&DeviceAddress {
            domain: 0,
            bus: 0x04,
            slot: 0,
            function: 0,
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not in the declared function set")
    );
}

#[test]
fn vfio_loaded_without_disable_idle_d3_refuses_the_bind() {
    let sysfs = RecordingSysfs::card();
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("N"), card(), "amdgpu");

    let error = driver.bind_passthrough(&function(0)).unwrap_err();
    assert!(error.to_string().contains("disable_idle_d3"));
    assert!(writes.borrow().is_empty());
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn link(target: &Path, at: &Path) {
    if let Some(parent) = at.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    symlink(target, at).unwrap();
}

/// A fixture tree that looks like `/sys` for one card.
fn fixture() -> TempDir {
    let root = TempDir::new().unwrap();
    let gpu = bdf(&function(0));
    let audio = bdf(&function(1));
    fs::create_dir_all(root.path().join(format!("bus/pci/devices/{gpu}"))).unwrap();
    fs::create_dir_all(root.path().join(format!("bus/pci/devices/{audio}"))).unwrap();
    write(
        &root.path().join(format!("bus/pci/devices/{gpu}/vendor")),
        "0x1002\n",
    );
    write(
        &root.path().join(format!("bus/pci/devices/{gpu}/class")),
        "0x030000\n",
    );
    write(
        &root.path().join(format!("bus/pci/devices/{audio}/vendor")),
        "0x1002\n",
    );
    write(
        &root.path().join(format!("bus/pci/devices/{audio}/class")),
        "0x040300\n",
    );
    fs::create_dir_all(root.path().join("bus/pci/drivers/amdgpu")).unwrap();
    fs::create_dir_all(root.path().join("kernel/iommu_groups/16")).unwrap();
    fs::create_dir_all(root.path().join("kernel/iommu_groups/17")).unwrap();
    link(
        &root.path().join("bus/pci/drivers/amdgpu"),
        &root.path().join(format!("bus/pci/devices/{gpu}/driver")),
    );
    link(
        &root.path().join("kernel/iommu_groups/16"),
        &root
            .path()
            .join(format!("bus/pci/devices/{gpu}/iommu_group")),
    );
    link(
        &root.path().join("kernel/iommu_groups/17"),
        &root
            .path()
            .join(format!("bus/pci/devices/{audio}/iommu_group")),
    );
    root
}

#[test]
fn enumeration_reads_live_facts_from_a_sysfs_tree() {
    let root = fixture();
    let driver = LinuxDevices::new(
        LinuxSysfs::new(root.path()),
        RecordingKernel::new("Y"),
        card(),
        "amdgpu",
    );

    let facts = driver.enumerate(DeviceFilter::all()).unwrap();
    assert_eq!(facts.len(), 2);
    let gpu = &facts[0];
    assert_eq!(gpu.name, bdf(&function(0)));
    assert_eq!(gpu.address, Some(function(0)));
    assert_eq!(gpu.class, DeviceClass::Gpu);
    assert_eq!(gpu.vendor.as_deref(), Some("0x1002"));
    assert_eq!(gpu.iommu_group, Some(16));
    assert_eq!(gpu.driver.as_deref(), Some("amdgpu"));
    assert_eq!(facts[1].class, DeviceClass::Audio);
    assert_eq!(facts[1].driver, None);

    let gpus = driver
        .enumerate(DeviceFilter {
            class: Some(DeviceClass::Gpu),
            name: None,
        })
        .unwrap();
    assert_eq!(gpus.len(), 1);
}

#[test]
fn the_bar0_size_is_reported_as_a_fact() {
    let root = fixture();
    let resource = root
        .path()
        .join(format!("bus/pci/devices/{}/resource0", bdf(&function(0))));
    fs::write(&resource, []).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&resource)
        .unwrap()
        .set_len(16 * (1 << 30))
        .unwrap();
    let driver = LinuxDevices::new(
        LinuxSysfs::new(root.path()),
        RecordingKernel::new("Y"),
        card(),
        "amdgpu",
    );

    let bar = driver.bar0_size(&function(0)).unwrap();
    assert_eq!(
        bar,
        Bar0Size {
            bytes: 16 * (1 << 30)
        }
    );
    assert_eq!(bar.gib(), 16);
    assert!(bar.is_large());
}

#[test]
fn reset_writes_the_device_reset_attribute_once() {
    let root = fixture();
    let driver = LinuxDevices::new(
        LinuxSysfs::new(root.path()),
        RecordingKernel::new("Y"),
        card(),
        "amdgpu",
    );
    driver.reset(&function(0)).unwrap();
    let reset = root
        .path()
        .join(format!("bus/pci/devices/{}/reset", bdf(&function(0))));
    assert_eq!(fs::read_to_string(reset).unwrap(), "1");
}

#[test]
fn a_module_parameter_is_read_from_sysfs() {
    let root = TempDir::new().unwrap();
    let path = root
        .path()
        .join("module/vfio_pci/parameters/disable_idle_d3");
    write(&path, "Y\n");
    let kernel = LinuxKernel::new(root.path(), "/bin/true");
    assert_eq!(
        kernel
            .module_parameter("vfio-pci", "disable_idle_d3")
            .unwrap(),
        "Y"
    );
}

#[test]
fn parse_bdf_round_trips() {
    let address = DeviceAddress {
        domain: 0,
        bus: 0x0a,
        slot: 0x1f,
        function: 7,
    };
    assert_eq!(clnx::parse_bdf(&bdf(&address)), Some(address));
    assert_eq!(clnx::parse_bdf("not-a-bdf"), None);
}

#[test]
fn sysfs_reads_drop_one_trailing_newline() {
    let root = TempDir::new().unwrap();
    write(&root.path().join("value"), "Y\n");
    let sysfs = LinuxSysfs::new(root.path());
    assert_eq!(sysfs.read("value").unwrap(), "Y");
    let _: PathBuf = root.path().to_path_buf();
}
