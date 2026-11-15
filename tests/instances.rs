//! Lifecycle: a stop never destroys, and events are translated, not polled.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, channel};

use cdrv::traits::HostInstances;
use cdrv::{
    BalloonStats, CpuClaim, EventLog, GuestOs, InstanceClass, InstanceId, InstanceKind,
    InstanceSpec, InstanceState, MemoryClaim, Severity, Source, StopMode, Subject, Timestamp,
};
use clnx::{
    ClnxError, DomainEvent, DomainEventKind, Libvirt, LinuxInstances, RenderOptions, StartDetail,
    StopDetail, translate,
};

#[derive(Default)]
struct Log {
    calls: Vec<String>,
    defined: Option<String>,
}

struct FakeLibvirt {
    log: Rc<RefCell<Log>>,
    state: InstanceState,
    events: RefCell<Option<Receiver<DomainEvent>>>,
}

impl FakeLibvirt {
    fn new(log: Rc<RefCell<Log>>, state: InstanceState) -> Self {
        Self {
            log,
            state,
            events: RefCell::new(None),
        }
    }

    fn with_events(log: Rc<RefCell<Log>>, events: Receiver<DomainEvent>) -> Self {
        Self {
            log,
            state: InstanceState::Running,
            events: RefCell::new(Some(events)),
        }
    }
}

impl Libvirt for FakeLibvirt {
    fn define(&self, xml: &str) -> clnx::Result<()> {
        let mut log = self.log.borrow_mut();
        log.calls.push("define".to_owned());
        log.defined = Some(xml.to_owned());
        Ok(())
    }

    fn start(&self, name: &str) -> clnx::Result<()> {
        self.log.borrow_mut().calls.push(format!("start:{name}"));
        Ok(())
    }

    fn shutdown(&self, name: &str, mode: StopMode) -> clnx::Result<()> {
        self.log
            .borrow_mut()
            .calls
            .push(format!("shutdown:{name}:{mode:?}"));
        Ok(())
    }

    fn destroy(&self, name: &str) -> clnx::Result<()> {
        self.log.borrow_mut().calls.push(format!("destroy:{name}"));
        Ok(())
    }

    fn state(&self, _name: &str) -> clnx::Result<InstanceState> {
        Ok(self.state.clone())
    }

    fn list(&self) -> clnx::Result<Vec<(String, InstanceState)>> {
        Ok(vec![("demo-vm".to_owned(), self.state.clone())])
    }

    fn set_balloon(&self, name: &str, target_mib: u64) -> clnx::Result<()> {
        self.log
            .borrow_mut()
            .calls
            .push(format!("balloon:{name}:{target_mib}"));
        Ok(())
    }

    fn balloon_stats(&self, _name: &str) -> clnx::Result<BalloonStats> {
        Ok(BalloonStats {
            target_mib: 4096,
            actual_mib: Some(3900),
            free_pages: None,
        })
    }

    fn events(&self) -> clnx::Result<Receiver<DomainEvent>> {
        self.events
            .borrow_mut()
            .take()
            .ok_or_else(|| ClnxError::Unsupported("no event source in this fake".to_owned()))
    }
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        id: InstanceId::from("demo-vm"),
        kind: InstanceKind::Vm,
        guest_os: GuestOs::Linux,
        memory: MemoryClaim::fixed(4096),
        cpu: CpuClaim { quota_cores: 2.0 },
        class: InstanceClass::Dev,
        devices: Vec::new(),
        disks: Vec::new(),
        vcpus: None,
        nic: None,
        console: false,
        identity: None,
    }
}

fn driver(fake: FakeLibvirt) -> LinuxInstances<FakeLibvirt> {
    LinuxInstances::new("host-a", fake, RenderOptions::default())
}

fn log() -> Rc<RefCell<Log>> {
    Rc::new(RefCell::new(Log::default()))
}

#[test]
fn define_renders_the_spec_and_never_starts_it() {
    let log = log();
    let instance = driver(FakeLibvirt::new(Rc::clone(&log), InstanceState::Defined));
    instance.define(&spec()).unwrap();

    let log = log.borrow();
    assert_eq!(log.calls, vec!["define".to_owned()]);
    let xml = log.defined.as_deref().expect("the rendered XML");
    assert!(xml.contains("<name>demo-vm</name>"));
    assert!(xml.contains("<memory unit='MiB'>4096</memory>"));
    assert!(!log.calls.iter().any(|call| call.starts_with("start")));
}

#[test]
fn a_clean_stop_is_a_shutdown_and_never_a_destroy() {
    let log = log();
    let instance = driver(FakeLibvirt::new(Rc::clone(&log), InstanceState::Running));
    let id = InstanceId::from("demo-vm");
    instance.stop(&id, StopMode::Clean).unwrap();
    instance.stop(&id, StopMode::Hibernate).unwrap();

    let calls = &log.borrow().calls;
    assert!(calls.contains(&"shutdown:demo-vm:Clean".to_owned()));
    assert!(calls.contains(&"shutdown:demo-vm:Hibernate".to_owned()));
    assert!(
        !calls.iter().any(|call| call.starts_with("destroy")),
        "a stop must never destroy: {calls:?}"
    );
}

#[test]
fn the_explicit_destroy_is_the_only_destroy() {
    let log = log();
    let instance = driver(FakeLibvirt::new(Rc::clone(&log), InstanceState::Running));
    instance.destroy(&InstanceId::from("demo-vm")).unwrap();
    assert_eq!(log.borrow().calls, vec!["destroy:demo-vm".to_owned()]);
}

#[test]
fn list_uses_the_instance_identifier() {
    let instance = driver(FakeLibvirt::new(log(), InstanceState::Stopped));
    let listed = instance.list().unwrap();
    assert_eq!(
        listed,
        vec![(InstanceId::from("demo-vm"), InstanceState::Stopped)]
    );
}

#[test]
fn events_are_translated_from_the_source() {
    let (sender, receiver) = channel();
    sender
        .send(DomainEvent {
            domain: "demo-vm".to_owned(),
            at: Timestamp::from_secs(1),
            kind: DomainEventKind::Started(StartDetail::Booted),
        })
        .unwrap();
    sender
        .send(DomainEvent {
            domain: "demo-vm".to_owned(),
            at: Timestamp::from_secs(2),
            kind: DomainEventKind::Stopped(StopDetail::Crashed),
        })
        .unwrap();
    drop(sender);

    let instance = driver(FakeLibvirt::with_events(log(), receiver));
    let events: Vec<_> = instance.events().unwrap().collect();
    assert_eq!(events.len(), 2);

    assert_eq!(events[0].kind, "instance.starting");
    assert_eq!(events[0].source, Source::Clnx);
    assert_eq!(events[0].host, "host-a");
    assert_eq!(events[0].seq, 1);
    assert_eq!(
        events[0].subject,
        Subject::Instance(InstanceId::from("demo-vm"))
    );
    assert_eq!(events[0].severity, Severity::Info);
    assert_eq!(events[0].data["state"], "starting");
    assert_eq!(events[0].data["detail"], "booted");

    assert_eq!(events[1].kind, "instance.failed");
    assert_eq!(events[1].severity, Severity::Error);
    assert_eq!(events[1].seq, 2);
    assert_eq!(events[1].data["state"], "failed");
    assert_eq!(events[1].data["detail"], "crashed");
}

#[test]
fn a_missing_event_source_is_reported_not_polled() {
    let instance = driver(FakeLibvirt::new(log(), InstanceState::Running));
    match instance.events() {
        Ok(_) => panic!("the driver must not invent an event stream"),
        Err(error) => assert!(error.to_string().contains("no event source")),
    }
}

#[test]
fn suspend_is_a_warning_because_the_contract_has_no_paused_state() {
    let mut log = EventLog::new("host-a", Source::Clnx, 1);
    let event = DomainEvent {
        domain: "demo-vm".to_owned(),
        at: Timestamp::from_secs(3),
        kind: DomainEventKind::Suspended,
    };
    let translated = translate(&event, &mut log);
    assert_eq!(translated.kind, "instance.paused");
    assert_eq!(translated.severity, Severity::Warn);
    assert_eq!(translated.data["state"], "paused");
}

#[test]
fn a_destroyed_stop_is_an_info_event() {
    let mut log = EventLog::new("host-a", Source::Clnx, 1);
    let event = DomainEvent {
        domain: "demo-vm".to_owned(),
        at: Timestamp::from_secs(4),
        kind: DomainEventKind::Stopped(StopDetail::Destroyed),
    };
    let translated = translate(&event, &mut log);
    assert_eq!(translated.kind, "instance.stopped");
    assert_eq!(translated.severity, Severity::Info);
}
