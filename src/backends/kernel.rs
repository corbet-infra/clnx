//! The host-kernel seam: module loading and module parameters.
//!
//! Binding itself is sysfs writes (see [`Sysfs`](crate::backends::Sysfs));
//! loading the module and reading a parameter that must hold before the card
//! is touched (`vfio-pci disable_idle_d3=1`) are separate acts with no Rust
//! API, so they live behind this small trait and are faked in tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{ClnxError, Result};

/// The kernel operations a passthrough bind needs.
pub trait Kernel {
    /// Load a module with options, as `modprobe module key=value ...`.
    fn load_module(&self, module: &str, options: &[(&str, &str)]) -> Result<()>;

    /// The current value of a module parameter.
    fn module_parameter(&self, module: &str, parameter: &str) -> Result<String>;
}

/// The live kernel, with `modprobe` and sysfs.
#[derive(Debug, Clone)]
pub struct LinuxKernel {
    sysfs_root: PathBuf,
    modprobe: PathBuf,
}

impl LinuxKernel {
    /// The live kernel: `/sys` and `modprobe` from `PATH`.
    #[must_use]
    pub fn system() -> Self {
        Self::new("/sys", "modprobe")
    }

    /// A kernel seam with an explicit sysfs root and modprobe binary.
    #[must_use]
    pub fn new(sysfs_root: impl Into<PathBuf>, modprobe: impl Into<PathBuf>) -> Self {
        Self {
            sysfs_root: sysfs_root.into(),
            modprobe: modprobe.into(),
        }
    }

    /// The sysfs root this seam reads parameters from.
    #[must_use]
    pub fn sysfs_root(&self) -> &Path {
        &self.sysfs_root
    }
}

impl Kernel for LinuxKernel {
    fn load_module(&self, module: &str, options: &[(&str, &str)]) -> Result<()> {
        let mut command = Command::new(&self.modprobe);
        command.arg(module);
        for (key, value) in options {
            command.arg(format!("{key}={value}"));
        }
        let output = command
            .output()
            .map_err(|error| ClnxError::io(&self.modprobe, error))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ClnxError::backend(
                "modprobe",
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ))
        }
    }

    fn module_parameter(&self, module: &str, parameter: &str) -> Result<String> {
        let path = self
            .sysfs_root
            .join("module")
            .join(module.replace('-', "_"))
            .join("parameters")
            .join(parameter.replace('-', "_"));
        let value = std::fs::read_to_string(&path).map_err(|error| ClnxError::io(&path, error))?;
        Ok(value.trim().to_owned())
    }
}
