//! The live ladder's facts: the card's counters, the compute tenants, the
//! broker's stuck models and which broker events wake the observer.

use std::fs;

use cdrv::{DeviceAddress, GpuLadder};
use clnx::LinuxSysfs;
use clnx::backends::{Workload, WorkloadKind};
use clnx::pressure::{
    CardMemory, card_memory, compute_tenants, is_model_status_line, stuck_models,
};

fn card() -> DeviceAddress {
    DeviceAddress {
        domain: 0,
        bus: 0x0b,
        slot: 0,
        function: 0,
    }
}

#[test]
fn the_card_counters_come_from_the_gpu_function_in_sysfs() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("bus/pci/devices/0000:0b:00.0");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("mem_info_vram_used"), "1073741824\n").unwrap();
    fs::write(dir.join("mem_info_vram_total"), "17163091968\n").unwrap();
    fs::write(dir.join("mem_info_gtt_used"), "52428800\n").unwrap();
    let sysfs = LinuxSysfs::new(root.path());
    assert_eq!(
        card_memory(&sysfs, &card()),
        Some(CardMemory {
            vram_used: 1_073_741_824,
            vram_total: 17_163_091_968,
            gtt_used: 52_428_800,
        })
    );
    // A card on the passthrough driver has no counters: no fact, no guess.
    fs::remove_file(dir.join("mem_info_vram_used")).unwrap();
    assert_eq!(card_memory(&sysfs, &card()), None);
}

fn workload(name: &str, class: Option<GpuLadder>, compute: bool) -> Workload {
    Workload {
        namespace: "example".to_owned(),
        kind: WorkloadKind::Deployment,
        name: name.to_owned(),
        replicas: 1,
        ready: 0,
        ladder: class,
        compute,
    }
}

#[test]
fn only_compute_workloads_on_a_ladder_rung_are_tenants() {
    let tenants = compute_tenants(&[
        workload("broker", Some(GpuLadder::Interactive), true),
        workload("transcoder", Some(GpuLadder::BestEffort), false),
        workload("unranked", None, true),
    ]);
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0].id, "example/broker");
    assert_eq!(tenants[0].ladder, GpuLadder::Interactive);
    assert_eq!((tenants[0].replicas, tenants[0].ready), (1, 0));
}

#[test]
fn stuck_models_are_those_not_ready_and_an_unreadable_answer_is_no_count() {
    assert_eq!(stuck_models(r#"{"running":[]}"#), Some(0));
    assert_eq!(
        stuck_models(
            r#"{"running":[{"model":"a","state":"ready"},{"model":"b","state":"starting"}]}"#
        ),
        Some(1)
    );
    assert_eq!(stuck_models("<html>"), None);
    assert_eq!(stuck_models(r#"{"other":1}"#), None);
}

#[test]
fn only_model_status_events_wake_the_observer() {
    assert!(is_model_status_line(
        r#"data:{"type":"modelStatus","data":"[]"}"#
    ));
    // Log lines (which include the observer's own status requests) never wake it.
    assert!(!is_model_status_line(
        r#"data:{"type":"logData","data":"GET /running"}"#
    ));
    assert!(!is_model_status_line("event:message"));
    assert!(!is_model_status_line("data:not json"));
}

#[test]
fn a_guarded_resource_is_zero_when_it_reads_zero_or_is_missing() {
    let allocatable: std::collections::BTreeMap<String, String> = [
        ("example.com/compute".to_owned(), "16".to_owned()),
        ("example.com/video".to_owned(), "0".to_owned()),
    ]
    .into_iter()
    .collect();
    let guarded = vec![
        "example.com/compute".to_owned(),
        "example.com/video".to_owned(),
        "example.com/absent".to_owned(),
    ];
    assert_eq!(
        clnx::pressure::zero_resources(&allocatable, &guarded),
        vec![
            "example.com/video".to_owned(),
            "example.com/absent".to_owned()
        ]
    );
}
