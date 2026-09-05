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

use std::sync::Arc;

use tracing::debug;

use crate::fleet::connector::{FleetConnector, FleetEvent};
use crate::fleet::hosts::HostId;
use crate::fleet::state::{FleetChange, FleetState, HostEvent};
use crate::protocol::{ClientShellSnapshot, ServerMessage};

/// What one fleet event means to the client loop.
pub(super) enum Translated {
    /// Hand this to the loop's existing `ServerMessage` handling, unchanged.
    Server(Box<ServerMessage>),
    /// The active host replaced its projection.
    Snapshot {
        snapshot: Box<ClientShellSnapshot>,
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
// Constructed by `run_fleet` (E2 PR 4); PR 3 ships the type and its
// translation so the loop's fleet branch is complete before it is reachable.
#[allow(dead_code)]
pub(super) struct FleetClientState {
    /// Every host's connection and projection, merged.
    pub(super) state: FleetState,
    /// Shared with the loop's [`super::link::FleetLink`]; the receiver half is
    /// owned by the loop through `FleetConnector::take_events`.
    pub(super) connector: Arc<FleetConnector>,
    /// The one host whose surface the shell is showing and whose ids its input
    /// addresses.
    pub(super) active: HostId,
    /// A switch asked for, waiting for the new host's first full surface.
    pub(super) pending_switch: Option<HostId>,
}

#[allow(dead_code)]
impl FleetClientState {
    /// Turns one connector event into something the client loop can act on.
    pub(super) fn translate(&mut self, event: FleetEvent) -> Translated {
        translate_event(&mut self.state, &self.active, event)
    }
}

/// Applies fleet-model changes to the console.
///
/// A no-op until the sidebar model lands (E2 PR 5); the loop calls it now so
/// changes are never silently discarded once it does.
// Called from the loop's fleet branch, which PR 4 makes reachable.
#[allow(dead_code)]
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
        (HostEvent::Snapshot(snapshot), true) => Some(snapshot.clone()),
        _ => None,
    };
    let changes = state.apply(&host, event);
    match for_shell {
        Some(snapshot) => Translated::Snapshot { snapshot, changes },
        None => Translated::Changes(changes),
    }
}

fn dropped(host: &HostId, active: &HostId, kind: &'static str) -> Translated {
    debug!(%host, %active, kind, "dropping a fleet event from an inactive host");
    Translated::Dropped
}

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
    fn a_connection_change_is_model_only_for_every_host() {
        let mut state = fleet();
        let active = host_id("alpha");

        for host in ["alpha", "beta"] {
            let translated = translate_event(
                &mut state,
                &active,
                FleetEvent::Host {
                    host: host_id(host),
                    event: HostEvent::Connected {
                        server_version: "0.8.2-fork".to_string(),
                        methods: vec!["session.snapshot".to_string()],
                    },
                },
            );
            assert!(
                matches!(translated, Translated::Changes(changes) if !changes.is_empty()),
                "{host}'s connection is a fleet fact, not a shell message"
            );
        }
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
