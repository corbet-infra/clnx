//! The one error type every `clnx` operation returns.

use std::path::PathBuf;

use thiserror::Error;

/// What went wrong in a Linux driver operation.
///
/// The variants deliberately distinguish a refusal (the host says no, and the
/// caller must not retry) from a backend failure (something external failed)
/// and from a bug in a spec. A refusal is a reported fact, never a retry.
#[derive(Debug, Error)]
pub enum ClnxError {
    /// A file operation below a root (`/sys`, `/proc`, a fixture tree) failed.
    #[error("I/O on {path}: {source}")]
    Io {
        /// The path that was touched.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// An external backend (libvirt, D-Bus, Kubernetes, modprobe) failed.
    #[error("{backend}: {message}")]
    Backend {
        /// The backend that failed.
        backend: &'static str,
        /// Its message.
        message: String,
    },
    /// The host refused the operation; retrying the same request cannot help.
    #[error("refused: {0}")]
    Refused(String),
    /// The instance spec is inconsistent and must be corrected by its author.
    #[error("invalid instance spec: {0}")]
    InvalidSpec(String),
    /// This driver has no implementation for the requested operation.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Reading the clock failed.
    #[error("clock: {0}")]
    Clock(String),
}

impl ClnxError {
    /// An I/O error with the path that caused it.
    #[must_use]
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// A backend error.
    #[must_use]
    pub fn backend(backend: &'static str, message: impl Into<String>) -> Self {
        Self::Backend {
            backend,
            message: message.into(),
        }
    }
}

/// The result type every `clnx` operation returns.
pub type Result<T> = std::result::Result<T, ClnxError>;
