//! VM lifecycle on libvirt.
//!
//! A stop is always a clean request (guest agent or ACPI, or a configured
//! command the guest itself obeys, for a guest that ignores ACPI); the explicit
//! [`HostInstances::destroy`] is the only path that calls libvirt's destroy,
//! because a destroyed GPU guest is how cards are left unusable. Events come
//! from the backend's event source and are translated, never polled.

use std::collections::HashMap;

use cdrv::traits::{EventStream, HostInstances, Result as CdrvResult};
use cdrv::{EventLog, InstanceId, InstanceSpec, InstanceState, Source, StopMode};

use crate::backends::{CommandRunner, Libvirt, ProcessRunner};
use crate::domain_xml::{RenderOptions, render_domain};
use crate::error::Result;
use crate::events::translate;

/// The Linux instance driver: libvirt domains rendered from the spec.
#[derive(Debug, Clone)]
pub struct LinuxInstances<L: Libvirt, C: CommandRunner = ProcessRunner> {
    libvirt: L,
    runner: C,
    clean_stop: HashMap<String, Vec<String>>,
    options: RenderOptions,
    event_log: EventLog,
}

impl<L: Libvirt> LinuxInstances<L> {
    /// A driver for one host's libvirt connection.
    #[must_use]
    pub fn new(host: impl Into<String>, libvirt: L, options: RenderOptions) -> Self {
        Self {
            libvirt,
            runner: ProcessRunner,
            clean_stop: HashMap::new(),
            options,
            event_log: EventLog::new(host, Source::Clnx, 1),
        }
    }
}

impl<L: Libvirt, C: CommandRunner> LinuxInstances<L, C> {
    /// Use another command runner (a fake in tests).
    #[must_use]
    pub fn with_command_runner<D: CommandRunner>(self, runner: D) -> LinuxInstances<L, D> {
        LinuxInstances {
            libvirt: self.libvirt,
            runner,
            clean_stop: self.clean_stop,
            options: self.options,
            event_log: self.event_log,
        }
    }

    /// Stop `instance` cleanly by running `argv` (program first, no shell)
    /// instead of the libvirt guest-agent/ACPI request.
    ///
    /// For a guest that ignores ACPI, such as macOS: the command makes the
    /// guest halt itself (for example an ssh `shutdown -h now`). It is
    /// private deployment data.
    #[must_use]
    pub fn with_clean_stop(mut self, instance: impl Into<String>, argv: Vec<String>) -> Self {
        self.clean_stop.insert(instance.into(), argv);
        self
    }

    /// The deployment data used to render this host's domains.
    #[must_use]
    pub fn options(&self) -> &RenderOptions {
        &self.options
    }

    /// Render the domain XML for a spec without defining it.
    ///
    /// # Errors
    ///
    /// Fails on a spec the platform rules refuse.
    pub fn render(&self, spec: &InstanceSpec) -> Result<String> {
        render_domain(spec, &self.options)
    }
}

impl<L: Libvirt, C: CommandRunner> HostInstances for LinuxInstances<L, C> {
    fn list(&self) -> CdrvResult<Vec<(InstanceId, InstanceState)>> {
        Ok(self
            .libvirt
            .list()?
            .into_iter()
            .map(|(name, state)| (InstanceId::from(name), state))
            .collect())
    }

    fn define(&self, spec: &InstanceSpec) -> CdrvResult<()> {
        let xml = self.render(spec)?;
        self.libvirt.define(&xml)?;
        Ok(())
    }

    fn start(&self, id: &InstanceId) -> CdrvResult<()> {
        self.libvirt.start(id.as_str())?;
        Ok(())
    }

    fn stop(&self, id: &InstanceId, mode: StopMode) -> CdrvResult<()> {
        if mode == StopMode::Clean
            && let Some(argv) = self.clean_stop.get(id.as_str())
        {
            // The caller waits for the lifecycle event, not for this exit
            // status: the guest cuts the session while it halts.
            self.runner.run(argv)?;
            return Ok(());
        }
        self.libvirt.shutdown(id.as_str(), mode)?;
        Ok(())
    }

    fn destroy(&self, id: &InstanceId) -> CdrvResult<()> {
        self.libvirt.destroy(id.as_str())?;
        Ok(())
    }

    fn state(&self, id: &InstanceId) -> CdrvResult<InstanceState> {
        Ok(self.libvirt.state(id.as_str())?)
    }

    fn events(&self) -> CdrvResult<EventStream> {
        let receiver = self.libvirt.events()?;
        let mut log = self.event_log.clone();
        Ok(Box::new(
            receiver
                .into_iter()
                .map(move |event| translate(&event, &mut log)),
        ))
    }
}
