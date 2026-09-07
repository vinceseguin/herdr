//! The serializable shape of a fleet status.
//!
//! [`FleetStatusReport`] is what `herdr fleet status --json` prints and what
//! the fork's gateway serves. It is an **additive** JSON contract: fields are
//! added, never renamed or repurposed, and every enum a reader decodes has an
//! `Unknown` fallback so an older client keeps working against a newer one.
//!
//! Pure: no sockets, no async, no ratatui — the report is derived from
//! [`FleetState`] and nothing else.

use serde::{Deserialize, Serialize};

use crate::api::schema::AgentStatus;
use crate::fleet::hosts::HostId;
use crate::fleet::refs::{FleetPaneRef, FleetWorkspaceRef};
use crate::fleet::state::{AgentRollup, FleetState, HostConnection, HostState, MergedAgent};

/// Schema marker carried by every [`FleetStatusReport`].
pub const FLEET_STATUS_SCHEMA: &str = "herdr.fleet.status.v1";

/// One fleet status: every configured host, the merged agent list, the totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStatusReport {
    /// Always [`FLEET_STATUS_SCHEMA`] when produced by this client.
    pub schema: String,
    /// Version of the client that produced the report.
    pub client_version: String,
    pub active_host: Option<HostId>,
    pub hosts: Vec<HostReport>,
    /// Merged, blocked-first agent list; see `FleetState::merged_agents`.
    pub agents: Vec<AgentReport>,
    pub counts: AgentRollup,
}

/// One host in a [`FleetStatusReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReport {
    pub id: HostId,
    /// `"local"` or `"ssh"`; a reader must tolerate a future transport name.
    pub kind: String,
    pub target: Option<String>,
    pub session: Option<String>,
    pub enabled: bool,
    pub connection: ConnectionReport,
    /// Boot of the server the last snapshot came from, if any.
    pub boot_id: Option<String>,
    /// Revision of the last snapshot, if any.
    ///
    /// Comparable only within one *client connection*: the server restarts it
    /// at 1 for each connection while `boot_id` outlives them all, so a host
    /// that reconnects reports a lower revision on the same boot. A reader
    /// must not treat it as a monotonic freshness counter.
    pub revision: Option<u64>,
    pub counts: AgentRollup,
    /// Workspaces of the last snapshot. Kept while a host is unavailable so a
    /// client can dim them instead of blanking the host.
    pub workspaces: Vec<WorkspaceReport>,
}

/// Connection state of one host, as JSON.
///
/// `Unknown` is the required forward-compatibility fallback: a client reading a
/// report from a newer fleet must not fail on a state it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionReport {
    Connecting {
        attempt: u32,
    },
    Connected {
        server_version: String,
    },
    Unavailable {
        reason: String,
        retry_in_ms: Option<u64>,
    },
    Incompatible {
        generation: Option<u32>,
        reason: String,
    },
    /// A state produced by a newer fleet client.
    #[serde(other)]
    Unknown,
}

impl ConnectionReport {
    /// Short lowercase state name, matching the JSON `state` tag.
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Connecting { .. } => "connecting",
            Self::Connected { .. } => "connected",
            Self::Unavailable { .. } => "unavailable",
            Self::Incompatible { .. } => "incompatible",
            Self::Unknown => "unknown",
        }
    }

    /// Operator-facing explanation, when there is one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Connecting { .. } | Self::Connected { .. } | Self::Unknown => None,
            Self::Unavailable { reason, .. } | Self::Incompatible { reason, .. } => Some(reason),
        }
    }

    /// Server version, when the host is connected.
    pub fn server_version(&self) -> Option<&str> {
        match self {
            Self::Connected { server_version } => Some(server_version),
            _ => None,
        }
    }

    /// Back to runtime state.
    ///
    /// Lossy on purpose: the report never carries the advertised method list,
    /// so a connection rebuilt from JSON advertises nothing and every
    /// method-gated action fails closed. An unknown state becomes
    /// `Unavailable`, which is the safe reading of "this client cannot tell".
    pub fn into_connection(self) -> HostConnection {
        match self {
            Self::Connecting { attempt } => HostConnection::Connecting { attempt },
            Self::Connected { server_version } => HostConnection::Connected {
                server_version,
                methods: Vec::new(),
            },
            Self::Unavailable {
                reason,
                retry_in_ms,
            } => HostConnection::Unavailable {
                reason,
                retry_in: retry_in_ms.map(std::time::Duration::from_millis),
            },
            Self::Incompatible { generation, reason } => {
                HostConnection::Incompatible { generation, reason }
            }
            Self::Unknown => HostConnection::Unavailable {
                reason: "unknown connection state reported by a newer client".to_string(),
                retry_in: None,
            },
        }
    }
}

impl From<&HostConnection> for ConnectionReport {
    fn from(connection: &HostConnection) -> Self {
        match connection {
            HostConnection::Connecting { attempt } => Self::Connecting { attempt: *attempt },
            HostConnection::Connected { server_version, .. } => Self::Connected {
                server_version: server_version.clone(),
            },
            HostConnection::Unavailable { reason, retry_in } => Self::Unavailable {
                reason: reason.clone(),
                retry_in_ms: retry_in
                    .map(|retry_in| u64::try_from(retry_in.as_millis()).unwrap_or(u64::MAX)),
            },
            HostConnection::Incompatible { generation, reason } => Self::Incompatible {
                generation: *generation,
                reason: reason.clone(),
            },
        }
    }
}

/// One workspace of one host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReport {
    /// Host-qualified reference, `host/w1`.
    pub r#ref: FleetWorkspaceRef,
    pub workspace_id: String,
    pub label: String,
    #[serde(deserialize_with = "crate::fleet::state::deserialize_agent_status")]
    pub agent_status: AgentStatus,
    pub focused: bool,
}

/// One agent of the merged list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    /// Host-qualified reference, `host/w1:p1`.
    pub r#ref: FleetPaneRef,
    pub host: HostId,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub workspace_label: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    #[serde(deserialize_with = "crate::fleet::state::deserialize_agent_status")]
    pub agent_status: AgentStatus,
    /// The host's own counter; only comparable within one host boot.
    pub state_change_seq: u64,
    /// Fleet-wide recency, comparable across hosts.
    pub fleet_change_seq: u64,
    pub focused: bool,
}

impl From<&MergedAgent> for AgentReport {
    fn from(agent: &MergedAgent) -> Self {
        Self {
            r#ref: agent.pane.clone(),
            host: agent.pane.host.clone(),
            pane_id: agent.pane.pane_id.clone(),
            workspace_id: agent.workspace.workspace_id.clone(),
            tab_id: agent.tab.tab_id.clone(),
            workspace_label: agent.workspace_label.clone(),
            name: agent.name.clone(),
            title: agent.title.clone(),
            agent: agent.agent.clone(),
            display_agent: agent.display_agent.clone(),
            agent_status: agent.agent_status,
            state_change_seq: agent.state_change_seq,
            fleet_change_seq: agent.fleet_change_seq,
            focused: agent.focused,
        }
    }
}

impl FleetStatusReport {
    /// Render the current state as a report.
    ///
    /// Takes `&mut` because the merged list is cached in the state; nothing
    /// about the fleet changes.
    pub fn from_state(state: &mut FleetState, client_version: &str) -> Self {
        let active_host = state.active_host().cloned();
        let counts = state.totals();
        let hosts = state.hosts().iter().map(host_report).collect::<Vec<_>>();
        let agents = state
            .merged_agents()
            .iter()
            .map(AgentReport::from)
            .collect();
        Self {
            schema: FLEET_STATUS_SCHEMA.to_string(),
            client_version: client_version.to_string(),
            active_host,
            hosts,
            agents,
            counts,
        }
    }

    /// Human-readable rendering for `herdr fleet status`.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let active = self
            .active_host
            .as_ref()
            .map(HostId::to_string)
            .unwrap_or_else(|| "none".to_string());
        out.push_str(&format!(
            "client {}  active host: {active}\n\n",
            self.client_version
        ));

        let mut rows = vec![vec![
            "HOST".to_string(),
            "KIND".to_string(),
            "STATE".to_string(),
            "VERSION".to_string(),
            "BLOCKED".to_string(),
            "WORKING".to_string(),
            "DONE".to_string(),
            "IDLE".to_string(),
            "UNKNOWN".to_string(),
        ]];
        for host in &self.hosts {
            rows.push(vec![
                host.id.to_string(),
                host.kind.clone(),
                host.connection.state_name().to_string(),
                host.connection.server_version().unwrap_or("-").to_string(),
                host.counts.blocked.to_string(),
                host.counts.working.to_string(),
                host.counts.done.to_string(),
                host.counts.idle.to_string(),
                host.counts.unknown.to_string(),
            ]);
        }
        out.push_str(&render_table(&rows));

        for host in &self.hosts {
            if let Some(reason) = host.connection.reason() {
                out.push_str(&format!("  ! {}: {reason}\n", host.id));
            }
        }

        out.push('\n');
        if self.agents.is_empty() {
            out.push_str("no agents\n");
            return out;
        }
        let mut rows = vec![vec![
            "AGENT".to_string(),
            "STATUS".to_string(),
            "WORKSPACE".to_string(),
            "NAME".to_string(),
        ]];
        for agent in &self.agents {
            rows.push(vec![
                agent.r#ref.to_string(),
                agent_status_name(agent.agent_status).to_string(),
                agent.workspace_label.clone(),
                agent
                    .name
                    .clone()
                    .or_else(|| agent.title.clone())
                    .unwrap_or_else(|| "-".to_string()),
            ]);
        }
        out.push_str(&render_table(&rows));
        out
    }
}

fn host_report(host: &HostState) -> HostReport {
    let snapshot = host.snapshot.as_deref();
    HostReport {
        id: host.spec.id.clone(),
        kind: host.spec.kind.as_str().to_string(),
        target: host.spec.kind.target().map(str::to_string),
        session: host.spec.kind.session().map(str::to_string),
        enabled: host.spec.enabled,
        connection: ConnectionReport::from(&host.connection),
        boot_id: snapshot.map(|snapshot| snapshot.boot_id.clone()),
        revision: snapshot.map(|snapshot| snapshot.revision),
        counts: host.rollup,
        workspaces: snapshot
            .map(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .filter(|workspace| {
                        crate::fleet::refs::is_valid_resource_id(&workspace.workspace_id)
                    })
                    .map(|workspace| WorkspaceReport {
                        r#ref: FleetWorkspaceRef::new(
                            host.spec.id.clone(),
                            workspace.workspace_id.clone(),
                        ),
                        workspace_id: workspace.workspace_id.clone(),
                        label: workspace.label.clone(),
                        agent_status: workspace.agent_status,
                        focused: workspace.focused,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Lowercase status name, matching the JSON encoding of [`AgentStatus`].
fn agent_status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
    }
}

/// Left-aligned fixed-width table; every line is trimmed of trailing padding.
///
/// Column widths are terminal columns, not `char`s: workspace labels and agent
/// names are user data and may hold wide or zero-width characters.
fn render_table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(display_width(cell));
            }
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            let width = widths.get(index).copied().unwrap_or(0);
            if index + 1 < row.len() {
                for _ in display_width(cell)..width {
                    line.push(' ');
                }
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Terminal columns `text` occupies.
fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::state::HostEvent;
    use crate::protocol::ClientShellSnapshot;
    use std::time::Duration;

    /// The frozen generation-1 snapshot the endpoint contract pins.
    const FROZEN_SNAPSHOT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/endpoint-snapshot-v1.json"
    ));

    fn frozen_snapshot() -> Box<ClientShellSnapshot> {
        Box::new(serde_json::from_str(FROZEN_SNAPSHOT).expect("frozen snapshot decodes"))
    }

    fn connected(state: &mut FleetState, id: &HostId) {
        state.apply(
            id,
            HostEvent::Connected {
                server_version: "0.8.2-fork".to_string(),
                methods: vec!["pane.write".to_string()],
            },
        );
    }

    /// Both hosts of `test_new()` fed the same frozen snapshot.
    fn two_hosts_on_the_frozen_snapshot() -> FleetState {
        let mut state = FleetState::test_new();
        let local = HostId::local();
        let workbox = HostId::new("workbox").expect("valid host name");
        connected(&mut state, &local);
        connected(&mut state, &workbox);
        state.apply(&local, HostEvent::Snapshot(frozen_snapshot()));
        state.apply(&workbox, HostEvent::Snapshot(frozen_snapshot()));
        state
    }

    #[test]
    fn the_frozen_snapshot_on_two_hosts_reports_host_qualified_refs() {
        let mut state = two_hosts_on_the_frozen_snapshot();
        state.assert_invariants_for_test();
        let report = FleetStatusReport::from_state(&mut state, "0.8.2-test");

        assert_eq!(report.schema, FLEET_STATUS_SCHEMA);
        assert_eq!(report.client_version, "0.8.2-test");
        assert_eq!(report.active_host, Some(HostId::local()));
        assert_eq!(report.counts.blocked, 2);
        assert_eq!(report.counts.total(), 2);

        // The same `w1:p1` on two servers is two distinct fleet references.
        let refs = report
            .agents
            .iter()
            .map(|agent| agent.r#ref.to_string())
            .collect::<Vec<_>>();
        assert_eq!(refs, vec!["workbox/w1:p1", "local/w1:p1"]);
        assert!(report
            .agents
            .iter()
            .all(|agent| agent.pane_id == "w1:p1" && agent.workspace_label == "repo"));

        let workspaces = report
            .hosts
            .iter()
            .flat_map(|host| host.workspaces.iter())
            .map(|workspace| workspace.r#ref.to_string())
            .collect::<Vec<_>>();
        assert_eq!(workspaces, vec!["local/w1", "workbox/w1"]);
        // The fixture's workspace status is a value this client does not know.
        assert!(report
            .hosts
            .iter()
            .flat_map(|host| host.workspaces.iter())
            .all(|workspace| workspace.agent_status == AgentStatus::Unknown));

        for host in &report.hosts {
            assert_eq!(host.boot_id.as_deref(), Some("boot-v1"));
            assert_eq!(host.revision, Some(7));
            assert_eq!(host.kind, "local");
            assert!(host.enabled);
        }
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let mut state = two_hosts_on_the_frozen_snapshot();
        let report = FleetStatusReport::from_state(&mut state, "0.8.2-test");
        let json = serde_json::to_string(&report).expect("report serializes");
        let decoded: FleetStatusReport = serde_json::from_str(&json).expect("report decodes");
        assert_eq!(decoded, report);

        let value: serde_json::Value = serde_json::from_str(&json).expect("report is json");
        assert_eq!(value["schema"], FLEET_STATUS_SCHEMA);
        assert_eq!(value["active_host"], "local");
        assert_eq!(value["agents"][0]["ref"], "workbox/w1:p1");
        assert_eq!(value["agents"][0]["agent_status"], "blocked");
        assert_eq!(value["hosts"][0]["connection"]["state"], "connected");
    }

    #[test]
    fn an_unknown_connection_state_decodes_to_the_fallback() {
        let decoded: ConnectionReport =
            serde_json::from_str(r#"{"state":"quarantined","detail":"from a newer client"}"#)
                .expect("an unknown state decodes to the fallback");
        assert_eq!(decoded, ConnectionReport::Unknown);
        assert_eq!(decoded.state_name(), "unknown");
        assert!(matches!(
            decoded.into_connection(),
            HostConnection::Unavailable { .. }
        ));
    }

    #[test]
    fn connection_reports_mirror_every_runtime_state() {
        let cases = [
            (
                HostConnection::Connecting { attempt: 2 },
                ConnectionReport::Connecting { attempt: 2 },
            ),
            (
                HostConnection::Connected {
                    server_version: "0.8.2-fork".to_string(),
                    methods: vec!["pane.write".to_string()],
                },
                ConnectionReport::Connected {
                    server_version: "0.8.2-fork".to_string(),
                },
            ),
            (
                HostConnection::Unavailable {
                    reason: "connection refused".to_string(),
                    retry_in: Some(Duration::from_secs(4)),
                },
                ConnectionReport::Unavailable {
                    reason: "connection refused".to_string(),
                    retry_in_ms: Some(4000),
                },
            ),
            (
                HostConnection::Incompatible {
                    generation: Some(2),
                    reason: "endpoint generation 2".to_string(),
                },
                ConnectionReport::Incompatible {
                    generation: Some(2),
                    reason: "endpoint generation 2".to_string(),
                },
            ),
        ];
        for (connection, expected) in cases {
            let report = ConnectionReport::from(&connection);
            assert_eq!(report, expected);
            assert_eq!(report.state_name(), connection.state_name());
            assert_eq!(report.reason(), connection.reason());
        }
    }

    #[test]
    fn an_agent_report_carries_both_halves_of_every_reference() {
        let mut state = two_hosts_on_the_frozen_snapshot();
        let merged = state
            .merged_agents()
            .first()
            .cloned()
            .expect("the frozen snapshot has one agent per host");
        let report = AgentReport::from(&merged);
        assert_eq!(report.r#ref.to_string(), "workbox/w1:p1");
        assert_eq!(report.host, merged.pane.host);
        assert_eq!(report.pane_id, "w1:p1");
        assert_eq!(report.workspace_id, "w1");
        assert_eq!(report.tab_id, "w1:t1");
        assert_eq!(report.name.as_deref(), Some("reviewer"));
        assert_eq!(report.display_agent.as_deref(), Some("Claude"));
        assert_eq!(report.state_change_seq, 9);
        assert_eq!(report.fleet_change_seq, merged.fleet_change_seq);
    }

    #[test]
    fn render_text_shows_the_host_table_the_reasons_and_the_agents() {
        let mut state = FleetState::test_new();
        let local = HostId::local();
        let workbox = HostId::new("workbox").expect("valid host name");
        connected(&mut state, &local);
        state.apply(&local, HostEvent::Snapshot(frozen_snapshot()));
        state.apply(
            &workbox,
            HostEvent::Unavailable {
                reason: "connection refused".to_string(),
                retry_in: Some(Duration::from_secs(1)),
            },
        );
        let report = FleetStatusReport::from_state(&mut state, "0.8.2-test");
        assert_eq!(report.render_text(), EXPECTED_TEXT);
    }

    const EXPECTED_TEXT: &str = "\
client 0.8.2-test  active host: local

HOST     KIND   STATE        VERSION     BLOCKED  WORKING  DONE  IDLE  UNKNOWN
local    local  connected    0.8.2-fork  1        0        0     0     0
workbox  local  unavailable  -           0        0        0     0     0
  ! workbox: connection refused

AGENT        STATUS   WORKSPACE  NAME
local/w1:p1  blocked  repo       reviewer
";

    #[test]
    fn a_future_agent_status_in_a_report_decodes_to_unknown() {
        let mut state = two_hosts_on_the_frozen_snapshot();
        let report = FleetStatusReport::from_state(&mut state, "0.8.2-test");
        let mut value = serde_json::to_value(&report).expect("report is json");
        value["agents"][0]["agent_status"] = serde_json::Value::String("quarantined".to_string());
        value["hosts"][0]["workspaces"][0]["agent_status"] =
            serde_json::Value::String("quarantined".to_string());

        let decoded: FleetStatusReport =
            serde_json::from_value(value).expect("a future status must not break the report");
        assert_eq!(decoded.agents[0].agent_status, AgentStatus::Unknown);
        assert_eq!(
            decoded.hosts[0].workspaces[0].agent_status,
            AgentStatus::Unknown
        );
    }

    #[test]
    fn the_table_pads_by_terminal_columns_not_chars() {
        let rows = vec![
            vec!["A".to_string(), "x".to_string()],
            vec!["ワイド".to_string(), "y".to_string()],
        ];
        let rendered = render_table(&rows);
        let starts = rendered
            .lines()
            .filter_map(|line| {
                line.find(['x', 'y'])
                    .map(|byte| display_width(&line[..byte]))
            })
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 2, "{rendered}");
        assert_eq!(
            starts[0], starts[1],
            "a wide label must not push its neighbour out of column: {rendered}"
        );
    }

    #[test]
    fn render_text_says_so_when_no_agent_is_merged() {
        let mut state = FleetState::test_new();
        let report = FleetStatusReport::from_state(&mut state, "0.8.2-test");
        let text = report.render_text();
        assert!(text.ends_with("no agents\n"), "{text}");
        assert!(text.contains("local    local  connecting"), "{text}");
    }
}
