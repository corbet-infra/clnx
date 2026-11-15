//! The process seam: who holds a device node open, and waiting for them.
//!
//! The card can only be handed to vfio-pci when no process holds its DRM
//! nodes. The scan matches the DEVICE NUMBER of every open fd under
//! `/proc/<pid>/fd`, not the link text: a process in a container sees the
//! same node under its own name (the host's `card2` can be a container's
//! `card0`). The wait is event-driven through pidfds, never a poll loop with
//! sleeps. Nothing here kills a process.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

/// A character device node to look for, by device number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceNode {
    /// The host's name for the node, used in messages.
    pub node: PathBuf,
    /// Its device number (`st_rdev`).
    pub rdev: u64,
}

/// The device number Linux encodes for `major:minor` (glibc's `makedev`).
#[must_use]
pub const fn makedev(major: u32, minor: u32) -> u64 {
    let (major, minor) = (major as u64, minor as u64);
    ((major & 0xffff_f000) << 32)
        | ((major & 0x0000_0fff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0x0000_00ff)
}

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
    /// Every process with an open fd on one of `nodes` (matched by device
    /// number).
    fn holders(&self, nodes: &[DeviceNode]) -> Vec<Holder>;

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
    fn holders(&self, nodes: &[DeviceNode]) -> Vec<Holder> {
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
            let mut seen: Vec<&DeviceNode> = Vec::new();
            for fd in fds.flatten() {
                // `metadata` follows the fd's magic link to the open inode, so
                // the device number is the one the process really holds.
                let Ok(metadata) = fs::metadata(fd.path()) else {
                    continue;
                };
                if !metadata.file_type().is_char_device() {
                    continue;
                }
                let rdev = metadata.rdev();
                if let Some(node) = nodes.iter().find(|node| node.rdev == rdev)
                    && !seen.contains(&node)
                {
                    seen.push(node);
                    let comm = fs::read_to_string(entry.path().join("comm"))
                        .map(|text| text.trim_end().to_owned())
                        .unwrap_or_default();
                    found.push(Holder {
                        pid,
                        comm,
                        node: node.node.clone(),
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
