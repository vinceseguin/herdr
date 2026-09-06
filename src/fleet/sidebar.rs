//! Pure row model for the fleet sidebar and the host picker.
//!
//! The console draws one group per configured host — a header, that host's
//! workspaces and that host's agents — and a picker overlay listing the same
//! hosts. Both are *derived* views of [`FleetState`], and both are on a
//! multiplicative path: rows scale with hosts × agents and the sidebar is
//! composed on every frame. So the derivation happens here, once per
//! [`FleetChange`](crate::fleet::state::FleetChange), and the renderer only
//! walks the result: every label, count and glyph choice in this module is a
//! `String` computed at [`FleetSidebarModel::rebuild`] time, never at draw
//! time.
//!
//! Pure: no sockets, no async, no ratatui — see
//! `the_pure_fleet_modules_import_no_runtime` in the parent module. Colours
//! and glyphs for a row's [`AgentStatus`] stay with the renderer (upstream's
//! `status_icon`/`status_color`); what travels from here is the status itself.

use std::cmp::Reverse;
use std::collections::HashSet;

use crate::api::schema::AgentStatus;
use crate::config::AgentPanelSortConfig;
use crate::fleet::hosts::HostId;
use crate::fleet::refs::{is_valid_resource_id, FleetPaneRef, FleetWorkspaceRef};
use crate::fleet::state::{AgentRollup, FleetState, HostConnection, HostState};
use crate::protocol::ClientShellSnapshot;

/// Glyph in front of a collapsed host header.
const COLLAPSED_GLYPH: &str = "▸";
/// Glyph in front of an expanded host header.
const EXPANDED_GLYPH: &str = "▾";
/// Separator between the parts of a header or picker label.
const DOT: &str = " · ";

/// How a host is doing, reduced to what a row needs.
///
/// [`HostConnection`] carries the server version and the advertised method
/// list; a row needs neither, and keeping them out means a row is `Copy` and
/// comparing two rebuilds is cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRowState {
    Connected,
    Connecting { attempt: u32 },
    Unavailable,
    Incompatible,
}

impl HostRowState {
    fn from_connection(connection: &HostConnection) -> Self {
        match connection {
            HostConnection::Connected { .. } => Self::Connected,
            HostConnection::Connecting { attempt } => Self::Connecting { attempt: *attempt },
            HostConnection::Unavailable { .. } => Self::Unavailable,
            HostConnection::Incompatible { .. } => Self::Incompatible,
        }
    }

    /// Whether this host's rows describe a live server.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// The header row of one host group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHeaderRow {
    pub host: HostId,
    /// Whether input, resize and endpoint commands currently go to this host.
    pub active: bool,
    /// Whether the group's rows are hidden. Collapse hides rows at draw time;
    /// the rows themselves are still built, so expanding costs no rebuild.
    pub collapsed: bool,
    pub state: HostRowState,
    /// Whether `[fleet]` enables this host at all.
    ///
    /// A disabled host is listed (it is configuration the user can see) but
    /// [`FleetState::set_active_host`] refuses it, so a click or a picker
    /// selection on such a row must be ignored rather than routed.
    pub enabled: bool,
    /// Precomputed header text, `"▾ workbox · 2 blocked · 3 working"`.
    pub label: String,
    /// Why this host is not connected; the renderer dims it under the header.
    pub reason: Option<String>,
    /// Per-status counts. Zero for a host that is not contributing agents,
    /// exactly as [`FleetState`] reports it.
    pub rollup: AgentRollup,
}

/// One workspace of one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub workspace: FleetWorkspaceRef,
    pub label: String,
    pub status: AgentStatus,
    pub focused: bool,
}

/// One agent of one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub pane: FleetPaneRef,
    /// Agent name, else its title, else the pane id — the same fallback chain
    /// `herdr fleet status` prints.
    pub label: String,
    pub status: AgentStatus,
    pub focused: bool,
    /// The host's own change counter. Only comparable within one host boot, so
    /// it orders rows *inside* a group and never across groups.
    pub state_change_seq: u64,
}

/// One host's rows: a header, its workspaces, its agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGroup {
    pub host: HostId,
    pub header: HostHeaderRow,
    /// Snapshot order, minus any workspace whose id cannot be addressed.
    pub workspaces: Vec<WorkspaceRow>,
    /// Upstream's agent-panel order; see [`host_status_rank`].
    pub agents: Vec<AgentRow>,
}

/// Every host's rows, in `[fleet]` order with `local` first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetSidebarModel {
    pub groups: Vec<HostGroup>,
    /// The host picker's rows, same hosts and same order as `groups`.
    ///
    /// Cached here rather than derived when the overlay opens because the
    /// shell holds this model and not [`FleetState`] (the console's fleet
    /// state lives in the client loop), and because a picker that is open
    /// while a host changes state must redraw from fresh rows.
    pub picker: Vec<HostPickerRow>,
    /// Bumped by every [`FleetSidebarModel::rebuild`], so a renderer can key a
    /// derived cache (row heights, wrapped labels) on it and recompute only
    /// when the model actually changed.
    pub generation: u64,
}

impl FleetSidebarModel {
    /// Derive every row from `state`.
    ///
    /// Call this in response to a [`FleetChange`](crate::fleet::state::FleetChange),
    /// a collapse/active change, or a change of `sort` — never per frame. Cost
    /// is O(hosts × (workspaces + agents log agents)) per call.
    ///
    /// `sort` is the console's live agent-panel preference, the same value
    /// upstream's `ordered_agent_pane_ids` takes: it is configuration
    /// (`[ui] agent_panel_sort`) that the user can also toggle by clicking the
    /// agent panel's sort label, so it is a parameter rather than something
    /// this module reads for itself.
    ///
    /// A host that is not `Connected` keeps the rows of its last snapshot
    /// (with zeroed counts) so the console dims a host instead of blanking it,
    /// which is the same rule `herdr fleet status` follows.
    pub fn rebuild(
        &mut self,
        state: &FleetState,
        collapsed: &HashSet<HostId>,
        sort: AgentPanelSortConfig,
    ) {
        let active = state.active_host();
        self.picker = host_picker_rows(state);
        self.groups.clear();
        self.groups.reserve(state.hosts().len());
        for host in state.hosts() {
            let id = host.id();
            self.groups.push(host_group(
                host,
                active.is_some_and(|active| active == id),
                collapsed.contains(id),
                sort,
            ));
        }
        // Saturating rather than wrapping: at u64::MAX a renderer's cache goes
        // stale-but-correct only if it also ignores `groups`, whereas a wrap
        // could collide with a live generation. Neither is reachable.
        self.generation = self.generation.saturating_add(1);
    }

    /// Whether two models describe the same rows.
    ///
    /// `generation` changes on every rebuild, so it cannot answer this; the
    /// console asks before installing a rebuilt model, because installing one
    /// costs a full recompose while most fleet changes move no visible row.
    pub fn same_rows(&self, other: &Self) -> bool {
        self.groups == other.groups && self.picker == other.picker
    }

    pub fn group(&self, host: &HostId) -> Option<&HostGroup> {
        self.groups.iter().find(|group| &group.host == host)
    }

    /// Status of one agent, addressed across hosts.
    // Read by host-aware notifications (E2 PR 7) to decide whether a toast for
    // another host's agent is still worth showing.
    #[allow(dead_code)]
    pub fn agent_status(&self, pane: &FleetPaneRef) -> Option<AgentStatus> {
        self.group(&pane.host)?
            .agents
            .iter()
            .find(|agent| agent.pane.pane_id == pane.pane_id)
            .map(|agent| agent.status)
    }
}

/// One row of the host picker overlay.
///
/// Same hosts, same order as the sidebar, so a `1-9` jump in the picker names
/// the same host as the *n*-th sidebar group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPickerRow {
    pub host: HostId,
    pub active: bool,
    pub state: HostRowState,
    /// Whether the host may be activated at all; see [`HostHeaderRow::enabled`].
    pub enabled: bool,
    /// Precomputed row text, `"workbox  connected 0.8.2-fork  2 blocked"`.
    pub label: String,
    /// `"local"` or `"ssh"` — the transport name `[[fleet.hosts]].kind` uses.
    pub kind: &'static str,
}

/// The picker's rows, in `[fleet]` order with `local` first.
///
/// Called by [`FleetSidebarModel::rebuild`], which caches the result: the
/// picker overlay lives in the client shell, which holds the model and not
/// [`FleetState`], and a row built per frame would be a per-frame format.
pub fn host_picker_rows(state: &FleetState) -> Vec<HostPickerRow> {
    let active = state.active_host();
    state
        .hosts()
        .iter()
        .map(|host| {
            let id = host.id();
            let state_row = HostRowState::from_connection(&host.connection);
            let mut label = String::with_capacity(48);
            label.push_str(id.as_str());
            label.push_str("  ");
            label.push_str(&connection_summary(&host.connection));
            let detail = host_detail(host, state_row);
            if !detail.is_empty() {
                label.push_str("  ");
                label.push_str(&detail);
            }
            HostPickerRow {
                host: id.clone(),
                active: active.is_some_and(|active| active == id),
                state: state_row,
                enabled: host.spec.enabled,
                label,
                kind: host.spec.kind.as_str(),
            }
        })
        .collect()
}

/// Upstream's agent-panel priority, for ordering agents *inside* one host.
///
/// Deliberately a copy of `status_priority` in `src/client/shell.rs`, not a
/// re-export: this module may not reach into `crate::client` (the purity
/// guard), and a fleet group must sort exactly like the single-host sidebar it
/// replaces. The table below is pinned by a test; change it only when upstream
/// changes.
///
/// Note this is *not* [`crate::fleet::state`]'s fleet-wide `status_rank`, which
/// puts working above done to answer "where am I needed next" across hosts.
pub fn host_status_rank(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Blocked => 4,
        AgentStatus::Done => 3,
        AgentStatus::Working => 2,
        AgentStatus::Idle => 1,
        AgentStatus::Unknown => 0,
    }
}

fn host_group(
    host: &HostState,
    active: bool,
    collapsed: bool,
    sort: AgentPanelSortConfig,
) -> HostGroup {
    let id = host.id().clone();
    let state = HostRowState::from_connection(&host.connection);
    let snapshot = host.snapshot.as_deref();
    HostGroup {
        workspaces: workspace_rows(&id, snapshot),
        agents: agent_rows(&id, snapshot, sort),
        header: HostHeaderRow {
            active,
            collapsed,
            state,
            enabled: host.spec.enabled,
            label: header_label(host, state, collapsed),
            reason: host.connection.reason().map(str::to_string),
            rollup: host.rollup,
            host: id.clone(),
        },
        host: id,
    }
}

fn header_label(host: &HostState, state: HostRowState, collapsed: bool) -> String {
    let mut label = String::with_capacity(48);
    label.push_str(if collapsed {
        COLLAPSED_GLYPH
    } else {
        EXPANDED_GLYPH
    });
    label.push(' ');
    label.push_str(host.id().as_str());
    let detail = match state {
        // A connected host with no snapshot yet has nothing to count, and
        // saying "no agents" there would be a lie that resolves itself a
        // moment later.
        HostRowState::Connected if host.snapshot.is_none() => "loading".to_string(),
        HostRowState::Connected => rollup_summary(&host.rollup),
        _ => connection_summary(&host.connection),
    };
    label.push_str(DOT);
    label.push_str(&detail);
    label
}

/// The third column of a picker row: counts when the host is contributing
/// agents, the failure reason when it is not, nothing when there is neither.
fn host_detail(host: &HostState, state: HostRowState) -> String {
    if state.is_connected() {
        if host.snapshot.is_none() {
            return String::new();
        }
        return rollup_summary(&host.rollup);
    }
    host.connection.reason().unwrap_or_default().to_string()
}

/// `"2 blocked · 3 working"`, zero counts omitted; `"no agents"` when all are.
fn rollup_summary(rollup: &AgentRollup) -> String {
    let mut summary = String::with_capacity(32);
    for (count, name) in [
        (rollup.blocked, "blocked"),
        (rollup.working, "working"),
        (rollup.done, "done"),
        (rollup.idle, "idle"),
        (rollup.unknown, "unknown"),
    ] {
        if count == 0 {
            continue;
        }
        if !summary.is_empty() {
            summary.push_str(DOT);
        }
        summary.push_str(&count.to_string());
        summary.push(' ');
        summary.push_str(name);
    }
    if summary.is_empty() {
        summary.push_str("no agents");
    }
    summary
}

/// `"connected 0.8.2-fork"`, `"reconnecting (attempt 2)"`, `"unavailable"`, …
fn connection_summary(connection: &HostConnection) -> String {
    match connection {
        HostConnection::Connected { server_version, .. } if server_version.is_empty() => {
            "connected".to_string()
        }
        HostConnection::Connected { server_version, .. } => format!("connected {server_version}"),
        // The supervisor numbers its *first* attempt 1, and `FleetState` starts
        // a host at 0, so anything past 1 is a retry (fork, E2 PR 8: a first
        // connection used to read as "reconnecting (attempt 1)").
        HostConnection::Connecting { attempt } if *attempt <= 1 => "connecting".to_string(),
        // A retry is the interesting case for an operator: it says the host was
        // reachable once and the connector has not given up.
        HostConnection::Connecting { attempt } => format!("reconnecting (attempt {attempt})"),
        HostConnection::Unavailable { .. } => "unavailable".to_string(),
        HostConnection::Incompatible { .. } => "incompatible".to_string(),
    }
}

fn workspace_rows(host: &HostId, snapshot: Option<&ClientShellSnapshot>) -> Vec<WorkspaceRow> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    snapshot
        .workspaces
        .iter()
        // Agents are filtered at ingestion (`retain_addressable_agents`);
        // workspaces are not, so an id that would mint a reference parsing
        // back into a *different* host is dropped here instead.
        .filter(|workspace| is_valid_resource_id(&workspace.workspace_id))
        .map(|workspace| WorkspaceRow {
            workspace: FleetWorkspaceRef::new(host.clone(), workspace.workspace_id.clone()),
            label: if workspace.label.is_empty() {
                workspace.workspace_id.clone()
            } else {
                workspace.label.clone()
            },
            status: workspace.agent_status,
            focused: workspace.focused,
        })
        .collect()
}

/// One host's agents in upstream's agent-panel order.
///
/// Mirrors `ordered_agent_pane_ids` in `src/client/shell/agent_sidebar.rs`
/// arm for arm: a named view's `agent_order` wins outright, otherwise
/// `Priority` sorts and `Spaces` (the default) keeps the snapshot's own
/// workspace grouping.
fn agent_rows(
    host: &HostId,
    snapshot: Option<&ClientShellSnapshot>,
    sort: AgentPanelSortConfig,
) -> Vec<AgentRow> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    if snapshot.agent_view_label.is_some() {
        // The endpoint is presenting a named view; its `agent_order` is the
        // order, and an id it names that no longer exists is skipped —
        // upstream's `ordered_agent_pane_ids`. The scan is O(order × agents),
        // which at an agent panel's cardinality (tens at most, once per
        // change) costs less than building an index would.
        return snapshot
            .agent_order
            .iter()
            .filter_map(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| &agent.pane_id == pane_id)
                    .map(|agent| agent_row(host, agent))
            })
            .collect();
    }
    let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
    if sort == AgentPanelSortConfig::Priority {
        // Stable, so equal-priority agents keep snapshot order — upstream again.
        agents.sort_by_key(|agent| {
            (
                Reverse(host_status_rank(agent.agent_status)),
                Reverse(agent.state_change_seq),
            )
        });
    }
    agents
        .into_iter()
        .map(|agent| agent_row(host, agent))
        .collect()
}

fn agent_row(host: &HostId, agent: &crate::protocol::ClientShellAgent) -> AgentRow {
    AgentRow {
        pane: FleetPaneRef::new(host.clone(), agent.pane_id.clone()),
        label: agent
            .name
            .clone()
            .or_else(|| agent.title.clone())
            .unwrap_or_else(|| agent.pane_id.clone()),
        status: agent.agent_status,
        focused: agent.focused,
        state_change_seq: agent.state_change_seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::hosts::{HostKind, HostSpec};
    use crate::fleet::state::HostEvent;
    use crate::protocol::{ClientShellAgent, ClientShellWorkspace};

    fn host(name: &str) -> HostId {
        HostId::new(name).expect("valid host name")
    }

    fn local_spec() -> HostSpec {
        HostSpec::local_default()
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

    fn ssh_spec(name: &str) -> HostSpec {
        HostSpec {
            id: host(name),
            kind: HostKind::Ssh {
                target: name.to_string(),
                session: None,
            },
            enabled: true,
        }
    }

    fn agent(pane_id: &str, status: AgentStatus, state_change_seq: u64) -> ClientShellAgent {
        let workspace_id = pane_id.split_once(':').map_or("w1", |(prefix, _)| prefix);
        ClientShellAgent {
            pane_id: pane_id.to_string(),
            workspace_id: workspace_id.to_string(),
            tab_id: format!("{workspace_id}:t1"),
            name: Some(format!("agent-{pane_id}")),
            display_agent: Some("Claude".to_string()),
            agent: Some("claude".to_string()),
            title: Some("Review".to_string()),
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        }
    }

    fn workspace(workspace_id: &str, label: &str) -> ClientShellWorkspace {
        ClientShellWorkspace {
            active_tab_id: format!("{workspace_id}:t1"),
            new_workspace_cwd: "/repo".to_string(),
            number: 1,
            label: label.to_string(),
            custom_label: false,
            branch: None,
            git_ahead_behind: None,
            tokens: Vec::new(),
            worktree: None,
            focused: false,
            agent_status: AgentStatus::Idle,
            workspace_id: workspace_id.to_string(),
        }
    }

    fn snapshot(
        boot_id: &str,
        revision: u64,
        workspaces: Vec<ClientShellWorkspace>,
        agents: Vec<ClientShellAgent>,
    ) -> Box<ClientShellSnapshot> {
        Box::new(ClientShellSnapshot {
            boot_id: boot_id.to_string(),
            revision,
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
            workspaces,
            tabs: Vec::new(),
            panes: Vec::new(),
            agents,
            commands: Vec::new(),
        })
    }

    fn connect(state: &mut FleetState, id: &HostId, version: &str) {
        state.apply(
            id,
            HostEvent::Connected {
                server_version: version.to_string(),
                methods: vec!["pane.write".to_string()],
            },
        );
    }

    /// A model of `state` in priority order — what every test below that does
    /// not name a sort expects. `Spaces` has its own test.
    fn model_of(state: &FleetState) -> FleetSidebarModel {
        let mut model = FleetSidebarModel::default();
        model.rebuild(state, &HashSet::new(), AgentPanelSortConfig::Priority);
        model
    }

    fn labels(rows: &[AgentRow]) -> Vec<&str> {
        rows.iter().map(|row| row.label.as_str()).collect()
    }

    #[test]
    fn groups_follow_config_order_with_local_first() {
        let state = FleetState::new(vec![local_spec(), ssh_spec("workbox"), spec("off", false)]);
        let model = model_of(&state);
        assert_eq!(
            model
                .groups
                .iter()
                .map(|group| group.host.to_string())
                .collect::<Vec<_>>(),
            vec!["local", "workbox", "off"]
        );
        assert!(
            model.groups[0].header.active,
            "the first enabled host is active"
        );
        assert!(!model.groups[1].header.active);
        assert!(
            model
                .groups
                .iter()
                .all(|group| group.workspaces.is_empty() && group.agents.is_empty()),
            "headers only, no snapshots yet"
        );
    }

    #[test]
    fn a_disabled_host_appears_with_its_reason() {
        let state = FleetState::new(vec![local_spec(), spec("off", false)]);
        let model = model_of(&state);
        let group = model
            .group(&host("off"))
            .expect("the disabled host is listed");
        assert!(!group.header.enabled);
        assert_eq!(group.header.state, HostRowState::Unavailable);
        assert_eq!(
            group.header.reason.as_deref(),
            Some("host disabled in [fleet]")
        );
        assert_eq!(group.header.label, "▾ off · unavailable");
    }

    #[test]
    fn an_unavailable_host_keeps_its_last_snapshot_rows() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![workspace("w1", "repo")],
                vec![agent("w1:p1", AgentStatus::Blocked, 3)],
            )),
        );
        assert_eq!(
            model_of(&state).groups[0].header.label,
            "▾ local · 1 blocked"
        );

        state.apply(
            &local,
            HostEvent::Unavailable {
                reason: "connection closed".to_string(),
                retry_in: Some(std::time::Duration::from_secs(2)),
            },
        );
        let model = model_of(&state);
        let group = &model.groups[0];
        assert_eq!(group.header.state, HostRowState::Unavailable);
        assert_eq!(group.header.reason.as_deref(), Some("connection closed"));
        assert_eq!(
            group.workspaces.len(),
            1,
            "a dropped host keeps its rows so the console can dim them"
        );
        assert_eq!(labels(&group.agents), vec!["agent-w1:p1"]);
        assert_eq!(group.header.rollup, AgentRollup::default());
        assert_eq!(group.header.label, "▾ local · unavailable");
        state.assert_invariants_for_test();
    }

    #[test]
    fn a_connecting_host_says_how_many_attempts_it_has_made() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        assert_eq!(
            model_of(&state).groups[0].header.label,
            "▾ local · connecting"
        );
        state.apply(&local, HostEvent::Connecting { attempt: 2 });
        let model = model_of(&state);
        assert_eq!(
            model.groups[0].header.label,
            "▾ local · reconnecting (attempt 2)"
        );
        assert_eq!(
            model.groups[0].header.state,
            HostRowState::Connecting { attempt: 2 }
        );
    }

    #[test]
    fn a_connected_host_without_a_snapshot_is_loading_not_empty() {
        let mut state = FleetState::new(vec![local_spec()]);
        connect(&mut state, &HostId::local(), "0.8.2-fork");
        assert_eq!(model_of(&state).groups[0].header.label, "▾ local · loading");
    }

    #[test]
    fn the_header_label_lists_every_non_zero_count() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![workspace("w1", "repo")],
                vec![
                    agent("w1:p1", AgentStatus::Blocked, 1),
                    agent("w1:p2", AgentStatus::Blocked, 2),
                    agent("w1:p3", AgentStatus::Working, 3),
                    agent("w1:p4", AgentStatus::Idle, 4),
                ],
            )),
        );
        assert_eq!(
            model_of(&state).groups[0].header.label,
            "▾ local · 2 blocked · 1 working · 1 idle"
        );
    }

    #[test]
    fn a_connected_host_with_an_empty_snapshot_says_no_agents() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot("boot-1", 1, vec![workspace("w1", "repo")], vec![])),
        );
        assert_eq!(
            model_of(&state).groups[0].header.label,
            "▾ local · no agents"
        );
    }

    #[test]
    fn collapsing_a_host_hides_its_rows_but_keeps_them() {
        let mut state = FleetState::new(vec![local_spec(), ssh_spec("workbox")]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![workspace("w1", "repo")],
                vec![agent("w1:p1", AgentStatus::Working, 1)],
            )),
        );

        let mut model = FleetSidebarModel::default();
        model.rebuild(&state, &HashSet::new(), AgentPanelSortConfig::Priority);
        assert_eq!(
            (
                model.groups[0].workspaces.len(),
                model.groups[0].agents.len()
            ),
            (1, 1)
        );
        assert!(!model.groups[0].header.collapsed);
        assert_eq!(model.groups[0].header.label, "▾ local · 1 working");

        let collapsed = HashSet::from([local.clone()]);
        model.rebuild(&state, &collapsed, AgentPanelSortConfig::Priority);
        let group = &model.groups[0];
        assert!(group.header.collapsed);
        assert_eq!(group.header.label, "▸ local · 1 working");
        assert_eq!(
            (group.workspaces.len(), group.agents.len()),
            (1, 1),
            "collapse is a render decision; the rows stay built"
        );
        let other = &model.groups[1];
        assert!(
            other.workspaces.is_empty() && other.agents.is_empty(),
            "a host without a snapshot is a header and nothing else"
        );
    }

    #[test]
    fn groups_hold_each_hosts_own_workspaces_and_agents_in_host_order() {
        let mut state = FleetState::new(vec![local_spec(), ssh_spec("workbox")]);
        for (id, pane) in [(HostId::local(), "w1:p1"), (host("workbox"), "w2:p9")] {
            connect(&mut state, &id, "0.8.2-fork");
            state.apply(
                &id,
                HostEvent::Snapshot(snapshot(
                    "boot-1",
                    1,
                    vec![workspace(
                        pane.split_once(':').map_or("w1", |(prefix, _)| prefix),
                        "repo",
                    )],
                    vec![agent(pane, AgentStatus::Working, 1)],
                )),
            );
        }
        let model = model_of(&state);
        // The renderer walks the groups in this order, header first, then
        // that host's workspaces, then its agents.
        let described = model
            .groups
            .iter()
            .flat_map(|group| {
                std::iter::once(format!("h:{}", group.header.host))
                    .chain(
                        group
                            .workspaces
                            .iter()
                            .map(|row| format!("w:{}", row.workspace)),
                    )
                    .chain(group.agents.iter().map(|row| format!("a:{}", row.pane)))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            vec![
                "h:local",
                "w:local/w1",
                "a:local/w1:p1",
                "h:workbox",
                "w:workbox/w2",
                "a:workbox/w2:p9",
            ]
        );
        for group in &model.groups {
            assert!(
                group
                    .workspaces
                    .iter()
                    .all(|row| row.workspace.host == group.host)
                    && group.agents.iter().all(|row| row.pane.host == group.host),
                "every row names its own host"
            );
        }
    }

    #[test]
    fn agents_inside_a_group_use_upstreams_priority_order() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![workspace("w1", "repo")],
                vec![
                    agent("w1:p1", AgentStatus::Idle, 10),
                    agent("w1:p2", AgentStatus::Unknown, 99),
                    agent("w1:p3", AgentStatus::Working, 1),
                    agent("w1:p4", AgentStatus::Done, 1),
                    agent("w1:p5", AgentStatus::Blocked, 1),
                    agent("w1:p6", AgentStatus::Blocked, 2),
                ],
            )),
        );
        let model = model_of(&state);
        assert_eq!(
            labels(&model.groups[0].agents),
            vec![
                "agent-w1:p6",
                "agent-w1:p5",
                "agent-w1:p4",
                "agent-w1:p3",
                "agent-w1:p1",
                "agent-w1:p2",
            ],
            "blocked > done > working > idle > unknown, recent first inside a rank"
        );
    }

    #[test]
    fn agent_order_is_honoured_when_the_snapshot_names_a_view() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        let mut projection = snapshot(
            "boot-1",
            1,
            vec![workspace("w1", "repo")],
            vec![
                agent("w1:p1", AgentStatus::Idle, 1),
                agent("w1:p2", AgentStatus::Blocked, 2),
            ],
        );
        projection.agent_view_label = Some("recent".to_string());
        projection.agent_order = vec![
            "w1:p1".to_string(),
            "w1:gone".to_string(),
            "w1:p2".to_string(),
        ];
        state.apply(&local, HostEvent::Snapshot(projection));
        assert_eq!(
            labels(&model_of(&state).groups[0].agents),
            vec!["agent-w1:p1", "agent-w1:p2"],
            "the view's order wins and an id it names but no longer has is skipped"
        );
    }

    #[test]
    fn agent_labels_fall_back_from_name_to_title_to_pane_id() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        let mut named = agent("w1:p1", AgentStatus::Working, 3);
        named.name = None;
        let mut anonymous = agent("w1:p2", AgentStatus::Working, 2);
        anonymous.name = None;
        anonymous.title = None;
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![workspace("w1", "repo")],
                vec![agent("w1:p0", AgentStatus::Working, 4), named, anonymous],
            )),
        );
        assert_eq!(
            labels(&model_of(&state).groups[0].agents),
            vec!["agent-w1:p0", "Review", "w1:p2"]
        );
    }

    #[test]
    fn a_workspace_with_an_unaddressable_id_is_dropped() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![
                    workspace("w1", "repo"),
                    workspace("w2/evil", "spoofed"),
                    workspace("", "nameless"),
                    workspace("w3", ""),
                ],
                vec![],
            )),
        );
        let model = model_of(&state);
        let rows = &model.groups[0].workspaces;
        assert_eq!(
            rows.iter()
                .map(|row| (row.workspace.to_string(), row.label.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("local/w1".to_string(), "repo".to_string()),
                // An unlabelled workspace falls back to its id rather than
                // drawing a blank row.
                ("local/w3".to_string(), "w3".to_string()),
            ]
        );
    }

    #[test]
    fn agent_status_reads_a_status_across_hosts() {
        let mut state = FleetState::new(vec![local_spec(), ssh_spec("workbox")]);
        let local = HostId::local();
        let workbox = host("workbox");
        for id in [&local, &workbox] {
            connect(&mut state, id, "0.8.2-fork");
            state.apply(
                id,
                HostEvent::Snapshot(snapshot(
                    "boot-1",
                    1,
                    vec![workspace("w1", "repo")],
                    vec![agent("w1:p1", AgentStatus::Working, 1)],
                )),
            );
        }
        let model = model_of(&state);
        let local_pane = FleetPaneRef::new(local.clone(), "w1:p1");
        let workbox_pane = FleetPaneRef::new(workbox.clone(), "w1:p1");
        assert_eq!(model.agent_status(&local_pane), Some(AgentStatus::Working));
        assert_eq!(
            model.agent_status(&workbox_pane),
            Some(AgentStatus::Working)
        );

        state.apply(
            &workbox,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                2,
                vec![workspace("w1", "repo")],
                vec![agent("w1:p1", AgentStatus::Blocked, 2)],
            )),
        );
        let model = model_of(&state);
        assert_eq!(
            model.agent_status(&local_pane),
            Some(AgentStatus::Working),
            "one host's delta must not move another host's row"
        );
        assert_eq!(
            model.agent_status(&workbox_pane),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            model.agent_status(&FleetPaneRef::new(local, "w9:p9")),
            None,
            "an unknown pane has no status"
        );
        assert_eq!(
            model.agent_status(&FleetPaneRef::new(host("absent"), "w1:p1")),
            None,
            "an unknown host has no status"
        );
    }

    #[test]
    fn rebuild_bumps_the_generation_and_replaces_the_groups() {
        let mut state = FleetState::new(vec![local_spec()]);
        let mut model = FleetSidebarModel::default();
        assert_eq!(model.generation, 0);
        model.rebuild(&state, &HashSet::new(), AgentPanelSortConfig::Priority);
        assert_eq!(model.generation, 1);
        assert_eq!(model.groups.len(), 1);

        connect(&mut state, &HostId::local(), "0.8.2-fork");
        model.rebuild(&state, &HashSet::new(), AgentPanelSortConfig::Priority);
        assert_eq!(model.generation, 2);
        assert_eq!(model.groups.len(), 1, "a rebuild replaces, never appends");
        assert_eq!(model.groups[0].header.state, HostRowState::Connected);
    }

    #[test]
    fn host_picker_rows_label_every_connection_state() {
        let mut state = FleetState::new(vec![
            local_spec(),
            ssh_spec("workbox"),
            spec("stale", true),
            spec("old", true),
            spec("off", false),
        ]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![workspace("w1", "repo")],
                vec![
                    agent("w1:p1", AgentStatus::Blocked, 1),
                    agent("w1:p2", AgentStatus::Working, 2),
                ],
            )),
        );
        state.apply(&host("workbox"), HostEvent::Connecting { attempt: 1 });
        state.apply(
            &host("stale"),
            HostEvent::Unavailable {
                reason: "connection refused".to_string(),
                retry_in: None,
            },
        );
        state.apply(
            &host("old"),
            HostEvent::Incompatible {
                generation: Some(2),
                reason: "endpoint generation 2".to_string(),
            },
        );

        let rows = host_picker_rows(&state);
        assert_eq!(
            rows.iter().map(|row| row.label.clone()).collect::<Vec<_>>(),
            vec![
                "local  connected 0.8.2-fork  1 blocked · 1 working",
                "workbox  connecting",
                "stale  unavailable  connection refused",
                "old  incompatible  endpoint generation 2",
                "off  unavailable  host disabled in [fleet]",
            ]
        );
        assert_eq!(
            rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec!["local", "ssh", "local", "local", "local"]
        );
        assert!(rows[0].active, "the picker marks the routing target");
        assert!(rows[1..].iter().all(|row| !row.active));
        assert_eq!(
            rows.iter().map(|row| row.enabled).collect::<Vec<_>>(),
            vec![true, true, true, true, false],
            "a disabled host is listed but may not be activated"
        );
        assert_eq!(rows[3].state, HostRowState::Incompatible);
    }

    #[test]
    fn the_model_caches_the_picker_rows_it_was_rebuilt_from() {
        let mut state = FleetState::new(vec![local_spec(), ssh_spec("workbox")]);
        connect(&mut state, &HostId::local(), "0.8.2-fork");
        state.set_active_host(Some(HostId::local()));
        let model = model_of(&state);

        assert_eq!(
            model.picker,
            host_picker_rows(&state),
            "the picker overlay reads the model, so it must carry the same rows"
        );
        assert_eq!(
            model.picker.len(),
            model.groups.len(),
            "one picker row per group keeps the picker's 1-9 jump aligned with the sidebar"
        );
        assert!(model.picker[0].active);
    }

    #[test]
    fn same_rows_sees_a_change_the_groups_alone_would_hide() {
        let mut state = FleetState::new(vec![local_spec()]);
        connect(&mut state, &HostId::local(), "0.8.2-fork");
        let before = model_of(&state);

        // A reconnect to an upgraded server: same rollup, same header label,
        // so `groups` compares equal — but the picker names the version.
        connect(&mut state, &HostId::local(), "0.9.0-fork");
        let mut after = before.clone();
        after.rebuild(&state, &HashSet::new(), AgentPanelSortConfig::Priority);

        assert_eq!(before.groups, after.groups);
        assert!(!before.same_rows(&after), "the picker row moved");
        assert!(before.same_rows(&before.clone()));
        assert_ne!(
            before.generation, after.generation,
            "generation cannot answer this question"
        );
    }

    #[test]
    fn host_status_rank_matches_upstreams_table() {
        // Pinned against `status_priority` in `src/client/shell.rs`.
        assert_eq!(host_status_rank(AgentStatus::Blocked), 4);
        assert_eq!(host_status_rank(AgentStatus::Done), 3);
        assert_eq!(host_status_rank(AgentStatus::Working), 2);
        assert_eq!(host_status_rank(AgentStatus::Idle), 1);
        assert_eq!(host_status_rank(AgentStatus::Unknown), 0);
    }

    #[test]
    fn the_adversarial_state_rebuilds_without_mixing_hosts() {
        let state = FleetState::test_with_adversarial_identity_state();
        state.assert_invariants_for_test();
        let model = model_of(&state);
        assert_eq!(
            model
                .groups
                .iter()
                .map(|group| group.host.to_string())
                .collect::<Vec<_>>(),
            vec!["local", "twin", "off"]
        );
        // Both hosts report the same boot_id and the same `w1:p1`; the rows
        // must still address two different panes.
        assert_eq!(
            model
                .groups
                .iter()
                .flat_map(|group| group.agents.iter())
                .map(|agent| agent.pane.to_string())
                .collect::<Vec<_>>(),
            vec!["local/w1:p1", "twin/w1:p1"]
        );
        assert_eq!(
            model.agent_status(&FleetPaneRef::new(host("twin"), "w1:p1")),
            Some(AgentStatus::Blocked)
        );
        // `active_host` names a disabled host by construction; the flag follows
        // the state rather than second-guessing it, and `enabled` is what stops
        // a click from routing there.
        let off = model
            .group(&host("off"))
            .expect("the disabled host is listed");
        assert!(off.header.active);
        assert!(!off.header.enabled);
        assert!(model.groups[..2].iter().all(|group| !group.header.active));
        // `twin` is Incompatible while holding a snapshot: rows kept, no counts.
        let twin = model.group(&host("twin")).expect("twin is listed");
        assert_eq!(twin.header.state, HostRowState::Incompatible);
        assert_eq!(twin.header.rollup, AgentRollup::default());
        assert_eq!(twin.agents.len(), 1);
        assert_eq!(twin.header.label, "▾ twin · incompatible");
        assert_eq!(twin.header.reason.as_deref(), Some("endpoint generation 2"));
    }

    #[test]
    fn test_new_rebuilds_into_two_connecting_groups() {
        let state = FleetState::test_new();
        state.assert_invariants_for_test();
        let model = model_of(&state);
        assert_eq!(
            model
                .groups
                .iter()
                .map(|group| group.header.label.clone())
                .collect::<Vec<_>>(),
            vec!["▾ local · connecting", "▾ workbox · connecting"]
        );
        assert!(model.groups.iter().all(|group| group.agents.is_empty()));
        assert!(model.groups[0].header.active);
    }

    #[test]
    fn the_default_spaces_sort_keeps_the_snapshots_own_order() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-1",
                1,
                vec![workspace("w1", "repo")],
                vec![
                    agent("w1:p1", AgentStatus::Idle, 10),
                    agent("w1:p2", AgentStatus::Blocked, 1),
                    agent("w1:p3", AgentStatus::Working, 5),
                ],
            )),
        );

        // `Spaces` is the shipped default and upstream leaves the snapshot's
        // own workspace grouping alone under it; sorting here anyway would
        // make a fleet group disagree with the single-host sidebar it stands
        // in for.
        let mut model = FleetSidebarModel::default();
        model.rebuild(&state, &HashSet::new(), AgentPanelSortConfig::default());
        assert_eq!(
            AgentPanelSortConfig::default(),
            AgentPanelSortConfig::Spaces
        );
        assert_eq!(
            labels(&model.groups[0].agents),
            vec!["agent-w1:p1", "agent-w1:p2", "agent-w1:p3"]
        );

        model.rebuild(&state, &HashSet::new(), AgentPanelSortConfig::Priority);
        assert_eq!(
            labels(&model.groups[0].agents),
            vec!["agent-w1:p2", "agent-w1:p3", "agent-w1:p1"],
            "the same state under the other sort"
        );
    }

    #[test]
    fn a_named_view_wins_over_either_sort() {
        let mut state = FleetState::new(vec![local_spec()]);
        let local = HostId::local();
        connect(&mut state, &local, "0.8.2-fork");
        let mut projection = snapshot(
            "boot-1",
            1,
            vec![workspace("w1", "repo")],
            vec![
                agent("w1:p1", AgentStatus::Idle, 1),
                agent("w1:p2", AgentStatus::Blocked, 2),
            ],
        );
        projection.agent_view_label = Some("recent".to_string());
        projection.agent_order = vec!["w1:p1".to_string(), "w1:p2".to_string()];
        state.apply(&local, HostEvent::Snapshot(projection));
        for sort in [AgentPanelSortConfig::Spaces, AgentPanelSortConfig::Priority] {
            let mut model = FleetSidebarModel::default();
            model.rebuild(&state, &HashSet::new(), sort);
            assert_eq!(
                labels(&model.groups[0].agents),
                vec!["agent-w1:p1", "agent-w1:p2"],
                "{sort:?} must not reorder a named view"
            );
        }
    }

    #[test]
    fn a_fleet_with_no_hosts_has_nothing_to_draw() {
        let state = FleetState::new(Vec::new());
        let model = model_of(&state);
        assert!(model.groups.is_empty());
        assert!(model.group(&HostId::local()).is_none());
        assert!(model
            .agent_status(&FleetPaneRef::new(HostId::local(), "w1:p1"))
            .is_none());
        assert!(host_picker_rows(&state).is_empty());
    }

    #[test]
    fn row_state_follows_the_connection_it_came_from() {
        for connection in [
            HostConnection::Connected {
                server_version: "0.8.2-fork".to_string(),
                methods: Vec::new(),
            },
            HostConnection::Connecting { attempt: 3 },
            HostConnection::Unavailable {
                reason: "connection refused".to_string(),
                retry_in: None,
            },
            HostConnection::Incompatible {
                generation: Some(2),
                reason: "endpoint generation 2".to_string(),
            },
        ] {
            let row = HostRowState::from_connection(&connection);
            let expected = match &connection {
                HostConnection::Connected { .. } => HostRowState::Connected,
                HostConnection::Connecting { attempt } => {
                    HostRowState::Connecting { attempt: *attempt }
                }
                HostConnection::Unavailable { .. } => HostRowState::Unavailable,
                HostConnection::Incompatible { .. } => HostRowState::Incompatible,
            };
            assert_eq!(row, expected, "{}", connection.state_name());
            assert_eq!(row.is_connected(), connection.is_connected());
        }
    }

    #[test]
    fn host_status_rank_still_matches_upstreams_source() {
        // `host_status_rank` is a copy: the purity guard forbids this module
        // from reaching into `crate::client`, and upstream's `status_priority`
        // is private to `src/client/shell.rs`. Read the table out of that file
        // instead, so upstream renumbering it fails here rather than silently
        // reordering a fleet group.
        const UPSTREAM: &str = include_str!("../client/shell.rs");
        let start = UPSTREAM
            .find("fn status_priority(")
            .expect("upstream still has status_priority");
        let body = &UPSTREAM[start..];
        let end = body.find("\n}").expect("status_priority has a body");
        let body = &body[..end];
        for (status, rank) in [
            (AgentStatus::Blocked, 4),
            (AgentStatus::Done, 3),
            (AgentStatus::Working, 2),
            (AgentStatus::Idle, 1),
            (AgentStatus::Unknown, 0),
        ] {
            let arm = format!("AgentStatus::{status:?} => {rank},");
            assert!(
                body.contains(&arm),
                "upstream status_priority no longer maps {arm:?}; \
                 host_status_rank must be updated with it:\n{body}"
            );
            assert_eq!(host_status_rank(status), rank);
        }
    }
}
