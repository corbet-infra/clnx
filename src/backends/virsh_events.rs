//! Lifecycle events from libvirt's own client, `virsh event`.
//!
//! The `virt` 0.5.0 crate has no `virConnectDomainEventRegisterAny`, and this
//! crate forbids `unsafe`. `virsh event --loop` is the libvirt project's event
//! client: it blocks on libvirt's event stream and prints one line per
//! lifecycle change, so a reader thread wakes only when something happened.
//! Nothing polls. A native registration can replace this source behind the
//! same [`DomainEventSource`] seam.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

use super::libvirt::DomainEventSource;
use crate::error::{ClnxError, Result};
use crate::events::{DomainEvent, DomainEventKind, StartDetail, StopDetail, now_timestamp};

/// Streams lifecycle events by running `virsh event --loop`.
#[derive(Debug, Clone)]
pub struct VirshEvents {
    uri: String,
}

impl VirshEvents {
    /// An event source for one libvirt URI, such as `qemu:///system`.
    #[must_use]
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }
}

impl DomainEventSource for VirshEvents {
    fn subscribe(&self) -> Result<Receiver<DomainEvent>> {
        let mut child = Command::new("virsh")
            .args(["-c", &self.uri, "event", "--event", "lifecycle", "--loop"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| ClnxError::backend("virsh event", error.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClnxError::backend("virsh event", "no stdout".to_owned()))?;
        let (sender, receiver) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let Some((domain, kind)) = parse_line(&line) else {
                    continue;
                };
                let Ok(at) = now_timestamp() else { continue };
                if sender.send(DomainEvent { domain, at, kind }).is_err() {
                    break;
                }
            }
            // Receiver gone or stream ended: stop the client, then reap it.
            let _ = child.kill();
            let _ = child.wait();
        });
        Ok(receiver)
    }
}

/// Parse one `virsh event` line, such as
/// `event 'lifecycle' for domain 'name': Stopped Shutdown`.
///
/// Returns `None` for lines that are not lifecycle changes `clnx` reacts to.
#[must_use]
pub fn parse_line(line: &str) -> Option<(String, DomainEventKind)> {
    let rest = line.strip_prefix("event 'lifecycle' for domain ")?;
    let rest = rest.strip_prefix('\'').unwrap_or(rest);
    let (name, what) = rest.split_once(": ")?;
    let name = name.trim_end_matches('\'');
    let (event, detail) = what.trim().split_once(' ').unwrap_or((what.trim(), ""));
    let kind = match event {
        "Defined" => DomainEventKind::Defined,
        "Undefined" => DomainEventKind::Undefined,
        "Started" => DomainEventKind::Started(match detail {
            "Booted" => StartDetail::Booted,
            "Migrated" => StartDetail::Migrated,
            "Restored" => StartDetail::Restored,
            "Snapshot" => StartDetail::FromSnapshot,
            "Event wakeup" => StartDetail::Wakeup,
            _ => StartDetail::Unknown,
        }),
        "Suspended" => DomainEventKind::Suspended,
        "Resumed" => DomainEventKind::Resumed,
        "Shutdown" => DomainEventKind::Shutdown,
        "Stopped" => DomainEventKind::Stopped(match detail {
            "Shutdown" => StopDetail::Shutoff,
            "Destroyed" => StopDetail::Destroyed,
            "Crashed" => StopDetail::Crashed,
            "Migrated" => StopDetail::Migrated,
            "Saved" => StopDetail::Saved,
            "Failed" => StopDetail::Failed,
            "Snapshot" => StopDetail::FromSnapshot,
            _ => StopDetail::Unknown,
        }),
        "Crashed" => DomainEventKind::Stopped(StopDetail::Crashed),
        _ => return None,
    };
    Some((name.to_owned(), kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_lines_parse() {
        let (name, kind) =
            parse_line("event 'lifecycle' for domain 'alpha': Started Booted").unwrap();
        assert_eq!(name, "alpha");
        assert_eq!(kind, DomainEventKind::Started(StartDetail::Booted));
        let (_, kind) = parse_line("event 'lifecycle' for domain 'alpha': Stopped Saved").unwrap();
        assert_eq!(kind, DomainEventKind::Stopped(StopDetail::Saved));
        let (_, kind) = parse_line("event 'lifecycle' for domain 'a': Shutdown Finished").unwrap();
        assert_eq!(kind, DomainEventKind::Shutdown);
        let (_, kind) = parse_line("event 'lifecycle' for domain 'a': Suspended Paused").unwrap();
        assert_eq!(kind, DomainEventKind::Suspended);
    }

    #[test]
    fn other_lines_are_ignored() {
        assert!(parse_line("events received: 1").is_none());
        assert!(parse_line("event 'reboot' for domain 'a'").is_none());
        assert!(parse_line("event 'lifecycle' for domain 'a': Mystery Thing").is_none());
    }
}
