//! PCI enumeration, the live IOMMU refusal, the bind order and BAR facts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use cdrv::traits::HostDevices;
use cdrv::{DeviceAddress, DeviceClass, DeviceFilter};
use clnx::{
    Bar0Size, DeviceNode, Holder, Kernel, LinuxDevices, LinuxKernel, LinuxProcs, LinuxSysfs, Procs,
    Sysfs, bdf, makedev,
};
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
    fail_bind: Option<String>,
    drm: HashMap<String, Vec<String>>,
    refcnt: String,
    /// `resource<N>` lengths and `resource<N>_resize` masks by relative path.
    bars: HashMap<String, u64>,
    resize_masks: HashMap<String, String>,
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
            fail_bind: None,
            drm: HashMap::new(),
            refcnt: "0".to_owned(),
            bars: HashMap::new(),
            resize_masks: HashMap::new(),
        }
    }

    /// The GPU function's BAR `bar` is `bytes` large and resizable to `mask`.
    fn with_bar(mut self, bar: u8, bytes: u64, mask: &str) -> Self {
        let device = format!("bus/pci/devices/{}", bdf(&function(0)));
        self.bars.insert(format!("{device}/resource{bar}"), bytes);
        self.resize_masks
            .insert(format!("{device}/resource{bar}_resize"), mask.to_owned());
        self
    }

    fn with_extra(mut self, name: &str, group: u32, class: &'static str) -> Self {
        self.devices.push(name.to_owned());
        self.groups.insert(name.to_owned(), Some(group));
        self.classes.insert(name.to_owned(), class);
        self
    }

    /// The card exposes DRM nodes under its GPU function.
    fn with_drm(mut self, names: &[&str]) -> Self {
        self.drm.insert(
            format!("bus/pci/devices/{}/drm", bdf(&function(0))),
            names.iter().map(|name| (*name).to_owned()).collect(),
        );
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
        if relative == "module/amdgpu/refcnt" {
            return Ok(self.refcnt.clone());
        }
        if relative == "class/kfd/kfd/dev" {
            return Ok("235:0\n".to_owned());
        }
        if let Some(mask) = self.resize_masks.get(relative) {
            return Ok(mask.clone());
        }
        if let Some(name) = relative
            .strip_suffix("/dev")
            .and_then(|rest| rest.rsplit('/').next())
        {
            if let Some(minor) = name.strip_prefix("renderD") {
                return Ok(format!("226:{minor}\n"));
            }
            if let Some(minor) = name.strip_prefix("card") {
                return Ok(format!("226:{minor}\n"));
            }
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
            if self.fail_bind.as_deref() == Some(value) {
                return Err(clnx::ClnxError::io(
                    relative,
                    std::io::ErrorKind::PermissionDenied.into(),
                ));
            }
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
        if let Some(bytes) = self.bars.get(relative) {
            return Ok(*bytes);
        }
        Err(clnx::ClnxError::io(
            relative,
            std::io::ErrorKind::NotFound.into(),
        ))
    }

    fn list(&self, relative: &str) -> clnx::Result<Vec<String>> {
        Ok(self.drm.get(relative).cloned().unwrap_or_default())
    }

    fn exists(&self, relative: &str) -> bool {
        if relative == "class/kfd/kfd/dev" {
            return true;
        }
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

type Wait = (Vec<u32>, Duration);

/// A process table that replays scripted scans and records the waits.
#[derive(Default, Clone)]
struct FakeProcs {
    /// The answer of each successive scan; the last one repeats.
    scans: Rc<RefCell<Vec<Vec<Holder>>>>,
    scanned: Rc<RefCell<Vec<Vec<DeviceNode>>>>,
    waits: Rc<RefCell<Vec<Wait>>>,
    /// Whether a wait makes the holders leave.
    leave_on_wait: bool,
}

impl FakeProcs {
    fn holding(holders: Vec<Holder>, leave_on_wait: bool) -> Self {
        Self {
            scans: Rc::new(RefCell::new(vec![holders])),
            leave_on_wait,
            ..Self::default()
        }
    }
}

impl Procs for FakeProcs {
    fn holders(&self, nodes: &[DeviceNode]) -> Vec<Holder> {
        self.scanned.borrow_mut().push(nodes.to_vec());
        let scans = self.scans.borrow();
        scans.first().cloned().unwrap_or_default()
    }

    fn wait_exit(&self, pids: &[u32], timeout: Duration) {
        self.waits.borrow_mut().push((pids.to_vec(), timeout));
        if self.leave_on_wait {
            self.scans.borrow_mut().clear();
        }
    }
}

fn holder(pid: u32, comm: &str, node: &str) -> Holder {
    Holder {
        pid,
        comm: comm.to_owned(),
        node: PathBuf::from(node),
    }
}

#[test]
fn a_free_card_binds_in_the_usual_order_and_scans_every_node() {
    let sysfs = RecordingSysfs::card().with_drm(&["card1", "renderD128"]);
    let writes = sysfs.writes();
    let procs = FakeProcs::default();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(procs.clone());

    driver.bind_passthrough(&function(0)).unwrap();

    assert_eq!(
        procs.scanned.borrow()[0],
        vec![
            DeviceNode {
                node: PathBuf::from("/dev/dri/card1"),
                rdev: makedev(226, 1)
            },
            DeviceNode {
                node: PathBuf::from("/dev/dri/renderD128"),
                rdev: makedev(226, 128)
            },
            DeviceNode {
                node: PathBuf::from("/dev/kfd"),
                rdev: makedev(235, 0)
            },
        ]
    );
    assert!(procs.waits.borrow().is_empty(), "no holder, no wait");
    assert!(
        writes
            .borrow()
            .iter()
            .any(|(path, _)| path == "bus/pci/drivers/vfio-pci/bind")
    );
}

#[test]
fn a_holder_that_exits_during_the_wait_lets_the_bind_go_ahead() {
    let sysfs = RecordingSysfs::card().with_drm(&["card1"]);
    let procs = FakeProcs::holding(vec![holder(42, "compositor", "/dev/dri/card1")], true);
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(procs.clone())
        .with_holder_wait(Duration::from_secs(7));

    driver.bind_passthrough(&function(0)).unwrap();

    assert_eq!(
        *procs.waits.borrow(),
        vec![(vec![42], Duration::from_secs(7))]
    );
}

#[test]
fn a_holder_that_stays_refuses_the_bind_before_anything_is_written() {
    let sysfs = RecordingSysfs::card().with_drm(&["card1"]);
    let writes = sysfs.writes();
    let kernel = RecordingKernel::new("Y");
    let loads = Rc::clone(&kernel.calls);
    let procs = FakeProcs::holding(vec![holder(42, "compositor", "/dev/dri/card1")], false);
    let driver = LinuxDevices::new(sysfs, kernel, card(), "amdgpu").with_procs(procs.clone());

    let error = driver.bind_passthrough(&function(0)).unwrap_err();

    let text = error.to_string();
    assert!(text.contains("42 compositor /dev/dri/card1"), "{text}");
    assert_eq!(procs.waits.borrow().len(), 1);
    assert!(writes.borrow().is_empty(), "nothing may reach sysfs");
    assert!(loads.borrow().is_empty(), "no module load either");
}

#[test]
fn a_busy_host_driver_refuses_the_bind_and_names_the_count() {
    let mut sysfs = RecordingSysfs::card();
    sysfs.refcnt = "3".to_owned();
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default());

    let error = driver.bind_passthrough(&function(0)).unwrap_err();

    let text = error.to_string();
    assert!(text.contains("amdgpu") && text.contains('3'), "{text}");
    assert!(writes.borrow().is_empty());
}

#[test]
fn the_real_scan_matches_the_device_number_not_the_link_name() {
    // A process in a container names the node differently: the scan must
    // match what the fd really holds. `/dev/null` (1:3) stands in for a DRM
    // node, linked under another name as a container would show it.
    let root = TempDir::new().unwrap();
    link(Path::new("/dev/null"), &root.path().join("100/fd/5"));
    link(Path::new("/dev/null"), &root.path().join("100/fd/6"));
    write(&root.path().join("100/comm"), "compositor\n");
    link(Path::new("/dev/zero"), &root.path().join("200/fd/0"));
    let plain = root.path().join("plain");
    write(&plain, "not a device");
    link(&plain, &root.path().join("300/fd/1"));
    // A target outside /dev is never even stat'ed (a stuck mount would block).
    link(
        Path::new("/nonexistent/stuck-mount/file"),
        &root.path().join("300/fd/2"),
    );
    fs::create_dir_all(root.path().join("400")).unwrap();
    fs::create_dir_all(root.path().join("self")).unwrap();

    let procs = LinuxProcs::new(root.path());
    let card = DeviceNode {
        node: PathBuf::from("/dev/dri/card2"),
        rdev: makedev(1, 3),
    };
    let found = procs.holders(&[card]);

    assert_eq!(found, vec![holder(100, "compositor", "/dev/dri/card2")]);
    assert!(procs.holders(&[]).is_empty());
}

#[test]
fn makedev_matches_the_kernel_encoding() {
    use std::os::unix::fs::MetadataExt;
    assert_eq!(fs::metadata("/dev/null").unwrap().rdev(), makedev(1, 3));
    assert_eq!(makedev(226, 128), (226 << 8) | 128);
    assert_eq!(makedev(4096, 256), (1 << 44) | (1 << 20));
}

#[test]
fn waiting_for_a_gone_pid_or_an_exiting_child_returns_promptly() {
    let procs = LinuxProcs::system();
    let mut child = std::process::Command::new("sleep")
        .arg("0.2")
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    procs.wait_exit(&[child.id()], Duration::from_secs(20));
    assert!(started.elapsed() < Duration::from_secs(10));
    child.wait().unwrap();
    // The pid is reaped now: nothing to wait for.
    procs.wait_exit(&[child.id()], Duration::from_secs(20));
    // A process that stays is given up on at the deadline.
    let mut stays = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    procs.wait_exit(&[stays.id()], Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(4));
    stays.kill().unwrap();
    stays.wait().unwrap();
}

#[test]
fn a_failed_bind_stops_without_retry_and_names_the_functions_already_bound() {
    let mut sysfs = RecordingSysfs::card();
    sysfs.fail_bind = Some(bdf(&function(2)));
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default());

    let error = driver
        .bind_passthrough(&function(0))
        .unwrap_err()
        .to_string();

    assert!(error.contains(&bdf(&function(2))), "{error}");
    assert!(
        error.contains(&bdf(&function(1))),
        "names the bound one: {error}"
    );
    let binds: Vec<String> = writes
        .borrow()
        .iter()
        .filter(|(path, _)| path == "bus/pci/drivers/vfio-pci/bind")
        .map(|(_, value)| value.clone())
        .collect();
    assert_eq!(
        binds,
        vec![bdf(&function(1)), bdf(&function(2))],
        "no retry, no later function"
    );
}

#[test]
fn bind_takes_the_non_gpu_functions_first_and_the_gpu_function_last() {
    let sysfs = RecordingSysfs::card();
    let writes = sysfs.writes();
    let kernel = RecordingKernel::new("Y");
    let loads = Rc::clone(&kernel.calls);
    let driver =
        LinuxDevices::new(sysfs, kernel, card(), "amdgpu").with_procs(FakeProcs::default());

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
    let driver =
        LinuxDevices::new(sysfs, kernel, card(), "amdgpu").with_procs(FakeProcs::default());

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
    // A newline clears an override; a zero-byte write never reaches sysfs's
    // store handler and leaves vfio-pci in place (the live failure of 2026-11-15).
    let overrides: Vec<String> = writes
        .borrow()
        .iter()
        .filter(|(path, _)| path.ends_with("/driver_override"))
        .map(|(_, value)| value.clone())
        .collect();
    assert_eq!(overrides, vec!["\n".to_owned(); 4]);
}

#[test]
fn a_card_already_back_on_the_host_driver_is_a_no_op_apart_from_the_overrides() {
    let sysfs = RecordingSysfs::card();
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default());

    driver.unbind_passthrough(&function(0)).unwrap();

    let written = writes.borrow().clone();
    assert_eq!(written.len(), 4, "{written:?}");
    assert!(
        written
            .iter()
            .all(|(path, value)| path.ends_with("/driver_override") && value == "\n")
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
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default());

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
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default());

    let error = driver.bind_passthrough(&function(0)).unwrap_err();
    assert!(error.to_string().contains("no live IOMMU group"));
}

#[test]
fn a_device_outside_the_declared_set_is_refused() {
    let sysfs = RecordingSysfs::card();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default());

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
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("N"), card(), "amdgpu")
        .with_procs(FakeProcs::default());

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

const GIB: u64 = 1 << 30;

#[test]
fn a_bar_limit_shrinks_the_gpu_bar_between_its_unbind_and_the_passthrough_bind() {
    let sysfs = RecordingSysfs::card().with_bar(0, 16 * GIB, "0000000000007f00");
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default())
        .with_bar_limits(vec![clnx::BarLimit { bar: 0, mib: 1024 }]);

    driver.bind_passthrough(&function(0)).unwrap();

    let gpu = bdf(&function(0));
    let sequence: Vec<(String, String)> = writes
        .borrow()
        .iter()
        .filter(|(path, value)| path.contains(&gpu) || value == &gpu)
        .cloned()
        .collect();
    assert_eq!(
        sequence,
        vec![
            (
                format!("bus/pci/devices/{gpu}/driver_override"),
                "vfio-pci".to_owned()
            ),
            (format!("bus/pci/devices/{gpu}/driver/unbind"), gpu.clone()),
            // 1024 MiB = 2^10 MiB: the kernel's size code 10, written while no driver holds the function.
            (
                format!("bus/pci/devices/{gpu}/resource0_resize"),
                "10".to_owned()
            ),
            ("bus/pci/drivers/vfio-pci/bind".to_owned(), gpu.clone()),
        ]
    );
}

#[test]
fn a_bar_already_within_its_limit_is_left_alone() {
    let sysfs = RecordingSysfs::card().with_bar(0, 256 << 20, "0000000000007f00");
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default())
        .with_bar_limits(vec![clnx::BarLimit { bar: 0, mib: 1024 }]);

    driver.bind_passthrough(&function(0)).unwrap();

    assert!(
        !writes
            .borrow()
            .iter()
            .any(|(path, _)| path.ends_with("_resize"))
    );
}

#[test]
fn a_size_the_bar_does_not_support_is_refused_before_the_gpu_is_bound() {
    // 128 MiB is below the card's smallest BAR0 size (the mask starts at 256 MiB).
    let sysfs = RecordingSysfs::card().with_bar(0, 16 * GIB, "0000000000007f00");
    let writes = sysfs.writes();
    let driver = LinuxDevices::new(sysfs, RecordingKernel::new("Y"), card(), "amdgpu")
        .with_procs(FakeProcs::default())
        .with_bar_limits(vec![clnx::BarLimit { bar: 0, mib: 128 }]);

    let error = driver
        .bind_passthrough(&function(0))
        .unwrap_err()
        .to_string();

    assert!(error.contains("cannot be shrunk"), "{error}");
    let gpu = bdf(&function(0));
    assert!(!writes.borrow().iter().any(|(path, value)| {
        path.ends_with("_resize") || (path == "bus/pci/drivers/vfio-pci/bind" && value == &gpu)
    }));
}
