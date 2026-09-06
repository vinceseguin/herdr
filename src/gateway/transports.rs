//! Per-host connection factories for the gateway's *terminal* streams.
//!
//! The fleet connector already holds one endpoint connection per host, but a
//! terminal is a different kind of connection: a client socket whose mode is
//! fixed for its lifetime by its first message ([`ClientMessage::TerminalHello`]
//! then `ObserveTerminal`/`ControlTerminal`). So a terminal cannot ride the
//! aggregator's stream — it needs its own, and this is the only place the
//! gateway opens one.
//!
//! Three things make that safe next to the connector:
//!
//! * **A separate socket scope.** An ssh host's bridge binds a forward socket
//!   whose name is derived from (pid, scope, target, session). A second
//!   `SshTransport` with the *same* scope in the same process fails with
//!   `AddrInUse`, so these transports use the `gateway` scope and the
//!   connector keeps the bare host-id one.
//! * **A lease, and an ordered shutdown.** Dropping an `SshTransport` while a
//!   bridged stream is still open kills the ssh child under it, so
//!   [`HostTransports::shutdown`] waits (bounded) for every stream it handed
//!   out to be released before it drops a transport.
//! * **One bridged stream per ssh host at a time.** The stdio bridge accepts
//!   one connection and pumps it to one ssh child until that connection ends;
//!   a second connect queues in the listen backlog and its welcome read would
//!   time out 60 s later with no better diagnosis. So an ssh host's slot is
//!   claimed by the stream that holds it, and a second terminal on that host
//!   waits briefly for a release, then is refused with [`HOST_BUSY_KIND`].
//!   Local session sockets have no such limit and are never claimed.
//!
//! Everything here is blocking: `connect` runs ssh discovery and a socket
//! connect, so a caller inside the runtime must reach it through
//! `spawn_blocking`.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::config::Config;
use crate::fleet::connector::FleetConnectorOptions;
use crate::fleet::hosts::{HostId, HostKind, HostSpec};
use crate::fleet::transport::{transport_for_scoped, HostTransport};
use crate::ipc::LocalStream;

/// Socket scope for every transport the gateway opens.
///
/// Distinct from the connector's (which scopes by host id alone), so a gateway
/// and its own aggregator never contend for one forward socket.
pub(crate) const GATEWAY_TRANSPORT_SCOPE: &str = "gateway";

/// How long [`HostTransports::shutdown`] waits for open streams to be released.
///
/// A session ends as soon as it sees the stop latch, so this is normally
/// instant; the bound exists because a thread wedged in a socket write must not
/// hold the process open.
const RELEASE_WAIT: Duration = Duration::from_secs(3);
/// How often the release wait and the exclusive-slot wait re-check.
const RELEASE_POLL: Duration = Duration::from_millis(25);
/// How long [`HostTransports::connect`] waits for an ssh host's one stream
/// slot to be released before it refuses.
///
/// Long enough that a browser reloading a page — whose old socket's bridge
/// child takes a few hundred milliseconds to be reaped — gets its terminal
/// back without an error; short enough that a genuinely taken host answers
/// promptly.
const EXCLUSIVE_WAIT: Duration = Duration::from_secs(2);

/// The error kind `connect` uses when a host's only stream slot is taken.
///
/// The terminal route maps exactly this kind to `terminal.error host_busy`;
/// every other connect failure is `host_unavailable`.
pub(crate) const HOST_BUSY_KIND: io::ErrorKind = io::ErrorKind::ResourceBusy;

/// One open terminal stream's claim on the transports that produced it.
///
/// Held by the session and by both of its threads; [`HostTransports::shutdown`]
/// waits for the last one to drop before a transport is torn down, and an ssh
/// host's next terminal waits for it before it connects. Neither field is ever
/// read: their *existence* is the signal, counted through the `Arc`s they
/// clone.
#[derive(Clone)]
pub(crate) struct StreamLease {
    /// Counted by [`HostTransports::shutdown`], across every host.
    _all: Arc<()>,
    /// Counted per host, for the hosts that serve one stream at a time.
    _host: Arc<()>,
}

impl StreamLease {
    /// A lease attached to nothing, for tests that drive a session against a
    /// fake endpoint rather than through [`HostTransports`].
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self {
            _all: Arc::new(()),
            _host: Arc::new(()),
        }
    }
}

/// A freshly opened stream to one host, plus what the caller needs to use it.
pub(crate) struct OpenStream {
    pub(crate) stream: LocalStream,
    /// The transport's welcome deadline (5 s local, 60 s behind ssh).
    pub(crate) read_timeout: Duration,
    pub(crate) lease: StreamLease,
}

/// One host's transport and the streams in flight on it.
///
/// `Arc` so a caller can hold it while it blocks in `connect`; the transport
/// sits behind a `Mutex` because its `connect` takes `&mut self` and two
/// terminals on the same host must not run ssh discovery at once.
struct HostSlot {
    /// The spec the transport was built from. A host whose spec changed gets
    /// a new transport: one built for the old target would bridge every later
    /// terminal on this id to the wrong machine.
    spec: HostSpec,
    transport: Mutex<Box<dyn HostTransport>>,
    /// Cloned into the `_host` half of every lease on this host; a strong
    /// count of one means no stream is in flight.
    streams: Arc<()>,
}

/// Every host's slot plus the closed flag, under one lock: a connect that
/// races a shutdown must not rebuild a transport the shutdown just dropped and
/// leave its forward socket behind at exit.
#[derive(Default)]
struct Slots {
    closed: bool,
    hosts: HashMap<HostId, Arc<HostSlot>>,
}

/// The gateway's lazily built, reused per-host transports.
pub(crate) struct HostTransports {
    /// Daemon options: noninteractive ssh, `[remote] manage_ssh_config`.
    options: FleetConnectorOptions,
    /// One transport per host, built on that host's first terminal.
    slots: Mutex<Slots>,
    /// Cloned into every [`StreamLease`]; `strong_count == 1` means every
    /// stream has been released.
    leases: Arc<()>,
    /// Latched by [`Self::begin_shutdown`]; every terminal session watches it.
    stop: watch::Sender<bool>,
}

impl HostTransports {
    /// Read `[remote]`/`[fleet]` once, and open nothing.
    ///
    /// Construction performs no I/O: a gateway with no terminal client ever
    /// opened never runs ssh and never binds a forward socket.
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            options: FleetConnectorOptions::for_daemon(config),
            slots: Mutex::new(Slots::default()),
            leases: Arc::new(()),
            stop: watch::channel(false).0,
        }
    }

    /// A latch every terminal session selects on, so a stopping gateway ends
    /// its streams instead of waiting for the browsers to notice.
    ///
    /// A receiver only observes latches that happen *after* it subscribes, so
    /// a session must subscribe before it calls [`Self::connect`]: `connect`
    /// refuses once the latch is set, and the receiver catches everything
    /// later. Between the two there is no gap.
    pub(crate) fn stopping(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    /// Open a fresh stream to `spec`'s host. **Blocking.**
    ///
    /// The host's transport is built on first use and reused afterwards, so a
    /// second terminal on an ssh host pays neither the discovery probes nor a
    /// new control master. Two terminals on the same host serialize through
    /// that host's lock and no other host's. On an ssh host the second one
    /// also waits for the first to be released (see the module docs) and is
    /// refused with [`HOST_BUSY_KIND`] if it is not.
    pub(crate) fn connect(&self, spec: &HostSpec) -> io::Result<OpenStream> {
        // Claimed before anything else, so a shutdown that begins while this
        // connect is in flight waits for it rather than dropping the transport
        // under the stream it is about to produce.
        let all = Arc::clone(&self.leases);
        if *self.stop.borrow() {
            return Err(stopping());
        }
        let slot = self.slot_for(spec)?;
        let mut transport = lock(&slot.transport);
        // Under the transport lock, so the check and the claim are one step:
        // two terminals racing for an idle ssh host cannot both win it.
        let host = claim_host_stream(
            &slot.streams,
            serves_one_stream_at_a_time(spec),
            &spec.id,
            EXCLUSIVE_WAIT,
        )?;
        let stream = transport.connect()?;
        Ok(OpenStream {
            stream,
            read_timeout: transport.read_timeout(),
            lease: StreamLease {
                _all: all,
                _host: host,
            },
        })
    }

    /// Ask every session to end. Cheap, non-blocking, idempotent.
    ///
    /// `send_replace` rather than `send`: a plain `send` leaves the value
    /// untouched when there is no receiver, and a gateway that stopped before
    /// anyone opened a terminal must still latch.
    pub(crate) fn begin_shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Drop every transport, once the streams they produced are released.
    /// **Blocking**, and bounded by [`RELEASE_WAIT`].
    ///
    /// Order is the whole point: a bridge dropped under an open stream kills
    /// the ssh child, and the forward socket is unlinked by the bridge's own
    /// `Drop` — so this has to run before the process exits and after the
    /// sessions have let go.
    pub(crate) fn shutdown(&self) {
        self.shutdown_with_wait(RELEASE_WAIT);
    }

    /// [`Self::shutdown`] with an explicit bound, so a test does not wait
    /// three seconds to see the bound hold.
    fn shutdown_with_wait(&self, wait: Duration) {
        self.begin_shutdown();
        let deadline = Instant::now() + wait;
        while Arc::strong_count(&self.leases) > 1 {
            if Instant::now() >= deadline {
                tracing::warn!(
                    target: "gateway",
                    leases = Arc::strong_count(&self.leases) - 1,
                    "terminal streams were still open after the release wait; closing transports anyway"
                );
                break;
            }
            std::thread::sleep(RELEASE_POLL);
        }
        // Closed and drained under one lock, so no connect can slip a new
        // transport in between; dropped outside it, because a bridge's drop
        // joins its accept thread and nothing should queue on the map for that.
        let drained: Vec<Arc<HostSlot>> = {
            let mut slots = lock(&self.slots);
            slots.closed = true;
            slots.hosts.drain().map(|(_, slot)| slot).collect()
        };
        drop(drained);
    }

    /// This host's slot, built on first use.
    fn slot_for(&self, spec: &HostSpec) -> io::Result<Arc<HostSlot>> {
        let mut slots = lock(&self.slots);
        if slots.closed {
            return Err(stopping());
        }
        if let Some(slot) = slots.hosts.get(&spec.id) {
            if slot.spec == *spec {
                return Ok(Arc::clone(slot));
            }
            // The host id now names something else. Dropping the old slot
            // here tears its bridge down, so no stream ever reaches the old
            // target under the new spec; sessions on it see the host hang up.
            tracing::info!(
                target: "gateway",
                host = %spec.id,
                "fleet host spec changed; rebuilding its terminal transport"
            );
            slots.hosts.remove(&spec.id);
        }
        let transport = transport_for_scoped(spec, &self.options, GATEWAY_TRANSPORT_SCOPE)
            .map_err(io::Error::other)?;
        let slot = Arc::new(HostSlot {
            spec: spec.clone(),
            transport: Mutex::new(transport),
            streams: Arc::new(()),
        });
        slots.hosts.insert(spec.id.clone(), Arc::clone(&slot));
        Ok(slot)
    }

    /// How many hosts have a transport built. Test-only.
    #[cfg(test)]
    pub(crate) fn open_hosts(&self) -> usize {
        lock(&self.slots).hosts.len()
    }
}

fn stopping() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the gateway is stopping")
}

/// Whether `spec`'s transport carries one stream at a time.
///
/// An ssh host's stdio bridge accepts one connection and serves it until it
/// ends (its accept loop is sequential by design: the connector needs exactly
/// one endpoint stream per host). A local session socket accepts any number.
fn serves_one_stream_at_a_time(spec: &HostSpec) -> bool {
    matches!(spec.kind, HostKind::Ssh { .. })
}

/// Claim a stream on `streams`: immediately for a shared host, or once the
/// previous stream has been released — within `wait` — for an exclusive one.
///
/// The caller must hold the host's transport lock so that the idle check and
/// the clone that claims the slot cannot interleave with another claimant's.
fn claim_host_stream(
    streams: &Arc<()>,
    exclusive: bool,
    host: &HostId,
    wait: Duration,
) -> io::Result<Arc<()>> {
    if !exclusive {
        return Ok(Arc::clone(streams));
    }
    let deadline = Instant::now() + wait;
    loop {
        // One: the slot's own handle. Anything more is a stream still open,
        // or a reader thread that has not yet seen its host hang up.
        if Arc::strong_count(streams) == 1 {
            return Ok(Arc::clone(streams));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                HOST_BUSY_KIND,
                format!(
                    "host {host} already has an open terminal stream; an ssh host serves one terminal at a time"
                ),
            ));
        }
        std::thread::sleep(RELEASE_POLL);
    }
}

/// A mutex guard that survives a poisoned lock.
///
/// A panic while a transport is borrowed must not take every later terminal
/// with it: the map and the transport behind it have no invariant a panic can
/// break that a reconnect does not fix.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, kind: HostKind) -> HostSpec {
        HostSpec {
            id: HostId::new(id).expect("valid host id"),
            kind,
            enabled: true,
        }
    }

    fn ssh(id: &str, target: &str) -> HostSpec {
        spec(
            id,
            HostKind::Ssh {
                target: target.to_string(),
                session: None,
            },
        )
    }

    fn transports() -> HostTransports {
        let mut config = Config::default();
        config.fleet.include_local = false;
        HostTransports::new(&config)
    }

    #[test]
    fn construction_opens_nothing() {
        let transports = transports();
        assert_eq!(transports.open_hosts(), 0);
        assert!(!*transports.stopping().borrow());
    }

    /// The daemon switch has to reach the transport, or a gateway gets an
    /// interactive ssh child with no other symptom.
    #[test]
    fn the_options_are_the_daemon_ones() {
        let transports = transports();
        assert!(transports.options.ssh_noninteractive);
    }

    /// The second terminal on a host reuses the first one's transport, so an
    /// ssh host pays discovery once.
    #[test]
    fn a_host_gets_one_transport_however_many_terminals_it_serves() {
        let transports = transports();
        let workbox = ssh("workbox", "workbox");
        let first = transports.slot_for(&workbox).expect("a transport");
        let second = transports.slot_for(&workbox).expect("a transport");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(transports.open_hosts(), 1);

        // A different host is a different transport, even at the same target.
        let other = transports
            .slot_for(&ssh("laptop", "workbox"))
            .expect("a transport");
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(transports.open_hosts(), 2);
    }

    /// A transport is keyed by what it was built from, not by the id alone:
    /// an id that comes to name another target must not keep bridging to the
    /// old one.
    #[test]
    fn a_host_whose_spec_changed_gets_a_new_transport() {
        let transports = transports();
        let before = transports
            .slot_for(&ssh("workbox", "workbox.old"))
            .expect("a transport");
        let after = transports
            .slot_for(&ssh("workbox", "workbox.new"))
            .expect("a transport");
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(after.spec.kind, ssh("workbox", "workbox.new").kind);
        // Replaced, not added.
        assert_eq!(transports.open_hosts(), 1);
    }

    #[test]
    fn a_stopping_gateway_opens_no_new_stream() {
        let transports = transports();
        transports.begin_shutdown();
        assert!(*transports.stopping().borrow());
        let Err(error) = transports.connect(&spec("laptop", HostKind::Local { session: None }))
        else {
            panic!("a stopping gateway must not open a stream");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        // And it built nothing on the way to refusing.
        assert_eq!(transports.open_hosts(), 0);
    }

    /// The real local path: a session that is not running is one connect's
    /// error, named after the session, and the slot is kept for a retry.
    #[test]
    fn a_local_session_that_is_not_running_is_an_io_error() {
        let transports = transports();
        let missing = format!("herdr-gw-missing-{}", std::process::id());
        let Err(error) = transports.connect(&spec(
            "laptop",
            HostKind::Local {
                session: Some(missing.clone()),
            },
        )) else {
            panic!("nothing is listening for that session");
        };
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ),
            "unexpected kind: {error}"
        );
        assert!(error.to_string().contains(&missing), "{error}");
        assert_eq!(transports.open_hosts(), 1);
    }

    #[test]
    fn shutdown_latches_and_drops_every_transport() {
        let transports = transports();
        let _ = transports
            .slot_for(&spec("laptop", HostKind::Local { session: None }))
            .expect("a transport");
        assert_eq!(transports.open_hosts(), 1);

        let mut stopping = transports.stopping();
        transports.shutdown();
        assert!(*stopping.borrow_and_update());
        assert_eq!(transports.open_hosts(), 0);
        // Idempotent: a second stop is not an error and drops nothing twice.
        transports.shutdown();
        assert_eq!(transports.open_hosts(), 0);
    }

    /// A connect that lost the race with shutdown must not rebuild a transport
    /// into a map nobody will drain again: that is a forward socket left on
    /// disk after the process exits.
    #[test]
    fn nothing_is_built_after_shutdown_has_drained() {
        let transports = transports();
        transports.shutdown();
        let Err(error) = transports.slot_for(&ssh("workbox", "workbox")) else {
            panic!("a drained map is closed");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(transports.open_hosts(), 0);
    }

    /// The release wait is bounded: a leaked lease must not hold the process.
    #[test]
    fn shutdown_gives_up_on_a_lease_that_never_drops() {
        let transports = transports();
        let leaked = StreamLease {
            _all: Arc::clone(&transports.leases),
            _host: Arc::new(()),
        };
        let wait = Duration::from_millis(120);
        let started = Instant::now();
        transports.shutdown_with_wait(wait);
        let waited = started.elapsed();
        assert!(waited >= wait, "gave up too early: {waited:?}");
        assert!(waited < wait * 10, "waited far past the bound: {waited:?}");
        drop(leaked);
    }

    /// The ordinary path: a lease dropped while the wait is running lets
    /// shutdown finish immediately rather than sit out the whole bound.
    #[test]
    fn shutdown_returns_as_soon_as_the_last_stream_is_released() {
        let transports = Arc::new(transports());
        let lease = StreamLease {
            _all: Arc::clone(&transports.leases),
            _host: Arc::new(()),
        };
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(lease);
        });
        let started = Instant::now();
        transports.shutdown_with_wait(Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown did not notice the release"
        );
        releaser.join().expect("releaser");
    }

    #[test]
    fn only_ssh_hosts_serve_one_stream_at_a_time() {
        assert!(serves_one_stream_at_a_time(&ssh("workbox", "workbox")));
        assert!(!serves_one_stream_at_a_time(&spec(
            "laptop",
            HostKind::Local { session: None }
        )));
    }

    /// The exclusive claim in one table: the first stream wins, the second is
    /// refused with the busy kind once the wait is up, a release lets the
    /// next one in, and a shared host never refuses.
    #[test]
    fn an_exclusive_host_admits_one_stream_until_it_is_released() {
        let host = HostId::new("workbox").expect("valid host id");
        let streams = Arc::new(());
        let short = Duration::from_millis(60);

        let first = claim_host_stream(&streams, true, &host, short).expect("an idle host");
        let started = Instant::now();
        let error = claim_host_stream(&streams, true, &host, short).expect_err("taken");
        assert_eq!(error.kind(), HOST_BUSY_KIND);
        assert!(error.to_string().contains("workbox"), "{error}");
        assert!(started.elapsed() >= short, "refused before the wait was up");

        // The lease's clone is what counts: the session and its threads all
        // hold one, and the slot is free only when every one of them is gone.
        let twin = Arc::clone(&first);
        drop(first);
        assert_eq!(
            claim_host_stream(&streams, true, &host, short)
                .expect_err("a thread still holds it")
                .kind(),
            HOST_BUSY_KIND
        );
        drop(twin);
        let _next = claim_host_stream(&streams, true, &host, short).expect("released");

        let shared = Arc::new(());
        let _a = claim_host_stream(&shared, false, &host, short).expect("shared");
        let _b = claim_host_stream(&shared, false, &host, short).expect("shared twice");
    }

    /// A release during the wait is noticed at once — the page-reload case.
    #[test]
    fn an_exclusive_claim_returns_as_soon_as_the_previous_stream_is_released() {
        let host = HostId::new("workbox").expect("valid host id");
        let streams = Arc::new(());
        let first = claim_host_stream(&streams, true, &host, Duration::from_millis(10))
            .expect("an idle host");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            drop(first);
        });
        let started = Instant::now();
        let _second =
            claim_host_stream(&streams, true, &host, Duration::from_secs(30)).expect("released");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the claim did not notice the release"
        );
        releaser.join().expect("releaser");
    }
}
