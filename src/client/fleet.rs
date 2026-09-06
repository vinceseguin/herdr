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

use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use tracing::{debug, warn};

use crate::config::Config;
use crate::fleet::connector::{ActiveGeometry, FleetConnector, FleetConnectorOptions, FleetEvent};
use crate::fleet::handshake::HandshakeParams;
use crate::fleet::hosts::{resolve_hosts, HostId, HostSpec};
use crate::fleet::state::{FleetChange, FleetState, HostEvent};
use crate::protocol::{ClientShellSnapshot, ClientSurfaceSize, ServerMessage};

use super::link::{ClientLink, FleetLink, ServerLink};
use super::shell::ClientShellKeybindingSource;

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
    // Written by the host switch (E2 PR 5); the console tracks it from the
    // first switch on, and PR 8 renders the notice it implies.
    #[allow(dead_code)]
    pub(super) pending_switch: Option<HostId>,
}

impl FleetClientState {
    /// Turns one connector event into something the client loop can act on.
    pub(super) fn translate(&mut self, event: FleetEvent) -> Translated {
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

/// Applies fleet-model changes to the console.
///
/// A no-op until the sidebar model lands (E2 PR 5); the loop calls it now so
/// changes are never silently discarded once it does.
pub(super) fn apply_changes(fleet: &mut FleetClientState, changes: Vec<FleetChange>) {
    let _ = fleet;
    if !changes.is_empty() {
        debug!(
            changes = changes.len(),
            "fleet changes await the sidebar model"
        );
    }
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
        }),
    };
    Ok((
        link,
        Console {
            connector: Some(connector),
            stderr,
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
    stderr: ConsoleStderr,
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
        // After shutdown, so anything ssh says on its way out is still logged.
        self.stderr.restore();
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
}
