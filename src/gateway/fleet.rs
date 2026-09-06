//! The gateway's fleet half: one [`FleetConnector`], one shared [`FleetState`],
//! many readers.
//!
//! `herdr fleet status` folds connector events on the calling thread and exits.
//! A gateway cannot: it runs for days, serves `/api/fleet` from whatever the
//! fleet looks like *now*, and streams every delta to any number of WebSocket
//! clients. [`FleetRuntime`] is that shape — a tokio task owning the connector's
//! event receiver, folding each [`FleetEvent::Host`] into a `FleetState` behind
//! a mutex, and broadcasting each resulting [`FleetChange`] **serialized once**
//! to every subscriber.
//!
//! Three properties this module exists to hold:
//!
//! - **Passive.** The connector is started with
//!   [`FleetConnectorOptions::for_daemon`]: a `surface_active: false` hello, so
//!   no host makes the gateway its foreground client, and batch-mode ssh, so no
//!   bridged child prompts or writes to the daemon's stderr. The active host is
//!   cleared at construction, so `active_host` is `null` exactly as
//!   `herdr fleet status --json` prints it.
//! - **Fan-out is cheap.** A change is serialized once into an `Arc<str>`;
//!   N subscribers clone a pointer. Nothing here is per-pane or per-frame.
//! - **No lock is held across an `.await`.** The state is a `std::sync::Mutex`
//!   and every critical section is a few statements of pure data work; the one
//!   blocking call in the module ([`FleetConnector::shutdown`], which joins
//!   supervisor threads) runs on `spawn_blocking`.
//!
//! Nothing here imports axum: this is the runtime, not the HTTP surface.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::fleet::connector::{FleetConnector, FleetConnectorOptions, FleetEvent};
use crate::fleet::hosts::{HostId, HostSpec};
use crate::fleet::hosts_source::hosts_for_config;
use crate::fleet::report::FleetStatusReport;
use crate::fleet::state::{FleetChange, FleetState, HostConnection, HostEvent};

/// How many pre-serialized changes a slow subscriber may fall behind before it
/// is told it lagged.
///
/// Matches the connector's own event channel depth: a subscriber that cannot
/// keep up with one host's reconnect storm is better served by a `Lagged`
/// marker and a fresh report than by an unbounded queue in the gateway.
const CHANGE_CHANNEL_CAPACITY: usize = 256;

/// How long [`FleetRuntime::shutdown`] waits for the fold task to notice the
/// stop signal before aborting it.
const FOLD_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// How long [`FleetRuntime::shutdown`] waits for the connector's own bounded
/// join (2 s) to finish on the blocking pool.
const CONNECTOR_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// A running fleet, folded into shared state for many readers.
///
/// Built inside a tokio runtime ([`FleetRuntime::start`] spawns a task) and
/// stopped with [`FleetRuntime::shutdown`]. Handlers never see this type: they
/// get a [`FleetHandle`] clone.
pub struct FleetRuntime {
    task: JoinHandle<()>,
    /// Shared so [`FleetRuntime::stopper`] can hand out a latch that outlives
    /// this struct's borrow — the process's shutdown future needs to end every
    /// stream *before* the HTTP server drains, and it cannot own the runtime.
    stop: Arc<watch::Sender<bool>>,
    connector: FleetConnector,
    handle: FleetHandle,
}

/// A latch that ends every [`ChangeStream`], without owning the runtime.
///
/// The gateway's shutdown future holds one: an open WebSocket keeps the HTTP
/// server's graceful shutdown waiting, so the streams have to be told the fleet
/// is going away first. Latching early is safe — [`FleetRuntime::shutdown`]
/// sets the same flag and is what actually joins anything.
#[derive(Clone)]
pub struct FleetStopper(Arc<watch::Sender<bool>>);

impl FleetStopper {
    /// Tell every subscriber the fleet is stopping. Idempotent.
    pub fn stop(&self) {
        // `send_replace`, not `send`: `send` leaves the value untouched when it
        // sees no receivers, and the latch must hold for a stream opened after
        // this point.
        let _ = self.0.send_replace(true);
    }
}

/// A cheap, cloneable reader of the fleet.
///
/// This is the only way anything outside this module reaches fleet data: a
/// request handler clones one, reads a report or one host's facts, and drops
/// it. Every method is sync, bounded, and never touches a socket.
#[derive(Clone)]
pub struct FleetHandle {
    /// The merged view. A `std::sync::Mutex` on purpose: every critical
    /// section is pure data work, and it is never held across an `.await`.
    state: Arc<Mutex<FleetState>>,
    /// Each [`FleetChange`] serialized exactly once, whatever the subscriber
    /// count — the fan-out here is × clients, so the JSON must not be.
    changes: broadcast::Sender<Arc<str>>,
    /// Set once by [`FleetRuntime::shutdown`]. Handed to every [`ChangeStream`]
    /// so a subscriber ends when the *runtime* stops, not when the last handle
    /// is dropped — the router holds a handle for the life of the process, so
    /// "no senders left" would never happen and a WebSocket task would wait
    /// forever on a fleet that is gone.
    stop: watch::Receiver<bool>,
    client_version: Arc<str>,
}

/// One subscriber's view of the change stream.
pub struct ChangeStream {
    changes: broadcast::Receiver<Arc<str>>,
    stop: watch::Receiver<bool>,
    /// Latched once the runtime has stopped, so the stream drains what is left
    /// and then ends for good.
    stopped: bool,
}

/// What a subscriber reads next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeItem {
    /// One [`FleetChange`], already JSON and free of newlines.
    Change(Arc<str>),
    /// The subscriber fell behind and changes were dropped. The stream is
    /// still usable; a consumer that needs consistency re-reads a report.
    Lagged,
}

impl FleetRuntime {
    /// Resolve `[fleet]` (and the saved machines, when opted in), open every
    /// enabled host, and start folding.
    ///
    /// `Err` carries the `[fleet]` diagnostics: an invalid section is an
    /// operator error the caller reports and exits on, never a host failure.
    /// An unreachable host is data, not an error.
    ///
    /// Must be called from inside a tokio runtime: it spawns the fold task.
    pub fn start(config: &Config) -> Result<Self, Vec<String>> {
        let specs = hosts_for_config(config)?;
        Ok(Self::over(
            specs.clone(),
            FleetConnector::start(specs, FleetConnectorOptions::for_daemon(config)),
        ))
    }

    /// Start the fold over an already-built connector.
    ///
    /// Split out so tests can drive the same runtime over fake hosts; the
    /// production path is [`Self::start`].
    fn over(specs: Vec<HostSpec>, mut connector: FleetConnector) -> Self {
        let mut state = FleetState::new(specs);
        // A gateway renders nothing, so no host is active: every host stays at
        // the inactive surface size and the report's `active_host` is null,
        // exactly as `herdr fleet status --json` prints it.
        state.set_active_host(None);
        if let Err(error) = connector.set_active(None) {
            tracing::warn!(target: "gateway", error = %error, "could not clear the active fleet host");
        }

        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let (stop, stop_rx) = watch::channel(false);
        let handle = FleetHandle {
            state: Arc::new(Mutex::new(state)),
            changes,
            stop: stop_rx.clone(),
            client_version: Arc::from(crate::build_info::version().as_str()),
        };

        let events = connector.take_events();
        let task = tokio::spawn(fold(events, stop_rx, handle.clone()));

        Self {
            task,
            stop: Arc::new(stop),
            connector,
            handle,
        }
    }

    /// A reader for handlers to clone.
    pub fn handle(&self) -> FleetHandle {
        self.handle.clone()
    }

    /// A latch the caller can pull when the process is stopping.
    pub fn stopper(&self) -> FleetStopper {
        FleetStopper(Arc::clone(&self.stop))
    }

    /// Stop the fold task, then every host supervisor.
    ///
    /// Order matters. The fold task owns the connector's event receiver, and
    /// [`FleetConnector::shutdown`] documents that the receiver must be dropped
    /// first — otherwise a supervisor parked on a full channel could only exit
    /// once it was. So: signal, await the task (which drops the receiver on the
    /// way out), then join the supervisors.
    ///
    /// The join is blocking (it waits on OS threads), so it runs on
    /// `spawn_blocking` rather than on a runtime worker: a request in flight
    /// must never be stalled by a host that is slow to hang up. Every wait is
    /// bounded — a shutdown that cannot complete logs and detaches rather than
    /// hanging the process.
    pub async fn shutdown(self) {
        let Self {
            mut task,
            stop,
            connector,
            handle,
        } = self;
        // Latch the stop *before* releasing this handle, and with
        // `send_replace` rather than `send`: `watch::Sender::send` leaves the
        // value untouched when it happens to see no receivers, and the latch is
        // what ends every stream — including one a handle cloned earlier opens
        // after this point. Only then drop the runtime's own handle.
        let _ = stop.send_replace(true);
        drop(handle);
        if tokio::time::timeout(FOLD_STOP_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            tracing::warn!(
                target: "gateway",
                "the fleet fold task did not stop in time; aborting it"
            );
            task.abort();
            // Awaiting the aborted handle is what drops the task's future, and
            // with it the connector's event receiver. Bounded too: `abort`
            // can only take effect at a yield point, so a task that somehow
            // never reaches one must not turn this wait into a hang. The
            // connector's own shutdown tolerates a receiver that is still
            // alive (it detaches after its bounded wait).
            if tokio::time::timeout(FOLD_STOP_TIMEOUT, &mut task)
                .await
                .is_err()
            {
                tracing::warn!(
                    target: "gateway",
                    "the fleet fold task did not stop after being aborted; detaching it"
                );
            }
        }

        match tokio::time::timeout(
            CONNECTOR_STOP_TIMEOUT,
            tokio::task::spawn_blocking(move || connector.shutdown()),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(target: "gateway", error = %error, "the fleet shutdown task failed")
            }
            Err(_) => tracing::warn!(
                target: "gateway",
                "the fleet connector did not stop within the shutdown wait; detaching"
            ),
        }
    }
}

impl FleetHandle {
    /// The fleet as it stands.
    ///
    /// One `from_state` per call: O(hosts × agents), and deliberately not
    /// cached — it is cheap, and a cache would race the delta stream.
    pub fn report(&self) -> FleetStatusReport {
        let mut state = lock(&self.state);
        FleetStatusReport::from_state(&mut state, &self.client_version)
    }

    /// One host's connection state, or `None` if `host` is not configured.
    ///
    /// Handlers use it to fail a request fast (a terminal on a host that is
    /// down is a stream-local error, never a 5xx) without building a report.
    pub fn host_connection(&self, host: &HostId) -> Option<HostConnection> {
        lock(&self.state)
            .host(host)
            .map(|host| host.connection.clone())
    }

    /// One host's resolved spec, or `None` if `host` is not configured.
    ///
    /// This is how a terminal stream learns *what* to open (a local session, an
    /// ssh target) without a second copy of the host list living in the gateway.
    pub fn host_spec(&self, host: &HostId) -> Option<HostSpec> {
        lock(&self.state).host(host).map(|host| host.spec.clone())
    }

    /// Subscribe, and take the report that subscription starts from.
    ///
    /// Both happen under the state lock, which the fold task also holds while
    /// it applies a change *and* publishes it. So for any change: either the
    /// report already contains it, or it arrives on the stream. Never neither,
    /// and never both.
    pub fn subscribe_with_report(&self) -> (ChangeStream, FleetStatusReport) {
        let mut state = lock(&self.state);
        let stream = self.subscribe();
        let report = FleetStatusReport::from_state(&mut state, &self.client_version);
        (stream, report)
    }

    /// A change stream with no report.
    ///
    /// Private on purpose: a subscriber with no starting report has no way to
    /// tell a delta it missed from one that never happened, so the public entry
    /// is [`Self::subscribe_with_report`].
    fn subscribe(&self) -> ChangeStream {
        ChangeStream {
            changes: self.changes.subscribe(),
            stop: self.stop.clone(),
            stopped: false,
        }
    }

    /// Fold one host event in and publish what changed.
    ///
    /// Private: the gateway is a reader. The fold task is the only production
    /// caller, and it is in this module.
    ///
    /// The publish happens **under the state lock** so that a change is never
    /// visible in a report before it is queued for existing subscribers, which
    /// is what makes [`Self::subscribe_with_report`] gapless.
    /// `broadcast::Sender::send` never blocks and never awaits, so this holds
    /// the lock for a serialization and a pointer push.
    fn apply(&self, host: &HostId, event: HostEvent) {
        let mut state = lock(&self.state);
        for change in state.apply(host, event) {
            self.publish(&change);
        }
    }

    fn publish(&self, change: &FleetChange) {
        match serde_json::to_string(change) {
            // No receivers is the ordinary case (nobody has opened
            // `/api/events`), not a failure.
            Ok(json) => {
                let _ = self.changes.send(Arc::from(json.as_str()));
            }
            // Unreachable for the shapes `FleetChange` is made of, and not
            // worth a panic in a daemon if it ever stops being: the state is
            // still correct, so the next report carries the fact this delta
            // would have.
            Err(error) => tracing::error!(
                target: "gateway",
                error = %error,
                "dropping a fleet change that would not serialize"
            ),
        }
    }
}

/// Test-only constructors, so another module's tests can drive a real
/// `FleetHandle` without a connector, a socket or a tokio runtime.
///
/// A separate `#[cfg(test)]` impl block, like `AppState`'s own test helpers:
/// nothing in a release build can reach these, and the production impl above
/// stays exactly the reader surface a handler sees.
#[cfg(test)]
impl FleetHandle {
    /// A handle over `state`, with a broadcast channel of exactly `capacity`.
    ///
    /// The stop sender comes back with the handle: dropping it is what a
    /// stopped runtime looks like, so a test that wants a live stream has to
    /// hold it, exactly as [`FleetRuntime`] does.
    pub(crate) fn test_new(state: FleetState, capacity: usize) -> (Self, watch::Sender<bool>) {
        let (changes, _) = broadcast::channel(capacity);
        let (stop, stop_rx) = watch::channel(false);
        let handle = Self {
            state: Arc::new(Mutex::new(state)),
            changes,
            stop: stop_rx,
            client_version: Arc::from("0.0.0-test"),
        };
        (handle, stop)
    }

    /// Fold one host event in, exactly as the fold task would.
    pub(crate) fn test_apply(&self, host: &HostId, event: HostEvent) {
        self.apply(host, event);
    }
}

impl ChangeStream {
    /// The next change, or `None` once the runtime has stopped and everything
    /// already queued for this subscriber has been read.
    ///
    /// Cancel-safe: the only state it keeps between calls is the latch, which
    /// is set before anything is returned.
    pub async fn next(&mut self) -> Option<ChangeItem> {
        loop {
            if self.stopped || *self.stop.borrow() {
                self.stopped = true;
                return Self::tail(&mut self.changes);
            }
            // Biased so a change that is already queued is delivered before the
            // stop signal is considered: a subscriber reads the whole fleet
            // history it was promised, then the stream ends.
            let stop_gone = {
                let Self { changes, stop, .. } = self;
                tokio::select! {
                    biased;
                    received = changes.recv() => return Self::item(received),
                    // A stop the runtime sent is read at the top of the loop;
                    // an `Err` means the runtime was dropped without one, which
                    // is the same end for this subscriber.
                    result = stop.changed() => result.is_err(),
                }
            };
            if stop_gone {
                self.stopped = true;
            }
        }
    }

    /// Whatever is still queued for a stopped stream, then `None`.
    fn tail(changes: &mut broadcast::Receiver<Arc<str>>) -> Option<ChangeItem> {
        match changes.try_recv() {
            Ok(change) => Some(ChangeItem::Change(change)),
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => Some(Self::lagged(skipped)),
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => None,
        }
    }

    fn item(received: Result<Arc<str>, broadcast::error::RecvError>) -> Option<ChangeItem> {
        match received {
            Ok(change) => Some(ChangeItem::Change(change)),
            Err(broadcast::error::RecvError::Lagged(skipped)) => Some(Self::lagged(skipped)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    fn lagged(skipped: u64) -> ChangeItem {
        tracing::warn!(
            target: "gateway",
            skipped,
            "a fleet change subscriber fell behind"
        );
        ChangeItem::Lagged
    }
}

/// The fold: every host event the connector produces, into one state.
///
/// Ends on the stop signal or when the connector's channel closes (every
/// supervisor has exited). Dropping `events` on the way out is what lets
/// [`FleetConnector::shutdown`] wake a supervisor parked on a full channel.
async fn fold(
    events: Option<tokio::sync::mpsc::Receiver<FleetEvent>>,
    mut stop: watch::Receiver<bool>,
    handle: FleetHandle,
) {
    let Some(mut events) = events else {
        // Unreachable: the receiver is taken from a connector built moments
        // ago. A daemon logs it instead of panicking; the runtime then serves
        // the initial state and no deltas, which is visible rather than fatal.
        tracing::error!(
            target: "gateway",
            "the fleet connector handed out no event stream; no deltas will be folded"
        );
        return;
    };
    loop {
        tokio::select! {
            // `changed()` also resolves once the sender is dropped, which is
            // the other way this task is asked to end.
            _ = stop.changed() => break,
            event = events.recv() => match event {
                Some(FleetEvent::Host { host, event }) => handle.apply(&host, event),
                // Surfaces, notifications and endpoint responses are runtime
                // traffic, not fleet state. The gateway activates no host, so
                // no surface can arrive; it asks for nothing, so no response
                // can. E7 adds the request lane here.
                Some(_) => {}
                // Every supervisor exited: nothing else can arrive.
                None => break,
            },
        }
    }
}

/// A mutex guard that survives a poisoned lock.
///
/// Mirrors the connector's `lock`: a panic in one reader must not take the
/// gateway with it. `FleetState`'s invariants are per-call, so a half-applied
/// event cannot leave it inconsistent for the next one.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::api::schema::AgentStatus;
    use crate::fleet::hosts::HostKind;
    use crate::protocol::ClientShellSnapshot;

    fn host(id: &str) -> HostId {
        HostId::new(id).expect("valid host id")
    }

    fn spec(id: &str) -> HostSpec {
        HostSpec {
            id: host(id),
            kind: HostKind::Local {
                session: Some(id.to_string()),
            },
            enabled: true,
        }
    }

    /// A handle over `state`, with a broadcast channel of exactly `capacity`.
    ///
    /// The fan-out and ordering properties are pure: they need a state and a
    /// channel, not a connector, a socket or a runtime.
    /// The stop sender comes back with the handle: dropping it is what a
    /// stopped runtime looks like, so a test that wants a live stream has to
    /// hold it, exactly as `FleetRuntime` does.
    fn test_handle(state: FleetState, capacity: usize) -> (FleetHandle, watch::Sender<bool>) {
        FleetHandle::test_new(state, capacity)
    }

    fn connected() -> HostEvent {
        HostEvent::Connected {
            server_version: "0.8.2-fork".to_string(),
            methods: vec!["pane.write".to_string()],
        }
    }

    fn snapshot(boot_id: &str, revision: u64) -> HostEvent {
        let mut snapshot: ClientShellSnapshot = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-snapshot-v1.json"
        )))
        .expect("the frozen snapshot decodes");
        snapshot.boot_id = boot_id.to_string();
        snapshot.revision = revision;
        HostEvent::Snapshot(Box::new(snapshot))
    }

    fn kinds(items: &[ChangeItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                ChangeItem::Lagged => "lagged".to_string(),
                ChangeItem::Change(json) => serde_json::from_str::<serde_json::Value>(json)
                    .ok()
                    .and_then(|value| value.get("kind")?.as_str().map(str::to_string))
                    .unwrap_or_else(|| format!("undecodable: {json}")),
            })
            .collect()
    }

    /// Drain whatever is already queued for a subscriber, without awaiting.
    fn drain(stream: &mut ChangeStream) -> Vec<ChangeItem> {
        let mut items = Vec::new();
        loop {
            match stream.changes.try_recv() {
                Ok(change) => items.push(ChangeItem::Change(change)),
                Err(broadcast::error::TryRecvError::Lagged(_)) => items.push(ChangeItem::Lagged),
                Err(_) => return items,
            }
        }
    }

    /// One serialization, N readers: the fan-out cost is a pointer clone.
    #[test]
    fn every_subscriber_reads_the_same_serialized_change() {
        let (handle, _stop) = test_handle(FleetState::new(vec![spec("alpha")]), 16);
        let mut first = handle.subscribe();
        let mut second = handle.subscribe();

        handle.apply(&host("alpha"), connected());
        handle.apply(&host("alpha"), snapshot("boot-alpha", 1));

        let a = drain(&mut first);
        let b = drain(&mut second);
        assert_eq!(kinds(&a), kinds(&b));
        assert_eq!(
            kinds(&a),
            vec!["host_connection", "snapshot", "agent_added"],
            "changes: {a:?}"
        );
        for (left, right) in a.iter().zip(b.iter()) {
            let (ChangeItem::Change(left), ChangeItem::Change(right)) = (left, right) else {
                panic!("expected changes, got {left:?} and {right:?}");
            };
            assert!(
                Arc::ptr_eq(left, right),
                "each change must be serialized once and shared by pointer"
            );
            assert!(
                !left.contains('\n'),
                "a change must stay newline-free for the event stream: {left}"
            );
        }
    }

    /// A change is either in the report or on the stream — never in neither.
    ///
    /// The applier runs on another thread while the subscription is taken, so
    /// this exercises the real interleaving rather than a scripted one.
    #[test]
    fn subscribing_never_loses_a_concurrent_change() {
        for iteration in 0..200u32 {
            let (handle, _stop) = test_handle(FleetState::new(vec![spec("alpha")]), 16);
            let applier = {
                let handle = handle.clone();
                std::thread::spawn(move || handle.apply(&host("alpha"), connected()))
            };
            let (mut stream, report) = handle.subscribe_with_report();
            applier.join().expect("applier thread");

            let in_report = report.hosts[0].connection.state_name() == "connected";
            let items = drain(&mut stream);
            let on_stream = kinds(&items).contains(&"host_connection".to_string());
            assert!(
                in_report || on_stream,
                "iteration {iteration}: the connection change was in neither the report \
                 ({}) nor the stream ({items:?})",
                report.hosts[0].connection.state_name()
            );
            assert!(
                !(in_report && on_stream),
                "iteration {iteration}: the change was delivered twice"
            );
        }
    }

    /// The report a subscription starts from is the passive one: no active
    /// host, exactly what `herdr fleet status --json` prints.
    #[test]
    fn the_report_names_the_client_version_and_no_active_host() {
        let mut state = FleetState::new(vec![spec("alpha"), spec("beta")]);
        state.set_active_host(None);
        let (handle, _stop) = test_handle(state, 4);
        let report = handle.report();
        assert_eq!(report.client_version, "0.0.0-test");
        assert_eq!(report.active_host, None);
        assert_eq!(report.hosts.len(), 2);
    }

    #[test]
    fn a_slow_subscriber_is_told_it_lagged_and_then_resumes() {
        let (handle, _stop) = test_handle(FleetState::new(vec![spec("alpha")]), 2);
        let mut stream = handle.subscribe();

        // Four changes through a channel that holds two.
        handle.apply(&host("alpha"), connected());
        handle.apply(&host("alpha"), snapshot("boot-alpha", 1));
        handle.apply(
            &host("alpha"),
            HostEvent::Unavailable {
                reason: "host went away".to_string(),
                retry_in: Some(Duration::from_secs(1)),
            },
        );
        handle.apply(&host("alpha"), connected());

        let items = drain(&mut stream);
        assert_eq!(
            items.first(),
            Some(&ChangeItem::Lagged),
            "an overflowed subscriber must be told: {items:?}"
        );
        assert!(
            items.len() > 1,
            "the stream must resume after a lag: {items:?}"
        );
        assert!(
            items[1..]
                .iter()
                .all(|item| matches!(item, ChangeItem::Change(_))),
            "only the overflow is reported, once: {items:?}"
        );
    }

    /// A stopped runtime ends every stream, even though the router still holds
    /// a `FleetHandle` (and with it a broadcast sender) for the life of the
    /// process. Without this a WebSocket task would wait forever on a fleet
    /// that is gone.
    #[tokio::test]
    async fn a_stopped_runtime_ends_its_streams_after_draining() {
        let (handle, stop) = test_handle(FleetState::new(vec![spec("alpha")]), 16);
        let (mut stream, _) = handle.subscribe_with_report();
        handle.apply(&host("alpha"), connected());
        assert!(stop.send(true).is_ok());

        // What was queued before the stop still arrives …
        assert!(matches!(stream.next().await, Some(ChangeItem::Change(_))));
        // … and then the stream ends, with the handle still very much alive.
        assert_eq!(stream.next().await, None);
        assert_eq!(stream.next().await, None, "the end must be sticky");
        assert_eq!(handle.report().hosts.len(), 1);
    }

    /// A runtime dropped without `shutdown` is the same end for a subscriber:
    /// nothing can produce another change, so the stream must not hang.
    #[tokio::test]
    async fn a_dropped_stop_signal_also_ends_a_stream() {
        let (handle, stop) = test_handle(FleetState::new(vec![spec("alpha")]), 16);
        let (mut stream, _) = handle.subscribe_with_report();
        drop(stop);
        assert_eq!(stream.next().await, None);
    }

    /// Host facts a handler can read without building a report.
    #[test]
    fn host_lookups_answer_for_configured_hosts_only() {
        let (handle, _stop) = test_handle(FleetState::new(vec![spec("alpha")]), 4);
        assert!(matches!(
            handle.host_connection(&host("alpha")),
            Some(HostConnection::Connecting { .. })
        ));
        assert_eq!(
            handle.host_spec(&host("alpha")).map(|spec| spec.kind),
            Some(HostKind::Local {
                session: Some("alpha".to_string())
            })
        );
        assert_eq!(handle.host_connection(&host("ghost")), None);
        assert_eq!(handle.host_spec(&host("ghost")), None);

        handle.apply(&host("alpha"), connected());
        assert!(handle
            .host_connection(&host("alpha"))
            .is_some_and(|connection| connection.is_connected()));
    }

    /// An event for a host nobody configured is dropped, not invented, and
    /// publishes nothing.
    #[test]
    fn an_event_for_an_unknown_host_publishes_nothing() {
        let (handle, _stop) = test_handle(FleetState::new(vec![spec("alpha")]), 8);
        let mut stream = handle.subscribe();
        handle.apply(&host("ghost"), connected());
        assert!(drain(&mut stream).is_empty());
        assert_eq!(handle.report().hosts.len(), 1);
    }

    /// Agent deltas reach the stream too: the gateway's event surface is the
    /// whole `FleetChange` vocabulary, not just connection state.
    #[test]
    fn agent_changes_are_published_verbatim() {
        let (handle, _stop) = test_handle(FleetState::new(vec![spec("alpha")]), 32);
        handle.apply(&host("alpha"), connected());
        let mut stream = handle.subscribe();
        handle.apply(&host("alpha"), snapshot("boot-alpha", 1));

        let items = drain(&mut stream);
        let kinds = kinds(&items);
        assert!(
            kinds.contains(&"agent_added".to_string()),
            "changes: {kinds:?}"
        );
        // The payload is the `FleetChange` JSON verbatim, so a reader decodes
        // it with the same type the CLI's `--watch` stream uses.
        let added = items
            .iter()
            .find_map(|item| match item {
                ChangeItem::Change(json) => serde_json::from_str::<FleetChange>(json)
                    .ok()
                    .filter(|change| matches!(change, FleetChange::AgentAdded { .. })),
                ChangeItem::Lagged => None,
            })
            .expect("an agent_added change decodes as a FleetChange");
        let FleetChange::AgentAdded { agent } = added else {
            panic!("expected an agent_added change");
        };
        assert_eq!(agent.pane.host(), &host("alpha"));
        assert_ne!(agent.agent_status, AgentStatus::Unknown);
    }

    /// A panic while the state lock is held poisons it. A daemon must survive
    /// that: the next reader recovers the guard and answers, rather than every
    /// later request panicking too.
    #[test]
    fn a_poisoned_state_lock_still_answers_readers() {
        let (handle, _stop) = test_handle(FleetState::new(vec![spec("alpha")]), 4);
        let poisoner = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                let _guard = lock(&handle.state);
                panic!("a reader panicked while holding the fleet state");
            })
        };
        assert!(poisoner.join().is_err(), "the thread must have panicked");
        assert!(
            handle.state.is_poisoned(),
            "the panic must have poisoned the lock"
        );

        assert_eq!(handle.report().hosts.len(), 1);
        assert!(handle.host_spec(&host("alpha")).is_some());
        let mut stream = handle.subscribe();
        handle.apply(&host("alpha"), connected());
        assert_eq!(kinds(&drain(&mut stream)), vec!["host_connection"]);
    }

    /// An invalid `[fleet]` section is an operator error, not a host failure —
    /// and it is refused before a single supervisor thread is spawned.
    ///
    /// Pure: `start` returns on the `hosts_for_config` error before it reaches the
    /// connector, so this covers every platform, not only the socket ones.
    #[tokio::test]
    async fn an_invalid_fleet_section_is_a_config_error() {
        let config: crate::config::Config = toml::from_str(
            r#"
[fleet]
include_local = true

[[fleet.hosts]]
name = "local"
kind = "local"
"#,
        )
        .expect("config parses");
        let diagnostics = FleetRuntime::start(&config).err().expect(
            "a [fleet] section that names `local` while include_local is on must not start",
        );
        assert!(
            !diagnostics.is_empty(),
            "the error must carry the diagnostics"
        );
    }
}

#[cfg(all(test, unix))]
mod socket_tests {
    use super::*;

    use std::time::Instant;

    use crate::fleet::connector::test_support::{
        fake_connector, scratch_dir, snapshot, snapshot_message, Behaviour, FakeHost,
    };
    use crate::protocol::endpoint::{EndpointClientHello, ENDPOINT_HELLO_KIND};
    use crate::protocol::ClientMessage;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    /// The `surface_active` every hello in `messages` announced.
    fn hello_surface_active(messages: &[ClientMessage]) -> Vec<bool> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::EndpointControl { kind, data } if kind == ENDPOINT_HELLO_KIND => {
                    serde_json::from_str::<EndpointClientHello>(data)
                        .ok()
                        .map(|hello| hello.surface_active)
                }
                _ => None,
            })
            .collect()
    }

    /// A real socket, a real handshake, a real fold: the host reports
    /// connected, both deltas reach a subscriber, and the hello the host
    /// actually received was passive.
    #[test]
    fn a_live_host_connects_passively_and_its_deltas_reach_subscribers() {
        let runtime = runtime();
        let dir = scratch_dir("gateway-live");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let specs = vec![alpha.spec("alpha")];
        let connector = fake_connector(
            &[("alpha", &alpha)],
            FleetConnectorOptions::for_daemon(&crate::config::Config::default()),
        );

        runtime.block_on(async {
            let fleet = FleetRuntime::over(specs, connector);
            let handle = fleet.handle();
            let (mut stream, report) = handle.subscribe_with_report();
            assert_eq!(report.active_host, None, "a gateway activates no host");

            let mut kinds = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && !kinds.iter().any(|kind| kind == "snapshot") {
                let Ok(Some(item)) =
                    tokio::time::timeout(Duration::from_secs(2), stream.next()).await
                else {
                    break;
                };
                if let ChangeItem::Change(json) = item {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
                        if let Some(kind) = value.get("kind").and_then(|kind| kind.as_str()) {
                            kinds.push(kind.to_string());
                        }
                    }
                }
            }
            assert!(
                kinds.contains(&"host_connection".to_string())
                    && kinds.contains(&"snapshot".to_string()),
                "the subscriber must see the connection and the snapshot: {kinds:?}"
            );

            let report = handle.report();
            assert_eq!(report.hosts[0].connection.state_name(), "connected");
            assert!(
                handle
                    .host_connection(&HostId::new("alpha").expect("valid host id"))
                    .is_some_and(|connection| connection.is_connected()),
                "host_connection must agree with the report"
            );

            let received = alpha.received();
            assert_eq!(
                hello_surface_active(&received),
                vec![false],
                "the gateway's hello must be passive on the wire"
            );
            // Passivity is a property of what was *sent*, not only of the
            // hello's flag: the runtime clears the active host, and a cleared
            // host whose announced geometry already matches is not resized.
            assert!(
                !received
                    .iter()
                    .any(|message| matches!(message, ClientMessage::ClientShellResize { .. })),
                "a gateway must never resize a host: {received:?}"
            );

            let stopped = Instant::now();
            fleet.shutdown().await;
            assert!(
                stopped.elapsed() < Duration::from_secs(3),
                "shutdown took {:?}",
                stopped.elapsed()
            );
            // The stop latch the runtime set ends the stream rather than
            // hanging a WebSocket task forever — after the subscriber has read
            // whatever was already queued for it. (The broadcast sender is
            // still alive: every `FleetHandle` holds one, and in the gateway a
            // handle outlives the runtime.)
            let ended = loop {
                match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
                    Ok(None) => break true,
                    Ok(Some(_)) => continue,
                    Err(_) => break false,
                }
            };
            assert!(ended, "a stopped runtime must close its change streams");
        });
        drop(alpha);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host that drops its connection is one host's delta, not a runtime
    /// failure: the subscriber sees `unavailable`, the runtime keeps serving,
    /// and the host comes back on its own.
    #[test]
    fn a_host_that_drops_its_connection_becomes_an_unavailable_delta() {
        let runtime = runtime();
        let dir = scratch_dir("gateway-drops");
        // Welcome, then hang up on the first connection; serve normally after.
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::DropFirstConnection(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let specs = vec![alpha.spec("alpha")];
        let connector = fake_connector(
            &[("alpha", &alpha)],
            FleetConnectorOptions::for_daemon(&crate::config::Config::default()),
        );

        runtime.block_on(async {
            let fleet = FleetRuntime::over(specs, connector);
            let handle = fleet.handle();
            let (mut stream, _) = handle.subscribe_with_report();

            let mut states = Vec::new();
            let mut snapshots = 0usize;
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && snapshots == 0 {
                let Ok(Some(item)) =
                    tokio::time::timeout(Duration::from_secs(5), stream.next()).await
                else {
                    break;
                };
                let ChangeItem::Change(json) = item else {
                    continue;
                };
                match serde_json::from_str::<FleetChange>(&json) {
                    Ok(FleetChange::HostConnection { connection, .. }) => {
                        states.push(connection.state_name().to_string())
                    }
                    Ok(FleetChange::Snapshot { .. }) => snapshots += 1,
                    _ => {}
                }
            }

            assert!(
                states.iter().any(|state| state == "unavailable"),
                "a dropped connection must reach the subscriber: {states:?}"
            );
            assert_eq!(
                snapshots, 1,
                "the host must come back on its own: {states:?}"
            );
            // Still one host, still a report: a host failure is host-local.
            let report = handle.report();
            assert_eq!(report.hosts.len(), 1);
            assert_eq!(report.hosts[0].connection.state_name(), "connected");

            fleet.shutdown().await;
        });
        drop(alpha);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
