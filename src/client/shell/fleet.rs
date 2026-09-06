//! The client shell's fleet half (fork).
//!
//! The shell keeps exactly one host — the active one — in `snapshot` and
//! `pane_surface` (locked decision (c)). Every *other* host reaches the screen
//! only through [`FleetShellState`]: a pre-rendered, cached row model the
//! client loop rebuilds when the fleet changes, never per frame.
//!
//! Two rules make click-to-switch safe, and both live here:
//!
//! * A clickable row for another host is host-qualified. `ShellHitMap::agents`
//!   and `ShellHitMap::workspaces` hold bare server-side ids, which are only
//!   ever the *active* host's, so a click on them can only ever focus on the
//!   machine the shell is showing. Everything else goes into
//!   `ShellHitMap::fleet_rows` as a [`FleetSidebarHit`], which carries the host.
//! * A host switch drops the shell's whole view of the old host
//!   ([`ClientShellState::reset_for_host_switch`]) — the hit map included, so a
//!   pane id from the old machine cannot be replayed against the new one.
//!
//! E2 PR 5. PR 6 adds the picker overlay on top of the same action, PR 7 the
//! notification target, PR 8 the reconnect notice.

use crate::fleet::hosts::HostId;
use crate::fleet::refs::{FleetPaneRef, FleetWorkspaceRef};
use crate::fleet::sidebar::FleetSidebarModel;

use super::{ClientShellAction, ClientShellInput, ClientShellState, ShellHitMap};

/// A clickable fleet row, always naming the host it belongs to.
///
/// Never a bare pane or workspace id: the whole point of this type is that a
/// click cannot be resolved against the wrong machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FleetSidebarHit {
    /// The host's header line: switch to it.
    HostHeader(HostId),
    /// The `▾`/`▸` cell of a header: show or hide the group's rows.
    HostCollapse(HostId),
    /// Another host's workspace row: switch, then focus it there.
    Workspace(FleetWorkspaceRef),
    /// Another host's agent row: switch, then focus it there.
    Agent(FleetPaneRef),
}

/// What the shell asks the client loop to do about the fleet.
///
/// The shell cannot switch hosts itself: the connector, the link and the fleet
/// state live in the loop, and they must change together
/// (`crate::client::fleet::switch_host`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FleetShellAction {
    /// Make `host` the routing target, then focus `then_focus` on it.
    SwitchHost {
        host: HostId,
        then_focus: Option<FleetFocusTarget>,
    },
    /// Show or hide one host group's rows.
    ToggleCollapsed(HostId),
    /// The agent panel's sort preference changed, so every group's agent order
    /// is stale (the model is built with that preference; see
    /// `FleetSidebarModel::rebuild`).
    SortChanged,
}

/// What to focus on the host being switched to. Ids are that host's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FleetFocusTarget {
    Workspace(String),
    Pane(String),
}

/// Everything the shell holds about hosts other than the active one.
pub(super) struct FleetShellState {
    /// Rows for every configured host, rebuilt by the loop on every fleet
    /// change and read — never derived — at draw time.
    pub(super) model: FleetSidebarModel,
    /// The host whose snapshot and surface the shell is showing.
    pub(super) active: HostId,
    /// A switch in flight: its first full surface has not arrived yet.
    pub(super) switching_to: Option<HostId>,
    /// `model.generation` the height cache below was built for.
    heights_generation: u64,
    /// Drawn height of each group's header, by group index.
    ///
    /// One or two: a host that is not connected gets a second, dimmed line
    /// with the reason. Every other fleet row is exactly one line, so this is
    /// the only height the renderer would otherwise have to derive per frame.
    header_heights: Vec<u16>,
}

impl FleetShellState {
    fn new(model: FleetSidebarModel, active: HostId, switching_to: Option<HostId>) -> Self {
        let mut state = Self {
            heights_generation: model.generation,
            header_heights: header_heights(&model),
            model,
            active,
            switching_to,
        };
        state.heights_generation = state.model.generation;
        state
    }

    /// Drawn height of one group's header row.
    ///
    /// Falls back to a single line when the cache is somehow stale, which
    /// keeps the layout arithmetic total rather than panicking mid-frame.
    pub(super) fn header_height(&self, group: usize) -> u16 {
        if self.heights_generation != self.model.generation {
            return 1;
        }
        self.header_heights.get(group).copied().unwrap_or(1)
    }

    /// Whether this group is the one the shell is showing.
    pub(super) fn is_active(&self, host: &HostId) -> bool {
        &self.active == host
    }

    /// Whether a switch to this host is waiting for its first surface.
    pub(super) fn is_switching_to(&self, host: &HostId) -> bool {
        self.switching_to.as_ref() == Some(host)
    }
}

/// Header heights, computed once per rebuild: 1, or 2 when a reason is drawn.
fn header_heights(model: &FleetSidebarModel) -> Vec<u16> {
    model
        .groups
        .iter()
        .map(|group| 1 + u16::from(group.header.reason.is_some()))
        .collect()
}

impl ClientShellState {
    /// Install the loop's freshly rebuilt row model.
    ///
    /// Deviation from the plan's two-argument sketch: `switching_to` travels
    /// with the model because both are read by the same frame, and a separate
    /// setter would let a caller install one without the other.
    pub(crate) fn fleet_sidebar_update(
        &mut self,
        model: FleetSidebarModel,
        active: HostId,
        switching_to: Option<HostId>,
    ) {
        self.fleet = Some(FleetShellState::new(model, active, switching_to));
    }

    /// Whether the shell is already showing exactly this fleet view.
    ///
    /// The loop asks before installing: rebuilding is cheap, but installing
    /// means a repaint of the whole console, and most fleet changes (an
    /// inactive host bumping its revision) change no visible row.
    pub(crate) fn fleet_sidebar_matches(
        &self,
        model: &FleetSidebarModel,
        active: &HostId,
        switching_to: Option<&HostId>,
    ) -> bool {
        self.fleet.as_ref().is_some_and(|fleet| {
            fleet.active == *active
                && fleet.switching_to.as_ref() == switching_to
                // `generation` changes on every rebuild, so the rows — not the
                // counter — decide whether anything is different.
                && fleet.model.groups == model.groups
        })
    }

    /// Whether the shell has a projection to address ids against.
    pub(crate) fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }

    /// Update the "switching to this host" marker in place.
    ///
    /// Returns whether anything changed. In place, and not through
    /// [`ClientShellState::fleet_sidebar_update`], because a switch finishing
    /// changes one glyph and must not cost a model clone on the frame path.
    pub(crate) fn set_fleet_switching(&mut self, switching_to: Option<HostId>) -> bool {
        let Some(fleet) = self.fleet.as_mut() else {
            return false;
        };
        if fleet.switching_to == switching_to {
            return false;
        }
        fleet.switching_to = switching_to;
        true
    }

    /// The agent-panel order the model must be built with.
    ///
    /// Lives on the shell's config (the user can toggle it by clicking the
    /// panel's sort label), and `crate::fleet::sidebar` is pure, so the loop
    /// reads it from here and passes it to `FleetSidebarModel::rebuild`.
    pub(crate) fn agent_panel_sort(&self) -> crate::config::AgentPanelSortConfig {
        self.config.agent_panel_sort
    }

    /// Forget everything that described the host being switched away from.
    ///
    /// The new host's snapshot resets most of this again through
    /// `set_snapshot`'s boot change, but a host that has not sent one yet
    /// would otherwise leave the old machine's pane ids in the hit map and its
    /// leases, scroll targets and pending requests in flight.
    pub(crate) fn reset_for_host_switch(&mut self) {
        self.pane_surface = None;
        // The one that would mis-route: `hits.agents` and `hits.workspaces`
        // hold the *old* host's server-side ids, and a click resolves them
        // against whatever host is active now.
        self.hits = ShellHitMap::default();
        self.input_leases = Default::default();
        self.popup_terminal_id = None;
        self.popup_pending = false;
        self.popup_pending_deadline = None;
        self.chrome_drag = None;
        self.workspace_press = None;
        self.tab_press = None;
        self.pane_mouse_gesture = None;
        self.last_pane_click = None;
        self.selection = None;
        self.copy_mode = None;
        self.reset_copy_pipeline();
        self.pending_requests.clear();
        self.pane_scroll_in_flight.clear();
        self.pane_scroll_queued.clear();
        self.pane_scroll_targets.clear();
        self.pending_notifications.clear();
        self.visible_notification = None;
        self.navigate_workspace_id = None;
        self.previous_pane_id = None;
        self.endpoint_error = None;
        self.workspace_scroll = 0;
        self.agent_scroll = 0;
        self.tab_scroll = 0;
        self.reveal_focused_workspace = true;
        self.reveal_focused_tab = true;
        self.last_composed_size = None;
        self.last_tab_bar_width = None;
        // The snapshot is the new host's from here on; drop the old one so a
        // frame composed before it arrives cannot draw another machine.
        self.snapshot = None;
        self.endpoint_methods = None;
    }

    /// Resolve a click against the fleet rows.
    ///
    /// Returns `true` when the click belonged to a fleet row, so the caller
    /// stops before the single-host workspace and agent hit tests — those
    /// address the active host and must never see another host's row.
    pub(super) fn handle_fleet_sidebar_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hit) = self
            .hits
            .fleet_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, hit)| hit.clone())
        else {
            return false;
        };
        let action = match hit {
            FleetSidebarHit::HostCollapse(host) => Some(FleetShellAction::ToggleCollapsed(host)),
            FleetSidebarHit::HostHeader(host) => self.switch_action(host, None),
            FleetSidebarHit::Workspace(workspace) => {
                let target = FleetFocusTarget::Workspace(workspace.workspace_id);
                self.switch_action(workspace.host, Some(target))
            }
            FleetSidebarHit::Agent(pane) => {
                let target = FleetFocusTarget::Pane(pane.pane_id);
                self.switch_action(pane.host, Some(target))
            }
        };
        if let Some(action) = action {
            outcome.actions.push(ClientShellAction::Fleet(action));
            outcome.repaint = true;
        }
        true
    }

    /// A switch request, unless the row cannot be switched to.
    ///
    /// A disabled host is listed (it is configuration the user can see) but
    /// `FleetState::set_active_host` refuses it, and the active host is
    /// already the routing target — clicking either must not emit an action
    /// that the loop would only log and drop.
    fn switch_action(
        &self,
        host: HostId,
        then_focus: Option<FleetFocusTarget>,
    ) -> Option<FleetShellAction> {
        let fleet = self.fleet.as_ref()?;
        if fleet.is_active(&host) {
            tracing::debug!(%host, "ignoring a fleet row for the host already active");
            return None;
        }
        let enabled = fleet
            .model
            .group(&host)
            .is_some_and(|group| group.header.enabled);
        if !enabled {
            tracing::debug!(%host, "ignoring a fleet row for a host disabled in [fleet]");
            return None;
        }
        Some(FleetShellAction::SwitchHost { host, then_focus })
    }

    /// Build the focus request for a target on the host just switched to.
    ///
    /// The same endpoint methods the navigator uses
    /// (`accept_navigator_selection`), so a fleet switch focuses exactly the
    /// way picking the same row on one host would. Returns the shell outcome
    /// for the loop to dispatch — `push_endpoint_method` is `pub(super)`, and
    /// the loop lives outside this module.
    pub(crate) fn request_fleet_focus(&mut self, target: &FleetFocusTarget) -> ClientShellInput {
        let method = match target {
            FleetFocusTarget::Workspace(workspace_id) => {
                crate::api::schema::Method::WorkspaceFocus(crate::api::schema::WorkspaceTarget {
                    workspace_id: workspace_id.clone(),
                })
            }
            FleetFocusTarget::Pane(pane_id) => {
                crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                    pane_id: pane_id.clone(),
                })
            }
        };
        let mut outcome = ClientShellInput::default();
        self.push_endpoint_method(method, &mut outcome);
        outcome
    }

    /// Tell the loop that the agent-panel sort changed, if this is a console.
    pub(super) fn note_fleet_sort_changed(&self, outcome: &mut ClientShellInput) {
        if self.fleet.is_some() {
            outcome
                .actions
                .push(ClientShellAction::Fleet(FleetShellAction::SortChanged));
        }
    }
}
