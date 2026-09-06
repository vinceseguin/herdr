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
//!
//! Three mutexes are taken here, and always in this order:
//! `active_host` → one host's `HostLinkState` → `active_geometry`. A
//! supervisor thread never takes `active_host` (it reads its own `active`
//! atomic instead). Nothing takes two host link locks at once.

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
/// a 20x5 SIGWINCH — for a read-only `herdr fleet status`. E1 changes no
/// server code, so the value was the fix: herdr's own default headless
/// geometry, which a host with no attached client is *already* using, making
/// the common fleet case (headless servers running agents) a no-op resize.
///
/// Since upstream #3670 the size is no longer the whole story.
/// [`HandshakeParams::read_only`] announces `surface_active: false`, and a
/// server carrying that field never makes such a client the foreground one —
/// so a read-only consumer (`herdr fleet status`, the gateway) changes no
/// host's geometry at all, whatever this constant says. The value still
/// matters for two callers: a server *older* than #3670, which ignores the
/// field and sizes itself by the hello, and [`HandshakeParams::for_client`],
/// whose console genuinely is a foreground client for whichever host is
/// active.
///
/// The cost decision (e) was protecting is still paid where it matters: this
/// client drops an inactive host's frames in the reader thread before anything
/// reaches the event channel.
pub const INACTIVE_SURFACE: ClientSurfaceSize = ClientSurfaceSize {
    cols: crate::config::DEFAULT_HEADLESS_COLS,
    rows: crate::config::DEFAULT_HEADLESS_ROWS,
};

/// The geometry the console announces for whichever host is active.
///
/// One value rather than four parameters because every path that resizes the
/// active host — the activation in [`FleetConnector::set_active`], a terminal
/// resize through [`FleetConnector::set_active_geometry`], the hello a
/// reconnecting active host sends — has to announce exactly the same thing.
/// Splitting it is how the surface and the cell size drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveGeometry {
    pub surface: ClientSurfaceSize,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub pixel_mouse: bool,
}

impl ActiveGeometry {
    /// The resize that announces this geometry.
    fn resize_message(&self) -> ClientMessage {
        ClientMessage::ClientShellResize {
            cell_width_px: self.cell_width_px,
            cell_height_px: self.cell_height_px,
            surface_size: self.surface,
            pixel_mouse: self.pixel_mouse,
        }
    }
}

impl Default for ActiveGeometry {
    /// A read-only collector's geometry: the inactive size, no cell metrics,
    /// no pixel mouse — what `herdr fleet status` announces.
    fn default() -> Self {
        Self {
            surface: INACTIVE_SURFACE,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        }
    }
}

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
    /// Geometry announced to whichever host is active, at start.
    ///
    /// Only the starting value: the connector owns it from then on, so
    /// [`FleetConnector::set_active_geometry`] can follow the terminal.
    pub active: ActiveGeometry,
    /// Whether ssh transports use herdr's managed ssh config (`[remote]`'s
    /// `manage_ssh_config`): keepalive fallbacks and a private control master,
    /// exactly as `herdr --remote` uses them.
    pub manage_ssh_config: bool,
    /// Whether ssh transports run their per-connection `ssh` child
    /// noninteractively: `BatchMode=yes`, no password prompts, a bounded
    /// connect, and **stderr discarded** rather than inherited.
    ///
    /// `false` is `herdr --remote`'s behaviour and what a CLI wants — ssh's own
    /// warnings reach the operator's terminal. A daemon or a full-screen
    /// consumer has no terminal to prompt on and no place to paint an ssh
    /// warning, so it sets `true`; see [`FleetConnectorOptions::for_daemon`].
    pub ssh_noninteractive: bool,
    /// Largest endpoint frame accepted from a host.
    pub max_frame_size: usize,
    /// How long one endpoint request may wait for its answer before it is
    /// failed and the next queued request runs.
    pub endpoint_timeout: Duration,
}

impl FleetConnectorOptions {
    /// Read-only options for one `[fleet]` run.
    ///
    /// `[remote]`'s `manage_ssh_config` is the same switch `herdr --remote`
    /// reads, so an operator who turned herdr's managed ssh config off gets
    /// plain ssh for fleet hosts too.
    pub fn for_config(config: &crate::config::Config) -> Self {
        Self {
            manage_ssh_config: config.remote.manage_ssh_config,
            ..Self::default()
        }
    }

    /// Read-only options for a daemon with no controlling terminal.
    ///
    /// Everything [`Self::for_config`] decides, plus noninteractive ssh: a
    /// gateway cannot answer a password prompt, and `bridge_connection` sends
    /// the ssh child's stderr to `/dev/null` instead of inheriting the
    /// daemon's. The handshake stays [`HandshakeParams::read_only`], so every
    /// host sees a passive client.
    ///
    /// The switch reaches the *bridged* ssh children — the ones that carry the
    /// endpoint stream. It does not reach the short-lived discovery commands
    /// `SshTransport` runs first (`uname`, the binary probe): those go through
    /// `remote::attach`'s interactive `RemoteSsh`, whose output is piped (so
    /// nothing is painted on a daemon's stderr) but which has no `BatchMode`.
    /// Against a host that cannot authenticate without a prompt, that probe
    /// blocks its own supervisor thread — host-local, exactly as a hung ssh
    /// probe is elsewhere — and no other host or reader is affected. Closing
    /// it needs a constructor in `src/remote/`, which E3 deliberately leaves
    /// untouched.
    // The gateway is this constructor's only production caller, and it is
    // compiled out by `--no-default-features`. The allow is therefore scoped to
    // exactly that build rather than being unconditional, so a future default
    // build that stops calling it still fails the lint.
    #[cfg_attr(not(feature = "gateway"), allow(dead_code))]
    pub fn for_daemon(config: &crate::config::Config) -> Self {
        Self {
            ssh_noninteractive: true,
            ..Self::for_config(config)
        }
    }

    /// Options for a full-screen console.
    ///
    /// `handshake` is the console's hello for every host (see
    /// [`HandshakeParams::for_client`]) and `active` is the geometry the one
    /// active host renders at; both are the console terminal's, and the
    /// connector keeps `active` current from there.
    // Constructed by the fleet console (E2 PR 4); this PR only ships the
    // constructor it will call.
    #[allow(dead_code)]
    pub fn for_client(
        config: &crate::config::Config,
        handshake: HandshakeParams,
        active: ActiveGeometry,
    ) -> Self {
        Self {
            handshake,
            active,
            ..Self::for_config(config)
        }
    }
}

impl Default for FleetConnectorOptions {
    fn default() -> Self {
        Self {
            handshake: HandshakeParams::read_only(INACTIVE_SURFACE),
            active: ActiveGeometry::default(),
            manage_ssh_config: false,
            ssh_noninteractive: false,
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
    /// Geometry this client last announced to the host.
    announced: Option<ActiveGeometry>,
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
    /// `None` once [`FleetConnector::take_events`] handed the receiver to the
    /// caller, which then owns closing it.
    events: Option<mpsc::Receiver<FleetEvent>>,
    /// For failures `send` discovers on the caller's thread (an expired
    /// request). Weak so the channel still closes once every supervisor has
    /// exited, which is how a consumer learns nothing else can arrive.
    // Read by `send`, the write half; see the note on `HostCommand`.
    #[allow(dead_code)]
    events_tx: mpsc::WeakSender<FleetEvent>,
    options: Arc<FleetConnectorOptions>,
    active_host: Mutex<Option<HostId>>,
    /// Geometry for whichever host is active, shared with the supervisors so
    /// a host that reconnects while active handshakes at the *latest* size
    /// rather than the one the console started with.
    active_geometry: Arc<Mutex<ActiveGeometry>>,
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

    pub(crate) fn start_with(
        specs: Vec<HostSpec>,
        options: FleetConnectorOptions,
        factory: TransportFactory,
    ) -> Self {
        let active_geometry = Arc::new(Mutex::new(options.active));
        let options = Arc::new(options);
        let stop = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let events_tx_weak = events_tx.downgrade();
        let (finished_tx, finished) = std::sync::mpsc::channel();

        // Drop duplicate ids first, keeping the first occurrence, because
        // `FleetState::new` does exactly that; both must agree or an event
        // would address a host the state does not have (or worse, the wrong
        // one). It has to happen *before* the active host is chosen: for
        // `[a(disabled), a(enabled)]` the kept `a` is the disabled one, so
        // "the first enabled spec" read from the caller's list would name a
        // host this connector never opens while the state renders another —
        // and every frame from the state's host would be dropped as inactive.
        let mut kept: Vec<HostSpec> = Vec::with_capacity(specs.len());
        for spec in specs {
            if kept.iter().any(|host| host.id == spec.id) {
                tracing::warn!(host = %spec.id, "ignoring a duplicate fleet host id");
                continue;
            }
            kept.push(spec);
        }

        // The first enabled host is active, matching `FleetState::new`, so the
        // two agree about which host is being rendered.
        let active_host = kept
            .iter()
            .find(|spec| spec.enabled)
            .map(|spec| spec.id.clone());

        let mut hosts: Vec<HostLink> = Vec::with_capacity(kept.len());
        let mut supervisors = 0usize;
        for spec in kept {
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
                active_geometry: Arc::clone(&active_geometry),
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
            events: Some(events),
            events_tx: events_tx_weak,
            options,
            active_host: Mutex::new(active_host),
            active_geometry,
            stop,
            finished,
            supervisors,
        }
    }

    /// The merged event stream.
    ///
    /// Closes once every supervisor has exited, which is how a consumer knows
    /// no host can produce another event. `None` once [`Self::take_events`]
    /// handed the receiver to the caller.
    pub fn events(&mut self) -> Option<&mut mpsc::Receiver<FleetEvent>> {
        self.events.as_mut()
    }

    /// Detach the event stream so the caller can own it.
    ///
    /// A `select!` loop needs the receiver by value while it still calls
    /// [`Self::send`] on the connector; borrowing both at once would not
    /// compile. The second call returns `None`.
    ///
    /// The caller then owns closing it: [`Self::shutdown`] can no longer wake
    /// a supervisor parked on a full channel, so **drop the receiver before
    /// calling `shutdown`**.
    // Called by the fleet console (E2 PR 3/4), which owns the loop.
    #[allow(dead_code)]
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<FleetEvent>> {
        self.events.take()
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
            if let Err(error) = self.announce(previous, false) {
                tracing::warn!(host = %previous.id, error = %error, "failed to resize a fleet host");
            }
        }
        if let Some(next) = host.and_then(|id| self.link(id)) {
            next.active.store(true, Ordering::Release);
            if let Err(error) = self.announce(next, true) {
                tracing::warn!(host = %next.id, error = %error, "failed to resize a fleet host");
            }
        }
        Ok(())
    }

    /// Follow the console terminal: announce `geometry` to whichever host is
    /// active now, and use it for whichever host becomes active later.
    ///
    /// Called on every terminal resize. A host that is *not* connected needs
    /// nothing: the supervisor reads the same cell when it handshakes, so a
    /// host that drops and reconnects while active comes back at the size the
    /// console has now — not the one it started with.
    ///
    /// The geometry is adopted whether or not it can be delivered, so an
    /// `Err` only ever means the write to the *current* active host failed —
    /// a host-local fact (that connection is already dropped and its
    /// supervisor is reconnecting, at this geometry). Callers log it; it is
    /// never a reason to stop the console.
    // Called by the fleet console (E2 PR 3/4) on `ClientLoopEvent::Resize`.
    #[allow(dead_code)]
    pub fn set_active_geometry(&self, geometry: ActiveGeometry) -> Result<(), HostSendError> {
        // Lock order: active_host → link state → active_geometry. Holding the
        // active-host lock serializes this against `set_active`, so a resize
        // racing a switch cannot leave the two hosts at swapped sizes.
        let active = lock(&self.active_host);
        let Some(link) = active.as_ref().and_then(|id| self.link(id)) else {
            *lock(&self.active_geometry) = geometry;
            return Ok(());
        };
        let mut state = lock(&link.state);
        *lock(&self.active_geometry) = geometry;
        if state.stream.is_none() || state.announced == Some(geometry) {
            return Ok(());
        }
        state.announced = Some(geometry);
        write_locked(&mut state, &geometry.resize_message())
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
        match self.events.as_mut() {
            Some(events) => events.close(),
            // The caller took the receiver: dropping it is what closes the
            // channel, and it is documented to do that before shutting down.
            // If it still holds it, the bounded wait below detaches instead of
            // blocking here.
            None => tracing::debug!("fleet shutdown with a detached event receiver"),
        }
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

    /// Tell one host the geometry it should render at, if it is connected.
    ///
    /// `active` picks which geometry that is, and it is read *under the host's
    /// link lock* so an activation racing a reconnect resolves to exactly one
    /// order. A disconnected host needs nothing: its next handshake reads the
    /// same values and announces them in the hello.
    fn announce(&self, link: &HostLink, active: bool) -> Result<(), HostSendError> {
        let mut state = lock(&link.state);
        if state.stream.is_none() {
            return Ok(());
        }
        let geometry = if active {
            *lock(&self.active_geometry)
        } else {
            inactive_geometry(&self.options.handshake)
        };
        if state.announced == Some(geometry) {
            return Ok(());
        }
        state.announced = Some(geometry);
        write_locked(&mut state, &geometry.resize_message())
    }

    /// The geometry `link` is (or would next be) announced at.
    fn geometry_for(&self, link: &HostLink) -> ActiveGeometry {
        if link.active.load(Ordering::Acquire) {
            *lock(&self.active_geometry)
        } else {
            inactive_geometry(&self.options.handshake)
        }
    }

    /// Record `geometry` as announced to this host, and — when the host is the
    /// active one — as the connector's own active geometry, so a later
    /// activation, a reconnect and the console cannot disagree about the size
    /// the active host renders at. Returns the resize to write.
    fn adopt_geometry(
        &self,
        link: &HostLink,
        state: &mut MutexGuard<'_, HostLinkState>,
        geometry: ActiveGeometry,
    ) -> ClientMessage {
        if link.active.load(Ordering::Acquire) {
            *lock(&self.active_geometry) = geometry;
        }
        state.announced = Some(geometry);
        geometry.resize_message()
    }

    fn close_connections(&self) {
        for link in &self.hosts {
            let mut state = lock(&link.state);
            state.announced = None;
            for stream in [state.stream.take(), state.pending.take()]
                .into_iter()
                .flatten()
            {
                #[cfg(unix)]
                {
                    // Half-closing makes the server hang up, which unblocks the
                    // reader thread. Windows named pipes have no equivalent; the
                    // thread there exits on its next message or with the process.
                    if let Err(error) = shutdown_stream_write(&stream) {
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
                let geometry = ActiveGeometry {
                    surface: surface_size,
                    ..self.geometry_for(link)
                };
                self.adopt_geometry(link, &mut state, geometry)
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
                // A raw resize is still a resize: the bookkeeping is what keeps
                // a later activation from skipping a needed resize, and what
                // keeps a reconnecting active host at the size the console is
                // actually showing.
                ClientMessage::ClientShellResize {
                    cell_width_px,
                    cell_height_px,
                    surface_size,
                    pixel_mouse,
                } => self.adopt_geometry(
                    link,
                    &mut state,
                    ActiveGeometry {
                        surface: surface_size,
                        cell_width_px,
                        cell_height_px,
                        pixel_mouse,
                    },
                ),
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

/// The geometry every *inactive* host is announced at: the hello's own values.
///
/// One place, so the hello a supervisor sends and the resize an activation
/// sends cannot describe two different inactive hosts.
fn inactive_geometry(handshake: &HandshakeParams) -> ActiveGeometry {
    ActiveGeometry {
        surface: handshake.surface_size,
        cell_width_px: handshake.cell_width_px,
        cell_height_px: handshake.cell_height_px,
        pixel_mouse: handshake.pixel_mouse,
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
            state.announced = None;
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
    active_geometry: Arc<Mutex<ActiveGeometry>>,
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
        // The active host handshakes at the console's *current* geometry, not
        // the one the connector was started with: that is what makes a
        // reconnect of the active host come back at the right size.
        let wanted = self.wanted_geometry();
        params.surface_size = wanted.surface;
        params.cell_width_px = wanted.cell_width_px;
        params.cell_height_px = wanted.cell_height_px;
        params.pixel_mouse = wanted.pixel_mouse;
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
            state.announced = Some(wanted);
            state.lane = EndpointLane::with_timeout(self.options.endpoint_timeout);
            let now = self.wanted_geometry();
            if now != wanted {
                state.announced = Some(now);
                if let Err(error) = write_locked(&mut state, &now.resize_message()) {
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
            state.announced = None;
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

    /// The geometry this host should be at right now.
    ///
    /// Read under the host's link lock wherever a stream is being published,
    /// so an activation that runs during a handshake either finds no stream
    /// (and this re-check sends the resize) or finds one (and sent it itself).
    fn wanted_geometry(&self) -> ActiveGeometry {
        if self.is_active() {
            *lock(&self.active_geometry)
        } else {
            inactive_geometry(&self.options.handshake)
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

/// Half-close the write side of a unix-socket stream.
///
/// Upstream removed its `ipc::shutdown_local_stream_write` helper in #3670;
/// the fleet is its only remaining caller, so it lives here.
#[cfg(unix)]
fn shutdown_stream_write(stream: &LocalStream) -> io::Result<()> {
    match stream {
        LocalStream::UdSocket(stream) => stream.inner().shutdown(std::net::Shutdown::Write),
    }
}

#[cfg(all(test, unix))]
pub(crate) mod test_support {
    //! A real endpoint server, minus herdr, for tests that must drive the
    //! connector over a real socket.
    //!
    //! Lives here rather than in one test module because the fleet console
    //! (`src/client/fleet.rs`, E2 PR 3/4) has to test its own routing against
    //! the same fake: two hosts, one active, and an assertion about which of
    //! them received a write. Unix-only for the same reason the connector
    //! tests are: it binds a local socket by path and half-closes it.

    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;

    use crate::fleet::hosts::HostKind;
    use crate::fleet::state::FleetState;
    use crate::fleet::transport::LocalTransport;
    use crate::ipc::{bind_local_listener, connect_local_stream, LocalListener};
    use crate::protocol::endpoint::{
        EndpointServerWelcome, ENDPOINT_SNAPSHOT_KIND, ENDPOINT_WELCOME_KIND,
    };
    use crate::protocol::{FrameData, PaneSurfaceFrame, SurfaceGraphicsScene};
    use interprocess::local_socket::traits::Listener as _;

    /// The frozen generation-1 snapshot the endpoint contract pins.
    pub(crate) const FROZEN_SNAPSHOT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/endpoint-snapshot-v1.json"
    ));

    pub(crate) fn scratch_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("herdr-fleet-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    pub(crate) fn snapshot(boot_id: &str, revision: u64) -> ClientShellSnapshot {
        let mut snapshot: ClientShellSnapshot =
            serde_json::from_str(FROZEN_SNAPSHOT).expect("frozen snapshot decodes");
        snapshot.boot_id = boot_id.to_string();
        snapshot.revision = revision;
        snapshot
    }

    pub(crate) fn snapshot_message(snapshot: &ClientShellSnapshot) -> ServerMessage {
        ServerMessage::EndpointControl {
            kind: ENDPOINT_SNAPSHOT_KIND.to_string(),
            data: serde_json::to_string(snapshot).expect("snapshot encodes"),
        }
    }

    pub(crate) fn surface_message(boot_id: &str, surface_revision: u64) -> ServerMessage {
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
    pub(crate) enum Behaviour {
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
    pub(crate) struct FakeHost {
        pub(crate) socket: PathBuf,
        received: Arc<Mutex<Vec<ClientMessage>>>,
        connections: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
    }

    impl FakeHost {
        pub(crate) fn start(dir: &Path, name: &str, behaviour: Behaviour) -> Self {
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

        pub(crate) fn spec(&self, id: &str) -> HostSpec {
            HostSpec {
                id: HostId::new(id).expect("valid host id"),
                kind: HostKind::Local {
                    session: Some(id.to_string()),
                },
                enabled: true,
            }
        }

        pub(crate) fn received(&self) -> Vec<ClientMessage> {
            lock(&self.received).clone()
        }

        pub(crate) fn connections(&self) -> usize {
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

    pub(crate) fn accept_loop(
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

    pub(crate) fn serve_connection(
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
                capabilities: Vec::new(),
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
    pub(crate) fn answer_endpoint(stream: &mut LocalStream, boot_id: &str, request: &str) -> bool {
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

    pub(crate) fn fake_connector(
        hosts: &[(&str, &FakeHost)],
        options: FleetConnectorOptions,
    ) -> FleetConnector {
        let specs = hosts
            .iter()
            .map(|(id, host)| host.spec(id))
            .collect::<Vec<_>>();
        fake_connector_with_specs(hosts, specs, options)
    }

    /// A connector over `hosts`' sockets, driven by exactly `specs`.
    ///
    /// [`fake_connector`] derives one enabled spec per host; this takes the
    /// spec list itself, for the cases a fleet has to survive: a disabled
    /// host, or a caller that passed the same id twice.
    pub(crate) fn fake_connector_with_specs(
        hosts: &[(&str, &FakeHost)],
        specs: Vec<HostSpec>,
        options: FleetConnectorOptions,
    ) -> FleetConnector {
        let sockets: HashMap<HostId, PathBuf> = hosts
            .iter()
            .map(|(id, host)| (HostId::new(id).expect("valid host id"), host.socket.clone()))
            .collect();
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
    pub(crate) fn drain_until(
        connector: &mut FleetConnector,
        state: &mut FleetState,
        timeout: Duration,
        mut done: impl FnMut(&FleetState) -> bool,
    ) -> Vec<FleetEvent> {
        let deadline = Instant::now() + timeout;
        let mut other = Vec::new();
        while !done(state) && Instant::now() < deadline {
            let Some(events) = connector.events() else {
                break;
            };
            match events.try_recv() {
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

    pub(crate) fn connected_with_snapshot(state: &FleetState, id: &str) -> bool {
        let Ok(id) = HostId::new(id) else {
            return false;
        };
        state
            .host(&id)
            .is_some_and(|host| host.connection.is_connected() && host.snapshot.is_some())
    }

    pub(crate) fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    pub(crate) fn hello_surface(messages: &[ClientMessage]) -> Vec<ClientSurfaceSize> {
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

    /// The full geometry every hello in `messages` announced.
    pub(crate) fn hello_geometry(messages: &[ClientMessage]) -> Vec<ActiveGeometry> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::EndpointControl { kind, data }
                    if kind == crate::protocol::endpoint::ENDPOINT_HELLO_KIND =>
                {
                    serde_json::from_str::<crate::protocol::endpoint::EndpointClientHello>(data)
                        .ok()
                        .map(|hello| ActiveGeometry {
                            surface: hello.surface_size,
                            cell_width_px: hello.cell_width_px,
                            cell_height_px: hello.cell_height_px,
                            pixel_mouse: hello.pixel_mouse,
                        })
                }
                _ => None,
            })
            .collect()
    }

    /// The full geometry every resize in `messages` announced.
    pub(crate) fn resize_geometry(messages: &[ClientMessage]) -> Vec<ActiveGeometry> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::ClientShellResize {
                    cell_width_px,
                    cell_height_px,
                    surface_size,
                    pixel_mouse,
                } => Some(ActiveGeometry {
                    surface: *surface_size,
                    cell_width_px: *cell_width_px,
                    cell_height_px: *cell_height_px,
                    pixel_mouse: *pixel_mouse,
                }),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn resizes(messages: &[ClientMessage]) -> Vec<ClientSurfaceSize> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::ClientShellResize { surface_size, .. } => Some(*surface_size),
                _ => None,
            })
            .collect()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::test_support::*;
    use super::*;

    use crate::fleet::hosts::HostKind;
    use crate::fleet::state::{FleetState, HostConnection};
    use crate::fleet::transport::LocalTransport;

    /// The daemon options: passive hello, batch-mode ssh, everything else as
    /// `for_config` decided.
    #[test]
    fn the_daemon_options_are_passive_and_noninteractive() {
        let config: crate::config::Config =
            toml::from_str("[remote]\nmanage_ssh_config = false\n").expect("config parses");
        let daemon = FleetConnectorOptions::for_daemon(&config);
        assert!(daemon.ssh_noninteractive);
        assert!(!daemon.handshake.surface_active);
        assert_eq!(daemon.handshake.surface_size, INACTIVE_SURFACE);
        assert_eq!(daemon.manage_ssh_config, config.remote.manage_ssh_config);
        assert_eq!(daemon.max_frame_size, MAX_FRAME_SIZE);

        // Nothing but the ssh policy separates it from `for_config`.
        let cli = FleetConnectorOptions::for_config(&config);
        assert!(!cli.ssh_noninteractive);
        assert_eq!(
            FleetConnectorOptions {
                ssh_noninteractive: false,
                ..daemon
            }
            .handshake,
            cli.handshake
        );
    }

    /// The interactive default is what a CLI gets: `herdr fleet status` keeps
    /// ssh's own prompts and error output.
    #[test]
    fn ssh_is_interactive_by_default() {
        assert!(!FleetConnectorOptions::default().ssh_noninteractive);
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
            let events = connector.events()?;
            match events.try_recv() {
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
    /// An ssh host that cannot be reached must not disturb a healthy local
    /// host: the whole point of a per-host supervisor.
    #[cfg(unix)]
    #[test]
    fn a_failing_ssh_host_never_changes_a_local_host() {
        use crate::fleet::transport::ssh::fake_ssh::{ssh_env_lock, FakeSsh};

        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        // Every ssh command fails, so the ssh host loops connect → unavailable.
        let shim = FakeSsh::install_unreachable("connector-host-local");

        let dir = scratch_dir("host-local");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 3))]),
        );
        let ssh_spec = HostSpec {
            id: HostId::new("box").expect("valid host id"),
            kind: HostKind::Ssh {
                target: "unreachable-host".to_string(),
                session: Some("agents".to_string()),
            },
            enabled: true,
        };
        let specs = vec![ssh_spec.clone(), alpha.spec("alpha")];
        let mut state = FleetState::new(specs.clone());
        let socket = alpha.socket.clone();
        let mut connector = FleetConnector::start_with(
            specs,
            // `manage_ssh_config: false`: a test must never read the running
            // user's `~/.ssh/config` into a managed config or start a control
            // master.
            FleetConnectorOptions::default(),
            // The ssh host goes through the real `transport_for`; only the
            // local host is redirected at the fake endpoint's socket.
            Arc::new(
                move |spec: &HostSpec, options: &FleetConnectorOptions| match &spec.kind {
                    HostKind::Ssh { .. } => transport_for(spec, options),
                    HostKind::Local { .. } => {
                        Ok(Box::new(LocalTransport::with_socket(socket.clone()))
                            as Box<dyn HostTransport>)
                    }
                },
            ),
        );

        let alpha_id = HostId::new("alpha").expect("valid host id");
        let box_id = HostId::new("box").expect("valid host id");
        // Wait until the ssh host has failed at least twice, so the local host
        // has lived through more than one of its neighbour's failures.
        let mut ssh_failures = 0usize;
        let deadline = Instant::now() + Duration::from_secs(20);
        while ssh_failures < 2 && Instant::now() < deadline {
            let Some(events) = connector.events() else {
                break;
            };
            match events.try_recv() {
                Ok(FleetEvent::Host { host, event }) => {
                    if host == box_id && matches!(event, HostEvent::Unavailable { .. }) {
                        ssh_failures += 1;
                    }
                    state.apply(&host, event);
                    // The local host, once connected, never leaves.
                    if connected_with_snapshot(&state, "alpha") {
                        assert!(
                            state
                                .host(&alpha_id)
                                .is_some_and(|host| host.connection.is_connected()),
                            "the local host lost its connection when the ssh host failed"
                        );
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
            ssh_failures >= 2,
            "the ssh host must keep retrying, not stop after one failure"
        );
        assert!(
            connected_with_snapshot(&state, "alpha"),
            "the local host must be connected with a snapshot despite the ssh host"
        );
        assert_eq!(alpha.connections(), 1, "the local host reconnected");
        let trace = shim.trace();
        assert!(
            !trace.contains("remote-client-bridge") && !trace.contains("mkdir -p"),
            "an unreachable ssh host must not be bridged or installed to; trace: {trace}"
        );

        state.assert_invariants_for_test();
        connector.shutdown();
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
        let mut connector = fake_connector(
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
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());

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
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());

        let id = HostId::new("alpha").expect("valid host id");
        let mut retry_in = None;
        let mut attempts = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && !connected_with_snapshot(&state, "alpha") {
            let Some(events) = connector.events() else {
                break;
            };
            match events.try_recv() {
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
        let mut connector = fake_connector(
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
            active: ActiveGeometry {
                surface: active_surface,
                ..ActiveGeometry::default()
            },
            ..FleetConnectorOptions::default()
        };
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let mut state = FleetState::new(specs);
        let mut connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], options);

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
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
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
        let connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());

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
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
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
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
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
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
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
        let mut connector = fake_connector(&[("alpha", &alpha)], options);
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
        let connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
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
        let connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
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

    /// A console geometry set while one host is active reaches that host, and
    /// is the geometry the *next* host is activated at: one value, two paths.
    #[test]
    fn the_console_geometry_follows_the_active_host_across_a_switch() {
        let dir = scratch_dir("geometry");
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
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let mut state = FleetState::new(specs);
        let mut connector = fake_connector(
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

        // The console's terminal: a real cell size and pixel mouse, unlike the
        // read-only collector's zeroes.
        let console = ActiveGeometry {
            surface: ClientSurfaceSize {
                cols: 200,
                rows: 60,
            },
            cell_width_px: 9,
            cell_height_px: 19,
            pixel_mouse: true,
        };
        connector
            .set_active_geometry(console)
            .expect("the active host takes the console geometry");

        assert!(
            wait_for(Duration::from_secs(2), || resize_geometry(
                &alpha.received()
            ) == vec![console]),
            "the active host must be resized to the console geometry: {:?}",
            resize_geometry(&alpha.received())
        );
        // Setting the same geometry again is not a second resize.
        connector
            .set_active_geometry(console)
            .expect("an unchanged geometry is accepted");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            resize_geometry(&alpha.received()),
            vec![console],
            "an unchanged geometry must not be announced twice"
        );
        assert!(
            resize_geometry(&beta.received()).is_empty(),
            "an inactive host must not hear the console geometry: {:?}",
            resize_geometry(&beta.received())
        );

        let beta_id = HostId::new("beta").expect("valid host id");
        connector
            .set_active(Some(&beta_id))
            .expect("beta can be activated");
        assert!(
            wait_for(Duration::from_secs(2), || resize_geometry(&beta.received())
                == vec![console]),
            "the newly active host must be sized to the console geometry, not the start one: {:?}",
            resize_geometry(&beta.received())
        );
        let inactive = ActiveGeometry::default();
        assert!(
            wait_for(Duration::from_secs(2), || resize_geometry(
                &alpha.received()
            ) == vec![console, inactive]),
            "the old active host must go back to the inactive geometry: {:?}",
            resize_geometry(&alpha.received())
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The E1 gap this PR closes: a host that drops while active used to
    /// re-handshake at the geometry the connector was *started* with, so the
    /// console came back to a pane sized for a terminal nobody had any more.
    #[test]
    fn an_active_host_reconnects_at_the_latest_console_geometry() {
        let dir = scratch_dir("reconnect-geometry");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::DropFirstConnection(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let start = ActiveGeometry {
            surface: ClientSurfaceSize {
                cols: 100,
                rows: 30,
            },
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
        };
        let specs = vec![alpha.spec("alpha")];
        let mut state = FleetState::new(specs);
        let mut connector = fake_connector(
            &[("alpha", &alpha)],
            FleetConnectorOptions {
                active: start,
                ..FleetConnectorOptions::default()
            },
        );
        let id = HostId::new("alpha").expect("valid host id");

        // The first connection is dropped right after the welcome.
        assert!(
            wait_for(Duration::from_secs(5), || alpha.connections() == 1),
            "the host must be reached once"
        );
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

        // The console resized while its only host was down.
        let resized = ActiveGeometry {
            surface: ClientSurfaceSize {
                cols: 180,
                rows: 50,
            },
            cell_width_px: 10,
            cell_height_px: 21,
            pixel_mouse: true,
        };
        connector
            .set_active_geometry(resized)
            .expect("a disconnected active host is not an error");

        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );
        assert!(
            wait_for(Duration::from_secs(2), || hello_geometry(&alpha.received())
                == vec![start, resized]),
            "the reconnect must handshake at the latest console geometry: {:?}",
            hello_geometry(&alpha.received())
        );
        assert!(
            resize_geometry(&alpha.received()).is_empty(),
            "the hello already carried the size; no extra resize is needed: {:?}",
            resize_geometry(&alpha.received())
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A raw `ClientShellResize` (what the console's own loop writes on a
    /// terminal resize) teaches the connector the same geometry, so a later
    /// reconnect or activation cannot disagree with what the console showed.
    #[test]
    fn a_raw_resize_to_the_active_host_updates_the_console_geometry() {
        let dir = scratch_dir("raw-resize");
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
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let mut state = FleetState::new(specs);
        let mut connector = fake_connector(
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

        let typed = ActiveGeometry {
            surface: ClientSurfaceSize {
                cols: 132,
                rows: 43,
            },
            cell_width_px: 7,
            cell_height_px: 15,
            pixel_mouse: true,
        };
        let alpha_id = HostId::new("alpha").expect("valid host id");
        connector
            .send(
                &alpha_id,
                HostCommand::Raw(Box::new(typed.resize_message())),
            )
            .expect("a raw resize reaches the active host");

        let beta_id = HostId::new("beta").expect("valid host id");
        connector
            .set_active(Some(&beta_id))
            .expect("beta can be activated");
        assert!(
            wait_for(Duration::from_secs(2), || resize_geometry(&beta.received())
                == vec![typed]),
            "activation must use the geometry the raw resize announced: {:?}",
            resize_geometry(&beta.received())
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The console owns the receiver, so it also owns closing it: `shutdown`
    /// must still return promptly once it has been dropped.
    #[test]
    fn taken_events_are_handed_out_once_and_shutdown_still_returns() {
        let dir = scratch_dir("take-events");
        let mut messages = vec![snapshot_message(&snapshot("boot-alpha", 1))];
        messages.extend(
            (1..=(EVENT_CHANNEL_CAPACITY as u64 + 50))
                .map(|revision| surface_message("boot-alpha", revision)),
        );
        let alpha = FakeHost::start(&dir, "alpha", Behaviour::Serve(messages));
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());

        let mut events = connector.take_events().expect("the receiver is attached");
        assert!(
            connector.take_events().is_none(),
            "the receiver is handed out exactly once"
        );
        assert!(
            connector.events().is_none(),
            "a detached receiver cannot also be borrowed"
        );
        // It is a working receiver, not an empty one.
        let first = events.blocking_recv().expect("the host reports itself");
        assert!(matches!(first, FleetEvent::Host { .. }));

        // Let the supervisor park on a full channel, then do what the console
        // does on the way out: drop the receiver, then shut down.
        assert!(
            wait_for(Duration::from_secs(5), || alpha.connections() == 1),
            "the host must connect"
        );
        std::thread::sleep(Duration::from_millis(500));
        drop(events);
        let started = Instant::now();
        connector.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown must join once the caller dropped the receiver: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same, with the host in backoff rather than mid-write.
    #[test]
    fn shutdown_returns_with_taken_events_and_a_host_mid_backoff() {
        let dir = scratch_dir("take-events-backoff");
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
        let mut events = connector.take_events().expect("the receiver is attached");
        let mut unavailable = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !unavailable && Instant::now() < deadline {
            match events.blocking_recv() {
                Some(FleetEvent::Host {
                    event: HostEvent::Unavailable { .. },
                    ..
                }) => unavailable = true,
                Some(_) => {}
                None => break,
            }
        }
        assert!(unavailable, "the host must reach a backoff");
        drop(events);

        let started = Instant::now();
        connector.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown must not wait out the backoff: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `shutdown` must stay bounded even when the console did the wrong thing
    /// and kept the receiver: the supervisors it cannot wake are detached, not
    /// waited on forever.
    #[test]
    fn shutdown_returns_when_the_caller_still_holds_the_taken_receiver() {
        let dir = scratch_dir("held-events");
        let mut messages = vec![snapshot_message(&snapshot("boot-alpha", 1))];
        messages.extend(
            (1..=(EVENT_CHANNEL_CAPACITY as u64 + 50))
                .map(|revision| surface_message("boot-alpha", revision)),
        );
        let alpha = FakeHost::start(&dir, "alpha", Behaviour::Serve(messages));
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        let events = connector.take_events().expect("the receiver is attached");
        assert!(
            wait_for(Duration::from_secs(5), || alpha.connections() == 1),
            "the host must connect"
        );
        // The supervisor is parked on a full channel and only the receiver can
        // release it — and the caller is still holding it.
        std::thread::sleep(Duration::from_millis(500));

        let started = Instant::now();
        connector.shutdown();
        let elapsed = started.elapsed();
        assert!(
            elapsed < SHUTDOWN_JOIN_TIMEOUT + Duration::from_secs(2),
            "shutdown must detach rather than block on a receiver it cannot close: {elapsed:?}"
        );
        drop(events);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The console can resize before it has picked a host — or while it is
    /// pointed at none. That geometry is not lost: it is what the next
    /// activation announces.
    #[test]
    fn a_geometry_set_with_no_active_host_is_used_by_the_next_activation() {
        let dir = scratch_dir("geometry-no-active");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        // A starting console geometry, so deactivation is a visible resize.
        let start = ActiveGeometry {
            surface: ClientSurfaceSize {
                cols: 200,
                rows: 60,
            },
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
        };
        let specs = vec![alpha.spec("alpha")];
        let mut state = FleetState::new(specs);
        let mut connector = fake_connector(
            &[("alpha", &alpha)],
            FleetConnectorOptions {
                active: start,
                ..FleetConnectorOptions::default()
            },
        );
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );

        connector
            .set_active(None)
            .expect("the fleet can point at no host");
        let inactive = ActiveGeometry::default();
        assert!(
            wait_for(Duration::from_secs(2), || resize_geometry(
                &alpha.received()
            ) == vec![inactive]),
            "the deactivated host must go back to the inactive geometry: {:?}",
            resize_geometry(&alpha.received())
        );

        let console = ActiveGeometry {
            surface: ClientSurfaceSize {
                cols: 160,
                rows: 48,
            },
            cell_width_px: 9,
            cell_height_px: 18,
            pixel_mouse: true,
        };
        connector
            .set_active_geometry(console)
            .expect("a fleet with no active host accepts a geometry");
        assert!(
            connector.active_host().is_none(),
            "the geometry must not activate anything"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            resize_geometry(&alpha.received()),
            vec![inactive],
            "a host nobody is looking at must not hear the console geometry"
        );

        let id = HostId::new("alpha").expect("valid host id");
        connector
            .set_active(Some(&id))
            .expect("alpha can be activated");
        assert!(
            wait_for(Duration::from_secs(2), || resize_geometry(
                &alpha.received()
            ) == vec![inactive, console]),
            "activation must announce the geometry set while no host was active: {:?}",
            resize_geometry(&alpha.received())
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `FleetState::new` drops duplicate host ids keeping the *first*
    /// occurrence and only then picks the first enabled host as the active
    /// one. The connector has to do it in the same order: reading "the first
    /// enabled spec" from the caller's list instead would name a host the
    /// connector never opens, while the state renders another whose frames the
    /// connector would drop as inactive.
    #[test]
    fn a_duplicate_host_id_leaves_the_connector_and_the_state_on_one_active_host() {
        let dir = scratch_dir("duplicate-ids");
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
        let mut disabled_alpha = alpha.spec("alpha");
        disabled_alpha.enabled = false;
        // The kept `alpha` is the disabled one, so beta is the first enabled
        // host that survives deduplication.
        let specs = vec![disabled_alpha, alpha.spec("alpha"), beta.spec("beta")];
        let active_surface = ClientSurfaceSize {
            cols: 200,
            rows: 60,
        };
        let options = FleetConnectorOptions {
            active: ActiveGeometry {
                surface: active_surface,
                ..ActiveGeometry::default()
            },
            ..FleetConnectorOptions::default()
        };
        let mut state = FleetState::new(specs.clone());
        let mut connector =
            fake_connector_with_specs(&[("alpha", &alpha), ("beta", &beta)], specs, options);

        assert_eq!(
            connector.active_host().as_ref(),
            state.active_host(),
            "the connector and the state must agree on the active host"
        );
        assert_eq!(
            connector.active_host(),
            HostId::new("beta").ok(),
            "the surviving enabled host is the active one"
        );

        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "beta"),
        );
        assert!(
            wait_for(Duration::from_secs(2), || hello_surface(&beta.received())
                == vec![active_surface]),
            "the active host must handshake at the active geometry: {:?}",
            hello_surface(&beta.received())
        );
        assert_eq!(
            alpha.connections(),
            0,
            "the kept duplicate is disabled, so it is never opened"
        );
        connector.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
