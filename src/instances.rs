//! VM lifecycle on libvirt.
//!
//! A stop is always a clean request (guest agent or ACPI); the explicit
//! [`HostInstances::destroy`] is the only path that calls libvirt's destroy,
//! because a destroyed GPU guest is how cards are left unusable. Events come
//! from the backend's event source and are translated, never polled.

use cdrv::traits::{EventStream, HostInstances, Result as CdrvResult};
use cdrv::{EventLog, InstanceId, InstanceSpec, InstanceState, Source, StopMode};

use crate::backends::Libvirt;
use crate::domain_xml::{RenderOptions, render_domain};
use crate::error::Result;
use crate::events::translate;

/// The Linux instance driver: libvirt domains rendered from the spec.
#[derive(Debug, Clone)]
pub struct LinuxInstances<L: Libvirt> {
    libvirt: L,
    options: RenderOptions,
    event_log: EventLog,
}

impl<L: Libvirt> LinuxInstances<L> {
    /// A driver for one host's libvirt connection.
    #[must_use]
    pub fn new(host: impl Into<String>, libvirt: L, options: RenderOptions) -> Self {
        Self {
            libvirt,
            options,
            event_log: EventLog::new(host, Source::Clnx, 1),
        }
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

impl<L: Libvirt> HostInstances for LinuxInstances<L> {
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
