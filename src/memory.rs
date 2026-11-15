//! Memory facts and balloon control on Linux.
//!
//! The facts are the ones the fleet's memory guard uses: `MemAvailable` plus
//! the evictable ZFS ARC, capped at `size - c_min`, and the memory PSI `full
//! avg10` share. The parsers are pure and live in [`crate::proc`]; they are
//! the same rules `cdct` applies. The README notes that they belong in `cdrv`
//! long term, so the conductor and every driver share one implementation.

use std::path::{Path, PathBuf};

use cdrv::traits::{HostMemory, Result as CdrvResult};
use cdrv::{BalloonStats, InstanceId, MemoryFacts};

use crate::backends::Libvirt;
use crate::error::{ClnxError, Result};
use crate::proc::{
    ParseError, parse_evictable_arc_mib, parse_mem_available_mib, parse_pressure_full_avg10,
};

/// The pressure a decision must assume when the kernel reports none.
///
/// A missing PSI file is not calm: the fleet treats incomplete pressure facts
/// as severe, so no swap is allowed on them.
pub const SEVERE_PRESSURE_AVG10: f32 = 100.0;

/// The Linux memory driver over a libvirt connection and a proc root.
#[derive(Debug, Clone)]
pub struct LinuxMemory<L: Libvirt> {
    libvirt: L,
    proc_root: PathBuf,
}

impl<L: Libvirt> LinuxMemory<L> {
    /// A driver reading the live `/proc`.
    #[must_use]
    pub fn new(libvirt: L) -> Self {
        Self::with_proc_root(libvirt, "/proc")
    }

    /// A driver reading a fixture tree that looks like `/proc`.
    #[must_use]
    pub fn with_proc_root(libvirt: L, proc_root: impl Into<PathBuf>) -> Self {
        Self {
            libvirt,
            proc_root: proc_root.into(),
        }
    }

    /// The proc root this driver reads.
    #[must_use]
    pub fn proc_root(&self) -> &Path {
        &self.proc_root
    }

    fn read(&self, relative: &str) -> Result<String> {
        let path = self.proc_root.join(relative);
        std::fs::read_to_string(&path).map_err(|error| ClnxError::io(&path, error))
    }

    fn read_optional(&self, relative: &str) -> Result<Option<String>> {
        let path = self.proc_root.join(relative);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ClnxError::io(&path, error)),
        }
    }
}

impl<L: Libvirt> HostMemory for LinuxMemory<L> {
    fn facts(&self) -> CdrvResult<MemoryFacts> {
        let available_mib = parse_mem_available_mib(&self.read("meminfo")?).map_err(parse_error)?;
        let evictable_arc_mib = match self.read_optional("spl/kstat/zfs/arcstats")? {
            Some(text) => parse_evictable_arc_mib(&text).map_err(parse_error)?,
            None => 0,
        };
        let pressure_full_avg10 = match self.read_optional("pressure/memory")? {
            Some(text) => parse_pressure_full_avg10(&text)
                .map_err(parse_error)?
                .unwrap_or(SEVERE_PRESSURE_AVG10),
            None => SEVERE_PRESSURE_AVG10,
        };
        Ok(MemoryFacts {
            available_mib,
            evictable_arc_mib,
            pressure_full_avg10,
        })
    }

    fn set_balloon(&self, id: &InstanceId, target_mib: u64) -> CdrvResult<()> {
        self.libvirt.set_balloon(id.as_str(), target_mib)?;
        Ok(())
    }

    fn balloon_stats(&self, id: &InstanceId) -> CdrvResult<BalloonStats> {
        Ok(self.libvirt.balloon_stats(id.as_str())?)
    }
}

fn parse_error(error: ParseError) -> ClnxError {
    ClnxError::backend("proc", error.to_string())
}
