//! The libvirt seam, realised with the libvirt project's own `virt` crate.
//!
//! The driver never polls: a [`DomainEventSource`] pushes lifecycle events and
//! [`Libvirt::events`] hands its receiver to the caller. The `virt` 0.5.0 crate
//! exposes the libvirt event loop but not
//! `virConnectDomainEventRegisterAny`, so the production source is injected
//! and the default [`NoDomainEvents`] reports that gap instead of inventing a
//! poll. Tests inject a source fed by a channel.

use std::sync::mpsc::Receiver;

use cdrv::{BalloonStats, InstanceState, StopMode};
use virt::connect::Connect;
use virt::domain::{Domain, DomainState};
use virt::enumutil::Enum;

use crate::error::{ClnxError, Result};
use crate::events::DomainEvent;

/// One libvirt connection's operations, as the driver needs them.
pub trait Libvirt {
    /// Define (or redefine) a domain from rendered XML; never start it.
    fn define(&self, xml: &str) -> Result<()>;

    /// Start a defined domain.
    fn start(&self, name: &str) -> Result<()>;

    /// Ask a guest to stop cleanly; never a destroy.
    fn shutdown(&self, name: &str, mode: StopMode) -> Result<()>;

    /// Force a domain off. Only the contract's explicit destroy reaches this.
    fn destroy(&self, name: &str) -> Result<()>;

    /// Observe a domain's state.
    fn state(&self, name: &str) -> Result<InstanceState>;

    /// Every domain with its observed state.
    fn list(&self) -> Result<Vec<(String, InstanceState)>>;

    /// Move a domain's balloon target.
    fn set_balloon(&self, name: &str, target_mib: u64) -> Result<()>;

    /// Current balloon and resident figures for a domain.
    fn balloon_stats(&self, name: &str) -> Result<BalloonStats>;

    /// Subscribe to lifecycle events; the source pushes, nothing polls.
    fn events(&self) -> Result<Receiver<DomainEvent>>;
}

/// A source of lifecycle events, injected into [`RealLibvirt`].
pub trait DomainEventSource {
    /// Subscribe to events from now on.
    fn subscribe(&self) -> Result<Receiver<DomainEvent>>;
}

/// The default event source: reports that the binding is missing.
///
/// The `virt` crate has no domain-event registration in 0.5.0. A deployment
/// that needs lifecycle events injects a source built on the C binding (or a
/// future `virt` release) rather than polling. This is listed as a contract
/// gap in the README.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDomainEvents;

impl DomainEventSource for NoDomainEvents {
    fn subscribe(&self) -> Result<Receiver<DomainEvent>> {
        Err(ClnxError::Unsupported(
            "the virt 0.5.0 crate exposes no virConnectDomainEventRegisterAny binding; inject a DomainEventSource"
                .to_owned(),
        ))
    }
}

/// The real libvirt driver over one connection.
pub struct RealLibvirt {
    connection: Connect,
    events: Box<dyn DomainEventSource>,
}

impl std::fmt::Debug for RealLibvirt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealLibvirt").finish_non_exhaustive()
    }
}

impl RealLibvirt {
    /// The system connection, `qemu:///system`.
    ///
    /// # Errors
    ///
    /// Fails when libvirt does not answer.
    pub fn system() -> Result<Self> {
        Self::connect("qemu:///system")
    }

    /// A connection to an explicit libvirt URI.
    ///
    /// # Errors
    ///
    /// Fails when libvirt does not answer.
    pub fn connect(uri: &str) -> Result<Self> {
        let connection = Connect::open(Some(uri)).map_err(backend)?;
        Ok(Self {
            connection,
            events: Box::new(NoDomainEvents),
        })
    }

    /// Use an explicit lifecycle event source.
    #[must_use]
    pub fn with_event_source(mut self, events: Box<dyn DomainEventSource>) -> Self {
        self.events = events;
        self
    }

    fn domain(&self, name: &str) -> Result<Domain> {
        self.connection.lookup_domain_by_name(name).map_err(backend)
    }
}

impl Libvirt for RealLibvirt {
    fn define(&self, xml: &str) -> Result<()> {
        self.connection
            .define_domain_xml(xml)
            .map(|_domain| ())
            .map_err(backend)
    }

    fn start(&self, name: &str) -> Result<()> {
        self.domain(name)?.create().map_err(backend)
    }

    fn shutdown(&self, name: &str, mode: StopMode) -> Result<()> {
        let domain = self.domain(name)?;
        match mode {
            StopMode::Clean => domain
                .shutdown_flags(
                    virt::sys::VIR_DOMAIN_SHUTDOWN_GUEST_AGENT
                        | virt::sys::VIR_DOMAIN_SHUTDOWN_ACPI_POWER_BTN,
                )
                .map_err(backend),
            StopMode::Hibernate => domain.managed_save(0).map_err(backend),
        }
    }

    fn destroy(&self, name: &str) -> Result<()> {
        self.domain(name)?.destroy().map_err(backend)
    }

    fn state(&self, name: &str) -> Result<InstanceState> {
        domain_state(&self.domain(name)?)
    }

    fn list(&self) -> Result<Vec<(String, InstanceState)>> {
        let domains = self.connection.list_all_domains(0).map_err(backend)?;
        let mut observed = Vec::with_capacity(domains.len());
        for domain in domains {
            let name = domain.name().map_err(backend)?;
            observed.push((name, domain_state(&domain)?));
        }
        Ok(observed)
    }

    fn set_balloon(&self, name: &str, target_mib: u64) -> Result<()> {
        // virDomainSetMemory takes KiB.
        self.domain(name)?
            .set_memory(target_mib.saturating_mul(1024))
            .map_err(backend)
    }

    fn balloon_stats(&self, name: &str) -> Result<BalloonStats> {
        let stats = self.domain(name)?.memory_stats(0).map_err(backend)?;
        let mut target_kib = None;
        let mut resident_kib = None;
        for stat in stats {
            if stat.tag == virt::sys::VIR_DOMAIN_MEMORY_STAT_ACTUAL_BALLOON {
                target_kib = Some(stat.val);
            } else if stat.tag == virt::sys::VIR_DOMAIN_MEMORY_STAT_RSS {
                resident_kib = Some(stat.val);
            }
        }
        Ok(BalloonStats {
            target_mib: target_kib.unwrap_or(0) / 1024,
            actual_mib: resident_kib.map(|kib| kib / 1024),
            free_pages: None,
        })
    }

    fn events(&self) -> Result<Receiver<DomainEvent>> {
        self.events.subscribe()
    }
}

fn backend(error: virt::error::Error) -> ClnxError {
    ClnxError::backend("libvirt", error.to_string())
}

/// Map libvirt's state onto the contract's state set.
///
/// The contract has no paused state; a paused or PM-suspended domain is live,
/// so it reads as [`InstanceState::Running`]. The README lists the gap.
fn domain_state(domain: &Domain) -> Result<InstanceState> {
    let (state, _reason) = domain.state().map_err(backend)?;
    Ok(match state {
        Enum::Known(DomainState::NoState | DomainState::Shutoff) => InstanceState::Stopped,
        Enum::Known(DomainState::Running | DomainState::Blocked) => InstanceState::Running,
        Enum::Known(DomainState::Paused | DomainState::PMSuspended) => InstanceState::Running,
        Enum::Known(DomainState::Shutdown) => InstanceState::Stopping,
        Enum::Known(DomainState::Crashed) => InstanceState::Failed("domain crashed".to_owned()),
        Enum::Unknown(raw) => InstanceState::Failed(format!("unknown libvirt state {raw}")),
    })
}
