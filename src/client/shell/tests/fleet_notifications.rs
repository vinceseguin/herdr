//! Host-aware notifications in a Fleet console (fork, E2 PR 7).
//!
//! A notification names a pane, a tab and a workspace on the machine that sent
//! it, and every herdr server starts at `w1`/`w1:p1` — so the same ids exist on
//! every host of a fleet. The three questions this file pins are the three the
//! shell asks about a notification, and each of them has a wrong answer that is
//! a mis-route:
//!
//! * *Is it still current?* Answered against that host's rows, never the
//!   active host's agents.
//! * *Is the user already looking at it?* Never, for a host that is not on
//!   screen — otherwise the active host's focused tab silently swallows
//!   another machine's toast.
//! * *What does opening it do?* Switch to that host and focus there, never
//!   `pane.focus` on the machine the console happens to be showing.

use super::*;

use std::collections::HashSet;

use crate::client::shell::{FleetFocusTarget, FleetShellAction};
use crate::fleet::hosts::{HostId, HostKind, HostSpec};
use crate::fleet::sidebar::FleetSidebarModel;
use crate::fleet::state::{FleetState, HostEvent};
use crate::protocol::{ClientShellAgent, SemanticNotificationSound};

fn host(name: &str) -> HostId {
    HostId::new(name).expect("valid host id")
}

fn spec(name: &str, enabled: bool) -> HostSpec {
    HostSpec {
        id: host(name),
        kind: HostKind::Local {
            session: Some(name.to_string()),
        },
        enabled,
    }
}

fn agent(pane_id: &str, name: &str, status: AgentStatus) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some(name.into()),
        display_agent: None,
        agent: None,
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: 3,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

/// One host's projection. Every host uses the *same* agent pane id on purpose:
/// that collision is what host-qualified notifications exist for.
fn host_snapshot(label: &str, boot: &str, status: AgentStatus) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.boot_id = boot.into();
    snapshot.workspaces[0].label = format!("{label}-space");
    snapshot.agents = vec![agent("pane_9", &format!("{label}-agent"), status)];
    snapshot
}

/// `alpha` active with a blocked agent, `beta` connected with the same pane id
/// in another state, `gamma` configured but disabled.
fn fleet_state(beta_status: AgentStatus) -> FleetState {
    let mut state = FleetState::new(vec![
        spec("alpha", true),
        spec("beta", true),
        spec("gamma", false),
    ]);
    for (id, boot, status) in [
        ("alpha", "boot-alpha", AgentStatus::Blocked),
        ("beta", "boot-beta", beta_status),
    ] {
        state.apply(
            &host(id),
            HostEvent::Connected {
                server_version: "0.8.2-fork".into(),
                methods: vec!["pane.focus".into()],
            },
        );
        state.apply(
            &host(id),
            HostEvent::Snapshot(Box::new(host_snapshot(id, boot, status))),
        );
    }
    state.set_active_host(Some(host("alpha")));
    state
}

fn model_for(state: &FleetState) -> FleetSidebarModel {
    let mut model = FleetSidebarModel::default();
    model.rebuild(
        state,
        &HashSet::new(),
        crate::config::AgentPanelSortConfig::Priority,
    );
    model
}

/// A console showing `alpha`, with toasts on and no validation delay.
fn console(beta_status: AgentStatus) -> ClientShellState {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.toast_delivery = crate::config::ToastDelivery::Herdr;
    config.toast_delay_seconds = 0;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(host_snapshot(
        "alpha",
        "boot-alpha",
        AgentStatus::Blocked,
    )));
    state.set_pane_surface(surface());
    state.set_endpoint_methods(Some(vec!["pane.focus".into()]));
    state.fleet_sidebar_update(model_for(&fleet_state(beta_status)), host("alpha"), None);
    state
}

/// A blocked-agent notification as the console's `translate` builds it: the
/// title already carries the `[host] ` prefix, the ids are the sender's.
fn blocked_on(label: &str, pane_id: &str) -> SemanticNotification {
    SemanticNotification {
        kind: SemanticNotificationKind::NeedsAttention,
        title: format!("[{label}] codex needs attention"),
        body: Some("space · 1".into()),
        sound: Some(SemanticNotificationSound::Request),
        agent: Some("codex".into()),
        // The *active* host's focused ids, deliberately: `snapshot()` focuses
        // `ws_1`/`tab_1`, so a shell that resolved these against the machine on
        // screen would call this notification "already visible" and drop it.
        workspace_id: Some("ws_1".into()),
        tab_id: Some("tab_1".into()),
        pane_id: Some(pane_id.into()),
        position: None,
    }
}

fn fleet_actions(outcome: &ClientShellInput) -> Vec<FleetShellAction> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Fleet(action) => Some(action.clone()),
            _ => None,
        })
        .collect()
}

fn focused_panes(outcome: &ClientShellInput) -> Vec<String> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                crate::api::schema::Method::PaneFocus(params) => Some(params.pane_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn open_the_target(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::OpenNotificationTarget),
        &mut outcome,
    );
    outcome
}

#[test]
fn another_hosts_notification_is_shown_even_when_the_active_host_focuses_the_same_ids() {
    let mut state = console(AgentStatus::Blocked);
    let now = std::time::Instant::now();

    let (effects, repaint) =
        state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);

    assert!(repaint, "a toast for another host repaints the console");
    assert!(
        matches!(
            effects.as_slice(),
            [ClientShellNotificationEffect::Sound { .. }]
        ),
        "the existing sound rules are unchanged"
    );
    let visible = state
        .visible_notification
        .as_ref()
        .expect("another host's notification is visible");
    assert_eq!(visible.host.as_ref(), Some(&host("beta")));
    assert!(
        visible.event.title.starts_with("[beta] "),
        "the host prefix is what says which machine is asking: {}",
        visible.event.title
    );
}

#[test]
fn the_active_hosts_notification_is_still_suppressed_by_its_own_focused_tab() {
    let mut state = console(AgentStatus::Blocked);
    let now = std::time::Instant::now();

    let (_, _) =
        state.receive_fleet_notification(host("alpha"), blocked_on("alpha", "pane_9"), now);

    assert!(
        state.visible_notification.is_none(),
        "the user is already looking at the active host's focused tab"
    );
}

#[test]
fn another_hosts_notification_is_validated_against_that_hosts_agents() {
    // beta's agent is *not* blocked; the active host's agent with the very
    // same pane id is. A shell that validated against the machine on screen
    // would show this stale toast.
    let mut state = console(AgentStatus::Working);
    state.config.toast_delay_seconds = 1;
    let now = std::time::Instant::now();

    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);
    assert!(state.visible_notification.is_none(), "still pending");
    let (_, _) = state.tick_notifications(now + std::time::Duration::from_secs(2));

    assert!(
        state.visible_notification.is_none(),
        "beta's agent is no longer blocked, so the toast is stale"
    );
    assert!(state.pending_notifications.is_empty());
}

#[test]
fn another_hosts_notification_survives_validation_when_that_host_still_says_blocked() {
    let mut state = console(AgentStatus::Blocked);
    state.config.toast_delay_seconds = 1;
    let now = std::time::Instant::now();

    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);
    state.tick_notifications(now + std::time::Duration::from_secs(2));

    let visible = state
        .visible_notification
        .as_ref()
        .expect("beta's agent is blocked, so the toast is current");
    assert_eq!(visible.host.as_ref(), Some(&host("beta")));
}

#[test]
fn a_notification_for_a_pane_that_host_does_not_have_is_dropped() {
    let mut state = console(AgentStatus::Blocked);
    state.config.toast_delay_seconds = 1;
    let now = std::time::Instant::now();

    // `pane_1` is a real pane on the *active* host's projection and on no
    // other: validating there would keep this toast alive.
    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_1"), now);
    state.tick_notifications(now + std::time::Duration::from_secs(2));

    assert!(state.visible_notification.is_none());
}

#[test]
fn two_hosts_do_not_replace_each_others_notifications() {
    let mut state = console(AgentStatus::Blocked);
    state.config.toast_delay_seconds = 1;
    let now = std::time::Instant::now();

    state.receive_fleet_notification(host("alpha"), blocked_on("alpha", "pane_9"), now);
    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);

    assert_eq!(
        state.pending_notifications.len(),
        2,
        "a pane id is only unique per host"
    );
    // The same host replaces its own, exactly as upstream does.
    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);
    assert_eq!(state.pending_notifications.len(), 2);
}

#[test]
fn opening_another_hosts_notification_switches_there_and_focuses_nothing_here() {
    let mut state = console(AgentStatus::Blocked);
    let now = std::time::Instant::now();
    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);

    let outcome = open_the_target(&mut state);

    assert_eq!(
        fleet_actions(&outcome),
        vec![FleetShellAction::SwitchHost {
            host: host("beta"),
            then_focus: Some(FleetFocusTarget::Pane("pane_9".into())),
        }]
    );
    assert!(
        focused_panes(&outcome).is_empty(),
        "another host's pane id must never be focused on the host on screen"
    );
    assert!(state.visible_notification.is_none(), "and it is dismissed");
}

#[test]
fn clicking_another_hosts_toast_switches_there_too() {
    let mut state = console(AgentStatus::Blocked);
    let now = std::time::Instant::now();
    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);
    state.compose(120, 40).expect("the console composes");

    let hit = state.hits.notification_toast;
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.x,
        row: hit.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert_eq!(
        fleet_actions(&outcome),
        vec![FleetShellAction::SwitchHost {
            host: host("beta"),
            then_focus: Some(FleetFocusTarget::Pane("pane_9".into())),
        }]
    );
    assert!(focused_panes(&outcome).is_empty());
}

#[test]
fn opening_the_active_hosts_notification_focuses_it_exactly_as_one_host_does() {
    let mut state = console(AgentStatus::Blocked);
    let now = std::time::Instant::now();
    // Not the focused tab, so it is actually shown.
    let mut event = blocked_on("alpha", "pane_9");
    event.tab_id = Some("tab_other".into());
    event.workspace_id = Some("ws_other".into());
    state.receive_fleet_notification(host("alpha"), event, now);
    assert!(state.visible_notification.is_some());

    let outcome = open_the_target(&mut state);

    assert_eq!(focused_panes(&outcome), vec!["pane_9".to_string()]);
    assert!(
        fleet_actions(&outcome).is_empty(),
        "the active host's own notification is not a host switch"
    );
}

#[test]
fn a_notification_from_a_host_that_cannot_be_switched_to_focuses_nothing() {
    let mut state = console(AgentStatus::Blocked);
    let now = std::time::Instant::now();
    // `gamma` is configured but disabled: `switch_action` refuses it, and the
    // fall-through must not reach the active host's `pane.focus`.
    let mut event = blocked_on("gamma", "pane_9");
    event.kind = SemanticNotificationKind::Custom;
    state.receive_fleet_notification(host("gamma"), event, now);
    assert!(state.visible_notification.is_some());

    let outcome = open_the_target(&mut state);

    assert!(fleet_actions(&outcome).is_empty());
    assert!(
        focused_panes(&outcome).is_empty(),
        "a disabled host's pane id must not be focused on the active host"
    );
    assert!(state.visible_notification.is_none());
}

#[test]
fn another_hosts_notification_is_current_when_its_agent_is_hidden_by_a_named_view() {
    // beta's panel is a named view that does not list `pane_9`, so the agent
    // is absent from beta's *rows* while beta's snapshot still says it is
    // blocked. The single-host client validates against `snapshot.agents`,
    // never the panel; the console must do the same for another host.
    let mut fleet = fleet_state(AgentStatus::Blocked);
    let mut projection = host_snapshot("beta", "boot-beta", AgentStatus::Blocked);
    projection.revision += 1;
    projection.agent_view_label = Some("recent".into());
    projection.agent_order = vec!["pane_elsewhere".into()];
    fleet.apply(&host("beta"), HostEvent::Snapshot(Box::new(projection)));
    let mut state = console(AgentStatus::Blocked);
    state.fleet_sidebar_update(model_for(&fleet), host("alpha"), None);
    state.config.toast_delay_seconds = 1;
    let now = std::time::Instant::now();

    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);
    state.tick_notifications(now + std::time::Duration::from_secs(2));

    let visible = state
        .visible_notification
        .as_ref()
        .expect("a view that hides the agent does not make its toast stale");
    assert_eq!(visible.host.as_ref(), Some(&host("beta")));
}

#[test]
fn a_host_qualified_notification_before_any_fleet_view_is_never_resolved_here() {
    // A console whose shell has a projection but no fleet view yet: nothing
    // says which host that projection belongs to, so a host-qualified
    // notification's ids must not be validated or focused against it.
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.toast_delivery = crate::config::ToastDelivery::Herdr;
    config.toast_delay_seconds = 0;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(host_snapshot(
        "alpha",
        "boot-alpha",
        AgentStatus::Blocked,
    )));
    state.set_pane_surface(surface());
    state.set_endpoint_methods(Some(vec!["pane.focus".into()]));
    assert!(state.fleet.is_none(), "no fleet view installed yet");
    let now = std::time::Instant::now();

    // Shown — the active host's focused ids do not suppress it — but not
    // openable: neither a switch (no fleet to switch with) nor a `pane.focus`
    // on the projection at hand.
    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);
    assert!(state.visible_notification.is_some());
    let outcome = open_the_target(&mut state);
    assert!(fleet_actions(&outcome).is_empty());
    assert!(
        focused_panes(&outcome).is_empty(),
        "another host's pane id must not be focused on the projection at hand"
    );
    assert!(state.visible_notification.is_none());

    // Validated: the shell's snapshot is not the answer, and there is no
    // model to ask, so a delayed notification is dropped rather than shown
    // on the strength of a different machine's agent.
    state.config.toast_delay_seconds = 1;
    state.receive_fleet_notification(host("beta"), blocked_on("beta", "pane_9"), now);
    state.tick_notifications(now + std::time::Duration::from_secs(2));
    assert!(state.visible_notification.is_none());
    assert!(state.pending_notifications.is_empty());
}

#[test]
fn the_single_host_client_is_unchanged() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.toast_delivery = crate::config::ToastDelivery::Herdr;
    config.toast_delay_seconds = 0;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(host_snapshot(
        "alpha",
        "boot-alpha",
        AgentStatus::Blocked,
    )));
    state.set_pane_surface(surface());
    let now = std::time::Instant::now();

    let mut event = blocked_on("alpha", "pane_9");
    event.title = "codex needs attention".into();
    event.tab_id = Some("tab_other".into());
    event.workspace_id = Some("ws_other".into());
    state.receive_notification(event, now);

    let visible = state
        .visible_notification
        .as_ref()
        .expect("a single-host client shows its own toast");
    assert!(
        visible.host.is_none(),
        "one server: nothing to qualify an id with"
    );
    let outcome = open_the_target(&mut state);
    assert_eq!(focused_panes(&outcome), vec!["pane_9".to_string()]);
}
