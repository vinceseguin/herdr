//! The Fleet console's client-loop state (fork).
//!
//! One console, one active host: the shell keeps the active host's snapshot and
//! surface exactly as the single-host client does, and every other host lives
//! in [`FleetState`] until the sidebar (E2 PR 5) reads it. This module is the
//! translation seam between the connector's host-tagged events and the events
//! the client loop already knows how to handle.
//!
//! Nothing here guesses a host. An event is compared against the one active
//! [`HostId`] the loop set, and anything else is dropped with a `debug!` that
//! names both — a frame attributed to the wrong host would be a silent
//! mis-render, and an input routed by it a silent mis-type.
//!
//! E2 PR 3 lands the seam; PR 4 constructs it (`run_fleet`), PR 5 renders the
//! host groups, PR 7 targets notifications and PR 8 shows a reconnect notice.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use tracing::{debug, warn};

use crate::config::Config;
use crate::fleet::connector::{ActiveGeometry, FleetConnector, FleetConnectorOptions, FleetEvent};
use crate::fleet::handshake::HandshakeParams;
use crate::fleet::hosts::{resolve_hosts, HostId, HostSpec};
use crate::fleet::sidebar::FleetSidebarModel;
use crate::fleet::state::{FleetChange, FleetState, HostConnection, HostEvent};
use crate::protocol::{ClientShellSnapshot, ClientSurfaceSize, ServerMessage};

use super::endpoint_commands::EndpointCommands;
use super::errors::ClientError;
use super::link::{ClientLink, FleetLink, ServerLink};
use super::shell::{
    ClientShellAction, ClientShellKeybindingSource, FleetFocusTarget, FleetShellAction,
};
use super::ClientState;

/// What one fleet event means to the client loop.
pub(super) enum Translated {
    /// Hand this to the loop's existing `ServerMessage` handling, unchanged.
    Server(Box<ServerMessage>),
    /// The active host replaced its projection.
    Snapshot {
        snapshot: Box<ClientShellSnapshot>,
        changes: Vec<FleetChange>,
    },
    /// The active host said which endpoint methods it serves. The shell gates
    /// commands on them, so they are the active host's, never the fleet's.
    EndpointMethods {
        methods: Vec<String>,
        changes: Vec<FleetChange>,
    },
    /// One endpoint request finished, already reassembled by the connector.
    EndpointResponse {
        request_id: String,
        result: Result<Vec<u8>, String>,
    },
    /// Fleet-model changes only: nothing for the shell to draw yet.
    Changes(Vec<FleetChange>),
    /// Not this host's business: dropped, and logged.
    Dropped,
}

/// Everything the console keeps that the single-host client does not.
pub(super) struct FleetClientState {
    /// Every host's connection and projection, merged.
    pub(super) state: FleetState,
    /// Shared with the loop's [`super::link::FleetLink`]; the receiver half is
    /// owned by the loop through `FleetConnector::take_events`. `Rc` for the
    /// reason `FleetLink` gives: the connector is not `Sync`, and both owners
    /// live on the client loop's one thread. The console's exit path reclaims
    /// it with `Rc::try_unwrap` for `shutdown` once the link is dropped.
    pub(super) connector: Rc<FleetConnector>,
    /// The one host whose surface the shell is showing and whose ids its input
    /// addresses.
    pub(super) active: HostId,
    /// A switch asked for, waiting for the new host's first full surface.
    pub(super) pending_switch: Option<HostId>,
    /// The sidebar rows, rebuilt here and handed to the shell. Owning it on
    /// this side means a rebuild can be compared against what the shell is
    /// already drawing before it costs a repaint.
    pub(super) sidebar: FleetSidebarModel,
    /// Host groups the user folded away. A render decision, so it lives with
    /// the console rather than in [`FleetState`].
    pub(super) collapsed: HashSet<HostId>,
    /// What to focus once the host being switched to has a snapshot.
    pub(super) pending_focus: Option<FleetFocusTarget>,
}

impl FleetClientState {
    /// Turns one connector event into something the client loop can act on.
    pub(super) fn translate(&mut self, event: FleetEvent) -> Translated {
        // The switch is over when the host being switched to has drawn: that
        // is the first frame the console shows from the new machine.
        if let FleetEvent::Surface { host, .. } = &event {
            if self.pending_switch.as_ref() == Some(host) && *host == self.active {
                self.pending_switch = None;
            }
        }
        translate_event(&mut self.state, &self.active, event)
    }

    /// Follow the console terminal.
    ///
    /// The active host is resized by the loop's own `ClientShellResize` write,
    /// which the connector bookkeeps — but only while that host is connected.
    /// This tells the connector unconditionally, so a host that is down right
    /// now re-handshakes at the size the console is actually showing.
    pub(super) fn announce_geometry(
        &self,
        surface: ClientSurfaceSize,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
    ) {
        let geometry = ActiveGeometry {
            surface,
            cell_width_px,
            cell_height_px,
            pixel_mouse,
        };
        if let Err(error) = self.connector.set_active_geometry(geometry) {
            // Host-local: that connection is already gone and its supervisor
            // will reconnect at this geometry. Never a reason to stop.
            debug!(host = %self.active, error = %error, "could not resize the active fleet host");
        }
    }
}

/// Rebuilds the sidebar rows after a fleet change and installs them.
///
/// Returns whether the shell's view actually changed, so the caller repaints
/// only then: an inactive host bumping a revision is a change to
/// [`FleetState`] and, most of the time, to no visible row — and a repaint is
/// a full console compose.
pub(super) fn apply_changes(state: &mut ClientState, changes: Vec<FleetChange>) -> bool {
    if changes.is_empty() {
        return false;
    }
    rebuild_sidebar(state)
}

/// Rebuild the model from the current fleet state and install it if it differs.
fn rebuild_sidebar(state: &mut ClientState) -> bool {
    let (Some(fleet), Some(shell)) = (state.fleet.as_mut(), state.shell.as_mut()) else {
        return false;
    };
    // The live agent-panel preference, which the user can toggle by clicking
    // the panel's sort label; `crate::fleet::sidebar` is pure and cannot read
    // it for itself.
    let sort = shell.agent_panel_sort();
    fleet.sidebar.rebuild(&fleet.state, &fleet.collapsed, sort);
    if shell.fleet_sidebar_matches(&fleet.sidebar, &fleet.active, fleet.pending_switch.as_ref()) {
        return false;
    }
    shell.fleet_sidebar_update(
        fleet.sidebar.clone(),
        fleet.active.clone(),
        fleet.pending_switch.clone(),
    );
    true
}

/// Keep the header's "switching…" marker in step with the console's state.
///
/// A switch ends when the new host's first surface arrives, which is a plain
/// `ServerMessage` to the rest of the loop; this is where the header stops
/// saying it is waiting. Returns whether anything changed.
pub(super) fn sync_switching_notice(state: &mut ClientState) -> bool {
    let Some(pending) = state
        .fleet
        .as_ref()
        .map(|fleet| fleet.pending_switch.clone())
    else {
        return false;
    };
    state
        .shell
        .as_mut()
        .is_some_and(|shell| shell.set_fleet_switching(pending))
}

/// Compose and present, after something that changed the console's chrome.
pub(super) fn present(state: &mut ClientState) {
    let Some(frame) = state
        .shell
        .as_mut()
        .and_then(|shell| shell.compose(state.reported_size.0, state.reported_size.1))
    else {
        return;
    };
    state.present_frame(frame);
}

/// What one fleet action did to the console.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FleetActionOutcome {
    /// The chrome changed: recompose rather than present a frame composed
    /// before the action.
    pub(super) repaint: bool,
    /// The routing target moved. Anything the shell produced *before* this —
    /// in the same input batch — named the previous host's ids and must not
    /// be sent to the new one.
    pub(super) switched: bool,
}

/// Recompose after a terminal resize the active host cannot answer.
///
/// A resize drops the pane surface *and the hit map* and waits for the server
/// to render at the new size (`invalidate_pane_surface`). A host that is down,
/// or one a switch is still waiting on, never will — and until something else
/// repainted, the console would have no clickable row to leave it by. A
/// connected host keeps the single-host behaviour: its next surface redraws.
pub(super) fn present_after_resize(state: &mut ClientState) {
    let waiting_on_host = state.fleet.as_ref().is_some_and(|fleet| {
        fleet.pending_switch.is_some()
            || !fleet
                .state
                .host(&fleet.active)
                .is_some_and(|host| matches!(host.connection, HostConnection::Connected { .. }))
    });
    if waiting_on_host {
        present(state);
    }
}

/// Handle one fleet action the shell asked the loop for.
pub(super) fn handle_shell_action(
    state: &mut ClientState,
    write_stream: &mut ServerLink,
    endpoint_commands: &mut EndpointCommands,
    action: FleetShellAction,
) -> Result<FleetActionOutcome, ClientError> {
    match action {
        FleetShellAction::SwitchHost { host, then_focus } => {
            switch_host(state, write_stream, endpoint_commands, host, then_focus)
        }
        FleetShellAction::ToggleCollapsed(host) => {
            let Some(fleet) = state.fleet.as_mut() else {
                return Ok(FleetActionOutcome::default());
            };
            if !fleet.collapsed.remove(&host) {
                fleet.collapsed.insert(host);
            }
            Ok(FleetActionOutcome {
                repaint: rebuild_sidebar(state),
                switched: false,
            })
        }
        // Agent order inside every group is a function of this preference, so
        // every group's rows are stale, not just the active host's.
        FleetShellAction::SortChanged => Ok(FleetActionOutcome {
            repaint: rebuild_sidebar(state),
            switched: false,
        }),
    }
}

/// Point the console at another host.
///
/// The one place the active host changes. Everything that names a host moves
/// together — the connector (which streams frames for exactly one host), the
/// write link, the fleet state and the shell — because a half-applied switch
/// is precisely a mis-route: frames from one machine, keystrokes to another.
///
/// The link is redirected *before* the shell is reset, so input typed during
/// the switch reaches the new host, and the shell's ids are the new host's
/// from the same moment.
pub(super) fn switch_host(
    state: &mut ClientState,
    write_stream: &mut ServerLink,
    endpoint_commands: &mut EndpointCommands,
    host: HostId,
    then_focus: Option<FleetFocusTarget>,
) -> Result<FleetActionOutcome, ClientError> {
    let Some(fleet) = state.fleet.as_mut() else {
        debug!(%host, "ignoring a host switch outside a fleet console");
        return Ok(FleetActionOutcome::default());
    };
    if !switch_target_allowed(fleet, &host) {
        return Ok(FleetActionOutcome::default());
    }

    // The connector decides what a host's surface is; tell it the console's
    // current geometry before it activates the new host, so the activation
    // resize is the size the pane area actually has.
    let geometry = ActiveGeometry {
        surface: state
            .shell
            .as_ref()
            .map(|shell| shell.surface_size(state.reported_size.0, state.reported_size.1))
            .unwrap_or(ClientSurfaceSize {
                cols: state.reported_size.0.max(1),
                rows: state.reported_size.1.max(1),
            }),
        cell_width_px: state.reported_cell_size.0,
        cell_height_px: state.reported_cell_size.1,
        pixel_mouse: state.pixel_geometry_exact,
    };
    let Some(fleet) = state.fleet.as_mut() else {
        return Ok(FleetActionOutcome::default());
    };
    let changes = retarget_host(
        fleet,
        write_stream,
        endpoint_commands,
        host.clone(),
        geometry,
    );
    fleet.pending_focus = then_focus;

    let snapshot = fleet
        .state
        .host(&host)
        .and_then(|host| host.snapshot.clone());
    let methods = fleet
        .state
        .host(&host)
        .and_then(|host| match &host.connection {
            HostConnection::Connected { methods, .. } => Some(methods.clone()),
            _ => None,
        });
    if let Some(shell) = state.shell.as_mut() {
        shell.reset_for_host_switch();
        shell.set_endpoint_methods(methods);
        if let Some(snapshot) = snapshot {
            shell.set_snapshot(snapshot);
        }
    }
    // The sidebar always changes (the active marker moved) and the pane area
    // now shows the switching notice: a frame composed before this call
    // describes the previous host.
    rebuild_sidebar(state);
    flush_pending_focus(state, write_stream, endpoint_commands)?;
    debug_assert!(
        !changes.is_empty(),
        "an allowed switch changes the active host"
    );
    Ok(FleetActionOutcome {
        repaint: true,
        switched: true,
    })
}

/// Whether this host may become the routing target at all.
///
/// A disabled host is listed in the sidebar (it is configuration the user can
/// see) but `FleetState::set_active_host` refuses it, so a switch that skipped
/// this check would move the connector and the link while the fleet state kept
/// naming the old host — the exact split the console must never have.
fn switch_target_allowed(fleet: &FleetClientState, host: &HostId) -> bool {
    if fleet.active == *host {
        debug!(%host, "the fleet console is already showing this host");
        return false;
    }
    match fleet.state.host(host) {
        Some(state) if state.spec.enabled => true,
        Some(_) => {
            warn!(%host, "refusing to switch to a host disabled in [fleet]");
            false
        }
        None => {
            warn!(%host, "refusing to switch to a host that is not configured");
            false
        }
    }
}

/// Move every routing target to `host`, together.
///
/// Split out of [`switch_host`] so the routing — which host the connector
/// streams, which host the link writes to, which host the fleet state calls
/// active, and the endpoint lane in between — is testable against fake hosts
/// without a terminal or a shell.
fn retarget_host(
    fleet: &mut FleetClientState,
    write_stream: &mut ServerLink,
    endpoint_commands: &mut EndpointCommands,
    host: HostId,
    geometry: ActiveGeometry,
) -> Vec<FleetChange> {
    if let Err(error) = fleet.connector.set_active_geometry(geometry) {
        debug!(%host, %error, "could not announce the console geometry before a switch");
    }
    if let Err(error) = fleet.connector.set_active(Some(&host)) {
        // Host-local: the old host may already be gone, or the new one not up
        // yet. The connector recorded the new active host either way, so the
        // console switches and the sidebar shows why the screen is empty.
        debug!(%host, %error, "the fleet connector could not resize on activation");
    }
    let previous = std::mem::replace(&mut fleet.active, host.clone());
    fleet.pending_switch = Some(host.clone());
    let changes = fleet.state.set_active_host(Some(host.clone()));
    // Redirect writes before anything else can be typed: from here every
    // keystroke addresses the new host.
    write_stream.set_active(host.clone());
    // An answer from the old host is dropped by `translate`, so a request in
    // flight would hold the single endpoint lane until its 60 s timeout.
    endpoint_commands.reset();
    debug!(from = %previous, to = %host, "the fleet console switched host");
    changes
}

/// Send the focus request a switch asked for, once its host has a projection.
///
/// A host that was already connected has one immediately; one that is still
/// connecting gets its focus when its first snapshot arrives.
pub(super) fn flush_pending_focus(
    state: &mut ClientState,
    write_stream: &mut ServerLink,
    endpoint_commands: &mut EndpointCommands,
) -> Result<(), ClientError> {
    let Some(fleet) = state.fleet.as_ref() else {
        return Ok(());
    };
    if fleet.pending_focus.is_none() {
        return Ok(());
    }
    // The shell's projection is the active host's by construction, but only
    // once it has installed one: focusing before that would address ids the
    // shell cannot resolve and the server has not announced.
    if !state
        .shell
        .as_ref()
        .is_some_and(super::shell::ClientShellState::has_snapshot)
    {
        return Ok(());
    }
    let Some(target) = state
        .fleet
        .as_mut()
        .and_then(|fleet| fleet.pending_focus.take())
    else {
        return Ok(());
    };
    let Some(shell) = state.shell.as_mut() else {
        return Ok(());
    };
    let outcome = shell.request_fleet_focus(&target);
    dispatch_focus_outcome(outcome, write_stream, endpoint_commands)
}

/// Dispatch the endpoint request a focus produced.
///
/// Deliberately not the loop's general action dispatcher: a focus can only
/// ever produce an endpoint request or a plain server message, and routing it
/// through the general path from inside that path would be re-entrant.
fn dispatch_focus_outcome(
    outcome: super::shell::ClientShellInput,
    write_stream: &mut ServerLink,
    endpoint_commands: &mut EndpointCommands,
) -> Result<(), ClientError> {
    for action in outcome.actions {
        match action {
            ClientShellAction::Endpoint { boot_id, request } => {
                endpoint_commands.enqueue(boot_id, request);
            }
            other => debug!(?other, "ignoring an unexpected action from a fleet focus"),
        }
    }
    for request in outcome.requests {
        super::write_to_server(write_stream, &request).map_err(ClientError::ConnectionLost)?;
    }
    endpoint_commands
        .send_next(write_stream)
        .map_err(ClientError::ConnectionLost)
}

/// The pure half of [`FleetClientState::translate`].
///
/// Split out so the routing decision — which host may reach the shell — is
/// testable without a connector, a socket or a terminal.
fn translate_event(state: &mut FleetState, active: &HostId, event: FleetEvent) -> Translated {
    match event {
        FleetEvent::Host { host, event } => translate_host_event(state, active, host, event),
        FleetEvent::Surface { host, frame } => {
            if host != *active {
                return dropped(&host, active, "surface");
            }
            Translated::Server(Box::new(ServerMessage::PaneSurface(*frame)))
        }
        FleetEvent::SurfacePatch { host, patch } => {
            if host != *active {
                return dropped(&host, active, "surface patch");
            }
            Translated::Server(Box::new(ServerMessage::PaneSurfacePatch(*patch)))
        }
        FleetEvent::ServerMessage { host, message } => {
            if host != *active {
                return dropped(&host, active, "server message");
            }
            Translated::Server(message)
        }
        // Every host may notify: which machine is asking is the point, so the
        // title carries the host. Targeting (clicking one, on another host) is
        // E2 PR 7.
        FleetEvent::Notification { host, notification } => {
            let mut notification = *notification;
            notification.title = format!("[{host}] {}", notification.title);
            if host != *active {
                // The shell resolves a notification's ids against the *active*
                // host's snapshot: a `w1:p1` on another machine would be
                // validated against, suppressed by, and — on click — focused
                // on this one. Until PR 7 targets across hosts, another
                // host's notification is informational only.
                notification.workspace_id = None;
                notification.tab_id = None;
                notification.pane_id = None;
            }
            Translated::Server(Box::new(ServerMessage::SemanticNotification(notification)))
        }
        FleetEvent::EndpointResponse {
            host,
            request_id,
            result,
        } => {
            if host != *active {
                return dropped(&host, active, "endpoint response");
            }
            Translated::EndpointResponse { request_id, result }
        }
    }
}

fn translate_host_event(
    state: &mut FleetState,
    active: &HostId,
    host: HostId,
    event: HostEvent,
) -> Translated {
    // The active host's projection has two readers: the shell draws it, and the
    // fleet model counts its agents for the sidebar. One clone, only for the
    // host the console is showing, and only when a projection actually changed.
    let for_shell = match (&event, host == *active) {
        (HostEvent::Snapshot(snapshot), true) => Some(ForShell::Snapshot(snapshot.clone())),
        // Endpoint methods are per host: the shell must gate its commands on
        // the machine it is showing, not on whatever answered last.
        (HostEvent::Connected { methods, .. }, true) => Some(ForShell::Methods(methods.clone())),
        _ => None,
    };
    let changes = state.apply(&host, event);
    match for_shell {
        Some(ForShell::Snapshot(snapshot)) => Translated::Snapshot { snapshot, changes },
        Some(ForShell::Methods(methods)) => Translated::EndpointMethods { methods, changes },
        None => Translated::Changes(changes),
    }
}

/// What the active host's own event gives the shell, beside the fleet changes.
enum ForShell {
    Snapshot(Box<ClientShellSnapshot>),
    Methods(Vec<String>),
}

fn dropped(host: &HostId, active: &HostId, kind: &'static str) -> Translated {
    debug!(%host, %active, kind, "dropping a fleet event from an inactive host");
    Translated::Dropped
}

// ---------------------------------------------------------------------------
// Opening the console
// ---------------------------------------------------------------------------

/// `herdr fleet` / `herdr --fleet`: the client-owned shell over N hosts.
///
/// Everything else about the launch — config, geometry, terminal setup, the
/// panic hook, the runtime, the termination handler, the exit codes — is the
/// single-host client's, so the console cannot drift away from it.
pub fn run_fleet() -> io::Result<()> {
    super::run_client_with_launch(super::ClientLaunch::fleet())
}

/// Marks a client launch as a Fleet console.
///
/// Deliberately empty: the hosts come from the config the launch path already
/// loads, and the geometry from the terminal it already reads. A preselected
/// host would live here.
pub(super) struct FleetLaunch;

/// The console terminal, as the connector needs to describe it to a host.
pub(super) struct ConsoleGeometry {
    /// What the *active* host renders into: the shell's pane area, not the
    /// whole terminal.
    pub(super) surface: ClientSurfaceSize,
    pub(super) cell_width_px: u32,
    pub(super) cell_height_px: u32,
    pub(super) pixel_mouse: bool,
    pub(super) mouse_capture: bool,
}

/// The hosts a console will open, or why it cannot open any.
///
/// An invalid `[fleet]` section is a user error the launch path reports and
/// exits on. A host that is merely *unreachable* is not: it is sidebar state,
/// and the console opens regardless.
pub(super) fn resolve_console_hosts(config: &Config) -> Result<Vec<HostSpec>, Vec<String>> {
    let specs = resolve_hosts(&config.fleet)?;
    if !specs.iter().any(|spec| spec.enabled) {
        return Err(vec![
            "no fleet hosts enabled in [fleet]; see docs/fork/fleet-core.md".to_string(),
        ]);
    }
    Ok(specs)
}

/// The console's keybinding source: always local, on every host.
///
/// Locked decision (g). `HERDR_REMOTE_KEYBINDINGS` describes one server's
/// profile; a console spanning N servers has no such thing, so it is ignored
/// loudly rather than applied to whichever host happens to be active.
pub(super) fn console_keybinding_source() -> ClientShellKeybindingSource {
    if std::env::var_os(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR).is_some() {
        warn!(
            "ignoring {}: the Fleet console uses local keybindings on every host",
            crate::remote::REMOTE_KEYBINDINGS_ENV_VAR
        );
    }
    ClientShellKeybindingSource::Local
}

/// Where the console's chrome preferences (sidebar width, collapse) live.
///
/// Not any host's socket: one console, one saved layout, whichever machine it
/// is currently showing. The path is only ever hashed into a state file name.
pub(super) fn console_preferences_key() -> PathBuf {
    crate::config::config_dir().join("fleet")
}

/// Start every host and hand the loop its link.
///
/// Runs *after* the terminal is set up (locked decision (i)): a host that
/// refuses, times out or is missing is that host's problem, never a reason to
/// refuse the console, so there is nothing left to bail out of cleanly.
pub(super) fn open_console(
    specs: Vec<HostSpec>,
    config: &Config,
    geometry: ConsoleGeometry,
) -> io::Result<(ClientLink, Console)> {
    // The ssh child inherits this process's stderr and would paint over the
    // TUI; from here until the console stops, it goes to the herdr log.
    let stderr = install_console_stderr();

    let handshake = HandshakeParams::for_client(
        geometry.cell_width_px,
        geometry.cell_height_px,
        geometry.pixel_mouse,
        geometry.mouse_capture,
    );
    let active = ActiveGeometry {
        surface: geometry.surface,
        cell_width_px: geometry.cell_width_px,
        cell_height_px: geometry.cell_height_px,
        pixel_mouse: geometry.pixel_mouse,
    };
    let options = FleetConnectorOptions::for_client(config, handshake, active);
    let state = FleetState::new(specs.clone());
    let connector = FleetConnector::start(specs, options);
    console_link(state, connector, stderr)
}

/// The link half of [`open_console`], with the connector already built.
///
/// Split out so the console's routing — which host the link addresses, which
/// events the loop is given — is testable against fake hosts.
fn console_link(
    mut state: FleetState,
    mut connector: FleetConnector,
    stderr: ConsoleStderr,
) -> io::Result<(ClientLink, Console)> {
    let events = match connector.take_events() {
        Some(events) => events,
        None => {
            connector.shutdown();
            return Err(io::Error::other(
                "the fleet connector handed out its event stream twice",
            ));
        }
    };
    // The connector picked the active host when it started (the first enabled
    // spec, after dropping duplicate ids). Read it back rather than picking
    // again: the link, the fleet state and the connector must name the same
    // machine, or a frame would be rendered from one host while input went to
    // another.
    let Some(active) = connector.active_host() else {
        drop(events);
        connector.shutdown();
        return Err(io::Error::other("no fleet host could be activated"));
    };
    state.set_active_host(Some(active.clone()));

    let connector = Rc::new(connector);
    let link = ClientLink {
        link: ServerLink::Fleet(FleetLink::new(Rc::clone(&connector), active.clone())),
        fleet_events: Some(events),
        fleet: Some(FleetClientState {
            state,
            connector: Rc::clone(&connector),
            active,
            pending_switch: None,
            sidebar: FleetSidebarModel::default(),
            collapsed: HashSet::new(),
            pending_focus: None,
        }),
    };
    Ok((
        link,
        Console {
            connector: Some(connector),
            _stderr: stderr,
        },
    ))
}

/// The console's lifetime, in one value.
///
/// Dropping it stops every host and restores stderr. It is held by the launch
/// path rather than the loop so that *every* way out runs it: a clean quit, a
/// detach, a termination signal, a terminal hangup — and a panic, which unwinds
/// through it after the panic hook has restored the terminal.
pub(super) struct Console {
    /// `None` once shut down. The loop holds the other references (the link and
    /// its own fleet state); both are gone by the time this drops.
    connector: Option<Rc<FleetConnector>>,
    /// Held for its `Drop`, which puts stderr back after the shutdown above.
    _stderr: ConsoleStderr,
}

impl Drop for Console {
    fn drop(&mut self) {
        if let Some(connector) = self.connector.take() {
            match Rc::try_unwrap(connector) {
                // The only path that unlinks an ssh host's forward socket and
                // waits for the supervisors (and so for their transports) to
                // stop. E1 constraint: it must run on every exit path.
                Ok(connector) => connector.shutdown(),
                Err(connector) => {
                    warn!(
                        owners = Rc::strong_count(&connector),
                        "the fleet console still shares its connector; falling back to Drop"
                    );
                }
            }
        }
        // `stderr` is a field, so it restores itself *after* this body: the
        // shutdown above runs with anything ssh says on its way out still in
        // the log.
    }
}

// ---------------------------------------------------------------------------
// stderr, while the console owns the screen
// ---------------------------------------------------------------------------

/// Whether this process's stderr is currently pointed at the herdr log.
///
/// A unit on Windows, where the console has no ssh child writing to an
/// inherited fd 2 (locked decision (j) is unix-only).
pub(super) struct ConsoleStderr {
    #[cfg(unix)]
    installed: bool,
}

impl ConsoleStderr {
    /// Stderr left exactly where it was.
    fn passthrough() -> Self {
        Self {
            #[cfg(unix)]
            installed: false,
        }
    }

    fn restore(&mut self) {
        #[cfg(unix)]
        if std::mem::take(&mut self.installed) {
            restore_console_stderr();
        }
    }
}

impl Drop for ConsoleStderr {
    /// Every owner — the console, and an `open_console` that failed between
    /// installing the redirect and building the console — puts stderr back.
    /// Without this an early error would be printed into the log the user
    /// cannot see, and the saved descriptor would leak.
    fn drop(&mut self) {
        self.restore();
    }
}

/// Point stderr at the herdr client log for as long as the console runs.
///
/// `herdr --remote`'s ssh child inherits this process's stderr (see
/// `remote::attach::bridge_connection`), and a fleet console runs N of them
/// behind a full-screen TUI: one "Connection to host closed" would corrupt the
/// screen. Redirecting the descriptor keeps that out of the terminal without
/// touching the shared remote code — and keeps it, in the log, where it can be
/// read after the fact.
#[cfg(unix)]
fn install_console_stderr() -> ConsoleStderr {
    use std::os::fd::AsRawFd as _;

    let path = crate::session::data_dir().join("herdr-client.log");
    let log = match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
    {
        Ok(log) => log,
        Err(error) => {
            warn!(path = %path.display(), %error, "keeping stderr on the terminal: the log is not writable");
            return ConsoleStderr::passthrough();
        }
    };
    // Duplicated close-on-exec so the ssh children inherit the *redirected*
    // fd 2 and never a handle on the console's real terminal.
    let original = match crate::pty::fd::duplicate_cloexec_fd(libc::STDERR_FILENO) {
        Ok(original) => original,
        Err(error) => {
            warn!(%error, "keeping stderr on the terminal: it could not be saved");
            return ConsoleStderr::passthrough();
        }
    };
    if ORIGINAL_STDERR
        .compare_exchange(-1, original, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Only one console runs per process; a second install would leak the
        // first saved descriptor and restore the wrong one.
        // SAFETY: `original` is a descriptor this call just created and no
        // longer owns a use for.
        unsafe { libc::close(original) };
        warn!("the console's stderr redirect is already installed");
        return ConsoleStderr::passthrough();
    }
    // SAFETY: both descriptors are open; `dup2` is the only way to move the
    // process's stderr, and the previous one is saved above.
    if unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        let error = std::io::Error::last_os_error();
        warn!(%error, "keeping stderr on the terminal: it could not be redirected");
        restore_console_stderr();
        return ConsoleStderr::passthrough();
    }
    debug!(path = %path.display(), "fleet console stderr redirected to the herdr log");
    ConsoleStderr { installed: true }
}

#[cfg(not(unix))]
fn install_console_stderr() -> ConsoleStderr {
    ConsoleStderr::passthrough()
}

/// The console's original stderr, duplicated; `-1` when nothing is redirected.
#[cfg(unix)]
static ORIGINAL_STDERR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
use std::sync::atomic::Ordering;

/// Put stderr back on the terminal. Idempotent, and safe to call from the
/// panic hook, which runs before the process unwinds past [`Console`].
#[cfg(unix)]
pub(super) fn restore_console_stderr() {
    let original = ORIGINAL_STDERR.swap(-1, Ordering::AcqRel);
    if original < 0 {
        return;
    }
    // SAFETY: `original` was duplicated from fd 2 by `install_console_stderr`
    // and is owned by this static until the swap above took it.
    unsafe {
        libc::dup2(original, libc::STDERR_FILENO);
        libc::close(original);
    }
}

#[cfg(not(unix))]
pub(super) fn restore_console_stderr() {}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fleet::hosts::{HostKind, HostSpec};
    use crate::protocol::{
        FrameData, PaneSurfaceFrame, PaneSurfacePatch, SemanticNotification,
        SemanticNotificationKind, SurfaceGraphicsScene,
    };

    fn host_id(id: &str) -> HostId {
        HostId::new(id).expect("valid host id")
    }

    fn spec(id: &str) -> HostSpec {
        HostSpec {
            id: host_id(id),
            kind: HostKind::Local {
                session: Some(id.to_string()),
            },
            enabled: true,
        }
    }

    fn fleet() -> FleetState {
        FleetState::new(vec![spec("alpha"), spec("beta")])
    }

    /// The frozen generation-1 snapshot, retagged. Parsing the same fixture
    /// the endpoint contract pins keeps this test honest about the real shape.
    fn snapshot(boot_id: &str, revision: u64) -> Box<ClientShellSnapshot> {
        let mut snapshot: ClientShellSnapshot = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-snapshot-v1.json"
        )))
        .expect("frozen snapshot decodes");
        snapshot.boot_id = boot_id.to_string();
        snapshot.revision = revision;
        Box::new(snapshot)
    }

    fn surface(boot_id: &str) -> Box<PaneSurfaceFrame> {
        Box::new(PaneSurfaceFrame {
            boot_id: boot_id.to_string(),
            projection_revision: 1,
            surface_revision: 1,
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

    fn notification(title: &str) -> Box<SemanticNotification> {
        Box::new(SemanticNotification {
            kind: SemanticNotificationKind::NeedsAttention,
            title: title.to_string(),
            body: None,
            sound: None,
            agent: None,
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            position: None,
        })
    }

    #[test]
    fn a_surface_from_an_inactive_host_never_reaches_the_shell() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Surface {
                host: host_id("beta"),
                frame: surface("boot-beta"),
            },
        );

        assert!(matches!(translated, Translated::Dropped));
    }

    #[test]
    fn the_active_hosts_surface_becomes_the_loops_own_pane_surface() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Surface {
                host: host_id("alpha"),
                frame: surface("boot-alpha"),
            },
        );

        let Translated::Server(message) = translated else {
            panic!("the active host's surface is a plain pane surface");
        };
        assert!(matches!(
            *message,
            ServerMessage::PaneSurface(frame) if frame.boot_id == "boot-alpha"
        ));
    }

    #[test]
    fn a_surface_patch_follows_the_same_host_check() {
        let mut state = fleet();
        let active = host_id("alpha");
        let patch = || {
            Box::new(PaneSurfacePatch {
                boot_id: "boot-alpha".to_string(),
                projection_revision: 1,
                base_surface_revision: 1,
                surface_revision: 2,
                rows: Vec::new(),
                panes: Vec::new(),
                cursor: None,
            })
        };

        let inactive = translate_event(
            &mut state,
            &active,
            FleetEvent::SurfacePatch {
                host: host_id("beta"),
                patch: patch(),
            },
        );
        let current = translate_event(
            &mut state,
            &active,
            FleetEvent::SurfacePatch {
                host: host_id("alpha"),
                patch: patch(),
            },
        );

        assert!(matches!(inactive, Translated::Dropped));
        assert!(matches!(
            current,
            Translated::Server(message) if matches!(*message, ServerMessage::PaneSurfacePatch(_))
        ));
    }

    #[test]
    fn any_hosts_server_message_that_is_not_the_active_one_is_dropped() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::ServerMessage {
                host: host_id("beta"),
                message: Box::new(ServerMessage::TerminalBell { count: 1 }),
            },
        );

        assert!(matches!(translated, Translated::Dropped));
    }

    #[test]
    fn the_active_hosts_snapshot_reaches_the_shell_and_the_fleet_model() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Host {
                host: host_id("alpha"),
                event: HostEvent::Snapshot(snapshot("boot-alpha", 7)),
            },
        );

        let Translated::Snapshot { snapshot, changes } = translated else {
            panic!("the active host's snapshot installs into the shell");
        };
        assert_eq!(snapshot.boot_id, "boot-alpha");
        assert_eq!(snapshot.revision, 7);
        assert!(
            changes.iter().any(
                |change| matches!(change, FleetChange::Snapshot { host, .. } if *host == active)
            ),
            "the fleet model saw the same snapshot: {changes:?}"
        );
        assert_eq!(
            state
                .host(&active)
                .and_then(|host| host.snapshot.as_ref())
                .map(|snapshot| snapshot.revision),
            Some(7),
            "the fleet model kept its own copy"
        );
    }

    #[test]
    fn another_hosts_snapshot_is_model_only() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Host {
                host: host_id("beta"),
                event: HostEvent::Snapshot(snapshot("boot-beta", 2)),
            },
        );

        let Translated::Changes(changes) = translated else {
            panic!("an inactive host's snapshot never installs into the shell");
        };
        assert!(!changes.is_empty(), "the fleet model still recorded it");
        assert_eq!(
            state
                .host(&host_id("beta"))
                .and_then(|host| host.snapshot.as_ref())
                .map(|snapshot| snapshot.revision),
            Some(2)
        );
    }

    #[test]
    fn only_the_active_hosts_connection_publishes_its_endpoint_methods() {
        let mut state = fleet();
        let active = host_id("alpha");
        let connected = |host: &str| FleetEvent::Host {
            host: host_id(host),
            event: HostEvent::Connected {
                server_version: "0.8.2-fork".to_string(),
                methods: vec![format!("{host}.snapshot")],
            },
        };

        let current = translate_event(&mut state, &active, connected("alpha"));
        let other = translate_event(&mut state, &active, connected("beta"));

        let Translated::EndpointMethods { methods, changes } = current else {
            panic!("the shell gates its commands on the active host's methods");
        };
        assert_eq!(methods, vec!["alpha.snapshot".to_string()]);
        assert!(!changes.is_empty(), "the fleet model saw it too");
        assert!(
            matches!(other, Translated::Changes(changes) if !changes.is_empty()),
            "another host's connection is a fleet fact, never the shell's method list"
        );
    }

    #[test]
    fn every_hosts_notification_arrives_prefixed_with_its_host() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Notification {
                host: host_id("beta"),
                notification: notification("agent is blocked"),
            },
        );

        let Translated::Server(message) = translated else {
            panic!("notifications from every host reach the shell");
        };
        assert!(matches!(
            *message,
            ServerMessage::SemanticNotification(event) if event.title == "[beta] agent is blocked"
        ));
    }

    fn targeted_notification(title: &str) -> Box<SemanticNotification> {
        let mut event = notification(title);
        event.workspace_id = Some("w1".to_string());
        event.tab_id = Some("w1:t1".to_string());
        event.pane_id = Some("w1:p1".to_string());
        event.agent = Some("claude".to_string());
        event
    }

    #[test]
    fn an_inactive_hosts_notification_keeps_no_target_the_active_host_could_resolve() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Notification {
                host: host_id("beta"),
                notification: targeted_notification("agent is blocked"),
            },
        );

        let Translated::Server(message) = translated else {
            panic!("notifications from every host reach the shell");
        };
        let ServerMessage::SemanticNotification(event) = *message else {
            panic!("a notification stays a notification");
        };
        assert_eq!(event.title, "[beta] agent is blocked");
        assert_eq!(
            event.agent.as_deref(),
            Some("claude"),
            "the agent name is display, kept"
        );
        assert_eq!(
            (event.workspace_id, event.tab_id, event.pane_id),
            (None, None, None),
            "ids are the other host's: the shell would validate, suppress or focus them on the active host"
        );
    }

    #[test]
    fn the_active_hosts_notification_keeps_its_target() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Notification {
                host: host_id("alpha"),
                notification: targeted_notification("agent is blocked"),
            },
        );

        let Translated::Server(message) = translated else {
            panic!("notifications from every host reach the shell");
        };
        let ServerMessage::SemanticNotification(event) = *message else {
            panic!("a notification stays a notification");
        };
        assert_eq!(event.title, "[alpha] agent is blocked");
        assert_eq!(event.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(event.workspace_id.as_deref(), Some("w1"));
        assert_eq!(event.tab_id.as_deref(), Some("w1:t1"));
    }

    #[test]
    fn an_endpoint_response_is_taken_only_from_the_active_host() {
        let mut state = fleet();
        let active = host_id("alpha");

        let foreign = translate_event(
            &mut state,
            &active,
            FleetEvent::EndpointResponse {
                host: host_id("beta"),
                request_id: "request-1".to_string(),
                result: Ok(b"{}".to_vec()),
            },
        );
        let current = translate_event(
            &mut state,
            &active,
            FleetEvent::EndpointResponse {
                host: host_id("alpha"),
                request_id: "request-1".to_string(),
                result: Err("host went away".to_string()),
            },
        );

        assert!(matches!(foreign, Translated::Dropped));
        let Translated::EndpointResponse { request_id, result } = current else {
            panic!("the active host's answer completes the command");
        };
        assert_eq!(request_id, "request-1");
        assert_eq!(result, Err("host went away".to_string()));
    }

    #[test]
    fn a_host_the_fleet_does_not_know_changes_nothing() {
        let mut state = fleet();
        let active = host_id("alpha");

        let translated = translate_event(
            &mut state,
            &active,
            FleetEvent::Host {
                host: host_id("ghost"),
                event: HostEvent::Connecting { attempt: 1 },
            },
        );

        assert!(matches!(translated, Translated::Changes(changes) if changes.is_empty()));
        assert!(state.host(&host_id("ghost")).is_none());
    }
}

/// The redirect's bookkeeping, without ever pointing this process's stderr
/// anywhere: fd 2 is duplicated onto itself, so a restore is a visible no-op.
#[cfg(all(test, unix))]
mod console_stderr_tests {
    use super::*;

    /// The tests share one process-wide static; `cargo test` runs them on
    /// threads (nextest does not), so they take turns.
    static REDIRECT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dropping_an_installed_redirect_restores_and_releases_the_saved_descriptor() {
        let _serial = REDIRECT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved =
            crate::pty::fd::duplicate_cloexec_fd(libc::STDERR_FILENO).expect("duplicate fd 2");
        assert!(
            ORIGINAL_STDERR
                .compare_exchange(-1, saved, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "no other test in this process may hold the redirect"
        );

        drop(ConsoleStderr { installed: true });

        assert_eq!(
            ORIGINAL_STDERR.load(Ordering::Acquire),
            -1,
            "the saved descriptor was handed back"
        );
        // SAFETY: `fcntl(F_GETFD)` only inspects a descriptor.
        assert_eq!(
            unsafe { libc::fcntl(saved, libc::F_GETFD) },
            -1,
            "the saved descriptor was closed, not leaked"
        );
        assert_ne!(
            unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFD) },
            -1,
            "fd 2 is still open"
        );
    }

    #[test]
    fn a_passthrough_restores_nothing() {
        let _serial = REDIRECT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(ConsoleStderr::passthrough());
        assert_eq!(ORIGINAL_STDERR.load(Ordering::Acquire), -1);
        restore_console_stderr();
        assert_eq!(ORIGINAL_STDERR.load(Ordering::Acquire), -1, "idempotent");
    }
}

#[cfg(test)]
mod console_config_tests {
    use super::*;

    use crate::config::{Config, FleetHostConfig, FleetHostKind};

    fn host(name: &str, enabled: bool) -> FleetHostConfig {
        FleetHostConfig {
            name: name.to_string(),
            kind: FleetHostKind::Local,
            session: Some(name.to_string()),
            enabled,
            ..FleetHostConfig::default()
        }
    }

    #[test]
    fn a_fleet_with_no_enabled_host_is_a_user_error_not_an_empty_console() {
        let mut config = Config::default();
        config.fleet.include_local = false;
        config.fleet.hosts = vec![host("lab-1", false)];

        let error = resolve_console_hosts(&config).expect_err("nothing to connect to");

        assert_eq!(error.len(), 1, "{error:?}");
        assert!(error[0].contains("no fleet hosts enabled"), "{error:?}");
    }

    #[test]
    fn an_empty_fleet_section_without_local_is_refused_too() {
        let mut config = Config::default();
        config.fleet.include_local = false;

        assert!(resolve_console_hosts(&config).is_err());
    }

    #[test]
    fn a_disabled_host_is_still_listed_beside_an_enabled_one() {
        let mut config = Config::default();
        config.fleet.include_local = false;
        config.fleet.hosts = vec![host("lab-1", false), host("lab-2", true)];

        let specs = resolve_console_hosts(&config).expect("one host is enabled");

        assert_eq!(
            specs.len(),
            2,
            "a disabled host stays visible in the sidebar"
        );
        assert!(!specs[0].enabled);
        assert!(specs[1].enabled);
    }

    #[test]
    fn an_invalid_fleet_section_reports_its_diagnostics() {
        let mut config = Config::default();
        config.fleet.include_local = false;
        config.fleet.hosts = vec![host("", true)];

        let error = resolve_console_hosts(&config).expect_err("an unnamed host is invalid");

        assert!(!error.is_empty());
        assert!(
            !error[0].contains("no fleet hosts enabled"),
            "the config diagnostic wins over the emptiness check: {error:?}"
        );
    }

    #[test]
    fn the_consoles_preferences_are_its_own_not_a_hosts() {
        let key = console_preferences_key();

        assert!(key.ends_with("fleet"), "{}", key.display());
        assert_ne!(
            key,
            crate::server::socket_paths::client_socket_path(),
            "chrome preferences follow the console, not whichever host it shows"
        );
    }
}

/// The console against real sockets: which host is opened, which host is told
/// how big the terminal is, and where a keystroke lands.
#[cfg(all(test, unix))]
mod console_tests {
    use super::*;

    use std::time::{Duration, Instant};

    use crate::config::Config;
    use crate::fleet::connector::test_support::{
        fake_connector, fake_connector_with_specs, hello_geometry, scratch_dir, snapshot,
        snapshot_message, Behaviour, FakeHost,
    };
    use crate::fleet::connector::INACTIVE_SURFACE;
    use crate::protocol::{ClientMessage, ClientPaneInputEvent};

    /// A console terminal that is not the inactive size, so the two are never
    /// confused in an assertion.
    const CONSOLE_SURFACE: ClientSurfaceSize = ClientSurfaceSize { cols: 96, rows: 38 };
    const CELL_WIDTH: u32 = 8;
    const CELL_HEIGHT: u32 = 17;

    fn console_geometry() -> ActiveGeometry {
        ActiveGeometry {
            surface: CONSOLE_SURFACE,
            cell_width_px: CELL_WIDTH,
            cell_height_px: CELL_HEIGHT,
            pixel_mouse: true,
        }
    }

    fn inactive() -> ActiveGeometry {
        ActiveGeometry {
            surface: INACTIVE_SURFACE,
            cell_width_px: CELL_WIDTH,
            cell_height_px: CELL_HEIGHT,
            pixel_mouse: true,
        }
    }

    fn console_options() -> FleetConnectorOptions {
        FleetConnectorOptions::for_client(
            &Config::default(),
            HandshakeParams::for_client(CELL_WIDTH, CELL_HEIGHT, true, true),
            console_geometry(),
        )
    }

    fn serving(dir: &std::path::Path, name: &str) -> FakeHost {
        FakeHost::start(
            dir,
            name,
            Behaviour::Serve(vec![snapshot_message(&snapshot(
                &format!("boot-{name}"),
                1,
            ))]),
        )
    }

    /// A minimal full surface from one host, for the translation checks.
    fn frame(boot_id: &str) -> Box<crate::protocol::PaneSurfaceFrame> {
        Box::new(crate::protocol::PaneSurfaceFrame {
            boot_id: boot_id.to_string(),
            projection_revision: 1,
            surface_revision: 1,
            frame: crate::protocol::FrameData {
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
            graphics: crate::protocol::SurfaceGraphicsScene::default(),
        })
    }

    fn pane_input(pane: &str) -> ClientMessage {
        ClientMessage::ClientShellPaneInput {
            pane_id: pane.to_string(),
            events: vec![ClientPaneInputEvent::TextCommit("x".to_string())],
        }
    }

    fn pane_inputs(messages: &[ClientMessage]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::ClientShellPaneInput { pane_id, .. } => Some(pane_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn active_of(link: &ClientLink) -> HostId {
        link.fleet
            .as_ref()
            .map(|fleet| fleet.active.clone())
            .expect("a console carries its fleet state")
    }

    #[test]
    fn the_console_opens_on_the_first_host_and_tells_only_it_the_terminal_size() {
        let dir = scratch_dir("console-open");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);

        let (link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");

        assert_eq!(active_of(&link), HostId::new("alpha").expect("host id"));
        assert!(matches!(link.link, ServerLink::Fleet(_)));
        assert!(
            link.fleet_events.is_some(),
            "the loop owns the event stream"
        );
        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(10), || {
                hello_geometry(&alpha.received()) == vec![console_geometry()]
                    && hello_geometry(&beta.received()) == vec![inactive()]
            }),
            "the active host renders at the console's size and every other host stays small: \
             alpha {:?} beta {:?}",
            hello_geometry(&alpha.received()),
            hello_geometry(&beta.received())
        );
        drop(link);
        drop(console);
    }

    #[test]
    fn the_first_enabled_host_is_active_even_when_it_is_not_the_first_host() {
        let dir = scratch_dir("console-disabled");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let mut disabled = alpha.spec("alpha");
        disabled.enabled = false;
        let specs = vec![disabled, beta.spec("beta")];
        let connector = fake_connector_with_specs(
            &[("alpha", &alpha), ("beta", &beta)],
            specs.clone(),
            console_options(),
        );
        let state = FleetState::new(specs);

        let (link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");

        let beta_id = HostId::new("beta").expect("host id");
        assert_eq!(active_of(&link), beta_id);
        assert_eq!(
            link.fleet
                .as_ref()
                .and_then(|fleet| fleet.state.active_host())
                .cloned(),
            Some(beta_id),
            "the fleet state and the link name the same machine"
        );
        drop(link);
        drop(console);
    }

    #[test]
    fn input_written_through_the_console_reaches_only_the_active_host() {
        let dir = scratch_dir("console-input");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);
        let (mut link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(10), || {
                !hello_geometry(&alpha.received()).is_empty()
            }),
            "alpha handshook"
        );

        link.link.write(&pane_input("w1:p1")).expect("write");

        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(5), || {
                pane_inputs(&alpha.received()) == vec!["w1:p1".to_string()]
            }),
            "the active host got the keystroke: {:?}",
            alpha.received()
        );
        assert!(
            pane_inputs(&beta.received()).is_empty(),
            "no other machine was typed into: {:?}",
            beta.received()
        );
        drop(link);
        drop(console);
    }

    #[test]
    fn a_host_that_never_answers_does_not_hold_the_console_shut() {
        let dir = scratch_dir("console-silent");
        // The first (and so active) host accepts the connection and then says
        // nothing: its welcome deadline is five seconds.
        let silent = FakeHost::start(&dir, "silent", Behaviour::Silent);
        let beta = serving(&dir, "beta");
        let specs = vec![silent.spec("silent"), beta.spec("beta")];
        let connector = fake_connector(&[("silent", &silent), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);

        let started = Instant::now();
        let (link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "the console is on screen before any handshake finishes, not after: {elapsed:?}"
        );
        assert_eq!(active_of(&link), HostId::new("silent").expect("host id"));
        drop(link);
        drop(console);
    }

    #[test]
    fn dropping_the_console_stops_every_host() {
        let dir = scratch_dir("console-shutdown");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);
        let (link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(10), || {
                !hello_geometry(&alpha.received()).is_empty()
                    && !hello_geometry(&beta.received()).is_empty()
            }),
            "both hosts connected"
        );

        // Exactly the console's exit path: the loop's link and event receiver
        // go first, so `shutdown` can close the channel and the supervisors
        // are not left parked on it.
        drop(link);
        let started = Instant::now();
        drop(console);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(3),
            "shutdown joins the supervisors within its own bounded wait: {elapsed:?}"
        );
    }

    // ---------------------------------------------------------------------
    // Host switching: every routing target moves, and only together.
    // ---------------------------------------------------------------------

    #[test]
    fn a_switch_moves_the_connector_the_link_and_the_fleet_state_together() {
        let dir = scratch_dir("switch-routing");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);
        let (mut link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(10), || {
                !hello_geometry(&alpha.received()).is_empty()
                    && !hello_geometry(&beta.received()).is_empty()
            }),
            "both hosts connected"
        );
        let beta_id = HostId::new("beta").expect("host id");
        let mut endpoint_commands = EndpointCommands::default();

        let changes = {
            let fleet = link.fleet.as_mut().expect("a console carries fleet state");
            retarget_host(
                fleet,
                &mut link.link,
                &mut endpoint_commands,
                beta_id.clone(),
                console_geometry(),
            )
        };

        assert!(
            changes
                .iter()
                .any(|change| matches!(change, FleetChange::ActiveHost { host: Some(host) } if *host == beta_id)),
            "{changes:?}"
        );
        let fleet = link.fleet.as_ref().expect("fleet state");
        assert_eq!(fleet.active, beta_id);
        assert_eq!(fleet.state.active_host(), Some(&beta_id));
        assert_eq!(fleet.pending_switch.as_ref(), Some(&beta_id));
        assert_eq!(
            fleet.connector.active_host(),
            Some(beta_id.clone()),
            "the connector streams the machine the console is showing"
        );

        // The old host is told to go back to the small inactive surface, the
        // new one to render at the console's size.
        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(5), || {
                crate::fleet::connector::test_support::resize_geometry(&alpha.received())
                    .last()
                    .copied()
                    == Some(inactive())
                    && crate::fleet::connector::test_support::resize_geometry(&beta.received())
                        .last()
                        .copied()
                        == Some(console_geometry())
            }),
            "alpha {:?} beta {:?}",
            crate::fleet::connector::test_support::resize_geometry(&alpha.received()),
            crate::fleet::connector::test_support::resize_geometry(&beta.received())
        );
        drop(link);
        drop(console);
    }

    #[test]
    fn what_is_typed_after_a_switch_reaches_only_the_new_host() {
        let dir = scratch_dir("switch-input");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);
        let (mut link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(10), || {
                !hello_geometry(&alpha.received()).is_empty()
                    && !hello_geometry(&beta.received()).is_empty()
            }),
            "both hosts connected"
        );
        link.link.write(&pane_input("before")).expect("write");
        let mut endpoint_commands = EndpointCommands::default();

        {
            let fleet = link.fleet.as_mut().expect("fleet state");
            retarget_host(
                fleet,
                &mut link.link,
                &mut endpoint_commands,
                HostId::new("beta").expect("host id"),
                console_geometry(),
            );
        }
        link.link.write(&pane_input("after")).expect("write");

        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(5), || {
                pane_inputs(&beta.received()) == vec!["after".to_string()]
            }),
            "the new host got what was typed after the switch: {:?}",
            beta.received()
        );
        assert_eq!(
            pane_inputs(&alpha.received()),
            vec!["before".to_string()],
            "and the old host got nothing after it: {:?}",
            alpha.received()
        );
        drop(link);
        drop(console);
    }

    #[test]
    fn a_switch_releases_the_endpoint_lane_the_old_host_was_holding() {
        let dir = scratch_dir("switch-lane");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);
        let (mut link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(10), || {
                !hello_geometry(&alpha.received()).is_empty()
            }),
            "alpha connected"
        );
        let mut endpoint_commands = EndpointCommands::default();
        endpoint_commands.enqueue(
            "boot-alpha".to_string(),
            Box::new(crate::api::schema::Request {
                id: "request-1".to_string(),
                method: crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                    pane_id: "w1:p1".to_string(),
                }),
            }),
        );
        endpoint_commands
            .send_next(&mut link.link)
            .expect("the request goes to the active host");

        {
            let fleet = link.fleet.as_mut().expect("fleet state");
            retarget_host(
                fleet,
                &mut link.link,
                &mut endpoint_commands,
                HostId::new("beta").expect("host id"),
                console_geometry(),
            );
        }

        // The old host's answer is dropped by `translate`, so a lane still
        // held here would only free at its 60 s timeout — every command the
        // user tries on the new host would silently queue behind it.
        assert!(
            endpoint_commands.is_idle(),
            "the switch released the endpoint lane"
        );
        drop(link);
        drop(console);
    }

    #[test]
    fn a_disabled_or_unknown_host_is_never_a_switch_target() {
        let dir = scratch_dir("switch-refused");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let mut disabled = beta.spec("beta");
        disabled.enabled = false;
        let specs = vec![alpha.spec("alpha"), disabled];
        let connector = fake_connector_with_specs(
            &[("alpha", &alpha), ("beta", &beta)],
            specs.clone(),
            console_options(),
        );
        let state = FleetState::new(specs);
        let (link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        let fleet = link.fleet.as_ref().expect("fleet state");

        assert!(!switch_target_allowed(
            fleet,
            &HostId::new("beta").expect("host id")
        ));
        assert!(!switch_target_allowed(
            fleet,
            &HostId::new("ghost").expect("host id")
        ));
        assert!(!switch_target_allowed(
            fleet,
            &HostId::new("alpha").expect("host id")
        ));
        drop(link);
        drop(console);
    }

    #[test]
    fn the_old_hosts_frames_stop_reaching_the_shell_the_moment_it_is_switched_away_from() {
        let dir = scratch_dir("switch-frames");
        let alpha = serving(&dir, "alpha");
        let beta = serving(&dir, "beta");
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let connector = fake_connector(&[("alpha", &alpha), ("beta", &beta)], console_options());
        let state = FleetState::new(specs);
        let (mut link, console) =
            console_link(state, connector, ConsoleStderr::passthrough()).expect("console opens");
        let mut endpoint_commands = EndpointCommands::default();
        {
            let fleet = link.fleet.as_mut().expect("fleet state");
            retarget_host(
                fleet,
                &mut link.link,
                &mut endpoint_commands,
                HostId::new("beta").expect("host id"),
                console_geometry(),
            );
        }
        let fleet = link.fleet.as_mut().expect("fleet state");

        let old = fleet.translate(FleetEvent::Surface {
            host: HostId::new("alpha").expect("host id"),
            frame: frame("boot-alpha"),
        });
        let new = fleet.translate(FleetEvent::Surface {
            host: HostId::new("beta").expect("host id"),
            frame: frame("boot-beta"),
        });

        assert!(matches!(old, Translated::Dropped));
        assert!(matches!(new, Translated::Server(_)));
        assert!(
            fleet.pending_switch.is_none(),
            "the new host's first surface ends the switch"
        );
        drop(link);
        drop(console);
    }
}
