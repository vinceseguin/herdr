//! The Fleet console when the machine it is pointed at is not there.
//!
//! Two invariants are load-bearing here and neither is visible from the code
//! that draws:
//!
//! * The pane area must stop showing the last frame of a host that dropped.
//!   That frame is a live terminal as far as the user is concerned — it has a
//!   cursor, it is clickable, it invites typing — and it describes a machine
//!   that is not answering.
//! * Input aimed at that pane must be *dropped*, never queued. A console that
//!   buffered it would replay a burst of keystrokes into whatever the host
//!   came back as, or — after a switch — into a different machine entirely.

use super::*;

use std::collections::HashSet;

use crate::client::shell::{FleetShellAction, FleetSidebarHit};
use crate::fleet::hosts::{HostId, HostKind, HostSpec};
use crate::fleet::sidebar::FleetSidebarModel;
use crate::fleet::state::{FleetState, HostEvent};

fn host(name: &str) -> HostId {
    HostId::new(name).expect("valid host id")
}

fn spec(name: &str) -> HostSpec {
    HostSpec {
        id: host(name),
        kind: HostKind::Local {
            session: Some(name.to_string()),
        },
        enabled: true,
    }
}

/// One host's projection: a workspace named after it.
fn host_snapshot(label: &str, boot: &str) -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.boot_id = boot.into();
    snapshot.workspaces[0].label = format!("{label}-space");
    snapshot
}

fn connect(state: &mut FleetState, id: &str, boot: &str) {
    state.apply(
        &host(id),
        HostEvent::Connected {
            server_version: "0.8.2-fork".into(),
            methods: vec!["pane.focus".into()],
        },
    );
    state.apply(
        &host(id),
        HostEvent::Snapshot(Box::new(host_snapshot(id, boot))),
    );
}

/// `alpha` (active) and `beta`, both connected with a projection.
fn fleet_state() -> FleetState {
    let mut state = FleetState::new(vec![spec("alpha"), spec("beta")]);
    connect(&mut state, "alpha", "boot-1");
    connect(&mut state, "beta", "boot-beta");
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

/// A console showing `alpha`, with a real projection and a real surface.
fn console(fleet: &FleetState) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(host_snapshot("alpha", "boot-1")));
    state.set_pane_surface(surface());
    state.set_endpoint_methods(Some(vec!["pane.focus".into()]));
    state.fleet_sidebar_update(model_for(fleet), host("alpha"), None);
    state
}

/// Install a rebuilt model, the way the client loop's `apply_changes` does.
fn refresh(state: &mut ClientShellState, fleet: &FleetState, switching_to: Option<HostId>) {
    state.fleet_sidebar_update(model_for(fleet), host("alpha"), switching_to);
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

fn type_x(state: &mut ClientShellState) -> ClientShellInput {
    state.handle_input_bytes(b"x")
}

fn pane_inputs(outcome: &ClientShellInput) -> Vec<String> {
    outcome
        .requests
        .iter()
        .filter_map(|request| match request {
            ClientMessage::ClientShellPaneInput { pane_id, .. } => Some(pane_id.clone()),
            _ => None,
        })
        .collect()
}

fn drop_active_host(fleet: &mut FleetState) {
    fleet.apply(
        &host("alpha"),
        HostEvent::Unavailable {
            reason: "host closed the connection".into(),
            retry_in: Some(std::time::Duration::from_secs(2)),
        },
    );
}

#[test]
fn the_active_host_dropping_replaces_its_frame_with_a_reconnect_notice() {
    let mut fleet = fleet_state();
    let mut state = console(&fleet);
    let text = screen(&mut state);
    assert!(
        text.contains("LIVE"),
        "the host's frame is on screen:\n{text}"
    );

    drop_active_host(&mut fleet);
    refresh(&mut state, &fleet, None);
    let text = screen(&mut state);

    assert!(
        text.contains("alpha · reconnecting · host closed the connection"),
        "the pane area names the host, its state and why:\n{text}"
    );
    assert!(
        !text.contains("LIVE"),
        "and stops showing a frame from a machine that is not answering:\n{text}"
    );
    assert!(
        state.hits.panes.is_empty(),
        "nothing in the pane area is clickable while the notice shows"
    );
    // The rest of the console keeps working: this is what makes a host
    // failure host-local rather than a dead console.
    assert!(
        text.contains("beta-space"),
        "the other host's rows are still drawn:\n{text}"
    );
    let _ = fleet_row(&state, &FleetSidebarHit::HostHeader(host("beta")));
}

#[test]
fn input_typed_while_the_active_host_is_down_is_dropped_and_never_queued() {
    let mut fleet = fleet_state();
    let mut state = console(&fleet);
    let before = type_x(&mut state);
    assert_eq!(
        pane_inputs(&before),
        vec!["pane_1".to_string()],
        "a connected host takes what is typed"
    );

    drop_active_host(&mut fleet);
    refresh(&mut state, &fleet, None);
    let _ = screen(&mut state);

    let outcome = type_x(&mut state);
    assert!(
        pane_inputs(&outcome).is_empty(),
        "nothing pane-bound leaves the console: {:?}",
        outcome.requests
    );
    assert!(
        outcome.repaint,
        "the notice is redrawn, so the keystroke visibly went nowhere"
    );

    // The host comes back. What was typed while it was down must not arrive
    // now: the shell kept none of it.
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
    refresh(&mut state, &fleet, None);
    state.set_pane_surface(surface());
    let after = type_x(&mut state);
    assert_eq!(
        pane_inputs(&after),
        vec!["pane_1".to_string()],
        "one keystroke in, one keystroke out — not two"
    );
    assert_eq!(
        after
            .requests
            .iter()
            .filter(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. }))
            .count(),
        1,
        "the drop was a drop, not a delay: {:?}",
        after.requests
    );
}

#[test]
fn a_host_that_never_answered_says_connecting_rather_than_reconnecting() {
    let mut fleet = FleetState::new(vec![spec("alpha"), spec("beta")]);
    connect(&mut fleet, "beta", "boot-beta");
    fleet.set_active_host(Some(host("alpha")));
    fleet.apply(&host("alpha"), HostEvent::Connecting { attempt: 1 });

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.fleet_sidebar_update(model_for(&fleet), host("alpha"), None);
    let text = screen(&mut state);

    assert!(
        text.contains("alpha · connecting"),
        "a first attempt is not a retry:\n{text}"
    );
    assert!(
        !text.contains("reconnecting"),
        "the supervisor numbers its first attempt 1; that is not attempt 1 of a retry:\n{text}"
    );

    // Once it has been up, a retry says so and counts.
    connect(&mut fleet, "alpha", "boot-1");
    fleet.apply(&host("alpha"), HostEvent::Connecting { attempt: 3 });
    state.fleet_sidebar_update(model_for(&fleet), host("alpha"), None);
    let text = screen(&mut state);
    assert!(
        text.contains("alpha · reconnecting (attempt 3)"),
        "a retry counts:\n{text}"
    );
}

#[test]
fn an_incompatible_active_host_says_what_is_wrong_with_it() {
    let mut fleet = fleet_state();
    fleet.apply(
        &host("alpha"),
        HostEvent::Incompatible {
            generation: Some(2),
            reason: "endpoint generation 2".into(),
        },
    );
    let mut state = console(&fleet);
    refresh(&mut state, &fleet, None);
    let text = screen(&mut state);

    assert!(
        text.contains("alpha · incompatible · endpoint generation 2"),
        "an incompatible host is not something to wait for:\n{text}"
    );
    assert!(
        pane_inputs(&type_x(&mut state)).is_empty(),
        "and it takes no input either"
    );
}

#[test]
fn a_switch_still_reads_as_a_switch_only_while_its_host_is_up() {
    let mut fleet = fleet_state();
    let mut state = console(&fleet);

    // Switching to a host that is connected: the console is waiting for a
    // frame, and says so.
    state.fleet_sidebar_update(model_for(&fleet), host("beta"), Some(host("beta")));
    let text = screen(&mut state);
    assert!(text.contains("switching to beta…"), "{text}");

    // The same switch, to a host that is not coming back. "switching to…"
    // would be a console waiting forever on a machine it can see is down.
    fleet.apply(
        &host("beta"),
        HostEvent::Unavailable {
            reason: "connection refused".into(),
            retry_in: None,
        },
    );
    state.fleet_sidebar_update(model_for(&fleet), host("beta"), Some(host("beta")));
    let text = screen(&mut state);
    assert!(
        text.contains("beta · reconnecting · connection refused"),
        "the notice follows the host, not the intent:\n{text}"
    );
    assert!(!text.contains("switching to"), "{text}");
}

#[test]
fn a_click_on_another_host_still_switches_while_the_active_host_is_down() {
    let mut fleet = fleet_state();
    let mut state = console(&fleet);
    drop_active_host(&mut fleet);
    refresh(&mut state, &fleet, None);
    let _ = screen(&mut state);

    let rect = fleet_row(&state, &FleetSidebarHit::HostHeader(host("beta")));
    let outcome =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + rect.width / 2,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })]);

    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Fleet(FleetShellAction::SwitchHost { host: target, .. })
                if *target == host("beta")
        )),
        "leaving a host that is down is the one thing that must always work: {:?}",
        outcome.actions
    );
}

#[test]
fn a_narrow_sidebar_truncates_the_header_but_never_the_notice() {
    let mut fleet = fleet_state();
    let mut state = console(&fleet);
    // The header line the sidebar can afford at its minimum width; the pane
    // area is the surface that has room for the whole story.
    state.sidebar_width = 25;
    drop_active_host(&mut fleet);
    refresh(&mut state, &fleet, None);
    let text = screen(&mut state);

    assert!(
        text.contains("▾ alpha · unavailable"),
        "the header keeps the host id and its state, in that order:\n{text}"
    );
    // The reason under a header is what a 25-column sidebar cannot hold. That
    // is why the pane area, which has the whole width, carries it in full —
    // the console must not depend on the sidebar to say why it is empty.
    assert!(
        text.contains("host closed the connec") && !text.contains("  host closed the connection"),
        "a narrow sidebar cuts the reason short:\n{text}"
    );
    assert!(
        text.contains("alpha · reconnecting · host closed the connection"),
        "and the pane area carries the full reason whatever the sidebar fits:\n{text}"
    );
}

#[test]
fn a_console_reveals_the_active_hosts_row_and_not_the_row_that_many_lines_down() {
    let fleet = fleet_state();
    let mut state = console(&fleet);
    let _ = screen(&mut state);

    // alpha is the first group: its header, then its own workspace rows.
    assert_eq!(
        state.fleet_active_spaces_offset(),
        1,
        "one header before the active host's own rows"
    );

    // With alpha second, beta's header and its one workspace row precede it.
    let mut reordered = FleetState::new(vec![spec("beta"), spec("alpha")]);
    connect(&mut reordered, "alpha", "boot-1");
    connect(&mut reordered, "beta", "boot-beta");
    reordered.set_active_host(Some(host("alpha")));
    state.fleet_sidebar_update(model_for(&reordered), host("alpha"), None);
    assert_eq!(
        state.fleet_active_spaces_offset(),
        3,
        "beta's header, beta's workspace, then alpha's header"
    );

    // A single-host client has no groups and no offset.
    let single = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    assert_eq!(single.fleet_active_spaces_offset(), 0);
}

#[test]
fn a_drag_over_another_hosts_rows_has_no_drop_target() {
    let fleet = fleet_state();
    let mut state = console(&fleet);
    let _ = screen(&mut state);

    let beta = fleet_row(&state, &FleetSidebarHit::HostHeader(host("beta")));
    assert!(
        state.is_fleet_row(beta.y),
        "beta's header is a fleet row, not a workspace slot"
    );
    let alpha_rows = state
        .hits
        .workspaces
        .iter()
        .map(|hit| hit.rect.y)
        .collect::<Vec<_>>();
    assert!(
        !alpha_rows.is_empty() && alpha_rows.iter().all(|row| !state.is_fleet_row(*row)),
        "the active host's own rows stay draggable: {alpha_rows:?}"
    );
}

fn fleet_row(state: &ClientShellState, want: &FleetSidebarHit) -> Rect {
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
