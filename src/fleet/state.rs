//! Pure merged state for a fleet of herdr servers.
//!
//! [`FleetState`] is data: it holds one [`HostState`] per configured host, the
//! last snapshot each host sent, a merged cross-host agent list and the deltas
//! that got it there. It never opens a socket, spawns a task or renders
//! anything — the connector (a later PR) turns transport events into
//! [`HostEvent`]s and hands them to [`FleetState::apply`], and the TUI and the
//! gateway read the result. That split is upstream's "state is separated from
//! runtime" principle, and it is what makes every rule below testable without
//! a PTY.
//!
//! Two facts about herdr servers shape this module:
//!
//! - Ids are per server (`w1:p1` on two hosts are different panes), so
//!   everything user-visible is a [`crate::fleet::refs`] reference.
//! - `state_change_seq` is a per-server-boot counter, so it cannot order
//!   agents across hosts. [`FleetState`] therefore assigns its own monotonic
//!   `fleet_change_seq` whenever an agent appears or advances, and the merged
//!   list is ordered by status rank first, then that sequence.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::api::schema::AgentStatus;
use crate::fleet::hosts::{HostId, HostSpec};
use crate::fleet::refs::{is_valid_resource_id, FleetPaneRef, FleetTabRef, FleetWorkspaceRef};
use crate::fleet::report::ConnectionReport;
use crate::protocol::{ClientShellAgent, ClientShellSnapshot};

/// First reconnect delay.
const BACKOFF_START: Duration = Duration::from_secs(1);
/// Reconnect delay ceiling.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Where one host stands with the fleet client.
///
/// A host is only ever in one of these states, and a failure of any kind is
/// host-local: it becomes `Unavailable` (or `Incompatible`) for that host and
/// never propagates to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostConnection {
    /// No usable connection yet; `attempt` counts finished attempts.
    Connecting { attempt: u32 },
    /// Endpoint generation 1 handshake accepted.
    Connected {
        server_version: String,
        /// Shell-lane methods the server advertised, for feature gating.
        methods: Vec<String>,
    },
    /// Reachable state unknown or lost; the connector retries after `retry_in`.
    Unavailable {
        reason: String,
        retry_in: Option<Duration>,
    },
    /// The server answered but cannot speak endpoint generation 1.
    Incompatible {
        generation: Option<u32>,
        reason: String,
    },
}

impl HostConnection {
    /// Whether this host's agents count towards the merged view.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// Short lowercase state name used by reports and the CLI table.
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Connecting { .. } => "connecting",
            Self::Connected { .. } => "connected",
            Self::Unavailable { .. } => "unavailable",
            Self::Incompatible { .. } => "incompatible",
        }
    }

    /// Operator-facing explanation, when there is one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Connecting { .. } | Self::Connected { .. } => None,
            Self::Unavailable { reason, .. } | Self::Incompatible { reason, .. } => Some(reason),
        }
    }
}

/// Something one host's transport observed, in [`FleetState::apply`] terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    Connecting {
        attempt: u32,
    },
    Connected {
        server_version: String,
        methods: Vec<String>,
    },
    /// A full projection replacement from the endpoint.
    Snapshot(Box<ClientShellSnapshot>),
    Unavailable {
        reason: String,
        retry_in: Option<Duration>,
    },
    Incompatible {
        generation: Option<u32>,
        reason: String,
    },
}

/// What [`FleetState`] remembers about one agent between snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenAgent {
    state_change_seq: u64,
    status: AgentStatus,
    fleet_change_seq: u64,
}

/// Per-status agent counts for one host, or for the whole fleet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentRollup {
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
    pub unknown: usize,
}

impl AgentRollup {
    pub fn total(&self) -> usize {
        self.blocked
            .saturating_add(self.working)
            .saturating_add(self.done)
            .saturating_add(self.idle)
            .saturating_add(self.unknown)
    }

    fn add(&mut self, status: AgentStatus) {
        let slot = match status {
            AgentStatus::Blocked => &mut self.blocked,
            AgentStatus::Working => &mut self.working,
            AgentStatus::Done => &mut self.done,
            AgentStatus::Idle => &mut self.idle,
            AgentStatus::Unknown => &mut self.unknown,
        };
        *slot = slot.saturating_add(1);
    }

    fn merge(&mut self, other: &Self) {
        self.blocked = self.blocked.saturating_add(other.blocked);
        self.working = self.working.saturating_add(other.working);
        self.done = self.done.saturating_add(other.done);
        self.idle = self.idle.saturating_add(other.idle);
        self.unknown = self.unknown.saturating_add(other.unknown);
    }
}

/// One host: what it is, how it is doing, and what it last told us.
///
/// The last snapshot is kept when a host drops so a client can dim it instead
/// of blanking it, but a host that is not `Connected` contributes no agents to
/// the merged list and its roll-up is zero.
#[derive(Debug, Clone)]
pub struct HostState {
    pub spec: HostSpec,
    pub connection: HostConnection,
    pub snapshot: Option<Box<ClientShellSnapshot>>,
    pub rollup: AgentRollup,
    seen: HashMap<String, SeenAgent>,
}

impl HostState {
    fn new(spec: HostSpec) -> Self {
        // A disabled host is never opened by the connector, so reporting it as
        // "connecting" forever would be a lie; it starts unavailable with the
        // reason spelled out.
        let connection = if spec.enabled {
            HostConnection::Connecting { attempt: 0 }
        } else {
            HostConnection::Unavailable {
                reason: "host disabled in [fleet]".to_string(),
                retry_in: None,
            }
        };
        Self {
            spec,
            connection,
            snapshot: None,
            rollup: AgentRollup::default(),
            seen: HashMap::new(),
        }
    }

    pub fn id(&self) -> &HostId {
        &self.spec.id
    }

    /// Whether this host's agents are part of the merged view right now.
    pub fn contributes_agents(&self) -> bool {
        self.connection.is_connected() && self.snapshot.is_some()
    }

    /// Agents of the last snapshot whose ids fit the fleet id form.
    ///
    /// A server that sent an id containing `/` would produce a reference whose
    /// string form parses back into a different host, so its agents are
    /// dropped from the merged view rather than mis-attributed.
    fn snapshot_agents(&self) -> impl Iterator<Item = &ClientShellAgent> {
        self.snapshot
            .iter()
            .flat_map(|snapshot| snapshot.agents.iter())
            .filter(|agent| {
                is_valid_resource_id(&agent.pane_id)
                    && is_valid_resource_id(&agent.workspace_id)
                    && is_valid_resource_id(&agent.tab_id)
            })
    }

    fn workspace_label(&self, workspace_id: &str) -> String {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
            })
            .map(|workspace| workspace.label.clone())
            .unwrap_or_default()
    }

    fn merged_agent(&self, agent: &ClientShellAgent, fleet_change_seq: u64) -> MergedAgent {
        let host = self.spec.id.clone();
        MergedAgent {
            pane: FleetPaneRef::new(host.clone(), agent.pane_id.clone()),
            workspace: FleetWorkspaceRef::new(host.clone(), agent.workspace_id.clone()),
            tab: FleetTabRef::new(host, agent.tab_id.clone()),
            workspace_label: self.workspace_label(&agent.workspace_id),
            name: agent.name.clone(),
            title: agent.title.clone(),
            agent: agent.agent.clone(),
            display_agent: agent.display_agent.clone(),
            agent_status: agent.agent_status,
            state_change_seq: agent.state_change_seq,
            fleet_change_seq,
            focused: agent.focused,
        }
    }
}

/// One agent in the merged, cross-host list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedAgent {
    pub pane: FleetPaneRef,
    pub workspace: FleetWorkspaceRef,
    pub tab: FleetTabRef,
    pub workspace_label: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub agent_status: AgentStatus,
    /// The host's own change counter; only comparable within one host boot.
    pub state_change_seq: u64,
    /// Fleet-wide recency, comparable across hosts.
    pub fleet_change_seq: u64,
    pub focused: bool,
}

/// One observable difference between two [`FleetState`] revisions.
///
/// This is the fork's delta stream: `herdr fleet status --watch` prints one
/// per line and the gateway forwards them. It is an additive JSON contract —
/// tagged with `kind`, fields are added and never renamed, and a reader must
/// tolerate a `kind` it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetChange {
    HostConnection {
        host: HostId,
        /// Serialized as [`ConnectionReport`]; `methods` is not carried in the
        /// delta (read it from the report or from [`FleetState::host`]).
        #[serde(with = "connection_serde")]
        connection: HostConnection,
    },
    Snapshot {
        host: HostId,
        boot_id: String,
        revision: u64,
    },
    /// Boxed to keep `FleetChange` small: a merged agent is several times the
    /// size of every other variant.
    AgentAdded {
        agent: Box<MergedAgent>,
    },
    AgentRemoved {
        pane: FleetPaneRef,
    },
    AgentStatus {
        pane: FleetPaneRef,
        from: AgentStatus,
        to: AgentStatus,
    },
    ActiveHost {
        host: Option<HostId>,
    },
}

/// `FleetChange::HostConnection` travels as the report shape, so one JSON
/// vocabulary describes a connection everywhere.
mod connection_serde {
    use super::{ConnectionReport, HostConnection};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(
        connection: &HostConnection,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        ConnectionReport::from(connection).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HostConnection, D::Error> {
        Ok(ConnectionReport::deserialize(deserializer)?.into_connection())
    }
}

/// The merged view of every configured host.
#[derive(Debug, Clone)]
pub struct FleetState {
    hosts: Vec<HostState>,
    active_host: Option<HostId>,
    /// Monotonic fleet-wide recency counter; see the module docs.
    change_seq: u64,
    /// Cached merged list, dropped whenever a host's contribution changes.
    merged: Option<Vec<MergedAgent>>,
}

impl FleetState {
    /// Build the state for a resolved host list.
    ///
    /// Every enabled host starts `Connecting { attempt: 0 }`; the first enabled
    /// host is active, because the connector only streams pane surfaces for the
    /// active host.
    pub fn new(specs: Vec<HostSpec>) -> Self {
        let active_host = specs
            .iter()
            .find(|spec| spec.enabled)
            .map(|spec| spec.id.clone());
        Self {
            hosts: specs.into_iter().map(HostState::new).collect(),
            active_host,
            change_seq: 0,
            merged: None,
        }
    }

    pub fn hosts(&self) -> &[HostState] {
        &self.hosts
    }

    pub fn host(&self, id: &HostId) -> Option<&HostState> {
        self.hosts.iter().find(|host| host.id() == id)
    }

    pub fn active_host(&self) -> Option<&HostId> {
        self.active_host.as_ref()
    }

    /// Point the fleet at another host.
    ///
    /// An unknown or disabled host is refused (no change, empty delta) rather
    /// than accepted, so a caller can never make the connector stream frames
    /// for a host it does not open.
    pub fn set_active_host(&mut self, id: Option<HostId>) -> Vec<FleetChange> {
        if let Some(host_id) = id.as_ref() {
            match self.host(host_id) {
                Some(host) if host.spec.enabled => {}
                Some(_) => {
                    tracing::warn!(host = %host_id, "refusing to activate a disabled fleet host");
                    return Vec::new();
                }
                None => {
                    tracing::warn!(host = %host_id, "refusing to activate an unknown fleet host");
                    return Vec::new();
                }
            }
        }
        if self.active_host == id {
            return Vec::new();
        }
        self.active_host = id.clone();
        vec![FleetChange::ActiveHost { host: id }]
    }

    /// Fold one host event into the state and return what changed.
    ///
    /// An event for an unknown host is dropped with a warning: a connector bug
    /// must not invent a host or, worse, apply one host's data to another.
    pub fn apply(&mut self, host: &HostId, event: HostEvent) -> Vec<FleetChange> {
        let Some(index) = self.hosts.iter().position(|state| state.id() == host) else {
            tracing::warn!(host = %host, "dropping fleet event for an unknown host");
            return Vec::new();
        };
        match event {
            HostEvent::Connecting { attempt } => {
                self.set_connection(index, HostConnection::Connecting { attempt })
            }
            HostEvent::Connected {
                server_version,
                methods,
            } => self.set_connection(
                index,
                HostConnection::Connected {
                    server_version,
                    methods,
                },
            ),
            HostEvent::Unavailable { reason, retry_in } => {
                self.set_connection(index, HostConnection::Unavailable { reason, retry_in })
            }
            HostEvent::Incompatible { generation, reason } => {
                self.set_connection(index, HostConnection::Incompatible { generation, reason })
            }
            HostEvent::Snapshot(snapshot) => self.set_snapshot(index, snapshot),
        }
    }

    /// The merged agent list, blocked first then most recently changed.
    ///
    /// Recomputed only when a snapshot or a connection changed it — never per
    /// render.
    pub fn merged_agents(&mut self) -> &[MergedAgent] {
        if self.merged.is_none() {
            let merged = self.compute_merged();
            self.merged = Some(merged);
        }
        self.merged.as_deref().unwrap_or(&[])
    }

    /// Fleet-wide agent counts.
    pub fn totals(&self) -> AgentRollup {
        let mut totals = AgentRollup::default();
        for host in &self.hosts {
            totals.merge(&host.rollup);
        }
        totals
    }

    fn set_connection(&mut self, index: usize, next: HostConnection) -> Vec<FleetChange> {
        let Some(host) = self.hosts.get(index) else {
            return Vec::new();
        };
        if host.connection == next {
            return Vec::new();
        }
        let was_contributing = host.contributes_agents();
        let id = host.spec.id.clone();

        let Some(host) = self.hosts.get_mut(index) else {
            return Vec::new();
        };
        host.connection = next.clone();
        let now_contributing = host.contributes_agents();

        let mut changes = vec![FleetChange::HostConnection {
            host: id,
            connection: next,
        }];
        if was_contributing && !now_contributing {
            changes.extend(self.retire_agents(index));
        } else if !was_contributing && now_contributing {
            changes.extend(self.publish_agents(index));
        }
        changes
    }

    /// A host left `Connected`: its agents leave the merged list, its roll-up
    /// goes to zero, and the snapshot stays for a dimmed rendering.
    fn retire_agents(&mut self, index: usize) -> Vec<FleetChange> {
        let Some(host) = self.hosts.get(index) else {
            return Vec::new();
        };
        let host_id = host.spec.id.clone();
        let mut removed = host
            .snapshot_agents()
            .map(|agent| FleetPaneRef::new(host_id.clone(), agent.pane_id.clone()))
            .collect::<Vec<_>>();
        removed.sort();
        removed.dedup();
        if let Some(host) = self.hosts.get_mut(index) {
            host.rollup = AgentRollup::default();
        }
        self.merged = None;
        removed
            .into_iter()
            .map(|pane| FleetChange::AgentRemoved { pane })
            .collect()
    }

    /// A host became `Connected` with a snapshot already in hand: republish its
    /// agents, keeping the recency they had before the blip.
    fn publish_agents(&mut self, index: usize) -> Vec<FleetChange> {
        let mut change_seq = self.change_seq;
        let mut changes = Vec::new();
        if let Some(host) = self.hosts.get_mut(index) {
            let mut rollup = AgentRollup::default();
            let mut seen = std::mem::take(&mut host.seen);
            for agent in host.snapshot_agents() {
                rollup.add(agent.agent_status);
                let fleet_change_seq = match seen.get(&agent.pane_id) {
                    Some(previous) => previous.fleet_change_seq,
                    None => {
                        change_seq = change_seq.saturating_add(1);
                        change_seq
                    }
                };
                seen.insert(
                    agent.pane_id.clone(),
                    SeenAgent {
                        state_change_seq: agent.state_change_seq,
                        status: agent.agent_status,
                        fleet_change_seq,
                    },
                );
                changes.push(FleetChange::AgentAdded {
                    agent: Box::new(host.merged_agent(agent, fleet_change_seq)),
                });
            }
            host.seen = seen;
            host.rollup = rollup;
        }
        self.change_seq = change_seq;
        self.merged = None;
        changes
    }

    fn set_snapshot(
        &mut self,
        index: usize,
        snapshot: Box<ClientShellSnapshot>,
    ) -> Vec<FleetChange> {
        let Some(host) = self.hosts.get(index) else {
            return Vec::new();
        };
        // Mirror `ClientShellState::set_snapshot`: within one boot a lower
        // revision is stale and dropped; any new boot_id replaces outright.
        if host.snapshot.as_ref().is_some_and(|current| {
            current.boot_id == snapshot.boot_id && snapshot.revision < current.revision
        }) {
            return Vec::new();
        }
        let host_id = host.spec.id.clone();
        let contributing = host.connection.is_connected();
        let boot_changed = host
            .snapshot
            .as_ref()
            .is_none_or(|current| current.boot_id != snapshot.boot_id);

        let mut change_seq = self.change_seq;
        let mut changes = vec![FleetChange::Snapshot {
            host: host_id.clone(),
            boot_id: snapshot.boot_id.clone(),
            revision: snapshot.revision,
        }];

        let Some(host) = self.hosts.get_mut(index) else {
            return Vec::new();
        };
        if boot_changed {
            // A restarted server restarts `state_change_seq`, so nothing about
            // the previous boot may influence recency.
            host.seen.clear();
        }
        host.snapshot = Some(snapshot);

        let mut rollup = AgentRollup::default();
        let mut next_seen = HashMap::new();
        let mut added = Vec::new();
        let mut status_changes = Vec::new();
        for agent in host.snapshot_agents() {
            rollup.add(agent.agent_status);
            let fleet_change_seq = match host.seen.get(&agent.pane_id) {
                None => {
                    change_seq = change_seq.saturating_add(1);
                    added.push(FleetChange::AgentAdded {
                        agent: Box::new(host.merged_agent(agent, change_seq)),
                    });
                    change_seq
                }
                Some(previous) => {
                    let advanced = agent.state_change_seq > previous.state_change_seq
                        || agent.agent_status != previous.status;
                    let fleet_change_seq = if advanced {
                        change_seq = change_seq.saturating_add(1);
                        change_seq
                    } else {
                        previous.fleet_change_seq
                    };
                    if agent.agent_status != previous.status {
                        status_changes.push(FleetChange::AgentStatus {
                            pane: FleetPaneRef::new(host_id.clone(), agent.pane_id.clone()),
                            from: previous.status,
                            to: agent.agent_status,
                        });
                    }
                    fleet_change_seq
                }
            };
            next_seen.insert(
                agent.pane_id.clone(),
                SeenAgent {
                    state_change_seq: agent.state_change_seq,
                    status: agent.agent_status,
                    fleet_change_seq,
                },
            );
        }

        let mut removed = host
            .seen
            .keys()
            .filter(|pane_id| !next_seen.contains_key(pane_id.as_str()))
            .map(|pane_id| FleetPaneRef::new(host_id.clone(), pane_id.clone()))
            .collect::<Vec<_>>();
        removed.sort();

        host.seen = next_seen;
        host.rollup = if contributing {
            rollup
        } else {
            AgentRollup::default()
        };

        if contributing {
            changes.extend(
                removed
                    .into_iter()
                    .map(|pane| FleetChange::AgentRemoved { pane }),
            );
            changes.extend(added);
            changes.extend(status_changes);
        }

        self.change_seq = change_seq;
        self.merged = None;
        changes
    }

    fn compute_merged(&self) -> Vec<MergedAgent> {
        let mut merged = Vec::new();
        for (host_index, host) in self.hosts.iter().enumerate() {
            if !host.contributes_agents() {
                continue;
            }
            for agent in host.snapshot_agents() {
                let fleet_change_seq = host
                    .seen
                    .get(&agent.pane_id)
                    .map(|seen| seen.fleet_change_seq)
                    .unwrap_or_default();
                merged.push((host_index, host.merged_agent(agent, fleet_change_seq)));
            }
        }
        merged.sort_by(|(left_host, left), (right_host, right)| {
            merged_sort_key(*left_host, left).cmp(&merged_sort_key(*right_host, right))
        });
        merged.into_iter().map(|(_, agent)| agent).collect()
    }
}

/// Merged order: blocked first, then fleet-wide recency, then a deterministic
/// tie-break that never depends on `HashMap` iteration order.
fn merged_sort_key(host_index: usize, agent: &MergedAgent) -> (u8, Reverse<u64>, usize, &str) {
    (
        status_rank(agent.agent_status),
        Reverse(agent.fleet_change_seq),
        host_index,
        agent.pane.pane_id.as_str(),
    )
}

/// Fleet ordering rank: blocked → working → done → idle → unknown.
///
/// Deliberately different from upstream's per-host sidebar priority (which puts
/// done above working); the fleet list answers "where am I needed next".
fn status_rank(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Blocked => 0,
        AgentStatus::Working => 1,
        AgentStatus::Done => 2,
        AgentStatus::Idle => 3,
        AgentStatus::Unknown => 4,
    }
}

/// Reconnect delay: 1 s, doubling to a 30 s ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    current: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            current: BACKOFF_START,
        }
    }

    /// The delay to wait before the next attempt, then double it.
    pub fn next(&mut self) -> Duration {
        let current = self.current;
        self.current = self.current.saturating_mul(2).min(BACKOFF_MAX);
        current
    }

    /// The delay the next [`Backoff::next`] will return.
    pub fn peek(&self) -> Duration {
        self.current
    }

    pub fn reset(&mut self) {
        self.current = BACKOFF_START;
    }
}

#[cfg(test)]
impl FleetState {
    /// Two enabled local hosts, both still connecting, no snapshots.
    pub fn test_new() -> Self {
        Self::new(vec![
            HostSpec::local_default(),
            HostSpec {
                id: HostId::new("workbox").expect("valid host name"),
                kind: crate::fleet::hosts::HostKind::Local {
                    session: Some("agents".to_string()),
                },
                enabled: true,
            },
        ])
    }

    /// A state built to break naive identity assumptions.
    ///
    /// Two hosts report the *same* `boot_id`, the same `w1:p1` pane id and the
    /// same `state_change_seq`; one of them is `Incompatible` while holding a
    /// snapshot; and `active_host` names a disabled host (reachable only by
    /// construction, never through [`FleetState::set_active_host`]).
    pub fn test_with_adversarial_identity_state() -> Self {
        let mut state = Self::new(vec![
            HostSpec::local_default(),
            HostSpec {
                id: HostId::new("twin").expect("valid host name"),
                kind: crate::fleet::hosts::HostKind::Ssh {
                    target: "twin".to_string(),
                    session: None,
                },
                enabled: true,
            },
            HostSpec {
                id: HostId::new("off").expect("valid host name"),
                kind: crate::fleet::hosts::HostKind::Local {
                    session: Some("off".to_string()),
                },
                enabled: false,
            },
        ]);

        let local = HostId::local();
        let twin = HostId::new("twin").expect("valid host name");
        state.apply(
            &local,
            HostEvent::Connected {
                server_version: "0.8.2-fork".to_string(),
                methods: vec!["pane.write".to_string()],
            },
        );
        state.apply(
            &local,
            HostEvent::Snapshot(tests::snapshot(
                "shared-boot",
                4,
                vec![tests::agent("w1:p1", AgentStatus::Blocked, 9)],
            )),
        );
        state.apply(
            &twin,
            HostEvent::Connected {
                server_version: "0.8.2".to_string(),
                methods: Vec::new(),
            },
        );
        state.apply(
            &twin,
            HostEvent::Snapshot(tests::snapshot(
                "shared-boot",
                4,
                vec![tests::agent("w1:p1", AgentStatus::Blocked, 9)],
            )),
        );
        state.apply(
            &twin,
            HostEvent::Incompatible {
                generation: Some(2),
                reason: "endpoint generation 2".to_string(),
            },
        );
        state.active_host = Some(HostId::new("off").expect("valid host name"));
        state
    }

    /// Assert every invariant [`FleetState`] promises its readers.
    pub fn assert_invariants_for_test(&self) {
        let mut ids = self.hosts.iter().map(HostState::id).collect::<Vec<_>>();
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count, "host ids must be unique");

        if let Some(active) = &self.active_host {
            assert!(
                self.host(active).is_some(),
                "active host {active} is not a known host"
            );
        }

        for host in &self.hosts {
            let mut expected = AgentRollup::default();
            if host.contributes_agents() {
                for agent in host.snapshot_agents() {
                    expected.add(agent.agent_status);
                }
            }
            assert_eq!(
                host.rollup,
                expected,
                "roll-up of host {} does not match its snapshot",
                host.id()
            );
            for (pane_id, seen) in &host.seen {
                assert!(
                    seen.fleet_change_seq <= self.change_seq,
                    "{}/{pane_id} has a fleet_change_seq above the counter",
                    host.id()
                );
            }
        }

        let merged = self.compute_merged();
        let mut refs = merged.iter().map(|agent| &agent.pane).collect::<Vec<_>>();
        let merged_count = refs.len();
        refs.sort();
        refs.dedup();
        assert_eq!(refs.len(), merged_count, "merged pane refs must be unique");

        let host_index = |agent: &MergedAgent| {
            self.hosts
                .iter()
                .position(|host| host.id() == &agent.pane.host)
                .unwrap_or(usize::MAX)
        };
        for pair in merged.windows(2) {
            let (left, right) = (&pair[0], &pair[1]);
            assert!(
                merged_sort_key(host_index(left), left)
                    <= merged_sort_key(host_index(right), right),
                "merged agents are not in fleet order: {left:?} before {right:?}"
            );
        }

        if let Some(cache) = &self.merged {
            assert_eq!(cache, &merged, "merged cache is stale");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::hosts::HostKind;
    use crate::protocol::{ClientShellAgent, ClientShellWorkspace};

    pub(super) fn agent(
        pane_id: &str,
        status: AgentStatus,
        state_change_seq: u64,
    ) -> ClientShellAgent {
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

    pub(super) fn snapshot(
        boot_id: &str,
        revision: u64,
        agents: Vec<ClientShellAgent>,
    ) -> Box<ClientShellSnapshot> {
        let mut workspace_ids = agents
            .iter()
            .map(|agent| agent.workspace_id.clone())
            .collect::<Vec<_>>();
        workspace_ids.sort();
        workspace_ids.dedup();
        let workspaces = workspace_ids
            .into_iter()
            .map(|workspace_id| ClientShellWorkspace {
                active_tab_id: format!("{workspace_id}:t1"),
                new_workspace_cwd: "/repo".to_string(),
                number: 1,
                label: format!("repo-{workspace_id}"),
                custom_label: false,
                branch: None,
                git_ahead_behind: None,
                tokens: Vec::new(),
                worktree: None,
                focused: false,
                agent_status: AgentStatus::Idle,
                workspace_id,
            })
            .collect();
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

    fn host(name: &str) -> HostId {
        HostId::new(name).expect("valid host name")
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

    fn two_connected_hosts() -> (FleetState, HostId, HostId) {
        let mut state = FleetState::test_new();
        let local = HostId::local();
        let workbox = host("workbox");
        connected(&mut state, &local);
        connected(&mut state, &workbox);
        (state, local, workbox)
    }

    #[test]
    fn new_state_starts_connecting_and_activates_the_first_enabled_host() {
        let state = FleetState::new(vec![
            HostSpec {
                id: host("off"),
                kind: HostKind::Local { session: None },
                enabled: false,
            },
            HostSpec {
                id: host("workbox"),
                kind: HostKind::Ssh {
                    target: "workbox".to_string(),
                    session: None,
                },
                enabled: true,
            },
        ]);
        assert_eq!(state.active_host(), Some(&host("workbox")));
        assert_eq!(
            state.host(&host("workbox")).map(|h| &h.connection),
            Some(&HostConnection::Connecting { attempt: 0 })
        );
        assert_eq!(
            state.host(&host("off")).map(|h| h.connection.state_name()),
            Some("unavailable"),
            "a disabled host must not be reported as connecting"
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn a_stale_revision_within_one_boot_is_ignored() {
        let (mut state, local, _) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                5,
                vec![agent("w1:p1", AgentStatus::Idle, 1)],
            )),
        );
        let changes = state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                4,
                vec![agent("w1:p2", AgentStatus::Idle, 1)],
            )),
        );
        assert!(changes.is_empty(), "stale snapshot must produce no changes");
        assert_eq!(
            state
                .host(&local)
                .and_then(|h| h.snapshot.as_ref())
                .map(|s| s.revision),
            Some(5)
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn a_new_boot_id_replaces_the_snapshot_and_resets_recency() {
        let (mut state, local, _) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                9,
                vec![agent("w1:p1", AgentStatus::Idle, 40)],
            )),
        );
        // A restarted server sends a lower revision and a lower
        // state_change_seq under a new boot id; both must be accepted.
        let changes = state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-b",
                1,
                vec![agent("w1:p1", AgentStatus::Idle, 1)],
            )),
        );
        assert!(matches!(
            changes.first(),
            Some(FleetChange::Snapshot { boot_id, revision: 1, .. }) if boot_id == "boot-b"
        ));
        assert!(
            changes
                .iter()
                .any(|change| matches!(change, FleetChange::AgentAdded { .. })),
            "a cross-boot agent is re-added: {changes:?}"
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn rollups_count_every_status_including_unknown() {
        let (mut state, local, _) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![
                    agent("w1:p1", AgentStatus::Blocked, 1),
                    agent("w1:p2", AgentStatus::Working, 2),
                    agent("w1:p3", AgentStatus::Done, 3),
                    agent("w1:p4", AgentStatus::Idle, 4),
                    agent("w1:p5", AgentStatus::Unknown, 5),
                ],
            )),
        );
        let rollup = state.host(&local).map(|h| h.rollup).unwrap_or_default();
        assert_eq!(
            rollup,
            AgentRollup {
                blocked: 1,
                working: 1,
                done: 1,
                idle: 1,
                unknown: 1
            }
        );
        assert_eq!(rollup.total(), 5);
        assert_eq!(state.totals(), rollup);
        state.assert_invariants_for_test();
    }

    #[test]
    fn merged_order_is_blocked_first_then_recency_then_host_then_pane() {
        let (mut state, local, workbox) = two_connected_hosts();
        // Identical state_change_seq on both hosts: the tie-break must be
        // deterministic and must not depend on the per-server counter.
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![
                    agent("w1:p2", AgentStatus::Idle, 7),
                    agent("w1:p1", AgentStatus::Working, 7),
                ],
            )),
        );
        state.apply(
            &workbox,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![
                    agent("w1:p1", AgentStatus::Blocked, 7),
                    agent("w1:p2", AgentStatus::Working, 7),
                ],
            )),
        );
        let merged = state
            .merged_agents()
            .iter()
            .map(|agent| agent.pane.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            merged,
            vec![
                "workbox/w1:p1",
                "workbox/w1:p2",
                "local/w1:p1",
                "local/w1:p2",
            ]
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn a_status_change_bumps_recency_and_moves_the_agent_to_the_front_of_its_rank() {
        let (mut state, local, _) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![
                    agent("w1:p1", AgentStatus::Working, 1),
                    agent("w1:p2", AgentStatus::Working, 2),
                ],
            )),
        );
        assert_eq!(
            state
                .merged_agents()
                .iter()
                .map(|agent| agent.pane.pane_id.clone())
                .collect::<Vec<_>>(),
            vec!["w1:p2".to_string(), "w1:p1".to_string()]
        );
        let changes = state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                2,
                vec![
                    agent("w1:p1", AgentStatus::Working, 3),
                    agent("w1:p2", AgentStatus::Working, 2),
                ],
            )),
        );
        assert_eq!(
            changes.len(),
            1,
            "only the snapshot itself changed: {changes:?}"
        );
        assert_eq!(
            state
                .merged_agents()
                .iter()
                .map(|agent| agent.pane.pane_id.clone())
                .collect::<Vec<_>>(),
            vec!["w1:p1".to_string(), "w1:p2".to_string()]
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn snapshot_deltas_report_adds_removals_and_status_changes() {
        let (mut state, local, _) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![
                    agent("w1:p1", AgentStatus::Idle, 1),
                    agent("w1:p2", AgentStatus::Idle, 1),
                ],
            )),
        );
        let changes = state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                2,
                vec![
                    agent("w1:p1", AgentStatus::Blocked, 2),
                    agent("w1:p3", AgentStatus::Idle, 1),
                ],
            )),
        );
        let removed = changes
            .iter()
            .filter_map(|change| match change {
                FleetChange::AgentRemoved { pane } => Some(pane.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(removed, vec!["local/w1:p2"]);
        let added = changes
            .iter()
            .filter_map(|change| match change {
                FleetChange::AgentAdded { agent } => Some(agent.pane.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(added, vec!["local/w1:p3"]);
        let statuses = changes
            .iter()
            .filter_map(|change| match change {
                FleetChange::AgentStatus { pane, from, to } => Some((pane.to_string(), *from, *to)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![(
                "local/w1:p1".to_string(),
                AgentStatus::Idle,
                AgentStatus::Blocked
            )]
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn an_event_for_an_unknown_host_changes_nothing() {
        let (mut state, _, _) = two_connected_hosts();
        let before = state.totals();
        let changes = state.apply(
            &host("ghost"),
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![agent("w1:p1", AgentStatus::Blocked, 1)],
            )),
        );
        assert!(changes.is_empty());
        assert_eq!(state.totals(), before);
        assert_eq!(state.hosts().len(), 2);
        state.assert_invariants_for_test();
    }

    #[test]
    fn losing_a_host_removes_its_agents_and_reconnecting_restores_them() {
        let (mut state, local, workbox) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![agent("w1:p1", AgentStatus::Blocked, 1)],
            )),
        );
        state.apply(
            &workbox,
            HostEvent::Snapshot(snapshot(
                "boot-b",
                1,
                vec![agent("w1:p1", AgentStatus::Idle, 1)],
            )),
        );
        assert_eq!(state.merged_agents().len(), 2);
        let recency = state
            .merged_agents()
            .iter()
            .find(|agent| agent.pane.host == workbox)
            .map(|agent| agent.fleet_change_seq)
            .expect("workbox agent is merged");

        let changes = state.apply(
            &workbox,
            HostEvent::Unavailable {
                reason: "connection refused".to_string(),
                retry_in: Some(Duration::from_secs(1)),
            },
        );
        assert!(matches!(
            changes.first(),
            Some(FleetChange::HostConnection { .. })
        ));
        assert_eq!(
            changes
                .iter()
                .filter(|change| matches!(change, FleetChange::AgentRemoved { .. }))
                .count(),
            1
        );
        assert_eq!(state.merged_agents().len(), 1);
        assert_eq!(state.totals().total(), 1);
        assert!(
            state
                .host(&workbox)
                .is_some_and(|host| host.snapshot.is_some()),
            "an unavailable host keeps its last snapshot for a dimmed rendering"
        );
        state.assert_invariants_for_test();

        connected(&mut state, &workbox);
        assert_eq!(state.merged_agents().len(), 2);
        assert_eq!(
            state
                .merged_agents()
                .iter()
                .find(|agent| agent.pane.host == workbox)
                .map(|agent| agent.fleet_change_seq),
            Some(recency),
            "a reconnect must not reshuffle the merged list"
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn a_snapshot_from_a_host_that_is_not_connected_is_stored_but_not_merged() {
        let mut state = FleetState::test_new();
        let local = HostId::local();
        let changes = state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![agent("w1:p1", AgentStatus::Blocked, 1)],
            )),
        );
        assert_eq!(changes.len(), 1, "only the snapshot delta: {changes:?}");
        assert!(state.merged_agents().is_empty());
        assert_eq!(state.totals().total(), 0);
        connected(&mut state, &local);
        assert_eq!(state.merged_agents().len(), 1);
        state.assert_invariants_for_test();
    }

    #[test]
    fn agents_whose_ids_break_the_reference_form_are_dropped() {
        let (mut state, local, _) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![
                    agent("w1:p1", AgentStatus::Idle, 1),
                    agent("w1/p2", AgentStatus::Blocked, 1),
                ],
            )),
        );
        assert_eq!(
            state
                .merged_agents()
                .iter()
                .map(|agent| agent.pane.to_string())
                .collect::<Vec<_>>(),
            vec!["local/w1:p1"]
        );
        assert_eq!(state.totals().total(), 1);
        state.assert_invariants_for_test();
    }

    #[test]
    fn merged_agents_carry_the_workspace_label_of_their_host() {
        let (mut state, local, _) = two_connected_hosts();
        state.apply(
            &local,
            HostEvent::Snapshot(snapshot(
                "boot-a",
                1,
                vec![agent("w1:p1", AgentStatus::Idle, 1)],
            )),
        );
        let merged = state.merged_agents();
        assert_eq!(merged[0].workspace_label, "repo-w1");
        assert_eq!(merged[0].workspace.to_string(), "local/w1");
        assert_eq!(merged[0].tab.to_string(), "local/w1:t1");
    }

    #[test]
    fn set_active_host_refuses_unknown_and_disabled_hosts() {
        let mut state = FleetState::test_with_adversarial_identity_state();
        let before = state.active_host().cloned();
        assert!(state.set_active_host(Some(host("ghost"))).is_empty());
        assert!(state.set_active_host(Some(host("off"))).is_empty());
        assert_eq!(state.active_host().cloned(), before);

        let changes = state.set_active_host(Some(HostId::local()));
        assert_eq!(
            changes,
            vec![FleetChange::ActiveHost {
                host: Some(HostId::local())
            }]
        );
        assert!(
            state.set_active_host(Some(HostId::local())).is_empty(),
            "re-activating the active host is not a change"
        );
        assert_eq!(
            state.set_active_host(None),
            vec![FleetChange::ActiveHost { host: None }]
        );
        state.assert_invariants_for_test();
    }

    #[test]
    fn the_adversarial_state_holds_its_invariants_through_a_mutation_sequence() {
        let mut state = FleetState::test_with_adversarial_identity_state();
        state.assert_invariants_for_test();
        let twin = host("twin");
        let local = HostId::local();

        // The two hosts share boot_id, pane id and state_change_seq; only the
        // host half of the reference separates them.
        assert_eq!(state.merged_agents().len(), 1);
        state.assert_invariants_for_test();

        connected(&mut state, &twin);
        assert_eq!(state.merged_agents().len(), 2);
        state.assert_invariants_for_test();

        state.apply(
            &twin,
            HostEvent::Snapshot(snapshot(
                "shared-boot",
                5,
                vec![agent("w1:p1", AgentStatus::Done, 10)],
            )),
        );
        state.assert_invariants_for_test();

        state.apply(&local, HostEvent::Connecting { attempt: 1 });
        assert_eq!(state.merged_agents().len(), 1);
        state.assert_invariants_for_test();

        state.apply(
            &twin,
            HostEvent::Unavailable {
                reason: "ssh exited".to_string(),
                retry_in: None,
            },
        );
        assert!(state.merged_agents().is_empty());
        assert_eq!(state.totals(), AgentRollup::default());
        state.assert_invariants_for_test();
    }

    #[test]
    fn test_new_holds_its_invariants() {
        let mut state = FleetState::test_new();
        state.assert_invariants_for_test();
        assert_eq!(state.active_host(), Some(&HostId::local()));
        assert!(state.merged_agents().is_empty());
        state.assert_invariants_for_test();
    }

    #[test]
    fn fleet_changes_serialize_with_a_kind_tag() {
        let change = FleetChange::HostConnection {
            host: HostId::local(),
            connection: HostConnection::Connected {
                server_version: "0.8.2-fork".to_string(),
                methods: vec!["pane.write".to_string()],
            },
        };
        let json = serde_json::to_value(&change).expect("change serializes");
        assert_eq!(json["kind"], "host_connection");
        assert_eq!(json["connection"]["state"], "connected");
        assert_eq!(json["connection"]["server_version"], "0.8.2-fork");

        let decoded: FleetChange = serde_json::from_value(json).expect("change decodes");
        assert_eq!(
            decoded,
            FleetChange::HostConnection {
                host: HostId::local(),
                // The delta does not carry the advertised method list.
                connection: HostConnection::Connected {
                    server_version: "0.8.2-fork".to_string(),
                    methods: Vec::new(),
                },
            }
        );

        let removed = FleetChange::AgentRemoved {
            pane: FleetPaneRef::new(HostId::local(), "w1:p1"),
        };
        let json = serde_json::to_value(&removed).expect("change serializes");
        assert_eq!(json["kind"], "agent_removed");
        assert_eq!(json["pane"], "local/w1:p1");
    }

    #[test]
    fn backoff_doubles_to_a_thirty_second_ceiling() {
        let mut backoff = Backoff::new();
        let delays = (0..7).map(|_| backoff.next().as_secs()).collect::<Vec<_>>();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(backoff.peek(), Duration::from_secs(30));
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(1));
    }
}
