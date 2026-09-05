//! The runtime half of the fleet: one connection per host, one merged stream.
//!
//! [`FleetConnector`] owns a supervisor thread per enabled host. Each thread
//! connects, runs the generation-1 handshake, reads that host's endpoint
//! messages and reconnects with a 1 s → 30 s backoff — and every failure it
//! meets stays *its* failure: it becomes a [`HostEvent`] for that host and
//! never reaches another host, never exits the process, never panics.
//!
//! Two rules shape the code:
//!
//! - **The state is somewhere else.** The connector reports transport facts
//!   and forwards what a host says; [`crate::fleet::state::FleetState`] decides
//!   what that means for the merged view. The connector never edits a merged
//!   list.
//! - **Inactive hosts cost nothing.** Only one host is active at a time. An
//!   inactive host is handshaken at [`INACTIVE_SURFACE`], so the server renders
//!   a tiny surface, and its surface frames are dropped in the reader thread
//!   before anything is allocated into the event channel. Activation resizes
//!   the new host up and the old host back down.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use interprocess::TryClone as _;
use tokio::sync::mpsc;

use crate::fleet::endpoint_lane::{EndpointLane, LaneResponse, ENDPOINT_REQUEST_TIMEOUT};
use crate::fleet::handshake::{
    endpoint_handshake, framing_error, HandshakeOutcome, HandshakeParams,
};
use crate::fleet::hosts::{HostId, HostSpec};
use crate::fleet::state::{Backoff, HostEvent};
use crate::fleet::transport::{transport_for, HostTransport};
use crate::ipc::LocalStream;
use crate::protocol::endpoint::{ENDPOINT_SNAPSHOT_KIND, SNAPSHOT_CODEC_V1};
use crate::protocol::{
    self, ClientMessage, ClientPaneInputEvent, ClientShellSnapshot, ClientSurfaceSize,
    FramingError, PaneSurfaceFrame, PaneSurfacePatch, SemanticNotification, ServerMessage,
    MAX_FRAME_SIZE,
};

/// Surface size announced for a host nobody is looking at.
///
/// Not "as small as the server accepts", which is what the plan's decision (e)
/// assumed: validating against a live 0.8.2-fork server showed that a
/// connecting client shell becomes that host's *foreground* client
/// (`server::headless`'s `ClientShellConnected` arm), and the foreground
/// client's surface is the host's effective geometry. A 20x5 fleet client
/// therefore reflowed every pane on every configured host and sent each agent
/// a 20x5 SIGWINCH — for a read-only `herdr fleet status`. The endpoint
/// protocol has no observer mode and E1 changes no server code, so the fix is
/// the value: herdr's own default headless geometry, which a host with no
/// attached client is *already* using, making the common fleet case (headless
/// servers running agents) a no-op resize.
///
/// The residual is unavoidable without a protocol change: a host that already
/// has an attached client, or one whose `headless_size` is configured
/// differently, is resized for as long as the fleet client is connected and
/// restored when it disconnects. E2 should keep a fleet connection open only
/// while the fleet console is in use.
///
/// The cost decision (e) was protecting is still paid where it matters: this
/// client drops an inactive host's frames in the reader thread before anything
/// reaches the event channel.
pub const INACTIVE_SURFACE: ClientSurfaceSize = ClientSurfaceSize {
    cols: crate::config::DEFAULT_HEADLESS_COLS,
    rows: crate::config::DEFAULT_HEADLESS_ROWS,
};

/// How long the reconnect sleep waits between stop-flag checks.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// How long [`FleetConnector::shutdown`] waits for the supervisor threads.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Depth of the merged event channel; matches upstream's client reader channel.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// How the connector should talk to every host.
#[derive(Debug, Clone)]
pub struct FleetConnectorOptions {
    /// Hello parameters, whose `surface_size` is the *inactive* size.
    pub handshake: HandshakeParams,
    /// Surface size for whichever host is active.
    pub active_surface: ClientSurfaceSize,
    /// Whether ssh transports use herdr's managed ssh config (PR 6).
    // Read by PR 6's ssh transport; until then only the default constructs it.
    #[allow(dead_code)]
    pub manage_ssh_config: bool,
    /// Largest endpoint frame accepted from a host.
    pub max_frame_size: usize,
    /// How long one endpoint request may wait for its answer before it is
    /// failed and the next queued request runs.
    pub endpoint_timeout: Duration,
}

impl Default for FleetConnectorOptions {
    fn default() -> Self {
        Self {
            handshake: HandshakeParams::read_only(INACTIVE_SURFACE),
            active_surface: INACTIVE_SURFACE,
            manage_ssh_config: false,
            max_frame_size: MAX_FRAME_SIZE,
            endpoint_timeout: ENDPOINT_REQUEST_TIMEOUT,
        }
    }
}

/// Everything the connector reports, from every host, in arrival order.
// Read by E2/E3 only: `herdr fleet status` is read-only, so nothing in this PR
// reads the surface, notification, response and message payloads in production; E2 (input, surfaces) and E7 (requests)
// are its consumers. Allowed per item rather than per module so a genuinely
// unused helper still fails the lint.
#[allow(dead_code)]
#[derive(Debug)]
pub enum FleetEvent {
    /// A transport fact for [`crate::fleet::state::FleetState::apply`].
    Host { host: HostId, event: HostEvent },
    /// A full pane surface. Only ever from the active host.
    Surface {
        host: HostId,
        frame: Box<PaneSurfaceFrame>,
    },
    /// An incremental pane surface update. Only ever from the active host.
    SurfacePatch {
        host: HostId,
        patch: Box<PaneSurfacePatch>,
    },
    Notification {
        host: HostId,
        notification: Box<SemanticNotification>,
    },
    /// A finished endpoint request, or why it will never finish.
    EndpointResponse {
        host: HostId,
        request_id: String,
        result: Result<Vec<u8>, String>,
    },
    /// Anything else the active host said (bell, clipboard, window title …).
    ServerMessage {
        host: HostId,
        message: Box<ServerMessage>,
    },
}

/// Something to send to one host.
///
/// Nothing in this epic constructs [`HostCommand::PaneInput`]: `herdr fleet
/// status` is read-only. It exists for E2, which routes real input to the
/// active host, and the type makes the target host explicit at every call
/// site so input cannot land on the wrong machine.
// Write-half item: `herdr fleet status` is read-only, so nothing in this PR
// constructs or reads it in production; E2 (input, surfaces) and E7 (requests)
// are its consumers. Allowed per item rather than per module so a genuinely
// unused helper still fails the lint.
#[allow(dead_code)]
#[derive(Debug)]
pub enum HostCommand {
    Resize(ClientSurfaceSize),
    PaneInput {
        pane_id: String,
        events: Vec<ClientPaneInputEvent>,
    },
    Focus(bool),
    MouseCapture(bool),
    /// One `api::schema::Request`, already serialized. `request_id` must equal
    /// the JSON `id` field: the host correlates its answer by that field, so a
    /// mismatch could never be matched to this request.
    ///
    /// Accepted means exactly one [`FleetEvent::EndpointResponse`] follows —
    /// the answer, the host's error, an expiry, or the disconnect that made an
    /// answer impossible.
    Endpoint {
        request_id: String,
        request: String,
    },
    /// Any other shell-lane message, written as given. The messages the
    /// connector itself owns — the endpoint hello, endpoint requests — are
    /// refused here: they carry a boot id or a handshake state that only the
    /// connector can keep true.
    Raw(Box<ClientMessage>),
}

/// Why a command could not be handed to a host.
#[derive(Debug)]
pub enum HostSendError {
    /// No such host in this fleet.
    UnknownHost,
    /// The host exists but has no usable connection right now.
    NotConnected,
    /// The write failed; the supervisor will notice and reconnect.
    Io(io::Error),
    /// The command is malformed or would corrupt the connector's own view of
    /// the host (a foreign boot id, a second handshake); nothing was sent.
    // Constructed by `send`, the write half; see the note on `HostCommand`.
    #[allow(dead_code)]
    Refused(&'static str),
}

impl std::fmt::Display for HostSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownHost => f.write_str("no such fleet host"),
            Self::NotConnected => f.write_str("fleet host is not connected"),
            Self::Io(error) => write!(f, "fleet host write failed: {error}"),
            Self::Refused(reason) => write!(f, "fleet command refused: {reason}"),
        }
    }
}

impl std::error::Error for HostSendError {}

/// Builds one host's transport. Injected so tests drive real sockets without
/// depending on the machine's session directory.
type TransportFactory = Arc<
    dyn Fn(&HostSpec, &FleetConnectorOptions) -> Result<Box<dyn HostTransport>, String>
        + Send
        + Sync,
>;

/// The writable half of one host's connection, plus what a writer must know.
///
/// Guarded by a mutex shared with that host's supervisor thread, so a
/// `set_active` resize and the supervisor's post-handshake size check can never
/// interleave into the wrong order.
#[derive(Default)]
struct HostLinkState {
    /// `None` whenever the host is not connected.
    stream: Option<LocalStream>,
    /// A connection that is still handshaking.
    ///
    /// Never written to — a resize sent before the hello would break the
    /// handshake — but `shutdown` half-closes it, so a host that accepts and
    /// then says nothing cannot hold the shutdown for the welcome deadline.
    pending: Option<LocalStream>,
    /// Boot of the projection the host is currently serving.
    boot_id: Option<String>,
    /// Surface size this client last announced to the host.
    surface: Option<ClientSurfaceSize>,
    lane: EndpointLane,
}

struct HostLink {
    id: HostId,
    enabled: bool,
    /// Whether this host is the active one. Read by its reader thread on every
    /// surface frame, so it is an atomic rather than a lock.
    active: Arc<AtomicBool>,
    state: Arc<Mutex<HostLinkState>>,
}

/// N host connections behind one event stream.
pub struct FleetConnector {
    hosts: Vec<HostLink>,
    events: mpsc::Receiver<FleetEvent>,
    /// For failures `send` discovers on the caller's thread (an expired
    /// request). Weak so the channel still closes once every supervisor has
    /// exited, which is how a consumer learns nothing else can arrive.
    // Read by `send`, the write half; see the note on `HostCommand`.
    #[allow(dead_code)]
    events_tx: mpsc::WeakSender<FleetEvent>,
    options: Arc<FleetConnectorOptions>,
    active_host: Mutex<Option<HostId>>,
    stop: Arc<AtomicBool>,
    /// One message per supervisor thread that has exited.
    finished: std::sync::mpsc::Receiver<HostId>,
    supervisors: usize,
}

impl FleetConnector {
    /// Open every enabled host and start streaming.
    ///
    /// Returns immediately: hosts connect on their own threads, and their
    /// first `Connecting` event is already on the way.
    pub fn start(specs: Vec<HostSpec>, options: FleetConnectorOptions) -> Self {
        Self::start_with(specs, options, Arc::new(transport_for))
    }

    fn start_with(
        specs: Vec<HostSpec>,
        options: FleetConnectorOptions,
        factory: TransportFactory,
    ) -> Self {
        let options = Arc::new(options);
        let stop = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let events_tx_weak = events_tx.downgrade();
        let (finished_tx, finished) = std::sync::mpsc::channel();

        // The first enabled host is active, matching `FleetState::new`, so the
        // two agree about which host is being rendered.
        let active_host = specs
            .iter()
            .find(|spec| spec.enabled)
            .map(|spec| spec.id.clone());

        let mut hosts: Vec<HostLink> = Vec::with_capacity(specs.len());
        let mut supervisors = 0usize;
        for spec in specs {
            if hosts.iter().any(|host| host.id == spec.id) {
                // `FleetState::new` drops duplicates the same way; both must
                // agree or an event would address a host the state does not
                // have (or worse, the wrong one).
                tracing::warn!(host = %spec.id, "ignoring a duplicate fleet host id");
                continue;
            }
            let active = Arc::new(AtomicBool::new(
                active_host.as_ref() == Some(&spec.id) && spec.enabled,
            ));
            let state = Arc::new(Mutex::new(HostLinkState::default()));
            hosts.push(HostLink {
                id: spec.id.clone(),
                enabled: spec.enabled,
                active: Arc::clone(&active),
                state: Arc::clone(&state),
            });
            if !spec.enabled {
                // A disabled host is never opened: `FleetState::new` already
                // reports it as unavailable, so a supervisor would only
                // contradict that.
                continue;
            }
            let supervisor = Supervisor {
                spec,
                options: Arc::clone(&options),
                factory: Arc::clone(&factory),
                state,
                active,
                events: events_tx.clone(),
                stop: Arc::clone(&stop),
                finished: finished_tx.clone(),
            };
            supervisors += 1;
            // Detached on purpose: a blocking socket read cannot be
            // interrupted portably, so `shutdown` waits on the exit channel
            // instead of a join handle.
            std::thread::spawn(move || supervisor.run());
        }

        Self {
            hosts,
            events,
            events_tx: events_tx_weak,
            options,
            active_host: Mutex::new(active_host),
            stop,
            finished,
            supervisors,
        }
    }

    /// The merged event stream.
    ///
    /// Closes once every supervisor has exited, which is how a consumer knows
    /// no host can produce another event.
    pub fn events(&mut self) -> &mut mpsc::Receiver<FleetEvent> {
        &mut self.events
    }

    /// Point the fleet at one host, or at none.
    ///
    /// The old host is deactivated *before* the new one is activated, so no
    /// frame can ever be attributed to two active hosts, and each side is
    /// resized under its own link lock, which is the same lock the supervisor
    /// takes when it registers a fresh connection. That is what makes an
    /// activation racing a reconnect land on exactly one of the two paths.
    pub fn set_active(&self, host: Option<&HostId>) -> Result<(), HostSendError> {
        let mut active = lock(&self.active_host);
        if let Some(id) = host {
            let Some(link) = self.link(id) else {
                return Err(HostSendError::UnknownHost);
            };
            if !link.enabled {
                tracing::warn!(host = %id, "refusing to activate a disabled fleet host");
                return Err(HostSendError::NotConnected);
            }
        }
        if active.as_ref() == host {
            return Ok(());
        }
        let previous = active.take();
        *active = host.cloned();

        if let Some(previous) = previous.as_ref().and_then(|id| self.link(id)) {
            previous.active.store(false, Ordering::Release);
            self.announce_surface(previous, self.options.handshake.surface_size);
        }
        if let Some(next) = host.and_then(|id| self.link(id)) {
            next.active.store(true, Ordering::Release);
            self.announce_surface(next, self.options.active_surface);
        }
        Ok(())
    }

    /// Stop every host and wait, briefly, for the threads to notice.
    ///
    /// A supervisor sleeping between reconnect attempts checks the stop flag
    /// every 100 ms, so it exits promptly. A thread blocked in a socket read
    /// cannot be interrupted portably; closing the write half (unix) makes the
    /// server hang up, which ends the read. Anything still running after the
    /// bounded wait is left detached rather than blocking the caller — it owns
    /// no state the caller can observe once the receiver is gone.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        // Closing the receiver fails every `blocking_send`, including one
        // parked on a full channel because the consumer stopped reading before
        // it called shutdown; otherwise that supervisor could only exit once
        // the receiver was dropped, after the whole wait below.
        self.events.close();
        self.close_connections();

        let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        let mut finished = 0usize;
        while finished < self.supervisors {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.finished.recv_timeout(remaining) {
                Ok(host) => {
                    tracing::debug!(host = %host, "fleet host supervisor stopped");
                    finished += 1;
                }
                Err(_) => break,
            }
        }
        if finished < self.supervisors {
            tracing::warn!(
                pending = self.supervisors - finished,
                "fleet supervisors did not stop within the shutdown wait; detaching"
            );
        }
    }

    fn link(&self, id: &HostId) -> Option<&HostLink> {
        self.hosts.iter().find(|host| &host.id == id)
    }

    fn resize_message(&self, surface_size: ClientSurfaceSize) -> ClientMessage {
        ClientMessage::ClientShellResize {
            cell_width_px: self.options.handshake.cell_width_px,
            cell_height_px: self.options.handshake.cell_height_px,
            surface_size,
            pixel_mouse: self.options.handshake.pixel_mouse,
        }
    }

    /// Tell one host the size it should render, if it is connected.
    ///
    /// A disconnected host needs nothing: its next handshake reads the active
    /// gate and announces the right size in the hello.
    fn announce_surface(&self, link: &HostLink, surface_size: ClientSurfaceSize) {
        let mut state = lock(&link.state);
        if state.stream.is_none() || state.surface == Some(surface_size) {
            return;
        }
        state.surface = Some(surface_size);
        if let Err(error) = write_locked(&mut state, &self.resize_message(surface_size)) {
            tracing::warn!(host = %link.id, error = %error, "failed to resize a fleet host");
        }
    }

    fn close_connections(&self) {
        for link in &self.hosts {
            let mut state = lock(&link.state);
            state.surface = None;
            for stream in [state.stream.take(), state.pending.take()]
                .into_iter()
                .flatten()
            {
                #[cfg(unix)]
                {
                    // Half-closing makes the server hang up, which unblocks the
                    // reader thread. Windows named pipes have no equivalent; the
                    // thread there exits on its next message or with the process.
                    if let Err(error) = crate::ipc::shutdown_local_stream_write(&stream) {
                        tracing::debug!(host = %link.id, error = %error, "fleet host half-close failed");
                    }
                }
                drop(stream);
            }
        }
    }
}

// Write-half item: `herdr fleet status` is read-only, so nothing in this PR
// constructs or reads it in production; E2 (input, surfaces) and E7 (requests)
// are its consumers. Allowed per item rather than per module so a genuinely
// unused helper still fails the lint.
#[allow(dead_code)]
impl FleetConnector {
    pub fn active_host(&self) -> Option<HostId> {
        lock(&self.active_host).clone()
    }

    /// Send one command to one host.
    ///
    /// The host is always explicit: there is no "current host" fallback, so a
    /// caller cannot accidentally type into the machine it stopped looking at.
    pub fn send(&self, host: &HostId, command: HostCommand) -> Result<(), HostSendError> {
        let Some(link) = self.link(host) else {
            return Err(HostSendError::UnknownHost);
        };
        let mut state = lock(&link.state);
        if state.stream.is_none() {
            return Err(HostSendError::NotConnected);
        }

        let message = match command {
            HostCommand::Resize(surface_size) => {
                state.surface = Some(surface_size);
                self.resize_message(surface_size)
            }
            HostCommand::PaneInput { pane_id, events } => {
                ClientMessage::ClientShellPaneInput { pane_id, events }
            }
            HostCommand::Focus(focused) => ClientMessage::ClientShellFocus { focused },
            HostCommand::MouseCapture(enabled) => {
                ClientMessage::ClientShellMouseCapture { enabled }
            }
            HostCommand::Raw(message) => match *message {
                ClientMessage::ClientShellEndpointRequest { .. } => {
                    return Err(HostSendError::Refused(
                        "endpoint requests go through HostCommand::Endpoint, which owns the boot id",
                    ));
                }
                ClientMessage::EndpointControl { .. } => {
                    return Err(HostSendError::Refused(
                        "the endpoint handshake belongs to the connector",
                    ));
                }
                // A raw resize is still a resize: the size bookkeeping is what
                // keeps a later activation from skipping a needed resize.
                ClientMessage::ClientShellResize { surface_size, .. } => {
                    state.surface = Some(surface_size);
                    *message
                }
                other => other,
            },
            HostCommand::Endpoint {
                request_id,
                request,
            } => {
                let (accepted, failed) = queue_endpoint(&mut state, request_id, request);
                drop(state);
                for response in failed {
                    self.report_lane_failure(host, response);
                }
                return accepted;
            }
        };
        write_locked(&mut state, &message)
    }

    /// Deliver a lane failure found on the caller's thread.
    ///
    /// `try_send`, never a blocking send: the caller may be the thread that
    /// drains the channel. A full channel here means the consumer is far
    /// behind an answer that already took the whole request timeout; the loss
    /// is logged rather than deadlocked on.
    fn report_lane_failure(&self, host: &HostId, response: LaneResponse) {
        let Some(events) = self.events_tx.upgrade() else {
            return;
        };
        let event = FleetEvent::EndpointResponse {
            host: host.clone(),
            request_id: response.request_id,
            result: response.result,
        };
        if let Err(error) = events.try_send(event) {
            tracing::warn!(host = %host, error = %error, "dropping a fleet endpoint failure report");
        }
    }
}

impl Drop for FleetConnector {
    fn drop(&mut self) {
        // A connector dropped without `shutdown` must not leave threads writing
        // into a channel nobody reads, or holding host sockets open.
        self.stop.store(true, Ordering::Release);
        self.close_connections();
    }
}

/// Write to a host under its link lock, clearing the connection on failure.
///
/// Dropping the write half on error means the next `send` reports
/// `NotConnected` instead of writing into a broken socket; the supervisor's
/// read fails too and drives the reconnect.
fn write_locked(
    state: &mut MutexGuard<'_, HostLinkState>,
    message: &ClientMessage,
) -> Result<(), HostSendError> {
    let Some(stream) = state.stream.as_mut() else {
        return Err(HostSendError::NotConnected);
    };
    match protocol::write_message(stream, message) {
        Ok(()) => Ok(()),
        Err(error) => {
            state.stream = None;
            state.surface = None;
            Err(HostSendError::Io(framing_error(error)))
        }
    }
}

/// Lock a fleet mutex, recovering a poisoned one.
///
/// A supervisor thread that panicked while holding the lock must not take the
/// whole fleet down with it (and production code here never unwraps).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("recovering a poisoned fleet lock");
        poisoned.into_inner()
    })
}

/// Accept one endpoint request into the host's lane and start it if the
/// lane is free.
///
/// The first value says whether the request was accepted; the second carries
/// lane failures found on the way (an earlier request that expired, or the
/// request whose write just failed), each of which the caller must report.
// Called by `send`, the write half; see the note on `HostCommand`.
#[allow(dead_code)]
fn queue_endpoint(
    state: &mut MutexGuard<'_, HostLinkState>,
    request_id: String,
    request: String,
) -> (Result<(), HostSendError>, Vec<LaneResponse>) {
    if let Err(reason) = check_endpoint_request(&request_id, &request) {
        return (Err(HostSendError::Refused(reason)), Vec::new());
    }
    if state.boot_id.is_none() {
        // No snapshot yet: there is no projection to address, and guessing a
        // boot id would run the request on the wrong one.
        return (Err(HostSendError::NotConnected), Vec::new());
    }
    let mut failed = expire_lane(state);
    state.lane.enqueue(request_id, request);
    failed.extend(pump_lane(state));
    (Ok(()), failed)
}

/// The host correlates an answer by the request's JSON `id`, and closes the
/// connection on an envelope it cannot read. Both are checked here so one bad
/// request cannot take the whole host down or leave the lane waiting on an id
/// that will never come back.
// Called by `send`, the write half; see the note on `HostCommand`.
#[allow(dead_code)]
fn check_endpoint_request(request_id: &str, request: &str) -> Result<(), &'static str> {
    let envelope: serde_json::Value =
        serde_json::from_str(request).map_err(|_| "endpoint request is not a JSON object")?;
    match envelope.get("id").and_then(serde_json::Value::as_str) {
        Some(id) if id == request_id => Ok(()),
        Some(_) => Err("endpoint request id does not match the JSON id field"),
        None => Err("endpoint request has no string id field"),
    }
}

/// Write the next queued request for the host's current boot, if the lane is
/// free. A request whose write failed is returned as its own failure; the
/// stream is already cleared, so the supervisor will reconnect.
fn pump_lane(state: &mut MutexGuard<'_, HostLinkState>) -> Vec<LaneResponse> {
    let Some(boot_id) = state.boot_id.clone() else {
        return Vec::new();
    };
    let Some((request_id, request)) = state.lane.take_next() else {
        return Vec::new();
    };
    let sent = write_locked(
        state,
        &ClientMessage::ClientShellEndpointRequest {
            boot_id: boot_id.clone(),
            request,
        },
    );
    state.lane.mark_sent(boot_id, request_id, Instant::now());
    match sent {
        Ok(()) => Vec::new(),
        Err(error) => state
            .lane
            .fail_in_flight(&format!("could not send the request: {error}"))
            .into_iter()
            .collect(),
    }
}

/// Fail an in-flight request that has outlived the timeout, then let the next
/// queued one run.
fn expire_lane(state: &mut MutexGuard<'_, HostLinkState>) -> Vec<LaneResponse> {
    let Some(expired) = state.lane.expire(Instant::now()) else {
        return Vec::new();
    };
    let mut failed = vec![expired];
    failed.extend(pump_lane(state));
    failed
}

/// What ended one connection's read loop.
enum ReadOutcome {
    /// The connection is gone; reconnect after the backoff.
    Disconnected(String),
    /// The host cannot speak generation 1; retry rarely.
    Incompatible {
        generation: Option<u32>,
        reason: String,
    },
    /// The connector is shutting down, or nobody is listening any more.
    Stopped,
}

/// One host's connect → handshake → read → backoff loop.
struct Supervisor {
    spec: HostSpec,
    options: Arc<FleetConnectorOptions>,
    factory: TransportFactory,
    state: Arc<Mutex<HostLinkState>>,
    active: Arc<AtomicBool>,
    events: mpsc::Sender<FleetEvent>,
    stop: Arc<AtomicBool>,
    finished: std::sync::mpsc::Sender<HostId>,
}

impl Supervisor {
    fn run(self) {
        let host = self.spec.id.clone();
        self.run_until_stopped();
        // Announce the exit so `shutdown` can wait for a bounded time.
        let _ = self.finished.send(host);
    }

    fn run_until_stopped(&self) {
        let host = &self.spec.id;
        let mut transport = match (self.factory)(&self.spec, &self.options) {
            Ok(transport) => transport,
            Err(reason) => {
                // No transport exists for this host in this build. Retrying
                // cannot change that, so report it once and stop.
                tracing::debug!(host = %host, reason = %reason, "fleet host has no transport");
                self.emit(HostEvent::Unavailable {
                    reason,
                    retry_in: None,
                });
                return;
            }
        };

        let mut backoff = Backoff::new();
        let mut attempt: u32 = 0;
        while !self.stopped() {
            attempt = attempt.saturating_add(1);
            if !self.emit(HostEvent::Connecting { attempt }) {
                return;
            }

            let delay = match self.connect_and_read(transport.as_mut(), &mut backoff) {
                ReadOutcome::Stopped => return,
                ReadOutcome::Disconnected(reason) => {
                    let delay = backoff.next();
                    tracing::debug!(host = %host, reason = %reason, "fleet host disconnected");
                    if !self.emit(HostEvent::Unavailable {
                        reason,
                        retry_in: Some(delay),
                    }) {
                        return;
                    }
                    delay
                }
                ReadOutcome::Incompatible { generation, reason } => {
                    // An incompatible host needs an update, not a fast retry,
                    // so it waits at the backoff ceiling. It is still retried:
                    // that update may happen while the fleet is running.
                    let delay = drive_to_ceiling(&mut backoff);
                    tracing::warn!(host = %host, reason = %reason, "fleet host is incompatible");
                    if !self.emit(HostEvent::Incompatible { generation, reason }) {
                        return;
                    }
                    delay
                }
            };
            if !self.sleep_unless_stopped(delay) {
                return;
            }
        }
    }

    /// One full connection attempt, from `connect` to the end of the reads.
    fn connect_and_read(
        &self,
        transport: &mut dyn HostTransport,
        backoff: &mut Backoff,
    ) -> ReadOutcome {
        let host = &self.spec.id;
        let mut stream = match transport.connect() {
            Ok(stream) => stream,
            Err(error) => {
                return ReadOutcome::Disconnected(error.to_string());
            }
        };

        // Publish the half-open connection so `shutdown` can end it even while
        // the welcome has not arrived.
        match stream.try_clone() {
            Ok(pending) => lock(&self.state).pending = Some(pending),
            Err(error) => {
                tracing::debug!(host = %host, error = %error, "fleet host connection is not splittable");
            }
        }

        let mut params = self.options.handshake.clone();
        params.surface_size = self.wanted_surface();
        params.read_timeout = transport.read_timeout();
        let handshake = endpoint_handshake(&mut stream, &params);
        let welcome = match handshake {
            Ok(HandshakeOutcome::Connected(welcome)) => welcome,
            // Every other outcome ends this connection: release the pending
            // slot so the socket is not held for the whole backoff.
            Ok(HandshakeOutcome::Incompatible { generation, reason }) => {
                self.clear_pending();
                return ReadOutcome::Incompatible { generation, reason };
            }
            Ok(HandshakeOutcome::Rejected { code, message }) => {
                self.clear_pending();
                return ReadOutcome::Incompatible {
                    generation: None,
                    reason: format!("host refused the connection ({code}): {message}"),
                };
            }
            Err(error) => {
                self.clear_pending();
                return ReadOutcome::Disconnected(format!("{}: {error}", transport.describe()));
            }
        };

        let read_stream = match stream.try_clone() {
            Ok(read_stream) => read_stream,
            Err(error) => {
                self.clear_pending();
                return ReadOutcome::Disconnected(format!(
                    "could not split the host connection: {error}"
                ));
            }
        };

        {
            // Register the write half and re-check the active gate under the
            // same lock `set_active` uses: an activation that ran during the
            // handshake either found no stream (and this check sends the
            // resize) or found one (and sent it itself).
            let mut state = lock(&self.state);
            state.pending = None;
            state.stream = Some(stream);
            state.boot_id = None;
            state.surface = Some(params.surface_size);
            state.lane = EndpointLane::with_timeout(self.options.endpoint_timeout);
            let wanted = self.wanted_surface();
            if wanted != params.surface_size {
                state.surface = Some(wanted);
                let resize = ClientMessage::ClientShellResize {
                    cell_width_px: params.cell_width_px,
                    cell_height_px: params.cell_height_px,
                    surface_size: wanted,
                    pixel_mouse: params.pixel_mouse,
                };
                if let Err(error) = write_locked(&mut state, &resize) {
                    tracing::warn!(host = %host, error = %error, "failed to size a fleet host");
                }
            }
        }

        backoff.reset();
        let outcome = if self.emit(HostEvent::Connected {
            server_version: welcome.server_version.clone(),
            methods: welcome.methods.clone(),
        }) {
            self.read_loop(read_stream)
        } else {
            ReadOutcome::Stopped
        };
        self.disconnect(match &outcome {
            ReadOutcome::Disconnected(reason) | ReadOutcome::Incompatible { reason, .. } => {
                reason.as_str()
            }
            ReadOutcome::Stopped => "fleet client stopped",
        });
        outcome
    }

    fn read_loop(&self, mut stream: LocalStream) -> ReadOutcome {
        loop {
            if self.stopped() {
                return ReadOutcome::Stopped;
            }
            match protocol::read_message::<_, ServerMessage>(
                &mut stream,
                self.options.max_frame_size,
            ) {
                Ok(message) => match self.handle_message(message) {
                    Some(outcome) => return outcome,
                    None => continue,
                },
                Err(FramingError::UnexpectedEof) => {
                    return ReadOutcome::Disconnected("host closed the connection".to_string())
                }
                Err(error) => {
                    return ReadOutcome::Disconnected(format!("host read failed: {error}"))
                }
            }
        }
    }

    /// Fold one server message in. `Some` ends the connection.
    fn handle_message(&self, message: ServerMessage) -> Option<ReadOutcome> {
        let host = &self.spec.id;
        match message {
            ServerMessage::EndpointControl { kind, data } => {
                if kind == ENDPOINT_SNAPSHOT_KIND {
                    return self.handle_snapshot(&data);
                }
                if kind.starts_with("shell.snapshot.") {
                    return Some(ReadOutcome::Incompatible {
                        generation: None,
                        reason: format!(
                            "host sent snapshot codec {kind}; this client negotiated {SNAPSHOT_CODEC_V1}"
                        ),
                    });
                }
                // Unknown named controls are optional and ignored by contract.
                tracing::debug!(host = %host, kind = %kind, "ignoring an endpoint control");
                None
            }
            ServerMessage::ClientShellSnapshot(_) => Some(ReadOutcome::Incompatible {
                generation: None,
                reason: "host sent a binary snapshot on the endpoint lane".to_string(),
            }),
            ServerMessage::PaneSurface(frame) => {
                // Dropped before the box: an inactive host must cost nothing.
                if !self.is_active() {
                    return None;
                }
                self.forward(FleetEvent::Surface {
                    host: host.clone(),
                    frame: Box::new(frame),
                })
            }
            ServerMessage::PaneSurfacePatch(patch) => {
                if !self.is_active() {
                    return None;
                }
                self.forward(FleetEvent::SurfacePatch {
                    host: host.clone(),
                    patch: Box::new(patch),
                })
            }
            ServerMessage::SemanticNotification(notification) => {
                self.forward(FleetEvent::Notification {
                    host: host.clone(),
                    notification: Box::new(notification),
                })
            }
            ServerMessage::ClientShellEndpointResponseChunk {
                boot_id,
                request_id,
                final_chunk,
                data,
            } => self.handle_endpoint_chunk(&boot_id, &request_id, final_chunk, data),
            ServerMessage::ServerShutdown { reason } => Some(ReadOutcome::Disconnected(
                reason.unwrap_or_else(|| "host server shut down".to_string()),
            )),
            ServerMessage::ClientShellError { message } => {
                tracing::warn!(host = %host, message = %message, "fleet host reported an error");
                if !self.is_active() {
                    return None;
                }
                self.forward(FleetEvent::ServerMessage {
                    host: host.clone(),
                    message: Box::new(ServerMessage::ClientShellError { message }),
                })
            }
            other => {
                if !self.is_active() {
                    return None;
                }
                self.forward(FleetEvent::ServerMessage {
                    host: host.clone(),
                    message: Box::new(other),
                })
            }
        }
    }

    fn handle_snapshot(&self, data: &str) -> Option<ReadOutcome> {
        let snapshot: ClientShellSnapshot = match serde_json::from_str(data) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                // The negotiated codec did not decode: this connection is not
                // speaking what it agreed to.
                return Some(ReadOutcome::Disconnected(format!(
                    "host sent an unreadable snapshot: {error}"
                )));
            }
        };
        let failed = {
            let mut state = lock(&self.state);
            state.boot_id = Some(snapshot.boot_id.clone());
            // The lane is checked for an expired request wherever this lock is
            // already taken, so an unanswered request cannot block the queue
            // for as long as the host keeps publishing.
            expire_lane(&mut state)
        };
        for response in failed {
            if let Some(stopped) = self.report_lane_response(response) {
                return Some(stopped);
            }
        }
        self.forward(FleetEvent::Host {
            host: self.spec.id.clone(),
            event: HostEvent::Snapshot(Box::new(snapshot)),
        })
    }

    fn report_lane_response(&self, response: LaneResponse) -> Option<ReadOutcome> {
        self.forward(FleetEvent::EndpointResponse {
            host: self.spec.id.clone(),
            request_id: response.request_id,
            result: response.result,
        })
    }

    fn handle_endpoint_chunk(
        &self,
        boot_id: &str,
        request_id: &str,
        final_chunk: bool,
        data: Vec<u8>,
    ) -> Option<ReadOutcome> {
        let mut state = lock(&self.state);
        let received = state
            .lane
            .receive_chunk(boot_id, request_id, final_chunk, data);
        let mut finished = match received {
            Ok(Some(response)) => vec![response],
            Ok(None) => return None,
            Err(reason) => {
                // A miscorrelated answer is a protocol violation: fail the real
                // request rather than hand it someone else's bytes, and drop
                // the connection.
                let failed = state.lane.fail_in_flight(&reason);
                drop(state);
                if let Some(failed) = failed {
                    if let Some(stopped) = self.report_lane_response(failed) {
                        return Some(stopped);
                    }
                }
                return Some(ReadOutcome::Disconnected(reason));
            }
        };
        // Free lane: start the next queued request on this boot.
        finished.extend(pump_lane(&mut state));
        drop(state);
        for response in finished {
            if let Some(stopped) = self.report_lane_response(response) {
                return Some(stopped);
            }
        }
        None
    }

    /// Tear the connection down and fail everything that was waiting on it.
    fn disconnect(&self, reason: &str) {
        let failed = {
            let mut state = lock(&self.state);
            state.stream = None;
            state.pending = None;
            state.boot_id = None;
            state.surface = None;
            state.lane.fail_all(reason)
        };
        for response in failed {
            if self
                .forward(FleetEvent::EndpointResponse {
                    host: self.spec.id.clone(),
                    request_id: response.request_id,
                    result: response.result,
                })
                .is_some()
            {
                break;
            }
        }
    }

    /// Release a connection that never became usable.
    fn clear_pending(&self) {
        lock(&self.state).pending = None;
    }

    fn wanted_surface(&self) -> ClientSurfaceSize {
        if self.is_active() {
            self.options.active_surface
        } else {
            self.options.handshake.surface_size
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Put one event on the merged stream. `false` means nobody is listening.
    fn emit(&self, event: HostEvent) -> bool {
        self.forward(FleetEvent::Host {
            host: self.spec.id.clone(),
            event,
        })
        .is_none()
    }

    /// Put one event on the merged stream, mapping a closed channel to
    /// [`ReadOutcome::Stopped`] so callers can `?` it out of the read loop.
    fn forward(&self, event: FleetEvent) -> Option<ReadOutcome> {
        match self.events.blocking_send(event) {
            Ok(()) => None,
            Err(_) => Some(ReadOutcome::Stopped),
        }
    }

    /// Sleep, waking every 100 ms to check the stop flag. `false` means stop.
    fn sleep_unless_stopped(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.stopped() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            std::thread::sleep(remaining.min(STOP_CHECK_INTERVAL));
        }
    }
}

/// Drive a backoff to its ceiling and return it.
///
/// Reuses `state::Backoff` rather than restating the 30 s cap here.
fn drive_to_ceiling(backoff: &mut Backoff) -> Duration {
    let mut delay = backoff.next();
    while backoff.peek() != delay {
        delay = backoff.next();
    }
    delay
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;

    use crate::fleet::hosts::HostKind;
    use crate::fleet::state::{FleetState, HostConnection};
    use crate::fleet::transport::LocalTransport;
    use crate::ipc::{bind_local_listener, connect_local_stream, LocalListener};
    use crate::protocol::endpoint::{
        EndpointServerWelcome, ENDPOINT_SNAPSHOT_KIND, ENDPOINT_WELCOME_KIND,
    };
    use crate::protocol::{FrameData, PaneSurfaceFrame, SurfaceGraphicsScene};
    use interprocess::local_socket::traits::Listener as _;

    /// The frozen generation-1 snapshot the endpoint contract pins.
    const FROZEN_SNAPSHOT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/endpoint-snapshot-v1.json"
    ));

    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("herdr-fleet-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn snapshot(boot_id: &str, revision: u64) -> ClientShellSnapshot {
        let mut snapshot: ClientShellSnapshot =
            serde_json::from_str(FROZEN_SNAPSHOT).expect("frozen snapshot decodes");
        snapshot.boot_id = boot_id.to_string();
        snapshot.revision = revision;
        snapshot
    }

    fn snapshot_message(snapshot: &ClientShellSnapshot) -> ServerMessage {
        ServerMessage::EndpointControl {
            kind: ENDPOINT_SNAPSHOT_KIND.to_string(),
            data: serde_json::to_string(snapshot).expect("snapshot encodes"),
        }
    }

    fn surface_message(boot_id: &str, surface_revision: u64) -> ServerMessage {
        ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: boot_id.to_string(),
            projection_revision: 1,
            surface_revision,
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: SurfaceGraphicsScene::default(),
        })
    }

    /// What a fake host does once it has read the hello.
    #[derive(Debug, Clone)]
    enum Behaviour {
        /// Welcome, then these messages, then stay connected.
        Serve(Vec<ServerMessage>),
        /// Welcome then hang up on the first connection; serve afterwards.
        DropFirstConnection(Vec<ServerMessage>),
        /// Answer with a welcome this client must refuse.
        Incompatible,
        /// Accept the connection and never answer the hello.
        Silent,
        /// Welcome, then these messages, then answer every endpoint request
        /// in two chunks — tagged with the request's boot id, or `reply_boot`.
        Answer {
            messages: Vec<ServerMessage>,
            reply_boot: Option<String>,
        },
    }

    /// A herdr endpoint server, minus herdr.
    ///
    /// Speaks the real wire (`protocol::read_message`/`write_message`) over a
    /// real local socket, so the connector under test is not talking to a stub
    /// of itself.
    struct FakeHost {
        socket: PathBuf,
        received: Arc<Mutex<Vec<ClientMessage>>>,
        connections: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
    }

    impl FakeHost {
        fn start(dir: &Path, name: &str, behaviour: Behaviour) -> Self {
            let socket = dir.join(format!("{name}.sock"));
            let listener = bind_local_listener(&socket).expect("bind fake host");
            let received = Arc::new(Mutex::new(Vec::new()));
            let connections = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let host = Self {
                socket,
                received: Arc::clone(&received),
                connections: Arc::clone(&connections),
                stop: Arc::clone(&stop),
            };
            std::thread::spawn(move || {
                accept_loop(listener, behaviour, received, connections, stop)
            });
            host
        }

        fn spec(&self, id: &str) -> HostSpec {
            HostSpec {
                id: HostId::new(id).expect("valid host id"),
                kind: HostKind::Local {
                    session: Some(id.to_string()),
                },
                enabled: true,
            }
        }

        fn received(&self) -> Vec<ClientMessage> {
            lock(&self.received).clone()
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::Acquire)
        }
    }

    impl Drop for FakeHost {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Wake the blocking accept so the listener thread can exit.
            let _ = connect_local_stream(&self.socket);
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn accept_loop(
        listener: LocalListener,
        behaviour: Behaviour,
        received: Arc<Mutex<Vec<ClientMessage>>>,
        connections: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
    ) {
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let Ok(mut stream) = listener.accept() else {
                return;
            };
            if stop.load(Ordering::Acquire) {
                return;
            }
            let index = connections.fetch_add(1, Ordering::AcqRel);
            let behaviour = behaviour.clone();
            let received = Arc::clone(&received);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                serve_connection(&mut stream, index, &behaviour, &received, &stop);
            });
        }
    }

    fn serve_connection(
        stream: &mut LocalStream,
        index: usize,
        behaviour: &Behaviour,
        received: &Arc<Mutex<Vec<ClientMessage>>>,
        stop: &Arc<AtomicBool>,
    ) {
        if matches!(behaviour, Behaviour::Silent) {
            // Reads like a real server (so the client's half-close ends the
            // connection) but never answers.
            while protocol::read_message::<_, ClientMessage>(stream, MAX_FRAME_SIZE).is_ok() {}
            return;
        }
        let Ok(hello) = protocol::read_message::<_, ClientMessage>(stream, MAX_FRAME_SIZE) else {
            return;
        };
        lock(received).push(hello);

        let welcome = match behaviour {
            Behaviour::Incompatible => EndpointServerWelcome {
                generation: 2,
                server_version: "9.9.9".to_string(),
                snapshot_codec: "shell.snapshot.v2".to_string(),
                surface_codec: "shell.surface.v2".to_string(),
                input_codec: "shell.input.semantic.v2".to_string(),
                blob_codec: "shell.blob.v2".to_string(),
                methods: Vec::new(),
                error: None,
            },
            _ => EndpointServerWelcome::compatible(vec!["pane.write".to_string()]),
        };
        let welcome = ServerMessage::EndpointControl {
            kind: ENDPOINT_WELCOME_KIND.to_string(),
            data: serde_json::to_string(&welcome).expect("welcome encodes"),
        };
        if protocol::write_message(stream, &welcome).is_err() {
            return;
        }

        let messages = match behaviour {
            Behaviour::Incompatible => return,
            Behaviour::Silent => return,
            Behaviour::DropFirstConnection(_) if index == 0 => return,
            Behaviour::Serve(messages)
            | Behaviour::DropFirstConnection(messages)
            | Behaviour::Answer { messages, .. } => messages,
        };
        for message in messages {
            if protocol::write_message(stream, message).is_err() {
                return;
            }
        }
        // Keep the connection alive, recording whatever the client sends.
        while !stop.load(Ordering::Acquire) {
            match protocol::read_message::<_, ClientMessage>(stream, MAX_FRAME_SIZE) {
                Ok(message) => {
                    if let (
                        Behaviour::Answer { reply_boot, .. },
                        ClientMessage::ClientShellEndpointRequest { boot_id, request },
                    ) = (behaviour, &message)
                    {
                        if !answer_endpoint(
                            stream,
                            reply_boot.as_deref().unwrap_or(boot_id),
                            request,
                        ) {
                            return;
                        }
                    }
                    lock(received).push(message);
                }
                Err(_) => return,
            }
        }
    }

    /// Answer one endpoint request the way the server does: correlated by the
    /// request's JSON `id`, split into chunks.
    fn answer_endpoint(stream: &mut LocalStream, boot_id: &str, request: &str) -> bool {
        let request_id = serde_json::from_str::<serde_json::Value>(request)
            .ok()
            .and_then(|value| value.get("id")?.as_str().map(str::to_string))
            .unwrap_or_default();
        let body = format!("{{\"id\":\"{request_id}\",\"ok\":true}}");
        let (head, tail) = body.split_at(body.len() / 2);
        for (data, final_chunk) in [(head, false), (tail, true)] {
            let chunk = ServerMessage::ClientShellEndpointResponseChunk {
                boot_id: boot_id.to_string(),
                request_id: request_id.clone(),
                final_chunk,
                data: data.as_bytes().to_vec(),
            };
            if protocol::write_message(stream, &chunk).is_err() {
                return false;
            }
        }
        true
    }

    fn endpoint_requests(messages: &[ClientMessage]) -> Vec<(String, String)> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                    Some((boot_id.clone(), request.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Drain events, applying host events to `state`, until one other event
    /// satisfies `wanted` (returned) or the deadline passes.
    fn next_event(
        connector: &mut FleetConnector,
        state: &mut FleetState,
        timeout: Duration,
        mut wanted: impl FnMut(&FleetEvent) -> bool,
    ) -> Option<FleetEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match connector.events().try_recv() {
                Ok(FleetEvent::Host { host, event }) => {
                    state.apply(&host, event);
                }
                Ok(event) if wanted(&event) => return Some(event),
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
        None
    }

    fn endpoint_response(event: &FleetEvent) -> bool {
        matches!(event, FleetEvent::EndpointResponse { .. })
    }

    fn endpoint_command(request_id: &str) -> HostCommand {
        HostCommand::Endpoint {
            request_id: request_id.to_string(),
            request: format!("{{\"id\":\"{request_id}\",\"method\":\"session.snapshot\"}}"),
        }
    }

    /// A connector whose hosts are the given fakes, by id.
    fn connector(hosts: &[(&str, &FakeHost)], options: FleetConnectorOptions) -> FleetConnector {
        let sockets: HashMap<HostId, PathBuf> = hosts
            .iter()
            .map(|(id, host)| (HostId::new(id).expect("valid host id"), host.socket.clone()))
            .collect();
        let specs = hosts
            .iter()
            .map(|(id, host)| host.spec(id))
            .collect::<Vec<_>>();
        FleetConnector::start_with(
            specs,
            options,
            Arc::new(move |spec: &HostSpec, _options: &FleetConnectorOptions| {
                let Some(socket) = sockets.get(&spec.id) else {
                    return Err("no fake host".to_string());
                };
                Ok(Box::new(LocalTransport::with_socket(socket.clone())) as Box<dyn HostTransport>)
            }),
        )
    }

    /// Drain events into a state until `done`, or until the deadline.
    fn drain_until(
        connector: &mut FleetConnector,
        state: &mut FleetState,
        timeout: Duration,
        mut done: impl FnMut(&FleetState) -> bool,
    ) -> Vec<FleetEvent> {
        let deadline = Instant::now() + timeout;
        let mut other = Vec::new();
        while !done(state) && Instant::now() < deadline {
            match connector.events().try_recv() {
                Ok(FleetEvent::Host { host, event }) => {
                    state.apply(&host, event);
                }
                Ok(event) => other.push(event),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        other
    }

    fn connected_with_snapshot(state: &FleetState, id: &str) -> bool {
        let Ok(id) = HostId::new(id) else {
            return false;
        };
        state
            .host(&id)
            .is_some_and(|host| host.connection.is_connected() && host.snapshot.is_some())
    }

    fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    fn hello_surface(messages: &[ClientMessage]) -> Vec<ClientSurfaceSize> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::EndpointControl { kind, data }
                    if kind == crate::protocol::endpoint::ENDPOINT_HELLO_KIND =>
                {
                    serde_json::from_str::<crate::protocol::endpoint::EndpointClientHello>(data)
                        .ok()
                        .map(|hello| hello.surface_size)
                }
                _ => None,
            })
            .collect()
    }

    fn resizes(messages: &[ClientMessage]) -> Vec<ClientSurfaceSize> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::ClientShellResize { surface_size, .. } => Some(*surface_size),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn two_hosts_merge_into_one_state() {
        let dir = scratch_dir("merge");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 3))]),
        );
        let beta = FakeHost::start(
            &dir,
            "beta",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-beta", 5))]),
        );
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let mut state = FleetState::new(specs);
        let mut connector = connector(
            &[("alpha", &alpha), ("beta", &beta)],
            FleetConnectorOptions::default(),
        );

        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| {
                connected_with_snapshot(state, "alpha") && connected_with_snapshot(state, "beta")
            },
        );

        // Both agents are blocked with the same rank, so their order is
        // arrival recency — a race between two live sockets. What must hold is
        // that each host contributed exactly its own, host-prefixed reference.
        let mut merged = state
            .merged_agents()
            .iter()
            .map(|agent| agent.pane.to_string())
            .collect::<Vec<_>>();
        merged.sort();
        assert_eq!(merged, vec!["alpha/w1:p1", "beta/w1:p1"]);
        state.assert_invariants_for_test();

        // A read-only status client never types on a host.
        for host in [&alpha, &beta] {
            assert!(
                !host
                    .received()
                    .iter()
                    .any(|message| matches!(message, ClientMessage::ClientShellPaneInput { .. })),
                "the connector must not send pane input"
            );
        }
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_revision_is_dropped_through_the_connector() {
        let dir = scratch_dir("stale");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![
                snapshot_message(&snapshot("boot-alpha", 7)),
                snapshot_message(&snapshot("boot-alpha", 2)),
            ]),
        );
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        let mut connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());

        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );
        // Give the second, older snapshot time to arrive and be ignored.
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_millis(300),
            |_| false,
        );

        let id = HostId::new("alpha").expect("valid host id");
        let host = state.host(&id).expect("known host");
        assert_eq!(
            host.snapshot.as_ref().map(|snapshot| snapshot.revision),
            Some(7),
            "an older revision of the same boot must not replace the newer one"
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_host_that_hangs_up_reconnects_after_the_first_backoff() {
        let dir = scratch_dir("reconnect");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::DropFirstConnection(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        let mut connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());

        let id = HostId::new("alpha").expect("valid host id");
        let mut retry_in = None;
        let mut attempts = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && !connected_with_snapshot(&state, "alpha") {
            match connector.events().try_recv() {
                Ok(FleetEvent::Host { host, event }) => {
                    state.apply(&host, event);
                    if let Some(host) = state.host(&id) {
                        match &host.connection {
                            HostConnection::Unavailable { retry_in: next, .. } => retry_in = *next,
                            HostConnection::Connecting { attempt } => attempts.push(*attempt),
                            _ => {}
                        }
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        assert!(
            connected_with_snapshot(&state, "alpha"),
            "the host must come back after the first backoff"
        );
        assert_eq!(
            retry_in,
            Some(Duration::from_secs(1)),
            "the first reconnect waits the 1 s floor"
        );
        assert!(
            attempts.contains(&2),
            "the second attempt must be reported: {attempts:?}"
        );
        assert!(alpha.connections() >= 2, "the host must be reconnected");
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frames_from_an_inactive_host_never_reach_the_channel() {
        let dir = scratch_dir("frames");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![
                snapshot_message(&snapshot("boot-alpha", 1)),
                surface_message("boot-alpha", 1),
            ]),
        );
        let beta = FakeHost::start(
            &dir,
            "beta",
            Behaviour::Serve(vec![
                snapshot_message(&snapshot("boot-beta", 1)),
                surface_message("boot-beta", 1),
            ]),
        );
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let mut state = FleetState::new(specs);
        let mut connector = connector(
            &[("alpha", &alpha), ("beta", &beta)],
            FleetConnectorOptions::default(),
        );
        // `start` makes the first enabled host active, which is what the fake
        // frames are testing: alpha's must arrive, beta's must not.
        let other = drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| {
                connected_with_snapshot(state, "alpha") && connected_with_snapshot(state, "beta")
            },
        );
        let mut other = other;
        other.extend(drain_until(
            &mut connector,
            &mut state,
            Duration::from_millis(400),
            |_| false,
        ));

        let surfaces = other
            .iter()
            .filter_map(|event| match event {
                FleetEvent::Surface { host, .. } => Some(host.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            surfaces,
            vec!["alpha"],
            "only the active host's surfaces may be forwarded"
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn activation_sizes_the_new_host_up_and_the_old_one_down() {
        let dir = scratch_dir("activate");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let beta = FakeHost::start(
            &dir,
            "beta",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-beta", 1))]),
        );
        // Deliberately different from `INACTIVE_SURFACE` in both dimensions.
        let active_surface = ClientSurfaceSize {
            cols: 200,
            rows: 60,
        };
        let options = FleetConnectorOptions {
            active_surface,
            ..FleetConnectorOptions::default()
        };
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let mut state = FleetState::new(specs);
        let mut connector = connector(&[("alpha", &alpha), ("beta", &beta)], options);

        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| {
                connected_with_snapshot(state, "alpha") && connected_with_snapshot(state, "beta")
            },
        );

        // alpha started active, so it announced the active size in its hello.
        assert!(
            wait_for(Duration::from_secs(2), || hello_surface(&alpha.received())
                == vec![active_surface]),
            "alpha's hello must carry the active size: {:?}",
            hello_surface(&alpha.received())
        );
        assert_eq!(
            hello_surface(&beta.received()),
            vec![INACTIVE_SURFACE],
            "an inactive host is handshaken small"
        );

        let beta_id = HostId::new("beta").expect("valid host id");
        connector
            .set_active(Some(&beta_id))
            .expect("beta can be activated");

        assert!(
            wait_for(Duration::from_secs(2), || resizes(&alpha.received())
                == vec![INACTIVE_SURFACE]),
            "the old active host must be resized down: {:?}",
            resizes(&alpha.received())
        );
        assert!(
            wait_for(Duration::from_secs(2), || resizes(&beta.received())
                == vec![active_surface]),
            "the new active host must be resized up: {:?}",
            resizes(&beta.received())
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_incompatible_host_is_reported_and_not_retried_fast() {
        let dir = scratch_dir("incompatible");
        let alpha = FakeHost::start(&dir, "alpha", Behaviour::Incompatible);
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        let mut connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        let id = HostId::new("alpha").expect("valid host id");

        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| {
                state.host(&id).is_some_and(|host| {
                    matches!(host.connection, HostConnection::Incompatible { .. })
                })
            },
        );

        let host = state.host(&id).expect("known host");
        let HostConnection::Incompatible { generation, reason } = &host.connection else {
            panic!("expected an incompatible host, got {:?}", host.connection);
        };
        assert_eq!(*generation, Some(2));
        assert!(
            reason.contains("generation 2"),
            "unexpected reason: {reason}"
        );
        // The retry sits at the 30 s ceiling, so no second attempt lands here.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(alpha.connections(), 1);
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sending_to_an_unknown_or_disconnected_host_errors_without_panicking() {
        let dir = scratch_dir("send");
        let alpha = FakeHost::start(&dir, "alpha", Behaviour::Silent);
        let connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());

        let unknown = HostId::new("nowhere").expect("valid host id");
        assert!(matches!(
            connector.send(&unknown, HostCommand::Focus(true)),
            Err(HostSendError::UnknownHost)
        ));
        assert!(matches!(
            connector.set_active(Some(&unknown)),
            Err(HostSendError::UnknownHost)
        ));

        let alpha_id = HostId::new("alpha").expect("valid host id");
        // The fake never answers the hello, so the host never becomes usable.
        assert!(matches!(
            connector.send(&alpha_id, HostCommand::Resize(INACTIVE_SURFACE)),
            Err(HostSendError::NotConnected)
        ));
        assert!(matches!(
            connector.send(
                &alpha_id,
                HostCommand::Endpoint {
                    request_id: "r1".to_string(),
                    request: "{}".to_string(),
                }
            ),
            Err(HostSendError::NotConnected)
        ));
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_disabled_host_is_never_opened_or_activated() {
        let dir = scratch_dir("disabled");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let mut spec = alpha.spec("alpha");
        spec.enabled = false;
        let connector = FleetConnector::start_with(
            vec![spec],
            FleetConnectorOptions::default(),
            Arc::new(|_spec: &HostSpec, _options: &FleetConnectorOptions| {
                panic!("a disabled host must never build a transport")
            }),
        );

        let id = HostId::new("alpha").expect("valid host id");
        assert!(matches!(
            connector.set_active(Some(&id)),
            Err(HostSendError::NotConnected)
        ));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(alpha.connections(), 0);
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_returns_while_a_host_is_mid_backoff() {
        let dir = scratch_dir("shutdown");
        // No listener at all: every attempt fails and the host sits in backoff.
        let missing = dir.join("missing.sock");
        let spec = HostSpec {
            id: HostId::new("gone").expect("valid host id"),
            kind: HostKind::Local {
                session: Some("gone".to_string()),
            },
            enabled: true,
        };
        let mut connector = FleetConnector::start_with(
            vec![spec],
            FleetConnectorOptions::default(),
            Arc::new(move |_spec: &HostSpec, _options: &FleetConnectorOptions| {
                Ok(Box::new(LocalTransport::with_socket(missing.clone()))
                    as Box<dyn HostTransport>)
            }),
        );
        let mut state = FleetState::new(vec![HostSpec {
            id: HostId::new("gone").expect("valid host id"),
            kind: HostKind::Local {
                session: Some("gone".to_string()),
            },
            enabled: true,
        }]);
        let id = HostId::new("gone").expect("valid host id");
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(5),
            |state| {
                state.host(&id).is_some_and(|host| {
                    matches!(host.connection, HostConnection::Unavailable { .. })
                })
            },
        );

        let started = Instant::now();
        connector.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown must not wait out the backoff: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_endpoint_request_round_trips_in_two_chunks_on_the_hosts_boot() {
        let dir = scratch_dir("endpoint");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Answer {
                messages: vec![snapshot_message(&snapshot("boot-alpha", 1))],
                reply_boot: None,
            },
        );
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        let mut connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        let id = HostId::new("alpha").expect("valid host id");
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );

        connector
            .send(&id, endpoint_command("r1"))
            .expect("a connected host with a snapshot accepts a request");
        // A second request waits its turn: exactly one is in flight.
        connector
            .send(&id, endpoint_command("r2"))
            .expect("queued behind r1");

        let first = next_event(
            &mut connector,
            &mut state,
            Duration::from_secs(5),
            endpoint_response,
        )
        .expect("r1 is answered");
        let FleetEvent::EndpointResponse {
            host,
            request_id,
            result,
        } = first
        else {
            panic!("expected an endpoint response, got {first:?}");
        };
        assert_eq!(host, id);
        assert_eq!(request_id, "r1");
        assert_eq!(result, Ok(br#"{"id":"r1","ok":true}"#.to_vec()));

        let second = next_event(
            &mut connector,
            &mut state,
            Duration::from_secs(5),
            endpoint_response,
        )
        .expect("r2 runs once r1 is answered");
        let FleetEvent::EndpointResponse {
            request_id, result, ..
        } = second
        else {
            panic!("expected an endpoint response, got {second:?}");
        };
        assert_eq!(request_id, "r2");
        assert_eq!(result, Ok(br#"{"id":"r2","ok":true}"#.to_vec()));

        // Both requests carried the boot id this connection's snapshot
        // announced, never a caller-supplied one.
        assert!(wait_for(Duration::from_secs(2), || endpoint_requests(
            &alpha.received()
        )
        .len()
            == 2));
        for (boot_id, _) in endpoint_requests(&alpha.received()) {
            assert_eq!(boot_id, "boot-alpha");
        }
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_answer_for_another_boot_fails_the_request_and_the_connection() {
        let dir = scratch_dir("foreign-boot");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Answer {
                messages: vec![snapshot_message(&snapshot("boot-alpha", 1))],
                reply_boot: Some("boot-elsewhere".to_string()),
            },
        );
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        let mut connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        let id = HostId::new("alpha").expect("valid host id");
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );

        connector
            .send(&id, endpoint_command("r1"))
            .expect("request accepted");
        let event = next_event(
            &mut connector,
            &mut state,
            Duration::from_secs(5),
            endpoint_response,
        )
        .expect("the request must be failed, not silently dropped");
        let FleetEvent::EndpointResponse {
            request_id, result, ..
        } = event
        else {
            panic!("expected an endpoint response, got {event:?}");
        };
        assert_eq!(request_id, "r1");
        let reason = result.expect_err("bytes for another boot must not be handed over");
        assert!(
            reason.contains("boot-elsewhere"),
            "unexpected reason: {reason}"
        );

        // The connection is treated as broken and comes back on a fresh one.
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| {
                state.host(&id).is_some_and(|host| {
                    matches!(host.connection, HostConnection::Unavailable { .. })
                })
            },
        );
        assert!(
            state
                .host(&id)
                .is_some_and(|host| matches!(host.connection, HostConnection::Unavailable { .. })),
            "a miscorrelated answer must drop the connection"
        );
        assert!(
            wait_for(Duration::from_secs(5), || alpha.connections() >= 2),
            "the host must be reconnected"
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_and_connector_owned_commands_are_refused_before_the_host_sees_them() {
        let dir = scratch_dir("refused");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Answer {
                messages: vec![snapshot_message(&snapshot("boot-alpha", 1))],
                reply_boot: None,
            },
        );
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        let mut connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        let id = HostId::new("alpha").expect("valid host id");
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );

        let refused = [
            HostCommand::Endpoint {
                request_id: "r1".to_string(),
                request: r#"{"id":"r2","method":"session.snapshot"}"#.to_string(),
            },
            HostCommand::Endpoint {
                request_id: "r1".to_string(),
                request: r#"{"method":"session.snapshot"}"#.to_string(),
            },
            HostCommand::Endpoint {
                request_id: "r1".to_string(),
                request: "not json".to_string(),
            },
            HostCommand::Raw(Box::new(ClientMessage::ClientShellEndpointRequest {
                boot_id: "boot-elsewhere".to_string(),
                request: r#"{"id":"r1","method":"session.snapshot"}"#.to_string(),
            })),
            HostCommand::Raw(Box::new(ClientMessage::EndpointControl {
                kind: crate::protocol::endpoint::ENDPOINT_HELLO_KIND.to_string(),
                data: "{}".to_string(),
            })),
        ];
        for command in refused {
            let description = format!("{command:?}");
            assert!(
                matches!(connector.send(&id, command), Err(HostSendError::Refused(_))),
                "must be refused: {description}"
            );
        }
        // Nothing refused reached the host: one hello, no requests.
        std::thread::sleep(Duration::from_millis(200));
        let received = alpha.received();
        assert!(endpoint_requests(&received).is_empty(), "{received:?}");
        assert_eq!(
            received
                .iter()
                .filter(|message| matches!(message, ClientMessage::EndpointControl { .. }))
                .count(),
            1,
            "{received:?}"
        );
        // A raw resize is still tracked, so it is not repeated on activation.
        let raw_size = ClientSurfaceSize { cols: 77, rows: 11 };
        connector
            .send(
                &id,
                HostCommand::Raw(Box::new(ClientMessage::ClientShellResize {
                    cell_width_px: 0,
                    cell_height_px: 0,
                    surface_size: raw_size,
                    pixel_mouse: false,
                })),
            )
            .expect("a raw resize is a plain shell-lane message");
        assert!(wait_for(Duration::from_secs(2), || resizes(
            &alpha.received()
        ) == vec![raw_size]));
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unanswered_request_expires_when_the_next_one_is_queued() {
        let dir = scratch_dir("expire");
        // `Serve` records requests and never answers them.
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let options = FleetConnectorOptions {
            endpoint_timeout: Duration::from_millis(200),
            ..FleetConnectorOptions::default()
        };
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        let mut connector = connector(&[("alpha", &alpha)], options);
        let id = HostId::new("alpha").expect("valid host id");
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );

        connector
            .send(&id, endpoint_command("r1"))
            .expect("request accepted");
        std::thread::sleep(Duration::from_millis(300));
        connector
            .send(&id, endpoint_command("r2"))
            .expect("request accepted");

        let event = next_event(
            &mut connector,
            &mut state,
            Duration::from_secs(5),
            endpoint_response,
        )
        .expect("the expired request is reported");
        let FleetEvent::EndpointResponse {
            request_id, result, ..
        } = event
        else {
            panic!("expected an endpoint response, got {event:?}");
        };
        assert_eq!(request_id, "r1");
        assert!(result.is_err(), "an expiry is a failure");
        // r2 was written the moment r1 expired, not stuck behind it.
        assert!(
            wait_for(Duration::from_secs(2), || endpoint_requests(
                &alpha.received()
            )
            .len()
                == 2),
            "{:?}",
            endpoint_requests(&alpha.received())
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_returns_while_a_host_is_mid_handshake() {
        let dir = scratch_dir("shutdown-handshake");
        let alpha = FakeHost::start(&dir, "alpha", Behaviour::Silent);
        let connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        assert!(
            wait_for(Duration::from_secs(5), || alpha.connections() == 1),
            "the host must be mid-handshake"
        );
        std::thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        connector.shutdown();
        assert!(
            started.elapsed() < SHUTDOWN_JOIN_TIMEOUT,
            "shutdown must end a pending handshake instead of waiting out the welcome deadline: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_returns_while_a_host_is_blocked_on_a_full_channel() {
        let dir = scratch_dir("shutdown-full");
        let mut messages = vec![snapshot_message(&snapshot("boot-alpha", 1))];
        messages.extend(
            (1..=(EVENT_CHANNEL_CAPACITY as u64 + 50))
                .map(|revision| surface_message("boot-alpha", revision)),
        );
        // alpha is active (first enabled host), so every frame is forwarded
        // into a channel nobody drains.
        let alpha = FakeHost::start(&dir, "alpha", Behaviour::Serve(messages));
        let connector = connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        assert!(
            wait_for(Duration::from_secs(5), || alpha.connections() == 1),
            "the host must connect"
        );
        std::thread::sleep(Duration::from_millis(500));

        let started = Instant::now();
        connector.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a supervisor parked on a full channel must be released by shutdown: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
