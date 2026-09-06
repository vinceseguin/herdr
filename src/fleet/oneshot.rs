//! Driving the connector from a plain, non-async caller.
//!
//! `herdr fleet status` needs the whole fleet answered once, or watched until
//! the operator stops it. Both are the same three steps — start the connector,
//! fold its events into a [`FleetState`], decide when there is nothing left to
//! wait for — so they live here rather than in the CLI, where E3 and the tests
//! cannot reuse them.
//!
//! These helpers block the calling thread. E2 and E3 own a runtime and drive
//! [`FleetConnector`] directly instead.

use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TryRecvError;

use crate::config::Config;
use crate::fleet::connector::{FleetConnector, FleetConnectorOptions, FleetEvent};
use crate::fleet::hosts::HostSpec;
use crate::fleet::hosts_source::hosts_for_config;
use crate::fleet::report::FleetStatusReport;
use crate::fleet::state::{FleetChange, FleetState, HostConnection, HostState};

/// How often a settling collector looks for the next event.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A running fleet: the connector, and the state its events are folded into.
pub struct FleetSession {
    state: FleetState,
    connector: FleetConnector,
}

impl FleetSession {
    /// Resolve `[fleet]` (and the saved machines, when opted in) and open
    /// every enabled host.
    ///
    /// `Err` carries the config diagnostics: an invalid `[fleet]` section is a
    /// user error the caller reports and exits on, not a host failure.
    pub fn start(config: &Config) -> Result<Self, Vec<String>> {
        let specs = hosts_for_config(config)?;
        Ok(Self::with_specs(
            specs,
            FleetConnectorOptions::for_config(config),
        ))
    }

    fn with_specs(specs: Vec<HostSpec>, options: FleetConnectorOptions) -> Self {
        let mut state = FleetState::new(specs.clone());
        let connector = FleetConnector::start(specs, options);
        // A status collector renders nothing, so no host is active and every
        // host stays at the inactive surface size.
        state.set_active_host(None);
        if let Err(error) = connector.set_active(None) {
            tracing::warn!(error = %error, "could not clear the active fleet host");
        }
        Self { state, connector }
    }

    /// Wait until every host has answered once, or until `timeout`.
    ///
    /// Returns the deltas observed while waiting.
    pub fn settle(&mut self, timeout: Duration) -> Vec<FleetChange> {
        let deadline = Instant::now() + timeout;
        let mut changes = Vec::new();
        while !self.is_settled() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            // A status collector never detaches the receiver, so `None` here
            // would be a bug in this module rather than a host fact; treat it
            // as "nothing else can arrive" instead of unwrapping.
            let Some(events) = self.connector.events() else {
                break;
            };
            let received = events.try_recv();
            match received {
                Ok(event) => changes.extend(self.apply(event)),
                Err(TryRecvError::Empty) => {
                    std::thread::sleep(SETTLE_POLL_INTERVAL.min(remaining));
                }
                // Every supervisor exited: nothing else can arrive.
                Err(TryRecvError::Disconnected) => break,
            }
        }
        changes
    }

    /// Block until at least one host changes something.
    ///
    /// `None` means no host can change anything again (every supervisor has
    /// stopped), which is how `--watch` ends without a signal.
    ///
    /// Must not be called from inside an async runtime; E3 drives the
    /// connector's receiver with `.await` instead.
    pub fn next_changes(&mut self) -> Option<Vec<FleetChange>> {
        loop {
            let event = self.connector.events()?.blocking_recv()?;
            let changes = self.apply(event);
            if !changes.is_empty() {
                return Some(changes);
            }
        }
    }

    /// The report for the state as it stands.
    pub fn report(&mut self) -> FleetStatusReport {
        FleetStatusReport::from_state(&mut self.state, &crate::build_info::version())
    }

    pub fn shutdown(self) {
        self.connector.shutdown();
    }

    fn apply(&mut self, event: FleetEvent) -> Vec<FleetChange> {
        match event {
            FleetEvent::Host { host, event } => self.state.apply(&host, event),
            // Surfaces, notifications and responses are runtime traffic, not
            // fleet state; a status collector has no active host and asks for
            // nothing, so anything else that arrives is simply dropped.
            _ => Vec::new(),
        }
    }

    /// Whether every host has reached a state that will not change on its own.
    fn is_settled(&self) -> bool {
        self.state.hosts().iter().all(host_settled)
    }
}

/// A host is settled once it is connected *with* a projection, or has failed.
///
/// Connected-without-a-snapshot is deliberately not settled: the report would
/// show a host with no workspaces and no agents, which reads as "empty" rather
/// than "still arriving".
fn host_settled(host: &HostState) -> bool {
    match &host.connection {
        HostConnection::Connected { .. } => host.snapshot.is_some(),
        HostConnection::Unavailable { .. } | HostConnection::Incompatible { .. } => true,
        HostConnection::Connecting { .. } => false,
    }
}

/// Collect one fleet status.
///
/// Exits early once every host has answered; `timeout` bounds the wait for the
/// ones that have not. Unreachable hosts are data, not an error — the only
/// `Err` is an invalid `[fleet]` section.
pub fn collect_status(
    config: &Config,
    timeout: Duration,
) -> Result<FleetStatusReport, Vec<String>> {
    let mut session = FleetSession::start(config)?;
    session.settle(timeout);
    let report = session.report();
    session.shutdown();
    Ok(report)
}

/// Watch the fleet: one settled report, then every delta as it happens.
///
/// `on_report` and `on_change` return `false` to stop watching. Returns when
/// they do, or when no host can produce another change.
pub fn watch(
    config: &Config,
    settle_timeout: Duration,
    on_report: impl FnOnce(&FleetStatusReport) -> bool,
    mut on_change: impl FnMut(&FleetChange) -> bool,
) -> Result<(), Vec<String>> {
    let mut session = FleetSession::start(config)?;
    session.settle(settle_timeout);
    let watching = on_report(&session.report());
    if watching {
        'watching: while let Some(changes) = session.next_changes() {
            for change in &changes {
                if !on_change(change) {
                    break 'watching;
                }
            }
        }
    }
    session.shutdown();
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::fleet::report::FLEET_STATUS_SCHEMA;
    use crate::ipc::bind_local_listener;
    use interprocess::local_socket::traits::Listener as _;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).expect("config parses")
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "herdr-oneshot-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn an_invalid_fleet_section_is_a_config_error() {
        let config = config(
            r#"
[fleet]
include_local = true

[[fleet.hosts]]
name = "local"
kind = "local"
"#,
        );
        let diagnostics = collect_status(&config, Duration::from_millis(50))
            .expect_err("an invalid [fleet] section must not produce a report");
        assert!(
            !diagnostics.is_empty(),
            "the error must carry the diagnostics"
        );
    }

    #[test]
    fn an_unreachable_host_settles_immediately() {
        let config = config(
            r#"
[fleet]
include_local = false

[[fleet.hosts]]
name = "nowhere"
kind = "local"
session = "herdr-fleet-oneshot-missing"
"#,
        );
        let started = Instant::now();
        let report = collect_status(&config, Duration::from_secs(30)).expect("a report");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a host that cannot be reached must not hold the report: {:?}",
            started.elapsed()
        );
        assert_eq!(report.schema, FLEET_STATUS_SCHEMA);
        assert_eq!(report.client_version, crate::build_info::version());
        assert_eq!(report.hosts.len(), 1);
        assert_eq!(report.hosts[0].connection.state_name(), "unavailable");
        assert!(report.hosts[0]
            .connection
            .reason()
            .is_some_and(|reason| reason.contains("no herdr server")));
        assert!(report.agents.is_empty());
        // Unreachable hosts are data: the caller still gets a report.
        assert_eq!(report.active_host, None);
    }

    #[test]
    fn a_host_that_never_answers_is_still_connecting_at_the_timeout() {
        let dir = scratch_dir("silent");
        // `LocalTransport` derives the socket from the session directory, so
        // the whole real path is exercised with an isolated config home.
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let socket = crate::session::client_socket_path_for(Some("silent"));
        std::fs::create_dir_all(socket.parent().expect("session dir")).expect("session dir");
        let listener = bind_local_listener(&socket).expect("bind");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = std::sync::Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let mut held = Vec::new();
            while !server_stop.load(std::sync::atomic::Ordering::Acquire) {
                match listener.accept() {
                    Ok(stream) => held.push(stream),
                    Err(_) => return,
                }
            }
        });

        let config = config(
            r#"
[fleet]
include_local = false

[[fleet.hosts]]
name = "silent"
kind = "local"
session = "silent"
"#,
        );
        let started = Instant::now();
        let report = collect_status(&config, Duration::from_millis(400)).expect("a report");
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "the collector must wait for a host that has not answered: {:?}",
            started.elapsed()
        );
        assert_eq!(report.hosts[0].connection.state_name(), "connecting");

        stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = crate::ipc::connect_local_stream(&socket);
        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
