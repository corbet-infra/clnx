//! The systemd seam over D-Bus.
//!
//! The desktop container session and the Sunshine unit are systemd units, not
//! shell commands: the driver talks to the manager's D-Bus interface through
//! `zbus`. The user-unit methods use a session-bus connection; a deployment
//! that must reach another user's manager sets `DBUS_SESSION_BUS_ADDRESS`
//! (for example by running inside the container) rather than gaining a shell.

use std::future::Future;

use zbus::Connection;

use crate::error::{ClnxError, Result};

/// The unit operations the host GPU platform needs.
pub trait Systemd {
    /// Whether a system unit is active (starting and reloading count as active).
    fn unit_active(&self, unit: &str) -> Result<bool>;

    /// Start a system unit.
    fn start_unit(&self, unit: &str) -> Result<()>;

    /// Stop a system unit.
    fn stop_unit(&self, unit: &str) -> Result<()>;

    /// Start a unit in the user manager the connection names.
    fn start_user_unit(&self, unit: &str) -> Result<()>;

    /// Stop a unit in the user manager the connection names.
    fn stop_user_unit(&self, unit: &str) -> Result<()>;
}

/// The unit-state tuple systemd returns from `ListUnitsByNames`.
type UnitState = (
    String,
    String,
    String,
    String,
    String,
    String,
    zbus::OwnedObjectPath,
    u32,
    String,
    zbus::OwnedObjectPath,
);

const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const DESTINATION: &str = "org.freedesktop.systemd1";
const PATH: &str = "/org/freedesktop/systemd1";

/// The real systemd driver over the system bus, with an optional session bus.
pub struct ZbusSystemd {
    runtime: tokio::runtime::Runtime,
    system: Connection,
    user: Option<Connection>,
}

impl std::fmt::Debug for ZbusSystemd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZbusSystemd")
            .field("user_bus", &self.user.is_some())
            .finish_non_exhaustive()
    }
}

impl ZbusSystemd {
    /// Connect to the system bus only.
    ///
    /// User-unit calls then report that no session bus is configured.
    ///
    /// # Errors
    ///
    /// Fails when the D-Bus daemon does not answer or the runtime cannot be
    /// created.
    pub fn system() -> Result<Self> {
        Self::connect(false)
    }

    /// Connect to the system bus and the session bus named by
    /// `DBUS_SESSION_BUS_ADDRESS`.
    ///
    /// # Errors
    ///
    /// Fails when either bus does not answer.
    pub fn with_user_bus() -> Result<Self> {
        Self::connect(true)
    }

    fn connect(user: bool) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| ClnxError::backend("tokio", error.to_string()))?;
        let system = runtime
            .block_on(Connection::system())
            .map_err(|error| ClnxError::backend("systemd", error.to_string()))?;
        let user = if user {
            Some(
                runtime
                    .block_on(Connection::session())
                    .map_err(|error| ClnxError::backend("systemd user bus", error.to_string()))?,
            )
        } else {
            None
        };
        Ok(Self {
            runtime,
            system,
            user,
        })
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    fn user_bus(&self) -> Result<&Connection> {
        self.user.as_ref().ok_or_else(|| {
            ClnxError::Unsupported(
                "no session bus is configured; construct with ZbusSystemd::with_user_bus"
                    .to_owned(),
            )
        })
    }
}

impl Systemd for ZbusSystemd {
    fn unit_active(&self, unit: &str) -> Result<bool> {
        let states = self.block_on(list_units(&self.system, unit))?;
        Ok(states
            .first()
            .is_some_and(|state| matches!(state.3.as_str(), "active" | "activating" | "reloading")))
    }

    fn start_unit(&self, unit: &str) -> Result<()> {
        self.block_on(unit_call(&self.system, "StartUnit", unit))
    }

    fn stop_unit(&self, unit: &str) -> Result<()> {
        self.block_on(unit_call(&self.system, "StopUnit", unit))
    }

    fn start_user_unit(&self, unit: &str) -> Result<()> {
        self.block_on(unit_call(self.user_bus()?, "StartUnit", unit))
    }

    fn stop_user_unit(&self, unit: &str) -> Result<()> {
        self.block_on(unit_call(self.user_bus()?, "StopUnit", unit))
    }
}

async fn unit_call(connection: &Connection, method: &str, unit: &str) -> Result<()> {
    connection
        .call_method(
            Some(DESTINATION),
            PATH,
            Some(MANAGER),
            method,
            &(unit, "replace"),
        )
        .await
        .map_err(dbus_error)?;
    Ok(())
}

async fn list_units(connection: &Connection, unit: &str) -> Result<Vec<UnitState>> {
    let names = vec![unit];
    let reply = connection
        .call_method(
            Some(DESTINATION),
            PATH,
            Some(MANAGER),
            "ListUnitsByNames",
            &names,
        )
        .await
        .map_err(dbus_error)?;
    reply.body().deserialize().map_err(dbus_error)
}

fn dbus_error(error: zbus::Error) -> ClnxError {
    ClnxError::backend("systemd", error.to_string())
}
