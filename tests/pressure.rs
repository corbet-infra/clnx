//! The live ladder's facts: the card's counters, the compute tenants, the
//! broker's stuck models and which broker events wake the observer.

use std::fs;

use cdrv::{DeviceAddress, GpuLadder};
use clnx::LinuxSysfs;
use clnx::backends::{Workload, WorkloadKind};
use std::time::Duration;

use clnx::pressure::{
    CardMemory, CardSample, CardSampler, SamplerConfig, card_memory, compute_tenants,
    is_model_status_line, stuck_models,
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

const MIB: u64 = 1024 * 1024;
const TOTAL: u64 = 16 * 1024 * MIB;

fn sampler() -> CardSampler {
    // The retired pressure-watcher's thresholds: 6 s, 64 MiB, 0.85.
    CardSampler::new(SamplerConfig {
        interval: Duration::from_secs(6),
        gtt_delta_bytes: 64 * MIB,
        vram_full_fraction: 0.85,
    })
}

/// A reading with VRAM at `vram_percent` of 16 GiB and GTT at `gtt_mib`.
fn reading(vram_percent: u64, gtt_mib: u64) -> Option<CardMemory> {
    Some(CardMemory {
        vram_used: TOTAL / 100 * vram_percent,
        vram_total: TOTAL,
        gtt_used: gtt_mib * MIB,
    })
}

#[test]
fn a_full_card_whose_gtt_grows_past_the_noise_floor_spills_and_is_counted() {
    let mut sampler = sampler();
    assert_eq!(
        sampler.sample(reading(95, 100)),
        None,
        "the first reading has nothing to compare"
    );
    let first = sampler
        .sample(reading(95, 300))
        .expect("a spill is reported");
    assert_eq!(first.spilling, 1);
    assert_eq!(first.gtt_previous, 100 * MIB);
    assert_eq!(first.memory.gtt_used, 300 * MIB);
    let second = sampler
        .sample(reading(96, 500))
        .expect("a running spill is reported");
    assert_eq!(
        second.spilling, 2,
        "consecutive spilling readings are the grace"
    );
    let ended = sampler
        .sample(reading(96, 520))
        .expect("the end of a spill is reported once");
    assert_eq!(ended.spilling, 0);
    assert_eq!(
        sampler.sample(reading(96, 540)),
        None,
        "a quiet full card wakes nothing"
    );
}

#[test]
fn gtt_growth_is_no_spill_while_vram_has_room_or_below_the_noise_floor() {
    let mut sampler = sampler();
    sampler.sample(reading(50, 100));
    assert_eq!(
        sampler.sample(reading(50, 900)),
        None,
        "a card with room does not spill"
    );
    sampler.sample(reading(95, 900));
    assert_eq!(
        sampler.sample(reading(95, 950)),
        None,
        "50 MiB in one reading is below the 64 MiB noise floor"
    );
}

#[test]
fn crossing_the_full_fraction_is_reported_both_ways() {
    let mut sampler = sampler();
    sampler.sample(reading(50, 100));
    let filled = sampler.sample(reading(90, 100)).expect("the card filled");
    assert_eq!(filled.spilling, 0);
    assert!(filled.memory.vram_full(0.85));
    let emptied = sampler
        .sample(reading(40, 100))
        .expect("the card has room again");
    assert!(!emptied.memory.vram_full(0.85));
    assert_eq!(sampler.sample(reading(41, 100)), None);
}

#[test]
fn unreadable_counters_end_a_running_spill() {
    let mut sampler = sampler();
    sampler.sample(reading(95, 100));
    sampler.sample(reading(95, 300));
    assert_eq!(
        sampler.sample(None),
        Some(CardSample {
            memory: reading(95, 300).unwrap(),
            gtt_previous: 300 * MIB,
            spilling: 0,
        })
    );
    assert_eq!(sampler.sample(None), None);
    assert_eq!(
        sampler.sample(reading(95, 900)),
        None,
        "after a gap the first reading compares nothing"
    );
}
