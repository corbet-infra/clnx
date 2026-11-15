//! Domain lifecycle events and their translation into the `cdrv` envelope.
//!
//! libvirt reports lifecycle changes through its event loop, never through a
//! poll. [`translate`] is pure: the same observed event always produces the
//! same [`cdrv::Event`], so the mapping is testable without a hypervisor.

use std::time::{SystemTime, UNIX_EPOCH};

use cdrv::{Event, EventLog, InstanceId, Severity, Subject, Timestamp};
use serde_json::json;

/// The current wall-clock time as the contract's [`Timestamp`].
///
/// # Errors
///
/// Fails only when the system clock is set before the Unix epoch.
pub fn now_timestamp() -> crate::Result<Timestamp> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| crate::ClnxError::Clock(error.to_string()))?
        .as_millis();
    Ok(Timestamp::from_millis(millis as u64))
}

/// Why a domain started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartDetail {
    /// A normal boot.
    Booted,
    /// Resumed after migration.
    Migrated,
    /// Restored from a saved image.
    Restored,
    /// Restored from a snapshot.
    FromSnapshot,
    /// Woken from a suspended state.
    Wakeup,
    /// The hypervisor did not name a reason.
    Unknown,
}

impl StartDetail {
    /// The detail as the event payload carries it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Booted => "booted",
            Self::Migrated => "migrated",
            Self::Restored => "restored",
            Self::FromSnapshot => "from_snapshot",
            Self::Wakeup => "wakeup",
            Self::Unknown => "unknown",
        }
    }
}

/// Why a domain stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopDetail {
    /// The guest powered itself off.
    Shutoff,
    /// An explicit destroy.
    Destroyed,
    /// The guest crashed.
    Crashed,
    /// Saved to a state file.
    Saved,
    /// Migrated away.
    Migrated,
    /// The hypervisor reported a failure.
    Failed,
    /// The state was restored from a snapshot.
    FromSnapshot,
    /// The hypervisor did not name a reason.
    Unknown,
}

impl StopDetail {
    /// Whether this stop is a failure rather than a normal shutdown.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(self, Self::Crashed | Self::Failed)
    }

    /// The detail as the event payload carries it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shutoff => "shutoff",
            Self::Destroyed => "destroyed",
            Self::Crashed => "crashed",
            Self::Saved => "saved",
            Self::Migrated => "migrated",
            Self::Failed => "failed",
            Self::FromSnapshot => "from_snapshot",
            Self::Unknown => "unknown",
        }
    }
}

/// One lifecycle change as the backend observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainEvent {
    /// The libvirt domain name, which is the instance identifier.
    pub domain: String,
    /// When the backend observed the event.
    pub at: Timestamp,
    /// What changed.
    pub kind: DomainEventKind,
}

/// The lifecycle changes `clnx` reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainEventKind {
    /// The domain was defined.
    Defined,
    /// The domain was undefined.
    Undefined,
    /// The domain started.
    Started(StartDetail),
    /// A suspended domain resumed.
    Resumed,
    /// The domain was suspended or paused.
    Suspended,
    /// The guest asked to shut down.
    Shutdown,
    /// The domain stopped.
    Stopped(StopDetail),
}

/// Translate one observed domain event into the contract's envelope.
///
/// A suspend is reported as `instance.paused` with the contract's `paused`
/// state.
#[must_use]
pub fn translate(event: &DomainEvent, log: &mut EventLog) -> Event {
    let (kind, state, severity, detail) = match event.kind {
        DomainEventKind::Defined => ("instance.defined", "defined", Severity::Info, None),
        DomainEventKind::Undefined => ("instance.undefined", "stopped", Severity::Info, None),
        DomainEventKind::Started(detail) => (
            "instance.starting",
            "starting",
            Severity::Info,
            Some(detail.as_str()),
        ),
        DomainEventKind::Resumed => ("instance.running", "running", Severity::Info, None),
        DomainEventKind::Suspended => ("instance.paused", "paused", Severity::Warn, Some("paused")),
        DomainEventKind::Shutdown => ("instance.stopping", "stopping", Severity::Info, None),
        DomainEventKind::Stopped(detail) if detail.is_failure() => (
            "instance.failed",
            "failed",
            Severity::Error,
            Some(detail.as_str()),
        ),
        DomainEventKind::Stopped(detail) => (
            "instance.stopped",
            "stopped",
            Severity::Info,
            Some(detail.as_str()),
        ),
    };
    let mut data = json!({ "state": state });
    if let Some(detail) = detail {
        data["detail"] = json!(detail);
    }
    log.emit(
        event.at,
        kind,
        Subject::Instance(InstanceId::from(event.domain.as_str())),
        severity,
        data,
    )
}
