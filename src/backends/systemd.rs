//! The systemd seam over D-Bus.
//!
//! The desktop container session and the Sunshine unit are systemd units, not
//! shell commands: the driver talks to the manager's D-Bus interface through
//! `zbus`. The user-unit methods use a session-bus connection; a deployment
//! that must reach another user's manager sets `DBUS_SESSION_BUS_ADDRESS`
//! (for example by running inside the container) rather than gaining a shell.
//!
//! `StartUnit` and `StopUnit` only queue a job. Every call here waits for that
//! job's `JobRemoved` signal (subscribed to before the call, so none is
//! missed) and fails on any result other than `done` or on the deadline.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use zbus::export::futures_core::Stream;
use zbus::message::Type;
use zbus::{Connection, MatchRule, MessageStream};

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
/// How long a unit job may take before the call gives up.
pub const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(120);

const PATH: &str = "/org/freedesktop/systemd1";

/// The real systemd driver over the system bus, with an optional session bus.
pub struct ZbusSystemd {
    runtime: tokio::runtime::Runtime,
    system: Connection,
    user: Option<Connection>,
    job_timeout: Duration,
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

    /// Connect to the systemd manager on the bus at `address` (for example a
    /// container's system bus, `unix:path=/proc/<pid>/root/run/dbus/system_bus_socket`),
    /// with no session bus.
    ///
    /// # Errors
    ///
    /// Fails when the address is invalid or the bus does not answer.
    pub fn system_at(address: &str) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| ClnxError::backend("tokio", error.to_string()))?;
        let builder = zbus::connection::Builder::address(address);
        let system = runtime
            .block_on(builder.build())
            .map_err(|error| ClnxError::backend("systemd", format!("{address}: {error}")))?;
        Ok(Self {
            runtime,
            system,
            user: None,
            job_timeout: DEFAULT_JOB_TIMEOUT,
        })
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
            job_timeout: DEFAULT_JOB_TIMEOUT,
        })
    }

    /// How long a start or stop may take to finish (default 120 s).
    #[must_use]
    pub fn with_job_timeout(mut self, timeout: Duration) -> Self {
        self.job_timeout = timeout;
        self
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
        self.block_on(unit_call(&self.system, "StartUnit", unit, self.job_timeout))
    }

    fn stop_unit(&self, unit: &str) -> Result<()> {
        self.block_on(unit_call(&self.system, "StopUnit", unit, self.job_timeout))
    }

    fn start_user_unit(&self, unit: &str) -> Result<()> {
        self.block_on(unit_call(
            self.user_bus()?,
            "StartUnit",
            unit,
            self.job_timeout,
        ))
    }

    fn stop_user_unit(&self, unit: &str) -> Result<()> {
        self.block_on(unit_call(
            self.user_bus()?,
            "StopUnit",
            unit,
            self.job_timeout,
        ))
    }
}

async fn unit_call(
    connection: &Connection,
    method: &str,
    unit: &str,
    timeout: Duration,
) -> Result<()> {
    // Subscribe to the signal first: the job can finish before the reply to
    // the call is read, and a signal sent earlier would be lost. No sender in
    // the rule: signals carry the manager's unique name, not the well-known
    // one, and only the manager emits `JobRemoved` on this path.
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(MANAGER)
        .member("JobRemoved")
        .path(PATH)
        .build()
        .map_err(dbus_error)?;
    let mut signals = JobSignals(
        MessageStream::for_match_rule(rule, connection, None)
            .await
            .map_err(dbus_error)?,
    );
    // Some systemd versions send job signals only to subscribed clients; an
    // answer such as "already subscribed" is not a failure.
    let _ = connection
        .call_method(Some(DESTINATION), PATH, Some(MANAGER), "Subscribe", &())
        .await;
    let reply = connection
        .call_method(
            Some(DESTINATION),
            PATH,
            Some(MANAGER),
            method,
            &(unit, "replace"),
        )
        .await
        .map_err(dbus_error)?;
    let job: zbus::OwnedObjectPath = reply.body().deserialize().map_err(dbus_error)?;
    wait_for_job(&mut signals, job.as_str(), unit, timeout).await
}

/// One `JobRemoved` signal: the job path, the unit it ran for and its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSignal {
    /// The job's object path.
    pub job: String,
    /// The unit name.
    pub unit: String,
    /// `done`, `canceled`, `timeout`, `failed`, `dependency` or `skipped`.
    pub result: String,
}

/// Wait for the signal of `job` in a stream of job signals.
///
/// Signals of other jobs are skipped. Returns `Ok` only for the result
/// `done`; any other result, the end of the stream, or `timeout` elapsing
/// first is an error that names the unit and the result.
///
/// # Errors
///
/// See above.
pub async fn wait_for_job<S>(
    signals: &mut S,
    job: &str,
    unit: &str,
    timeout: Duration,
) -> Result<()>
where
    S: Stream<Item = JobSignal> + Unpin,
{
    let waiting = async {
        loop {
            match poll_fn(|cx| Pin::new(&mut *signals).poll_next(cx)).await {
                Some(signal) if signal.job == job => return Some(signal),
                Some(_) => {}
                None => return None,
            }
        }
    };
    match tokio::time::timeout(timeout, waiting).await {
        Ok(Some(signal)) if signal.result == "done" => Ok(()),
        Ok(Some(signal)) => Err(ClnxError::backend(
            "systemd",
            format!("job for {unit} ended with result {}", signal.result),
        )),
        Ok(None) => Err(ClnxError::backend(
            "systemd",
            format!("the signal stream ended before the job for {unit} finished"),
        )),
        Err(_) => Err(ClnxError::backend(
            "systemd",
            format!(
                "job for {unit} did not finish within {} s (result: timeout)",
                timeout.as_secs()
            ),
        )),
    }
}

/// The manager's `JobRemoved` messages, parsed into [`JobSignal`]s.
struct JobSignals(MessageStream);

impl Stream for JobSignals {
    type Item = JobSignal;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<JobSignal>> {
        loop {
            match Pin::new(&mut self.0).poll_next(cx) {
                Poll::Ready(Some(Ok(message))) => {
                    let body = message.body();
                    if let Ok((_id, job, unit, result)) =
                        body.deserialize::<(u32, zbus::OwnedObjectPath, String, String)>()
                    {
                        return Poll::Ready(Some(JobSignal {
                            job: job.to_string(),
                            unit,
                            result,
                        }));
                    }
                }
                Poll::Ready(Some(Err(_))) => {}
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A scripted stream; once empty it ends, or stays silent when `silent`.
    struct Script {
        items: VecDeque<JobSignal>,
        silent: bool,
    }

    impl Stream for Script {
        type Item = JobSignal;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<JobSignal>> {
            match self.items.pop_front() {
                Some(item) => Poll::Ready(Some(item)),
                None if self.silent => Poll::Pending,
                None => Poll::Ready(None),
            }
        }
    }

    fn signal(job: &str, unit: &str, result: &str) -> JobSignal {
        JobSignal {
            job: job.to_owned(),
            unit: unit.to_owned(),
            result: result.to_owned(),
        }
    }

    fn run(items: Vec<JobSignal>, silent: bool, timeout: Duration) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let mut script = Script {
            items: items.into(),
            silent,
        };
        runtime.block_on(wait_for_job(&mut script, "/job/7", "a.service", timeout))
    }

    #[test]
    fn done_for_my_job_succeeds_and_other_jobs_are_skipped() {
        let items = vec![
            signal("/job/6", "b.service", "failed"),
            signal("/job/7", "a.service", "done"),
        ];
        run(items, false, Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn any_result_but_done_names_the_unit_and_the_result() {
        for result in ["failed", "timeout", "canceled", "dependency"] {
            let error = run(
                vec![signal("/job/7", "a.service", result)],
                false,
                Duration::from_secs(5),
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("a.service") && error.contains(result),
                "{error}"
            );
        }
    }

    #[test]
    fn a_silent_manager_hits_the_deadline() {
        let error = run(
            vec![signal("/job/6", "b.service", "done")],
            true,
            Duration::from_millis(50),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("a.service") && error.contains("timeout"),
            "{error}"
        );
    }

    #[test]
    fn a_stream_that_ends_is_an_error() {
        let error = run(vec![], false, Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        assert!(error.contains("a.service"), "{error}");
    }
}
