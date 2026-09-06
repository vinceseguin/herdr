//! The Fleet console's sidebar: host groups, dimming, and click routing.
//!
//! The routing assertions are the point of this file. A click on another
//! host's row must produce a *switch*, never a focus request against the host
//! the shell is currently showing — that is a keystroke landing on the wrong
//! machine — and a click on the active host's own rows must keep behaving
//! exactly as it does in the single-host client.

use super::*;

use std::collections::HashSet;

use crate::client::shell::{FleetFocusTarget, FleetShellAction};
use crate::fleet::hosts::{HostId, HostKind, HostSpec};
use crate::fleet::sidebar::FleetSidebarModel;
use crate::fleet::state::{FleetState, HostEvent};
use crate::protocol::ClientShellAgent;

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

fn agent(pane_id: &str, name: &str, status: AgentStatus, seq: u64) -> ClientShellAgent {
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
        state_change_seq: seq,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

/// One host's projection: a workspace named after it, and one agent.
fn host_snapshot(label: &str, boot: &str) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.boot_id = boot.into();
    snapshot.workspaces[0].label = format!("{label}-space");
    snapshot.agents = vec![agent(
        "pane_9",
        &format!("{label}-agent"),
        AgentStatus::Blocked,
        3,
    )];
    snapshot
}

/// A three-host fleet: `alpha` connected and active, `beta` connected,
/// `gamma` unavailable with a reason.
fn fleet_state() -> FleetState {
    let mut state = FleetState::new(vec![
        spec("alpha", true),
        spec("beta", true),
        spec("gamma", true),
    ]);
    for (id, boot) in [("alpha", "boot-1"), ("beta", "boot-beta")] {
        state.apply(
            &host(id),
            HostEvent::Connected {
                server_version: "0.8.2-fork".into(),
                methods: vec!["pane.focus".into(), "workspace.focus".into()],
            },
        );
        state.apply(
            &host(id),
            HostEvent::Snapshot(Box::new(host_snapshot(id, boot))),
        );
    }
    state.apply(
        &host("gamma"),
        HostEvent::Unavailable {
            reason: "connection refused".into(),
            retry_in: None,
        },
    );
    state.set_active_host(Some(host("alpha")));
    state
}

/// The model as the loop rebuilds it right after switching to `active`.
fn model_switched_to(active: &str) -> FleetSidebarModel {
    let mut fleet = fleet_state();
    fleet.set_active_host(Some(host(active)));
    model_for(&fleet, &[])
}

fn model_for(state: &FleetState, collapsed: &[&str]) -> FleetSidebarModel {
    let collapsed = collapsed
        .iter()
        .map(|name| host(name))
        .collect::<HashSet<_>>();
    let mut model = FleetSidebarModel::default();
    model.rebuild(
        state,
        &collapsed,
        crate::config::AgentPanelSortConfig::Priority,
    );
    model
}

/// A console showing `alpha`, with `beta` and `gamma` as sidebar groups.
fn console(collapsed: &[&str]) -> ClientShellState {
    let fleet = fleet_state();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(host_snapshot("alpha", "boot-1")));
    state.set_pane_surface(surface());
    state.set_endpoint_methods(Some(vec!["pane.focus".into(), "workspace.focus".into()]));
    state.fleet_sidebar_update(model_for(&fleet, collapsed), host("alpha"), None);
    state
}

fn screen(state: &mut ClientShellState) -> String {
    let frame = state.compose(120, 40).expect("the console composes");
    let mut text = String::new();
    for row in 0..frame.height {
        for column in 0..frame.width {
            let index = row as usize * frame.width as usize + column as usize;
            if let Some(cell) = frame.cells.get(index) {
                text.push_str(&cell.symbol);
            }
        }
        text.push('\n');
    }
    text
}

/// The style modifier of the first cell of a composed row.
fn row_modifier(frame: &crate::protocol::FrameData, needle: &str) -> u16 {
    for row in 0..frame.height {
        let mut text = String::new();
        for column in 0..frame.width {
            let index = row as usize * frame.width as usize + column as usize;
            if let Some(cell) = frame.cells.get(index) {
                text.push_str(&cell.symbol);
            }
        }
        if let Some(column) = text.find(needle) {
            let index = row as usize * frame.width as usize + column;
            if let Some(cell) = frame.cells.get(index) {
                return cell.modifier;
            }
        }
    }
    panic!("no row contains {needle:?}");
}

fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + rect.width / 2,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })])
}

fn fleet_hit(state: &ClientShellState, want: &FleetSidebarHit) -> Rect {
    state
        .hits
        .fleet_rows
        .iter()
        .find(|(_, hit)| hit == want)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| {
            panic!(
                "no fleet row for {want:?}; rows: {:?}",
                state.hits.fleet_rows
            )
        })
}

#[test]
fn the_console_draws_a_group_for_every_host_and_dims_the_one_that_is_down() {
    let mut state = console(&[]);
    let text = screen(&mut state);

    assert!(
        text.contains("▾ alpha"),
        "the active host's header:\n{text}"
    );
    assert!(
        text.contains("▾ beta"),
        "an inactive host's header:\n{text}"
    );
    assert!(
        text.contains("▾ gamma"),
        "a host that is down is still listed:\n{text}"
    );
    // Counts come from the model, not from a per-frame format.
    assert!(
        text.contains("1 blocked"),
        "the header carries the roll-up:\n{text}"
    );
    // The active host keeps the single-host rows; the others get one line each.
    assert!(
        text.contains("alpha-space"),
        "the active host's workspace:\n{text}"
    );
    assert!(
        text.contains("beta-space"),
        "another host's workspace:\n{text}"
    );
    assert!(text.contains("beta-agent"), "another host's agent:\n{text}");
    assert!(
        text.contains("unavailable") && text.contains("connection refused"),
        "the unreachable host says why:\n{text}"
    );

    // Dimming is the other half of "unreachable hosts dimmed with the reason".
    let frame = state.compose(120, 40).expect("the console composes");
    let dim = ratatui::style::Modifier::DIM.bits();
    assert_eq!(
        row_modifier(&frame, "gamma") & dim,
        dim,
        "an unreachable host's header is dimmed"
    );
    assert_eq!(
        row_modifier(&frame, "connection refused") & dim,
        dim,
        "and so is its reason"
    );
    assert_eq!(
        row_modifier(&frame, "▾ beta") & dim,
        0,
        "a connected host is not dimmed"
    );
}

#[test]
fn a_collapsed_group_draws_its_header_and_nothing_else() {
    let mut state = console(&["beta"]);
    let text = screen(&mut state);

    assert!(text.contains("▸ beta"), "a collapsed header:\n{text}");
    assert!(
        !text.contains("beta-space") && !text.contains("beta-agent"),
        "a collapsed group hides its rows:\n{text}"
    );
    assert!(
        text.contains("alpha-space"),
        "other groups are unaffected:\n{text}"
    );
}

#[test]
fn clicking_another_hosts_agent_switches_instead_of_focusing_on_this_one() {
    let mut state = console(&[]);
    let _ = screen(&mut state);
    let pane = crate::fleet::refs::FleetPaneRef::new(host("beta"), "pane_9");
    let rect = fleet_hit(&state, &FleetSidebarHit::Agent(pane));

    let outcome = click(&mut state, rect);

    let actions = outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Fleet(action) => Some(action.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        vec![FleetShellAction::SwitchHost {
            host: host("beta"),
            then_focus: Some(FleetFocusTarget::Pane("pane_9".into())),
        }]
    );
    // The dangerous outcome: a focus request built from another machine's pane
    // id and sent to the host this shell is showing.
    assert!(
        !outcome
            .actions
            .iter()
            .any(|action| matches!(action, ClientShellAction::Endpoint { .. })),
        "a switch must not also focus here: {:?}",
        outcome.actions
    );
    assert!(outcome.requests.is_empty(), "{:?}", outcome.requests);
}

#[test]
fn clicking_another_hosts_workspace_switches_and_carries_that_hosts_id() {
    let mut state = console(&[]);
    let _ = screen(&mut state);
    let workspace = crate::fleet::refs::FleetWorkspaceRef::new(host("beta"), "ws_1");
    let rect = fleet_hit(&state, &FleetSidebarHit::Workspace(workspace));

    let outcome = click(&mut state, rect);

    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Fleet(FleetShellAction::SwitchHost {
                host,
                then_focus: Some(FleetFocusTarget::Workspace(workspace)),
            }) if host.as_str() == "beta" && workspace == "ws_1"
        )),
        "{:?}",
        outcome.actions
    );
}

#[test]
fn clicking_the_active_hosts_agent_still_focuses_it_here() {
    let mut state = console(&[]);
    let _ = screen(&mut state);
    let (rect, pane_id) = state
        .hits
        .agents
        .first()
        .cloned()
        .expect("the active host's agent row is a plain hit");
    assert_eq!(pane_id, "pane_9");

    let outcome = click(&mut state, rect);

    assert!(
        outcome
            .actions
            .iter()
            .any(|action| matches!(action, ClientShellAction::Endpoint { .. })),
        "the active host's rows keep focusing: {:?}",
        outcome.actions
    );
    assert!(
        !outcome
            .actions
            .iter()
            .any(|action| matches!(action, ClientShellAction::Fleet(_))),
        "no switch for the host already active: {:?}",
        outcome.actions
    );
}

#[test]
fn clicking_the_header_glyph_collapses_and_the_rest_of_it_switches() {
    let mut state = console(&[]);
    let _ = screen(&mut state);
    let collapse = fleet_hit(&state, &FleetSidebarHit::HostCollapse(host("beta")));
    let header = fleet_hit(&state, &FleetSidebarHit::HostHeader(host("beta")));

    let outcome = click(&mut state, collapse);
    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Fleet(FleetShellAction::ToggleCollapsed(id)) if id.as_str() == "beta"
        )),
        "{:?}",
        outcome.actions
    );

    let _ = screen(&mut state);
    // Away from the glyph cell, the header is the switch target.
    let outcome = click(
        &mut state,
        Rect::new(header.x + 4, header.y, 1, header.height),
    );
    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Fleet(FleetShellAction::SwitchHost { host, then_focus: None })
                if host.as_str() == "beta"
        )),
        "{:?}",
        outcome.actions
    );
}

#[test]
fn the_header_of_the_host_already_active_is_not_a_switch() {
    let mut state = console(&[]);
    let _ = screen(&mut state);
    let header = fleet_hit(&state, &FleetSidebarHit::HostHeader(host("alpha")));

    let outcome = click(
        &mut state,
        Rect::new(header.x + 4, header.y, 1, header.height),
    );

    assert!(
        !outcome
            .actions
            .iter()
            .any(|action| matches!(action, ClientShellAction::Fleet(_))),
        "{:?}",
        outcome.actions
    );
}

#[test]
fn a_host_disabled_in_the_config_is_listed_but_never_a_switch_target() {
    let mut fleet = FleetState::new(vec![spec("alpha", true), spec("beta", false)]);
    fleet.apply(
        &host("alpha"),
        HostEvent::Connected {
            server_version: "0.8.2-fork".into(),
            methods: vec!["pane.focus".into()],
        },
    );
    fleet.apply(
        &host("alpha"),
        HostEvent::Snapshot(Box::new(host_snapshot("alpha", "boot-1"))),
    );
    fleet.set_active_host(Some(host("alpha")));
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(host_snapshot("alpha", "boot-1")));
    state.set_pane_surface(surface());
    state.fleet_sidebar_update(model_for(&fleet, &[]), host("alpha"), None);

    let text = screen(&mut state);
    assert!(
        text.contains("beta"),
        "a disabled host is still visible:\n{text}"
    );

    let header = fleet_hit(&state, &FleetSidebarHit::HostHeader(host("beta")));
    let outcome = click(
        &mut state,
        Rect::new(header.x + 4, header.y, 1, header.height),
    );
    assert!(
        !outcome
            .actions
            .iter()
            .any(|action| matches!(action, ClientShellAction::Fleet(_))),
        "a disabled host must not become the routing target: {:?}",
        outcome.actions
    );
}

#[test]
fn a_host_switch_drops_every_id_that_belonged_to_the_previous_host() {
    let mut state = console(&[]);
    let _ = screen(&mut state);
    assert!(!state.hits.agents.is_empty());
    assert!(state.snapshot.is_some());

    state.reset_for_host_switch();

    // The mis-route this exists to prevent: `hits.agents` holds bare pane ids
    // of the machine the console just left.
    assert!(state.hits.agents.is_empty());
    assert!(state.hits.workspaces.is_empty());
    assert!(state.hits.fleet_rows.is_empty());
    assert!(state.snapshot.is_none());
    assert!(state.pane_surface.is_none());
    assert!(state.pending_requests.is_empty());
    assert!(state.endpoint_methods.is_none());
    // The console keeps drawing — its chrome, from the placeholder — but
    // nothing of the previous host's projection or surface survives in it.
    let text = screen(&mut state);
    assert!(
        !text.contains("alpha-space"),
        "the old host's own rows are gone:\n{text}"
    );
    assert!(state.hits.panes.is_empty());
    assert!(state.hits.agents.is_empty());
    assert!(state.hits.workspaces.is_empty());
}

#[test]
fn the_focus_a_switch_asks_for_is_exactly_one_request_on_the_new_host() {
    let mut state = console(&[]);
    let _ = screen(&mut state);

    let outcome = state.request_fleet_focus(&FleetFocusTarget::Pane("pane_9".into()));

    let endpoints = outcome
        .actions
        .iter()
        .filter(|action| matches!(action, ClientShellAction::Endpoint { .. }))
        .count();
    assert_eq!(endpoints, 1, "{:?}", outcome.actions);
}

#[test]
fn the_switching_marker_appears_on_the_header_and_clears_in_place() {
    let fleet = fleet_state();
    let mut state = console(&[]);
    state.fleet_sidebar_update(model_for(&fleet, &[]), host("alpha"), Some(host("beta")));
    let text = screen(&mut state);
    assert!(
        text.contains('…'),
        "the host being switched to is marked:\n{text}"
    );

    assert!(state.set_fleet_switching(None), "the marker changed");
    assert!(!state.set_fleet_switching(None), "and only once");
    let text = screen(&mut state);
    assert!(
        !text.contains('…'),
        "the marker goes when the switch lands:\n{text}"
    );
}

#[test]
fn a_reason_line_makes_a_header_two_rows_and_the_cache_follows_the_model() {
    let mut fleet = fleet_state();
    let up = model_for(&fleet, &[]);
    let mut state = console(&[]);
    state.fleet_sidebar_update(up, host("alpha"), None);
    let two_line = state
        .fleet
        .as_ref()
        .map(|fleet| fleet.header_height(2))
        .expect("a fleet console");
    assert_eq!(
        two_line, 2,
        "an unavailable host draws its reason underneath"
    );

    fleet.apply(
        &host("gamma"),
        HostEvent::Connected {
            server_version: "0.8.2-fork".into(),
            methods: Vec::new(),
        },
    );
    state.fleet_sidebar_update(model_for(&fleet, &[]), host("alpha"), None);
    let one_line = state
        .fleet
        .as_ref()
        .map(|fleet| fleet.header_height(2))
        .expect("a fleet console");
    assert_eq!(one_line, 1, "the height cache follows the rebuilt model");
}

#[test]
fn five_hosts_of_fifteen_agents_can_be_scrolled_down_to_the_last_row() {
    let specs = (1..=5)
        .map(|index| spec(&format!("lab-{index}"), true))
        .collect::<Vec<_>>();
    let mut fleet = FleetState::new(specs);
    for index in 1..=5 {
        let id = host(&format!("lab-{index}"));
        fleet.apply(
            &id,
            HostEvent::Connected {
                server_version: "0.8.2-fork".into(),
                methods: Vec::new(),
            },
        );
        let mut snapshot = host_snapshot(&format!("lab-{index}"), &format!("boot-{index}"));
        snapshot.agents = (0..15)
            .map(|agent_index| {
                agent(
                    &format!("pane_{agent_index}"),
                    &format!("lab-{index}-agent-{agent_index}"),
                    AgentStatus::Idle,
                    agent_index,
                )
            })
            .collect();
        fleet.apply(&id, HostEvent::Snapshot(Box::new(snapshot)));
    }
    fleet.set_active_host(Some(host("lab-1")));

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(host_snapshot("lab-1", "boot-1")));
    state.set_pane_surface(surface());
    state.fleet_sidebar_update(model_for(&fleet, &[]), host("lab-1"), None);

    let _ = screen(&mut state);
    let max_scroll = state.hits.agent_max_scroll;
    assert!(max_scroll > 0, "5 x 15 agents do not fit in 40 rows");
    state.agent_scroll = max_scroll;
    let text = screen(&mut state);
    assert!(
        text.contains("lab-5-agent-14"),
        "scrolling reaches the last host's last agent:\n{text}"
    );
}

#[test]
fn a_switch_to_a_host_without_a_surface_still_draws_the_console_and_says_so() {
    let mut state = console(&[]);
    let _ = screen(&mut state);

    // The loop's order: reset, install the new host's projection (beta has
    // one), rebuild the model with the switch pending. No surface yet.
    state.reset_for_host_switch();
    state.set_snapshot(Box::new(host_snapshot("beta", "boot-beta")));
    state.fleet_sidebar_update(model_switched_to("beta"), host("beta"), Some(host("beta")));
    let text = screen(&mut state);
    assert!(
        text.contains("switching to beta…"),
        "the pane area says what it is waiting for:\n{text}"
    );
    assert!(
        text.contains("beta-space"),
        "the new host's own rows come from its projection:\n{text}"
    );
    assert!(
        state.hits.panes.is_empty(),
        "nothing of the pane area is clickable until a real surface draws it"
    );
    // The reason this exists: a switch to a host that never answers must
    // leave every other host one click away.
    let _ = fleet_hit(&state, &FleetSidebarHit::HostHeader(host("alpha")));

    // A host with no projection at all draws from the placeholder.
    state.reset_for_host_switch();
    state.fleet_sidebar_update(
        model_switched_to("gamma"),
        host("gamma"),
        Some(host("gamma")),
    );
    let text = screen(&mut state);
    assert!(
        text.contains("switching to gamma…") && text.contains("▾ alpha"),
        "the chrome draws with no projection at all:\n{text}"
    );
    let _ = fleet_hit(&state, &FleetSidebarHit::HostHeader(host("alpha")));

    // And once the switch lands, the notice is gone.
    assert!(state.set_fleet_switching(None));
    let text = screen(&mut state);
    assert!(!text.contains("switching to"), "{text}");
}

#[test]
fn a_single_host_client_still_draws_nothing_before_its_server_does() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(host_snapshot("alpha", "boot-1")));
    assert!(
        state.compose(120, 40).is_none(),
        "the placeholder is a fleet console's, not the single-host client's"
    );
}

#[test]
fn a_host_switch_closes_the_overlay_and_forgets_the_previous_hosts_timers() {
    let mut state = console(&[]);
    let _ = screen(&mut state);
    state.overlay = Some(ClientShellOverlay::Onboarding);
    state.selection_autoscroll_deadline = Some(std::time::Instant::now());
    state.pending_integration_installs = 2;
    state.workspace_press = Some(ClientWorkspacePress {
        workspace_id: "ws_1".into(),
        start_column: 3,
        start_row: 3,
    });

    state.reset_for_host_switch();

    assert!(
        state.overlay.is_none(),
        "an overlay accepted after the switch would carry the old host's ids"
    );
    assert!(state.selection_autoscroll_deadline.is_none());
    assert_eq!(state.pending_integration_installs, 0);
    assert!(
        state.workspace_press.is_none(),
        "a press released after the switch would focus the old workspace id"
    );
}

#[test]
fn fleet_rows_never_leave_the_sidebar_bodies() {
    fn inside(rect: Rect, body: Rect) -> bool {
        rect.x >= body.x
            && rect.right() <= body.right()
            && rect.y >= body.y
            && rect.bottom() <= body.bottom()
    }
    for rows in [40u16, 12, 9, 8, 7, 6, 5, 4, 3] {
        let mut state = console(&[]);
        if state.compose(120, rows).is_none() {
            continue;
        }
        let (spaces, agents) = (state.hits.workspace_body, state.hits.agent_body);
        for (rect, hit) in &state.hits.fleet_rows {
            assert!(
                inside(*rect, spaces) || inside(*rect, agents),
                "{hit:?} at {rect:?} is outside {spaces:?} and {agents:?} at {rows} rows: \
                 a click there would switch hosts from the footer"
            );
        }
    }
}
