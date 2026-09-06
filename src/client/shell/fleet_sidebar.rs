//! The Fleet console's sidebar: one group per host (fork).
//!
//! Both sections — spaces and agents — become a list of host groups: a header
//! with that host's counts, then its rows. The *active* host's rows are drawn
//! by the very same code the single-host client uses (worktree grouping,
//! tokens, gaps, hit rects), so nothing about the machine you are working on
//! changes when the console has five hosts instead of one.
//!
//! Performance (the constraint this whole design exists for): a frame does no
//! formatting, no sorting and no allocation *per host*. Every label, count and
//! glyph choice for an inactive host was computed once, at
//! `FleetSidebarModel::rebuild` time, and every one of those rows is exactly
//! one line high — the only variable height, a header's optional reason line,
//! is cached by `FleetShellState::header_height`. What a frame does is build
//! two small integer vectors, ask `list_scroll_metrics` (itself O(visible))
//! for the window, and draw the rows inside it.
//!
//! Hit rects follow the same split as the model: the active host's rows go
//! into `ShellHitMap::workspaces`/`agents` with bare server-side ids, exactly
//! as before; every other row goes into `ShellHitMap::fleet_rows` as a
//! host-qualified `FleetSidebarHit`. A click can therefore never resolve one
//! host's id against another.

use super::*;

use crate::fleet::sidebar::HostRowState;

use super::super::fleet::{FleetShellState, FleetSidebarHit};

/// Columns of indent for a row that belongs to a host group.
const GROUP_INDENT: u16 = 2;
/// Drawn on the header of the host a switch is waiting on.
const SWITCHING_GLYPH: &str = "…";

/// One drawable row of the spaces section.
enum SpaceItem {
    /// `groups[index]`'s header.
    Header(usize),
    /// `entries[index]` of the active host's own workspace list.
    ActiveWorkspace(usize),
    /// `groups[group].workspaces[row]` of a host that is not active.
    HostWorkspace { group: usize, row: usize },
}

/// One drawable row of the agents section.
enum AgentItem {
    Header(usize),
    /// `active[index]` of the active host's own agent rows.
    ActiveAgent(usize),
    HostAgent {
        group: usize,
        row: usize,
    },
}

/// The spaces section of a console: a host group per configured host.
pub(super) fn render_fleet_spaces(
    buffer: &mut Buffer,
    body: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
    fleet: &FleetShellState,
) {
    let entries = active_workspace_entries(snapshot, state, fleet);
    let mut items = Vec::with_capacity(fleet.model.groups.len() + entries.len());
    let mut heights = Vec::with_capacity(items.capacity());
    let mut gaps = Vec::with_capacity(items.capacity());
    for (index, group) in fleet.model.groups.iter().enumerate() {
        items.push(SpaceItem::Header(index));
        heights.push(fleet.header_height(index));
        gaps.push(0);
        if group.header.collapsed {
            continue;
        }
        if fleet.is_active(&group.host) {
            for (position, entry) in entries.iter().enumerate() {
                items.push(SpaceItem::ActiveWorkspace(position));
                heights.push(active_workspace_height(snapshot, config, state, entry));
                // Upstream's rule, and only between the active host's own
                // rows: a gap before the next host's header would read as a
                // gap inside this group.
                gaps.push(
                    entries
                        .get(position + 1)
                        .map_or(0, |next| u16::from(!next.indented) * config.spaces.row_gap),
                );
            }
            continue;
        }
        for row in 0..group.workspaces.len() {
            items.push(SpaceItem::HostWorkspace { group: index, row });
            heights.push(1);
            gaps.push(0);
        }
    }

    let mut metrics =
        super::scroll::list_scroll_metrics(&heights, &gaps, body.height, *state.workspace_scroll);
    if !body.is_empty() && std::mem::take(state.reveal_focused_workspace) {
        if let Some(target) = items.iter().position(|item| match item {
            SpaceItem::ActiveWorkspace(position) => entries
                .get(*position)
                .and_then(|entry| snapshot.workspaces.get(entry.index))
                .is_some_and(|workspace| workspace.focused),
            _ => false,
        }) {
            *state.workspace_scroll = super::scroll::list_scroll_start_to_reveal(
                &heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
                target,
            );
            metrics = super::scroll::list_scroll_metrics(
                &heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
            );
        }
    }
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *state.workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));

    let mut y = body.y;
    for (index, item) in items.iter().enumerate().skip(*state.workspace_scroll) {
        let height = heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        match item {
            SpaceItem::Header(group) => {
                render_host_header(buffer, rect, config, fleet, *group, hits);
            }
            SpaceItem::ActiveWorkspace(position) => {
                if let Some(entry) = entries.get(*position) {
                    render_active_workspace(buffer, rect, snapshot, config, state, entry, hits);
                }
            }
            SpaceItem::HostWorkspace { group, row } => {
                let Some(row) = fleet
                    .model
                    .groups
                    .get(*group)
                    .and_then(|group| group.workspaces.get(*row))
                else {
                    continue;
                };
                render_group_row(buffer, rect, config, row.status, &row.label, row.focused);
                hits.fleet_rows
                    .push((rect, FleetSidebarHit::Workspace(row.workspace.clone())));
            }
        }
        y = y.saturating_add(height).saturating_add(gaps[index]);
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, &config.palette);
    }
}

/// The agents section of a console: the same host groups, agent rows inside.
pub(super) fn render_fleet_agents(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
    fleet: &FleetShellState,
) {
    let body = super::super::agent_sidebar::render_agent_panel_header(
        buffer, area, snapshot, config, hits,
    );
    if body.is_empty() {
        *agent_scroll = 0;
        return;
    }
    let active_expanded = fleet
        .model
        .group(&fleet.active)
        .is_some_and(|group| !group.header.collapsed);
    let active = if active_expanded {
        super::super::agent_sidebar::agent_rows(snapshot, config)
    } else {
        Vec::new()
    };

    let mut items = Vec::with_capacity(fleet.model.groups.len() + active.len());
    let mut heights = Vec::with_capacity(items.capacity());
    let mut gaps = Vec::with_capacity(items.capacity());
    for (index, group) in fleet.model.groups.iter().enumerate() {
        items.push(AgentItem::Header(index));
        heights.push(fleet.header_height(index));
        gaps.push(0);
        if group.header.collapsed {
            continue;
        }
        if fleet.is_active(&group.host) {
            for (position, row) in active.iter().enumerate() {
                items.push(AgentItem::ActiveAgent(position));
                heights.push(row.rows.len().clamp(1, u16::MAX as usize) as u16);
                gaps.push(if position + 1 < active.len() {
                    config.agents.row_gap
                } else {
                    0
                });
            }
            continue;
        }
        for row in 0..group.agents.len() {
            items.push(AgentItem::HostAgent { group: index, row });
            heights.push(1);
            gaps.push(0);
        }
    }

    let metrics = super::scroll::list_scroll_metrics(&heights, &gaps, body.height, *agent_scroll);
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));

    let mut y = body.y;
    for (index, item) in items.iter().enumerate().skip(*agent_scroll) {
        let height = heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        match item {
            AgentItem::Header(group) => {
                render_host_header(buffer, rect, config, fleet, *group, hits);
            }
            AgentItem::ActiveAgent(position) => {
                let Some(row) = active.get(*position) else {
                    continue;
                };
                hits.agents.push((rect, row.pane_id.clone()));
                super::super::agent_sidebar::render_agent_row(buffer, rect, row, config);
            }
            AgentItem::HostAgent { group, row } => {
                let Some(row) = fleet
                    .model
                    .groups
                    .get(*group)
                    .and_then(|group| group.agents.get(*row))
                else {
                    continue;
                };
                render_group_row(buffer, rect, config, row.status, &row.label, row.focused);
                hits.fleet_rows
                    .push((rect, FleetSidebarHit::Agent(row.pane.clone())));
            }
        }
        y = y.saturating_add(height).saturating_add(gaps[index]);
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.agent_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, &config.palette);
    }
}

/// The active host's workspace entries, or none when its group is collapsed.
fn active_workspace_entries(
    snapshot: &ClientShellSnapshot,
    state: &ShellRenderState<'_>,
    fleet: &FleetShellState,
) -> Vec<WorkspaceEntry> {
    let expanded = fleet
        .model
        .group(&fleet.active)
        .is_some_and(|group| !group.header.collapsed);
    if !expanded {
        return Vec::new();
    }
    super::sidebar::workspace_entries(snapshot, state.collapsed_groups)
}

fn active_workspace_height(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &ShellRenderState<'_>,
    entry: &WorkspaceEntry,
) -> u16 {
    snapshot
        .workspaces
        .get(entry.index)
        .map(|workspace| {
            super::sidebar::workspace_rows(
                workspace,
                super::sidebar::displayed_workspace_status(
                    snapshot,
                    workspace,
                    state.collapsed_groups,
                ),
                entry.indented,
                &config.spaces,
            )
            .len()
            .clamp(1, u16::MAX as usize) as u16
        })
        .unwrap_or(1)
}

/// One host's header: the cached label, and its reason when it is not up.
fn render_host_header(
    buffer: &mut Buffer,
    rect: Rect,
    config: &ClientShellConfig,
    fleet: &FleetShellState,
    group: usize,
    hits: &mut ShellHitMap,
) {
    let Some(group) = fleet.model.groups.get(group) else {
        return;
    };
    let palette = &config.palette;
    let header = &group.header;
    let connected = header.state == HostRowState::Connected;
    let style = if header.active {
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD)
    } else if connected && header.enabled {
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD)
    } else {
        // Unreachable, incompatible, still connecting, or disabled: dimmed,
        // with the reason underneath.
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::DIM)
    };
    let line = Rect::new(rect.x, rect.y, rect.width, 1);
    if header.active {
        buffer.set_style(line, Style::default().bg(palette.active_row_bg));
    }
    put_text(buffer, line.x, line.y, line.width, &header.label, style);
    if fleet.is_switching_to(&header.host) {
        put_right_text(
            buffer,
            line,
            line.y,
            SWITCHING_GLYPH,
            Style::default().fg(palette.accent),
        );
    }
    if let Some(reason) = header.reason.as_deref() {
        if rect.height > 1 {
            put_text(
                buffer,
                rect.x.saturating_add(GROUP_INDENT),
                rect.y.saturating_add(1),
                rect.width.saturating_sub(GROUP_INDENT),
                reason,
                Style::default()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
    }
    // The collapse cell first: it is inside the header line, and the first
    // containing rect wins in `handle_fleet_sidebar_click`.
    hits.fleet_rows.push((
        Rect::new(line.x, line.y, 1.min(line.width), line.height),
        FleetSidebarHit::HostCollapse(header.host.clone()),
    ));
    hits.fleet_rows
        .push((line, FleetSidebarHit::HostHeader(header.host.clone())));
}

/// One line of a host that is not active: status glyph, then its label.
fn render_group_row(
    buffer: &mut Buffer,
    rect: Rect,
    config: &ClientShellConfig,
    status: crate::api::schema::AgentStatus,
    label: &str,
    focused: bool,
) {
    let palette = &config.palette;
    let x = put_segment(
        buffer,
        rect.x.saturating_add(GROUP_INDENT),
        rect.y,
        rect.right(),
        status_icon(status, config.status_indicators),
        Style::default().fg(status_color(status, palette)),
    );
    let x = put_segment(
        buffer,
        x,
        rect.y,
        rect.right(),
        " ",
        Style::default().fg(palette.overlay0),
    );
    put_text(
        buffer,
        x,
        rect.y,
        rect.right().saturating_sub(x),
        label,
        Style::default()
            .fg(if focused {
                palette.subtext0
            } else {
                palette.overlay0
            })
            .add_modifier(Modifier::DIM),
    );
}

/// The active host's workspace row, exactly as the single-host sidebar draws it.
fn render_active_workspace(
    buffer: &mut Buffer,
    rect: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &ShellRenderState<'_>,
    entry: &WorkspaceEntry,
    hits: &mut ShellHitMap,
) {
    let Some(workspace) = snapshot.workspaces.get(entry.index) else {
        return;
    };
    let palette = &config.palette;
    let status =
        super::sidebar::displayed_workspace_status(snapshot, workspace, state.collapsed_groups);
    let rows = super::sidebar::workspace_rows(workspace, status, entry.indented, &config.spaces);
    let selected = state.selected_workspace_id == Some(workspace.workspace_id.as_str());
    let dragged = state.dragged_workspace_id == Some(workspace.workspace_id.as_str());
    if selected {
        buffer.set_style(rect, Style::default().bg(palette.selection_bg));
    } else if dragged {
        buffer.set_style(rect, Style::default().bg(palette.surface1));
    } else if workspace.focused {
        buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
    }
    super::sidebar::render_workspace_rows(
        buffer,
        rect,
        workspace,
        status,
        config.status_indicators,
        entry,
        rows,
        selected,
        dragged,
        palette,
    );
    let group_toggle = super::sidebar::parent_group_key(snapshot, entry.index).map(|key| {
        let toggle = Rect::new(rect.right().saturating_sub(1), rect.y, 1, 1);
        put_text(
            buffer,
            toggle.x,
            toggle.y,
            toggle.width,
            if state.collapsed_groups.contains(&key) {
                "▸"
            } else {
                "▾"
            },
            Style::default().fg(palette.accent),
        );
        (toggle, key)
    });
    hits.workspaces.push(WorkspaceHit {
        rect,
        workspace_id: workspace.workspace_id.clone(),
        indented: entry.indented,
        group_toggle,
    });
}
