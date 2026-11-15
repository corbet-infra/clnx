//! The sysfs seam.
//!
//! Every host fact the device driver uses is a file below a root directory:
//! `/sys` in production, a temporary fixture tree in tests. Keeping the root a
//! parameter is what makes a live IOMMU group, a driver symlink and a BAR size
//! testable without a host.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{ClnxError, Result};

/// The sysfs operations the Linux driver needs.
pub trait Sysfs {
    /// Every PCI device name below `bus/pci/devices`, sorted.
    fn pci_devices(&self) -> Result<Vec<String>>;

    /// Read a text file below the root, with the trailing newline removed.
    fn read(&self, relative: &str) -> Result<String>;

    /// Write a value to a file below the root, without adding a newline.
    fn write(&self, relative: &str, value: &str) -> Result<()>;

    /// Resolve a symbolic link below the root and return its target text.
    fn read_link(&self, relative: &str) -> Result<String>;

    /// The size in bytes of a binary attribute, such as a BAR file.
    fn file_len(&self, relative: &str) -> Result<u64>;

    /// Whether a path below the root exists.
    fn exists(&self, relative: &str) -> bool;
}

/// The real sysfs tree, or a fixture tree that looks like one.
#[derive(Debug, Clone)]
pub struct LinuxSysfs {
    root: PathBuf,
}

impl LinuxSysfs {
    /// The live `/sys`.
    #[must_use]
    pub fn system() -> Self {
        Self::new("/sys")
    }

    /// A tree rooted elsewhere (a fixture or a test directory).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root this tree resolves paths under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl Sysfs for LinuxSysfs {
    fn pci_devices(&self) -> Result<Vec<String>> {
        let directory = self.path("bus/pci/devices");
        let entries = fs::read_dir(&directory).map_err(|error| ClnxError::io(&directory, error))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| ClnxError::io(&directory, error))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
        names.sort_unstable();
        Ok(names)
    }

    fn read(&self, relative: &str) -> Result<String> {
        let path = self.path(relative);
        let text = fs::read_to_string(&path).map_err(|error| ClnxError::io(&path, error))?;
        Ok(text.trim_end_matches(['\n', '\r']).to_owned())
    }

    fn write(&self, relative: &str, value: &str) -> Result<()> {
        let path = self.path(relative);
        fs::write(&path, value).map_err(|error| ClnxError::io(&path, error))
    }

    fn read_link(&self, relative: &str) -> Result<String> {
        let path = self.path(relative);
        let target = fs::read_link(&path).map_err(|error| ClnxError::io(&path, error))?;
        Ok(target.to_string_lossy().into_owned())
    }

    fn file_len(&self, relative: &str) -> Result<u64> {
        let path = self.path(relative);
        let metadata = fs::metadata(&path).map_err(|error| ClnxError::io(&path, error))?;
        Ok(metadata.len())
    }

    fn exists(&self, relative: &str) -> bool {
        self.path(relative).exists()
    }
}
