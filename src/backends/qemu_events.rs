//! The guest's own shutdown reason, from QEMU's `SHUTDOWN` monitor event.
//!
//! A guest that must not be reset in place (macOS with a passed-through card:
//! the AppleSMC halt) is defined with `on_reboot=destroy`, so libvirt reports
//! a reboot from inside the guest exactly like a power-off: the domain stops.
//! QEMU's own `SHUTDOWN` event still says which one it was
//! (`"reason":"guest-reset"` against `"guest-shutdown"`), and libvirt passes
//! it on through `virsh qemu-monitor-event`. This source streams those events
//! for every domain, so the runtime can start a rebooted guest again instead
//! of handing its card back. Nothing polls: `virsh` blocks on libvirt.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};

use serde_json::Value;

use crate::error::{ClnxError, Result};

/// One `SHUTDOWN` event of a guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestShutdown {
    /// The domain.
    pub domain: String,
    /// The guest reset itself (a reboot from inside), rather than power off.
    pub reset: bool,
    /// The guest initiated it (not the host).
    pub guest: bool,
}

/// Streams QEMU `SHUTDOWN` events through `virsh qemu-monitor-event --loop`.
#[derive(Debug, Clone)]
pub struct QemuShutdownEvents {
    uri: String,
}

impl QemuShutdownEvents {
    /// A source for one libvirt URI, such as `qemu:///system`.
    #[must_use]
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }

    /// Start streaming; the stream ends when the receiver is dropped or
    /// `virsh` exits.
    ///
    /// # Errors
    ///
    /// `virsh` cannot be started.
    pub fn subscribe(&self) -> Result<Receiver<GuestShutdown>> {
        let mut child = Command::new("virsh")
            .args([
                "-c",
                &self.uri,
                "qemu-monitor-event",
                "--event",
                "SHUTDOWN",
                "--loop",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| ClnxError::backend("virsh qemu-monitor-event", error.to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ClnxError::backend("virsh qemu-monitor-event", "no stdout".to_owned())
        })?;
        let (sender, receiver) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                let Some(event) = parse_shutdown_line(&line) else {
                    continue;
                };
                if sender.send(event).is_err() {
                    break;
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        });
        Ok(receiver)
    }
}

/// Parse one `virsh qemu-monitor-event` line, such as
/// `event SHUTDOWN at 1731700000.123456 for domain 'name': {"guest":true,"reason":"guest-reset"}`.
///
/// Returns `None` for any other line. A `SHUTDOWN` without details (an older
/// QEMU) is a power-off, never a reset.
#[must_use]
pub fn parse_shutdown_line(line: &str) -> Option<GuestShutdown> {
    let rest = line.strip_prefix("event SHUTDOWN at ")?;
    let (_, rest) = rest.split_once(" for domain '")?;
    let (domain, details) = rest.split_once("': ")?;
    let details: Value = serde_json::from_str(details.trim()).unwrap_or(Value::Null);
    let reason = details.get("reason").and_then(Value::as_str).unwrap_or("");
    Some(GuestShutdown {
        domain: domain.to_owned(),
        reset: reason == "guest-reset",
        guest: details
            .get("guest")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}
