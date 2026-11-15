//! The command seam: run one program with fixed arguments, no shell.
//!
//! Used for the per-instance clean-stop command, which is private deployment
//! data (for example an ssh call that makes the guest halt itself).

use std::process::{Command, Stdio};

use crate::error::{ClnxError, Result};

/// Runs a command to completion.
pub trait CommandRunner {
    /// Run `argv` (program first) and wait for it.
    ///
    /// The exit status is not an error: a session to a guest that halts is
    /// cut by the remote side. Failing to spawn the program is.
    fn run(&self, argv: &[String]) -> Result<()>;
}

/// Spawns real processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, argv: &[String]) -> Result<()> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| ClnxError::InvalidSpec("the clean-stop command is empty".to_owned()))?;
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .status()
            .map_err(|error| {
                ClnxError::backend("clean stop", format!("cannot run {program}: {error}"))
            })?;
        Ok(())
    }
}
