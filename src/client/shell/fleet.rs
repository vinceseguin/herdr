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
//! notification target. PR 8 turns the pane-area notice into the console's
//! answer to "the machine I am pointed at is not there": it names the host,
//! its connection state and the reason, and — because the notice is exactly
//! the state in which there is no pane to type into — it is also the gate that
//! *drops* pane-bound input. Dropped, never queued: a keystroke held while a
//! host reconnects would be replayed into a shell that has moved on, or into a
//! pane id the reconnected server no longer has.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use crate::fleet::hosts::HostId;
use crate::fleet::refs::{FleetPaneRef, FleetWorkspaceRef};
use crate::fleet::sidebar::{FleetSidebarModel, HostRowState};
use crate::protocol::{
    ClientShellSnapshot, FrameData, PaneSurfaceFrame, SemanticNotification,
    SemanticNotificationKind,
};

use super::{
    ClientShellAction, ClientShellConfig, ClientShellInput, ClientShellNotificationEffect,
    ClientShellState, ClientVisibleNotification, ShellHitMap,
};

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

impl FleetSidebarHit {
    /// The host this row belongs to. Every variant carries one: that is the
    /// type's whole reason to exist.
    pub(super) fn host(&self) -> &HostId {
        match self {
            Self::HostHeader(host) | Self::HostCollapse(host) => host,
            Self::Workspace(workspace) => &workspace.host,
            Self::Agent(pane) => &pane.host,
        }
    }
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
    /// The one line the pane area shows instead of a surface, built when the
    /// model or the switch marker changes rather than on every frame.
    ///
    /// `Some` is the console's "there is nothing to type into" state: a switch
    /// waiting on its host, or an active host that is connecting, unavailable
    /// or incompatible. [`FleetShellState::input_allowed`] reads exactly this,
    /// so what the pane area says and where input may go can never disagree.
    pane_notice: Option<String>,
    /// What the shell composes against while the active host has no
    /// projection or no surface: an empty one.
    ///
    /// A console must keep drawing its chrome — the host groups above all —
    /// when the machine it is pointed at has nothing to show, or a switch to
    /// a host that is down would leave the previous host's last frame on the
    /// terminal with no clickable row to leave it by. The single-host client
    /// has no such state: it draws nothing until its one server does.
    placeholder: Box<FleetPlaceholder>,
}

/// An empty projection and surface, for a console whose active host has none.
struct FleetPlaceholder {
    snapshot: ClientShellSnapshot,
    surface: PaneSurfaceFrame,
}

impl FleetPlaceholder {
    fn new() -> Self {
        Self {
            snapshot: ClientShellSnapshot {
                boot_id: String::new(),
                revision: 0,
                config_diagnostic: None,
                product_announcement: None,
                update_available: None,
                update_install_command: String::new(),
                server_keybindings_toml: None,
                latest_release_notes_available: false,
                integration_updates_available: false,
                worktree_directory: String::new(),
                release_notes: None,
                focused_workspace_id: None,
                focused_tab_id: None,
                focused_pane_id: None,
                tab_bar_right: Vec::new(),
                tab_bar_right_separator: String::new(),
                agent_view_label: None,
                agent_order: Vec::new(),
                workspaces: Vec::new(),
                tabs: Vec::new(),
                panes: Vec::new(),
                agents: Vec::new(),
                commands: Vec::new(),
            },
            surface: PaneSurfaceFrame {
                boot_id: String::new(),
                projection_revision: 0,
                surface_revision: 0,
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
                graphics: Default::default(),
            },
        }
    }
}

/// The pane-area line, or `None` when the active host can draw its own.
///
/// Precomputed on every model install and every switch-marker change, never
/// on the compose path: this is multiplicative work's neighbour, and a frame
/// must not format.
///
/// A switch is only reported as a switch while its host is actually connected.
/// A host that is down says so instead — otherwise "switching to lab-2…" is
/// what a console shows forever about a machine that is never coming back.
fn pane_notice(
    model: &FleetSidebarModel,
    active: &HostId,
    switching_to: Option<&HostId>,
) -> Option<String> {
    let switching = || switching_to.map(|host| format!("switching to {host}…"));
    let Some(group) = model.group(active) else {
        return switching();
    };
    // A host with rows has been connected at least once, so a retry is a
    // *re*connect. The connector's attempt counter cannot answer this on its
    // own: it counts from 1 for a host that has never been up, which is how
    // a first connection used to read as "reconnecting (attempt 1)".
    let connected_before = !group.workspaces.is_empty() || !group.agents.is_empty();
    let reason = group.header.reason.as_deref().unwrap_or_default();
    match group.header.state {
        HostRowState::Connected => switching(),
        HostRowState::Connecting { attempt } if attempt <= 1 && !connected_before => {
            Some(notice_line(active, "connecting", reason))
        }
        HostRowState::Connecting { attempt } => Some(notice_line(
            active,
            &format!("reconnecting (attempt {attempt})"),
            reason,
        )),
        HostRowState::Unavailable if connected_before => {
            Some(notice_line(active, "reconnecting", reason))
        }
        HostRowState::Unavailable => Some(notice_line(active, "unavailable", reason)),
        HostRowState::Incompatible => Some(notice_line(active, "incompatible", reason)),
    }
}

/// `"lab-2 · reconnecting · host closed the connection"`, reason omitted when
/// there is none.
fn notice_line(host: &HostId, state: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!("{host} · {state}")
    } else {
        format!("{host} · {state} · {reason}")
    }
}

impl FleetShellState {
    fn new(model: FleetSidebarModel, active: HostId, switching_to: Option<HostId>) -> Self {
        Self {
            heights_generation: model.generation,
            header_heights: header_heights(&model),
            pane_notice: pane_notice(&model, &active, switching_to.as_ref()),
            placeholder: Box::new(FleetPlaceholder::new()),
            model,
            active,
            switching_to,
        }
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

    /// What the pane area says instead of showing the active host, if
    /// anything.
    ///
    /// A borrow, not a built `String`: this is read on the compose path.
    pub(super) fn pane_area_notice(&self) -> Option<&str> {
        self.pane_notice.as_deref()
    }

    /// Whether pane-bound input may leave the console right now.
    ///
    /// The same fact as [`FleetShellState::pane_area_notice`], deliberately:
    /// what the pane area is showing *is* whether there is a pane to type
    /// into. Input produced while this is false is dropped, never buffered —
    /// a keystroke replayed after a reconnect would reach a shell that has
    /// moved on, and one replayed after a switch would reach another machine.
    pub(super) fn input_allowed(&self) -> bool {
        self.pane_notice.is_none()
    }

    /// Index of the active host's first workspace row in the console's
    /// spaces list.
    ///
    /// A console's spaces section is host headers plus every expanded group's
    /// rows, so an index into the *active host's* `workspace_entries` — which
    /// is what upstream's reveal and drag arithmetic produces — is that many
    /// rows short. Mirrors `render_fleet_spaces`'s item order exactly.
    pub(super) fn active_spaces_offset(&self) -> usize {
        let mut offset = 0;
        for group in &self.model.groups {
            // The header of this group.
            offset += 1;
            if group.header.host == self.active {
                return offset;
            }
            if !group.header.collapsed {
                offset += group.workspaces.len();
            }
        }
        offset
    }

    /// What to compose when the active host has no projection or no surface.
    ///
    /// The real projection is preferred when there is one: the sidebar then
    /// shows the host's own workspaces while its first surface is on its way.
    pub(super) fn placeholder<'a>(
        &'a self,
        snapshot: Option<&'a ClientShellSnapshot>,
    ) -> (&'a ClientShellSnapshot, &'a PaneSurfaceFrame) {
        (
            snapshot.unwrap_or(&self.placeholder.snapshot),
            &self.placeholder.surface,
        )
    }

    /// Draw the pane-area notice, if there is one.
    ///
    /// Only ever called for a frame composed against the placeholder surface,
    /// so it cannot paint over a real pane.
    pub(super) fn render_pane_notice(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        config: &ClientShellConfig,
    ) {
        let Some(notice) = self.pane_notice.as_deref() else {
            return;
        };
        if area.is_empty() {
            return;
        }
        super::render::put_text(
            buffer,
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(1),
            notice,
            Style::default()
                .fg(config.palette.overlay0)
                .add_modifier(Modifier::DIM),
        );
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
        // A picker that is open is showing rows from the previous model.
        self.refresh_host_picker_rows();
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
                // counter — decide whether anything is different. Picker rows
                // count: one can move (a host's version) while no group does.
                && fleet.model.same_rows(model)
        })
    }

    /// Whether the shell has a projection to address ids against.
    pub(crate) fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }

    /// Whether pane-bound input may leave this client right now.
    ///
    /// Always true for the single-host client, which has exactly one server
    /// and no notice to draw.
    pub(super) fn fleet_input_allowed(&self) -> bool {
        self.fleet
            .as_ref()
            .is_none_or(FleetShellState::input_allowed)
    }

    /// Throw away everything an input batch addressed at a pane.
    ///
    /// The console is showing a notice, not a pane: there is nothing on the
    /// other end to receive this. `FleetLink::send` would drop it a moment
    /// later anyway — this makes the drop the shell's own decision, so it
    /// happens once, before the message is built into the write path, and can
    /// never turn into a queue. Everything else the batch produced (a focus
    /// report, a theme update, a host switch, an overlay action) still goes.
    ///
    /// Two pieces of shell state are the queue in disguise, and go with the
    /// messages: keys parked behind a copy operation (`handle_key` holds them
    /// while a copy-mode read is in flight and replays them when it settles,
    /// which can be after the host is back), and a pane mouse gesture (its
    /// button-up is synthesized on the next focus loss, to whichever host is
    /// connected by then). Leases are left alone: they only ever produce a
    /// release for a key the host may have seen the press of, and a release
    /// to a shell that has moved on is harmless where a replayed keystroke is
    /// not.
    ///
    /// Returns whether anything was dropped, so the caller repaints and the
    /// user sees the notice answer the keystroke.
    pub(super) fn drop_pane_bound_input(&mut self, outcome: &mut ClientShellInput) -> bool {
        let before = outcome.requests.len();
        outcome.requests.retain(|request| {
            !matches!(
                request,
                crate::protocol::ClientMessage::ClientShellPaneInput { .. }
                    | crate::protocol::ClientMessage::ClientShellPopupInput { .. }
            )
        });
        self.copy_input_queue.clear();
        self.pane_mouse_gesture = None;
        let dropped = outcome.requests.len() != before;
        if dropped {
            tracing::debug!(
                host = %self.fleet.as_ref().map(|fleet| fleet.active.to_string()).unwrap_or_default(),
                dropped = before - outcome.requests.len(),
                "dropping pane input for a fleet host the console cannot show"
            );
        }
        dropped
    }

    /// Rows before the active host's own workspace rows, in a console.
    ///
    /// Zero for the single-host client, whose spaces list is its workspaces.
    pub(super) fn fleet_active_spaces_offset(&self) -> usize {
        self.fleet
            .as_ref()
            .map_or(0, FleetShellState::active_spaces_offset)
    }

    /// Whether this screen row belongs to a host other than the active one
    /// (its header, or one of its workspace or agent rows).
    ///
    /// The active host's *own* header is deliberately not excluded: the slot
    /// before its first workspace is the line above that workspace, which in
    /// a console is that header — exactly as the single-host client's slot
    /// before its first workspace is the section title line.
    pub(super) fn is_other_host_row(&self, row: u16) -> bool {
        let Some(fleet) = self.fleet.as_ref() else {
            return false;
        };
        self.hits
            .fleet_rows
            .iter()
            .any(|(rect, hit)| row >= rect.y && row < rect.bottom() && !fleet.is_active(hit.host()))
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
        fleet.pane_notice = pane_notice(&fleet.model, &fleet.active, switching_to.as_ref());
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
        // Everything `set_snapshot` resets on a boot change, and for the same
        // reason: the ids, deadlines and in-flight requests below describe a
        // server this shell is no longer talking to. `set_snapshot` will not
        // do it for us — the snapshot is dropped here, so the new host's first
        // projection does not read as a boot change.
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
        self.url_click_consumes_until_up = false;
        self.last_pane_click = None;
        self.selection = None;
        self.stop_selection_autoscroll();
        self.selection_highlight_clear_deadline = None;
        self.pending_word_selection = None;
        self.copy_mode = None;
        self.reset_copy_pipeline();
        self.copy_feedback = None;
        self.copy_feedback_deadline = None;
        self.pending_requests.clear();
        self.pane_scroll_in_flight.clear();
        self.pane_scroll_queued.clear();
        self.pane_scroll_targets.clear();
        self.pending_integration_installs = 0;
        self.pending_notifications.clear();
        self.visible_notification = None;
        self.endpoint_notice_seen.clear();
        self.visible_endpoint_notice = None;
        self.navigate_workspace_id = None;
        self.previous_pane_id = None;
        self.endpoint_error = None;
        self.dismissed_product_announcement = None;
        // Deviation from the plan's sketch, which kept the overlay: every
        // overlay that outlives a click carries the previous host's ids
        // (a rename, a close confirmation, a worktree action) or indexes
        // into its projection (the navigator), and accepting one after the
        // switch would send those ids to the new host. The picker overlay
        // (PR 6) emits the switch and expects to be gone afterwards anyway.
        self.overlay = None;
        self.workspace_scroll = 0;
        self.agent_scroll = 0;
        self.tab_scroll = 0;
        self.mobile_switcher_scroll = 0;
        self.reveal_focused_workspace = true;
        self.reveal_mobile_workspace = false;
        self.mobile_switcher_suspended = false;
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
    pub(super) fn switch_action(
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

    // -----------------------------------------------------------------------
    // Notifications (E2 PR 7)
    //
    // A notification names a pane, a tab and a workspace *on the machine that
    // sent it*. In a console those ids reach a shell whose projection is
    // another host's, where the same strings mean different things — every
    // herdr server starts at `w1`/`w1:p1`. So the host travels beside the
    // frozen wire event, and the three questions the shell asks about a
    // notification are answered against that host: is it still current, is the
    // user already looking at it, and what does opening it do.
    // -----------------------------------------------------------------------

    /// Take one host's notification.
    ///
    /// The console calls this for *every* host, the active one included, so
    /// nothing downstream has to infer which machine an id belongs to. The
    /// title already carries the `[host] ` prefix (`client::fleet::translate`):
    /// which machine is asking is the point of a fleet notification.
    pub(crate) fn receive_fleet_notification(
        &mut self,
        host: HostId,
        event: SemanticNotification,
        now: std::time::Instant,
    ) -> (Vec<ClientShellNotificationEffect>, bool) {
        self.receive_notification_from(Some(host), event, now)
    }

    /// The host of a notification whose ids must *not* be resolved against
    /// this shell's snapshot, if that is what this is.
    ///
    /// `None` for a notification with no host (the single-host client: one
    /// server, so its ids can only mean that one) and for the active host —
    /// the shell's own snapshot is the right thing to resolve those against.
    /// `Some` for every other host, and also for a host-qualified notification
    /// that arrives before the console has installed a fleet view: nothing
    /// then says the host on screen is the one that sent it, and the safe
    /// answer is that its ids belong elsewhere — it cannot be opened here.
    pub(super) fn remote_fleet_notification_host<'a>(
        &self,
        host: Option<&'a HostId>,
    ) -> Option<&'a HostId> {
        let host = host?;
        match self.fleet.as_ref() {
            Some(fleet) if fleet.is_active(host) => None,
            _ => Some(host),
        }
    }

    /// Whether this notification came from a host the shell is not showing.
    pub(super) fn is_remote_fleet_notification(&self, host: Option<&HostId>) -> bool {
        self.remote_fleet_notification_host(host).is_some()
    }

    /// Whether another host's notification still describes that host.
    ///
    /// Answered from the row model — the console's only view of a machine it
    /// is not showing — and never from `self.snapshot`, which is the *active*
    /// host's and would answer about a different agent that happens to share
    /// the pane id. A host whose model has no such agent (it has not sent a
    /// projection, or the agent is gone) suppresses the notification, exactly
    /// as an unknown pane does for the single-host client.
    pub(super) fn fleet_notification_still_current(
        &self,
        host: &HostId,
        event: &SemanticNotification,
    ) -> bool {
        let Some(pane_id) = event.pane_id.as_deref() else {
            return true;
        };
        let Some(fleet) = self.fleet.as_ref() else {
            return false;
        };
        let pane = FleetPaneRef::new(host.clone(), pane_id);
        let Some(status) = fleet.model.agent_status(&pane) else {
            return false;
        };
        match event.kind {
            SemanticNotificationKind::NeedsAttention => {
                status == crate::api::schema::AgentStatus::Blocked
            }
            SemanticNotificationKind::Finished => matches!(
                status,
                crate::api::schema::AgentStatus::Idle | crate::api::schema::AgentStatus::Done
            ),
            SemanticNotificationKind::UpdateInstalled | SemanticNotificationKind::Custom => true,
        }
    }

    /// Open a notification that belongs to another host: switch there, then
    /// focus its pane.
    ///
    /// Returns whether the fleet path owns this notification. `true` also
    /// covers the cases where nothing can be done (the notification names no
    /// pane, or that host can no longer be switched to): the caller must not
    /// fall through to `pane.focus`, which would send another machine's pane
    /// id to the one on screen.
    ///
    /// The focus itself is deferred by the loop
    /// (`FleetClientState::pending_focus`), which sends it once the target
    /// host has installed a projection.
    pub(super) fn open_fleet_notification_target(
        &self,
        notification: &ClientVisibleNotification,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(host) = self.remote_fleet_notification_host(notification.host.as_ref()) else {
            return false;
        };
        let Some(pane_id) = notification.event.pane_id.clone() else {
            tracing::debug!(%host, "a fleet notification with no pane cannot be opened");
            return true;
        };
        match self.switch_action(host.clone(), Some(FleetFocusTarget::Pane(pane_id))) {
            Some(action) => {
                outcome.actions.push(ClientShellAction::Fleet(action));
            }
            None => {
                tracing::debug!(%host, "cannot open a notification for this fleet host");
            }
        }
        true
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
