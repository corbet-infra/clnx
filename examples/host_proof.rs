//! Prove the instance and memory drivers against a live libvirt, without a GPU.
//!
//! Usage: `host_proof <libvirt-uri> <4Kn-test-disk-path> <synthetic-domain-name>`
//!
//! The domain is defined from the rendered XML, started, observed through
//! lifecycle events, ballooned, saved (managed save), restored and stopped
//! cleanly. It never calls `destroy`; if the guest does not stop, the run
//! reports that and leaves the domain for the caller to deal with.

use std::time::{Duration, Instant};

use cdrv::traits::{HostInstances, HostMemory};
use cdrv::{
    BlockDevice, CpuClaim, GuestOs, InstanceClass, InstanceId, InstanceKind, InstanceSpec,
    InstanceState, MemoryClaim, SectorSize, StopMode,
};
use clnx::{LinuxInstances, LinuxMemory, RealLibvirt, RenderOptions};

fn wait_for(
    events: &mut dyn Iterator<Item = cdrv::Event>,
    want: &str,
    seen: &mut Vec<String>,
) -> bool {
    // The event stream blocks until libvirt pushes the next change.
    for event in events {
        seen.push(event.kind.clone());
        println!("  event {} {:?}", event.kind, event.data);
        if event.kind == want {
            return true;
        }
    }
    false
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, uri, disk, name] = &args[..] else {
        eprintln!("usage: host_proof <uri> <disk> <name>");
        std::process::exit(2);
    };
    let spec = InstanceSpec {
        id: InstanceId::from(name.as_str()),
        kind: InstanceKind::Vm,
        guest_os: GuestOs::Linux,
        memory: MemoryClaim {
            min_mib: 256,
            max_mib: 512,
            balloon: true,
        },
        cpu: CpuClaim { quota_cores: 1.0 },
        class: InstanceClass::Dev,
        devices: Vec::new(),
        disks: vec![BlockDevice {
            path: disk.into(),
            serial: Some("SYNTHETIC-PROOF".to_owned()),
            sector: SectorSize::FOUR_KN,
            read_only: false,
        }],
        vcpus: Some(1),
        nic: None,
        console: false,
        identity: None,
    };
    let options = RenderOptions {
        loader: None,
        ..RenderOptions::default()
    };
    let instances = LinuxInstances::new("proof-host", RealLibvirt::connect(uri).unwrap(), options);
    let memory = LinuxMemory::new(RealLibvirt::connect(uri).unwrap());
    let id = spec.id.clone();

    println!("--- rendered XML\n{}", instances.render(&spec).unwrap());
    let mut events = instances.events().expect("event stream");
    let mut seen = Vec::new();
    // `virsh event` registers with libvirt a moment after it is spawned; events
    // fired before that are not delivered. A consumer subscribes first and then
    // reads `list()` to close the gap; the proof just lets the client connect.
    std::thread::sleep(Duration::from_secs(5));

    instances.define(&spec).expect("define");
    println!("defined; state {:?}", instances.state(&id).unwrap());
    assert!(wait_for(&mut events, "instance.defined", &mut seen));

    instances.start(&id).expect("start");
    assert!(wait_for(&mut events, "instance.starting", &mut seen));
    println!("started; state {:?}", instances.state(&id).unwrap());

    // Give the guest time to bring up its ACPI button and the balloon stats.
    std::thread::sleep(Duration::from_secs(25));
    let stats = memory.balloon_stats(&id).unwrap();
    println!("balloon stats {stats:?}");
    memory.set_balloon(&id, 300).expect("balloon down");
    std::thread::sleep(Duration::from_secs(12));
    println!("after target 300: {:?}", memory.balloon_stats(&id).unwrap());
    memory.set_balloon(&id, 512).expect("balloon up");

    // Managed save, then restore on start.
    instances
        .stop(&id, StopMode::Hibernate)
        .expect("managed save");
    assert!(wait_for(&mut events, "instance.stopped", &mut seen));
    println!("saved; state {:?}", instances.state(&id).unwrap());
    instances.start(&id).expect("restore");
    assert!(wait_for(&mut events, "instance.starting", &mut seen));
    println!("restored; state {:?}", instances.state(&id).unwrap());

    // Clean stop: ACPI power button; the guest powers itself off.
    let started = Instant::now();
    instances.stop(&id, StopMode::Clean).expect("clean stop");
    let stopped = wait_for(&mut events, "instance.stopped", &mut seen);
    println!(
        "clean stop {} after {:?}; state {:?}",
        if stopped {
            "COMPLETED"
        } else {
            "DID NOT COMPLETE"
        },
        started.elapsed(),
        instances.state(&id).unwrap()
    );
    assert_eq!(instances.state(&id).unwrap(), InstanceState::Stopped);
    println!("event kinds seen: {seen:?}");
}
