//! The Fleet console's host picker overlay.
//!
//! The picker changes the machine every later keystroke lands on, so the
//! assertions here are about what it may and may not emit: exactly one
//! `SwitchHost` for a host that can take over, and nothing at all for the host
//! that is already active, for a host `[fleet]` disables, or for a client that
//! is not a console.

use super::*;

use std::collections::HashSet;

use crate::client::shell::FleetShellAction;
use crate::fleet::hosts::{HostId, HostKind, HostSpec};
use crate::fleet::sidebar::FleetSidebarModel;
use crate::fleet::state::{FleetState, HostEvent};

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

/// `alpha` connected and active, `beta` connected, `gamma` down, `off`
/// disabled in `[fleet]`.
fn fleet_state() -> FleetState {
    let mut state = FleetState::new(vec![
        spec("alpha", true),
        spec("beta", true),
        spec("gamma", true),
        spec("off", false),
    ]);
    for id in ["alpha", "beta"] {
        state.apply(
            &host(id),
            HostEvent::Connected {
                server_version: "0.8.2-fork".into(),
                methods: vec!["pane.focus".into()],
            },
        );
        let mut projection = snapshot();
        projection.boot_id = format!("boot-{id}");
        state.apply(&host(id), HostEvent::Snapshot(Box::new(projection)));
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

fn model_for(state: &FleetState) -> FleetSidebarModel {
    let mut model = FleetSidebarModel::default();
    model.rebuild(
        state,
        &HashSet::new(),
        crate::config::AgentPanelSortConfig::Priority,
    );
    model
}

/// A console showing `alpha`.
fn console() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.fleet_sidebar_update(model_for(&fleet_state()), host("alpha"), None);
    state
}

/// A plain single-host client: no fleet, no picker.
fn single_host_client() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn key(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code,
        KeyModifiers::empty(),
    ))])
}

/// The console's default binding for the picker, pressed as prefix + key.
fn open_with_the_binding(state: &mut ClientShellState) -> ClientShellInput {
    let prefix = state.config.keybinds.prefix;
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        prefix.0, prefix.1,
    ))]);
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        KeyCode::Char('F'),
        KeyModifiers::SHIFT,
    ))])
}

fn switches(outcome: &ClientShellInput) -> Vec<FleetShellAction> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Fleet(action) => Some(action.clone()),
            _ => None,
        })
        .collect()
}

fn selected(state: &ClientShellState) -> usize {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::HostPicker(picker)) => picker.selected,
        other => panic!("the picker is not open: {other:?}"),
    }
}

fn click(state: &mut ClientShellState, column: u16, row: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })])
}

fn screen(state: &mut ClientShellState) -> String {
    screen_at(state, 120, 40)
}

fn screen_at(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    let frame = state.compose(cols, rows).expect("the console composes");
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

#[test]
fn the_default_binding_opens_the_picker_on_every_configured_host() {
    let mut state = console();

    let outcome = open_with_the_binding(&mut state);

    assert!(outcome.repaint);
    assert_eq!(
        state.overlay.as_ref().map(ClientShellOverlay::kind),
        Some(ClientShellOverlayKind::HostPicker)
    );
    let text = screen(&mut state);
    assert!(text.contains("Fleet"), "the picker names itself:\n{text}");
    for expected in [
        "alpha  connected 0.8.2-fork",
        "beta  connected 0.8.2-fork",
        "gamma  unavailable  connection refused",
        "off  unavailable  host disabled in [fleet]",
    ] {
        assert!(text.contains(expected), "{expected:?} missing:\n{text}");
    }
    assert!(
        text.contains("● alpha"),
        "the routing target is marked:\n{text}"
    );
    assert!(
        text.contains("4 hosts"),
        "every configured host is listed:\n{text}"
    );
    assert!(
        text.contains("local"),
        "each row names its transport:\n{text}"
    );
}

#[test]
fn the_picker_opens_on_the_active_host_and_enter_on_another_switches_once() {
    let mut state = console();
    open_with_the_binding(&mut state);

    let moved = key(&mut state, KeyCode::Down);
    assert!(moved.repaint);
    assert!(switches(&moved).is_empty(), "moving is not switching");

    let accepted = key(&mut state, KeyCode::Enter);

    assert_eq!(
        switches(&accepted),
        vec![FleetShellAction::SwitchHost {
            host: host("beta"),
            then_focus: None,
        }]
    );
    assert!(state.overlay.is_none(), "accepting closes the picker");
    // A focus request would carry beta's ids to whichever host is active when
    // it is written; the switch is the whole action.
    assert!(accepted.requests.is_empty(), "{:?}", accepted.requests);
    assert!(
        !accepted
            .actions
            .iter()
            .any(|action| matches!(action, ClientShellAction::Endpoint { .. })),
        "{:?}",
        accepted.actions
    );
}

#[test]
fn a_digit_jumps_to_that_host_and_enter_confirms_it() {
    let mut state = console();
    open_with_the_binding(&mut state);

    // Third row: `gamma`, which is down. Selecting it is allowed — switching
    // to a host that is not connected is how you watch it come back.
    let jumped = key(&mut state, KeyCode::Char('3'));
    assert!(jumped.repaint);
    assert!(switches(&jumped).is_empty(), "a digit selects, never acts");

    let accepted = key(&mut state, KeyCode::Enter);

    assert_eq!(
        switches(&accepted),
        vec![FleetShellAction::SwitchHost {
            host: host("gamma"),
            then_focus: None,
        }]
    );
}

#[test]
fn a_digit_past_the_last_host_changes_nothing() {
    let mut state = console();
    open_with_the_binding(&mut state);

    let ignored = key(&mut state, KeyCode::Char('9'));

    assert!(!ignored.repaint, "there is no ninth host to select");
    let accepted = key(&mut state, KeyCode::Enter);
    assert!(
        switches(&accepted).is_empty(),
        "the selection stayed on the active host: {:?}",
        switches(&accepted)
    );
}

#[test]
fn accepting_the_active_host_closes_without_switching() {
    let mut state = console();
    open_with_the_binding(&mut state);

    let accepted = key(&mut state, KeyCode::Enter);

    assert!(switches(&accepted).is_empty());
    assert!(state.overlay.is_none());
}

#[test]
fn a_host_disabled_in_fleet_is_listed_but_is_not_a_switch_target() {
    let mut state = console();
    open_with_the_binding(&mut state);
    key(&mut state, KeyCode::Char('4'));

    let accepted = key(&mut state, KeyCode::Enter);

    assert!(
        switches(&accepted).is_empty(),
        "`set_active_host` refuses a disabled host: {:?}",
        switches(&accepted)
    );
    assert_eq!(
        state.overlay.as_ref().map(ClientShellOverlay::kind),
        Some(ClientShellOverlayKind::HostPicker),
        "the picker stays open rather than swallowing the keystroke"
    );
}

#[test]
fn esc_closes_the_picker_without_touching_the_fleet() {
    let mut state = console();
    open_with_the_binding(&mut state);
    key(&mut state, KeyCode::Down);

    let closed = key(&mut state, KeyCode::Esc);

    assert!(state.overlay.is_none());
    assert!(switches(&closed).is_empty());
    assert!(closed.repaint);
}

#[test]
fn a_click_on_a_row_switches_and_a_click_outside_closes() {
    let mut state = console();
    open_with_the_binding(&mut state);
    let _ = screen(&mut state);

    let (rect, _) = state
        .hits
        .host_picker_rows
        .iter()
        .find(|(_, index)| *index == 1)
        .copied()
        .expect("the picker drew a row for the second host");
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + rect.width / 2,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })]);

    assert_eq!(
        switches(&outcome),
        vec![FleetShellAction::SwitchHost {
            host: host("beta"),
            then_focus: None,
        }]
    );

    open_with_the_binding(&mut state);
    let _ = screen(&mut state);
    let outside = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);

    assert!(
        state.overlay.is_none(),
        "a press outside the popup closes it"
    );
    assert!(switches(&outside).is_empty());
}

#[test]
fn an_open_picker_follows_the_host_that_drops_under_it() {
    let mut state = console();
    open_with_the_binding(&mut state);
    assert!(screen(&mut state).contains("beta  connected"));

    let mut fleet = fleet_state();
    fleet.apply(
        &host("beta"),
        HostEvent::Unavailable {
            reason: "connection refused".into(),
            retry_in: None,
        },
    );
    state.fleet_sidebar_update(model_for(&fleet), host("alpha"), None);

    let text = screen(&mut state);
    assert!(
        text.contains("beta  unavailable  connection refused"),
        "the open picker redrew from the new model:\n{text}"
    );
    assert_eq!(
        state.overlay.as_ref().map(ClientShellOverlay::kind),
        Some(ClientShellOverlayKind::HostPicker),
        "a fleet change must not close the picker"
    );
}

/// A console whose config binds the picker to `prefix+h`.
fn console_with_override() -> ClientShellState {
    let config: Config = toml::from_str(
        r#"
[fleet.keys]
host_picker = "prefix+h"
"#,
    )
    .expect("the override parses");
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.fleet_sidebar_update(model_for(&fleet_state()), host("alpha"), None);
    state
}

fn press_prefix_then(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    let prefix = state.config.keybinds.prefix;
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        prefix.0, prefix.1,
    ))]);
    key(state, code)
}

#[test]
fn a_configured_binding_opens_the_picker_and_the_default_one_no_longer_does() {
    let mut state = console_with_override();

    press_prefix_then(&mut state, KeyCode::Char('h'));
    assert_eq!(
        state.overlay.as_ref().map(ClientShellOverlay::kind),
        Some(ClientShellOverlayKind::HostPicker),
        "[fleet.keys] host_picker = \"prefix+h\" must be what opens it"
    );
    key(&mut state, KeyCode::Esc);

    open_with_the_binding(&mut state);
    assert!(
        state.overlay.is_none(),
        "the default combo is the user's to take back"
    );
}

#[test]
fn a_configured_binding_survives_the_servers_command_list() {
    let mut state = console_with_override();

    // Every connect republishes the server's custom commands, and the client
    // recompiles its keymap from its *local* config to merge them. A fleet
    // binding that did not travel with `[keys]` there would be silently
    // replaced by this build's default — the console would stop answering the
    // key the user configured, with nothing in the diagnostics to say why.
    state
        .config
        .apply_snapshot_keybindings(None, &[])
        .expect("local keybindings recompile");

    press_prefix_then(&mut state, KeyCode::Char('h'));
    assert_eq!(
        state.overlay.as_ref().map(ClientShellOverlay::kind),
        Some(ClientShellOverlayKind::HostPicker)
    );
}

#[test]
fn the_binding_is_a_no_op_outside_fleet_mode() {
    let mut state = single_host_client();

    let outcome = open_with_the_binding(&mut state);

    assert!(state.overlay.is_none(), "no fleet, no picker");
    assert!(switches(&outcome).is_empty());
    assert!(outcome.requests.is_empty(), "{:?}", outcome.requests);
}

#[test]
fn home_and_end_jump_to_the_first_and_last_host() {
    let mut state = console();
    open_with_the_binding(&mut state);
    key(&mut state, KeyCode::Down);

    key(&mut state, KeyCode::End);
    assert_eq!(selected(&state), 3);

    key(&mut state, KeyCode::Home);
    assert_eq!(selected(&state), 0);

    // Moving past either end stays put rather than wrapping or panicking.
    key(&mut state, KeyCode::Up);
    assert_eq!(selected(&state), 0);
}

#[test]
fn a_click_that_names_no_current_row_changes_nothing() {
    let mut state = console();
    open_with_the_binding(&mut state);
    key(&mut state, KeyCode::Down);
    let _ = screen(&mut state);
    // A hit rect from an older frame that no longer names a row: it must not
    // fall through to the highlighted row and switch to it.
    let (rect, _) = state.hits.host_picker_rows[1];
    state.hits.host_picker_rows = vec![(rect, 42)];

    let outcome = click(&mut state, rect.x + 1, rect.y);

    assert!(switches(&outcome).is_empty(), "{:?}", switches(&outcome));
    assert_eq!(
        state.overlay.as_ref().map(ClientShellOverlay::kind),
        Some(ClientShellOverlayKind::HostPicker),
        "the picker stays open with its selection intact"
    );
    assert_eq!(selected(&state), 1);
}

#[test]
fn a_press_in_the_opening_batch_cannot_use_an_earlier_frames_geometry() {
    let mut state = console();
    open_with_the_binding(&mut state);
    let _ = screen(&mut state);
    let (rect, _) = state
        .hits
        .host_picker_rows
        .iter()
        .find(|(_, index)| *index == 1)
        .copied()
        .expect("the picker drew a row for the second host");
    key(&mut state, KeyCode::Esc);

    // Open again and click in the same batch, before any frame has drawn the
    // new picker. The old frame's row rects must not resolve the press.
    let prefix = state.config.keybinds.prefix;
    let outcome = state.handle_raw_events(vec![
        RawInputEvent::Key(crate::input::TerminalKey::new(prefix.0, prefix.1)),
        RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char('F'),
            KeyModifiers::SHIFT,
        )),
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + rect.width / 2,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        }),
    ]);

    assert!(switches(&outcome).is_empty(), "{:?}", switches(&outcome));
    assert!(
        state.overlay.is_none(),
        "with no picker drawn yet, the press is outside it and closes it"
    );
}

#[test]
fn a_refresh_keeps_the_highlight_on_the_same_host_by_id() {
    let mut state = console();
    open_with_the_binding(&mut state);
    key(&mut state, KeyCode::Down);
    assert_eq!(selected(&state), 1, "beta");

    // Rows arrive in a different order than the open picker holds them. The
    // fleet never reorders hosts today; the guard is by id so that stays a
    // fact about the fleet and not a load-bearing assumption of the picker.
    let mut model = model_for(&fleet_state());
    model.picker.reverse();
    state.fleet_sidebar_update(model, host("alpha"), None);

    assert_eq!(selected(&state), 2, "beta, where it now sits");
    let accepted = key(&mut state, KeyCode::Enter);
    assert_eq!(
        switches(&accepted),
        vec![FleetShellAction::SwitchHost {
            host: host("beta"),
            then_focus: None,
        }]
    );
}

#[test]
fn the_list_scrolls_to_keep_the_selection_visible_on_a_short_terminal() {
    let mut state = console();
    open_with_the_binding(&mut state);

    // Eight rows: the popup gets six, its panel four, and one of those is the
    // list body.
    let text = screen_at(&mut state, 60, 8);
    assert!(text.contains("alpha  connected"), "{text}");
    assert!(!text.contains("off  unavailable"), "{text}");

    key(&mut state, KeyCode::End);
    let text = screen_at(&mut state, 60, 8);
    assert!(text.contains("off  unavailable"), "{text}");
    assert!(!text.contains("alpha  connected"), "{text}");

    // The one drawn row is the one a click resolves to.
    assert_eq!(state.hits.host_picker_rows.len(), 1);
    assert_eq!(state.hits.host_picker_rows[0].1, 3);
}

#[test]
fn a_tiny_terminal_and_an_oversized_label_do_not_panic() {
    let mut state = console();
    open_with_the_binding(&mut state);
    for (cols, rows) in [(1, 1), (5, 5), (20, 5), (40, 7), (200, 3)] {
        let _ = state.compose(cols, rows);
    }
    key(&mut state, KeyCode::End);
    let _ = state.compose(20, 5);

    let mut fleet = fleet_state();
    fleet.apply(
        &host("gamma"),
        HostEvent::Unavailable {
            reason: "connection refused: ".repeat(40),
            retry_in: None,
        },
    );
    state.fleet_sidebar_update(model_for(&fleet), host("alpha"), None);

    let text = screen(&mut state);
    assert!(
        text.contains("gamma  unavailable  connection refused"),
        "the label is truncated, not wrapped or dropped:\n{text}"
    );
}
