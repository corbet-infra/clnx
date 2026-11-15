//! The process seam: who holds a device node open, and waiting for them.
//!
//! The card can only be handed to vfio-pci when no process holds its DRM
//! nodes. The scan reads `/proc/<pid>/fd` links; the wait is event-driven
//! through pidfds, never a poll loop with sleeps. Nothing here kills a process.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

/// A process that holds one of the scanned nodes open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// The process id.
    pub pid: u32,
    /// The process name (`/proc/<pid>/comm`).
    pub comm: String,
    /// The node it holds.
    pub node: PathBuf,
}

/// The process operations the device driver needs.
pub trait Procs {
    /// Every process with an open fd whose link target is one of `nodes`.
    fn holders(&self, nodes: &[PathBuf]) -> Vec<Holder>;

    /// Wait until all of `pids` have exited, or `timeout` has passed.
    fn wait_exit(&self, pids: &[u32], timeout: Duration);
}

/// The real process table, or a fixture tree that looks like one.
#[derive(Debug, Clone)]
pub struct LinuxProcs {
    root: PathBuf,
}

impl LinuxProcs {
    /// The live `/proc`.
    #[must_use]
    pub fn system() -> Self {
        Self::new("/proc")
    }

    /// A tree rooted elsewhere (a fixture). Waiting still uses real pidfds.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root this tree resolves paths under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Procs for LinuxProcs {
    fn holders(&self, nodes: &[PathBuf]) -> Vec<Holder> {
        let mut found = Vec::new();
        if nodes.is_empty() {
            return found;
        }
        let Ok(entries) = fs::read_dir(&self.root) else {
            return found;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            // A process may vanish at any step; that is not an error.
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            let mut seen: Vec<&PathBuf> = Vec::new();
            for fd in fds.flatten() {
                let Ok(target) = fs::read_link(fd.path()) else {
                    continue;
                };
                if let Some(node) = nodes.iter().find(|node| **node == target)
                    && !seen.contains(&node)
                {
                    seen.push(node);
                    let comm = fs::read_to_string(entry.path().join("comm"))
                        .map(|text| text.trim_end().to_owned())
                        .unwrap_or_default();
                    found.push(Holder {
                        pid,
                        comm,
                        node: node.clone(),
                    });
                }
            }
        }
        found.sort_by(|a, b| (a.pid, &a.node).cmp(&(b.pid, &b.node)));
        found
    }

    fn wait_exit(&self, pids: &[u32], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        for &pid in pids {
            let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
                continue;
            };
            // A pid that is already gone cannot be opened: it has exited.
            let Ok(fd) = pidfd_open(pid, PidfdFlags::empty()) else {
                continue;
            };
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return;
                }
                let spec = Timespec {
                    tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
                    tv_nsec: i64::from(left.subsec_nanos()),
                };
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                match poll(&mut fds, Some(&spec)) {
                    Ok(0) => return,
                    Ok(_) => break,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(_) => return,
                }
            }
        }
    }
}
