//! Memory facts from fixture files and balloon control through libvirt.

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use cdrv::traits::HostMemory;
use cdrv::{BalloonStats, InstanceId, InstanceState, MemoryFacts, StopMode};
use clnx::{DomainEvent, Libvirt, LinuxMemory, SEVERE_PRESSURE_AVG10};
use tempfile::TempDir;

struct FakeLibvirt {
    calls: Rc<RefCell<Vec<String>>>,
}

impl Libvirt for FakeLibvirt {
    fn define(&self, _xml: &str) -> clnx::Result<()> {
        Ok(())
    }

    fn start(&self, _name: &str) -> clnx::Result<()> {
        Ok(())
    }

    fn shutdown(&self, _name: &str, _mode: StopMode) -> clnx::Result<()> {
        Ok(())
    }

    fn destroy(&self, _name: &str) -> clnx::Result<()> {
        Ok(())
    }

    fn state(&self, _name: &str) -> clnx::Result<InstanceState> {
        Ok(InstanceState::Running)
    }

    fn list(&self) -> clnx::Result<Vec<(String, InstanceState)>> {
        Ok(Vec::new())
    }

    fn set_balloon(&self, name: &str, target_mib: u64) -> clnx::Result<()> {
        self.calls
            .borrow_mut()
            .push(format!("balloon:{name}:{target_mib}"));
        Ok(())
    }

    fn balloon_stats(&self, name: &str) -> clnx::Result<BalloonStats> {
        Ok(BalloonStats {
            target_mib: 6144,
            actual_mib: Some(6000),
            free_pages: if name.is_empty() { None } else { Some(42) },
        })
    }

    fn events(&self) -> clnx::Result<Receiver<DomainEvent>> {
        Err(clnx::ClnxError::Unsupported("not used".to_owned()))
    }
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn fixture() -> (TempDir, Rc<RefCell<Vec<String>>>) {
    let root = TempDir::new().unwrap();
    write(
        &root.path().join("meminfo"),
        "MemTotal:       33554432 kB\nMemAvailable:    8388608 kB\n",
    );
    write(
        &root.path().join("spl/kstat/zfs/arcstats"),
        "9 1 0x01 123\n\
         size 4 4294967296\n\
         c_min 4 1073741824\n\
         mru_evictable_data 4 1073741824\n\
         mru_evictable_metadata 4 268435456\n\
         mfu_evictable_data 4 536870912\n\
         mfu_evictable_metadata 4 134217728\n",
    );
    write(
        &root.path().join("pressure/memory"),
        "some avg10=0.10 avg60=0.05 avg300=0.01 total=100\n\
         full avg10=1.50 avg60=0.80 avg300=0.20 total=50\n",
    );
    let calls = Rc::new(RefCell::new(Vec::new()));
    (root, calls)
}

fn memory(root: &TempDir, calls: Rc<RefCell<Vec<String>>>) -> LinuxMemory<FakeLibvirt> {
    LinuxMemory::with_proc_root(FakeLibvirt { calls }, root.path())
}

#[test]
fn facts_are_mem_available_plus_the_capped_evictable_arc_and_the_psi_share() {
    let (root, calls) = fixture();
    let facts = memory(&root, calls).facts().unwrap();
    assert_eq!(
        facts,
        MemoryFacts {
            available_mib: 8192,
            evictable_arc_mib: 1920,
            pressure_full_avg10: 1.5,
        }
    );
    assert_eq!(facts.headroom_mib(), 8192 + 1920);
}

#[test]
fn the_evictable_arc_is_capped_at_size_minus_c_min() {
    let (root, calls) = fixture();
    write(
        &root.path().join("spl/kstat/zfs/arcstats"),
        "size 4 2147483648\n\
         c_min 4 1073741824\n\
         mru_evictable_data 4 2147483648\n\
         mru_evictable_metadata 4 1073741824\n\
         mfu_evictable_data 4 536870912\n\
         mfu_evictable_metadata 4 134217728\n",
    );
    let facts = memory(&root, calls).facts().unwrap();
    assert_eq!(facts.evictable_arc_mib, 1024);
}

#[test]
fn no_zfs_means_no_evictable_arc() {
    let (root, calls) = fixture();
    fs::remove_file(root.path().join("spl/kstat/zfs/arcstats")).unwrap();
    let facts = memory(&root, calls).facts().unwrap();
    assert_eq!(facts.evictable_arc_mib, 0);
    assert_eq!(facts.available_mib, 8192);
}

#[test]
fn no_psi_file_is_severe_pressure_not_calm() {
    let (root, calls) = fixture();
    fs::remove_file(root.path().join("pressure/memory")).unwrap();
    let facts = memory(&root, calls).facts().unwrap();
    assert_eq!(facts.pressure_full_avg10, SEVERE_PRESSURE_AVG10);
}

#[test]
fn a_missing_full_line_is_severe_pressure() {
    let (root, calls) = fixture();
    write(
        &root.path().join("pressure/memory"),
        "some avg10=0.10 avg60=0.05 avg300=0.01 total=100\n",
    );
    let facts = memory(&root, calls).facts().unwrap();
    assert_eq!(facts.pressure_full_avg10, SEVERE_PRESSURE_AVG10);
}

#[test]
fn a_malformed_pressure_share_is_an_error() {
    let (root, calls) = fixture();
    write(
        &root.path().join("pressure/memory"),
        "full avg10=not-a-number avg60=0 avg300=0 total=0\n",
    );
    let error = memory(&root, calls).facts().unwrap_err();
    assert!(error.to_string().contains("pressure/memory"));
}

#[test]
fn a_missing_mem_available_is_an_error() {
    let (root, calls) = fixture();
    write(&root.path().join("meminfo"), "MemTotal: 33554432 kB\n");
    let error = memory(&root, calls).facts().unwrap_err();
    assert!(error.to_string().contains("MemAvailable"));
}

#[test]
fn the_balloon_moves_through_libvirt() {
    let (root, calls) = fixture();
    let memory = memory(&root, Rc::clone(&calls));
    let id = InstanceId::from("demo-vm");
    memory.set_balloon(&id, 6144).unwrap();
    assert_eq!(calls.borrow().clone(), vec!["balloon:demo-vm:6144"]);
    let stats = memory.balloon_stats(&id).unwrap();
    assert_eq!(
        stats,
        BalloonStats {
            target_mib: 6144,
            actual_mib: Some(6000),
            free_pages: Some(42),
        }
    );
}
