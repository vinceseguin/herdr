//! `just bench-fleet-scale`: what a host group costs per frame.
//!
//! The console's sidebar is on a multiplicative path — rows scale with hosts ×
//! agents and the whole thing is composed on every frame — so the design puts
//! every label, count and glyph choice in `FleetSidebarModel::rebuild`, which
//! runs once per fleet change. This profile is the evidence for that: it
//! composes a real `ClientShellState` at 120x40 with 1 and with 5 hosts of 15
//! agents each, collapsed and expanded, and reports the ratio.
//!
//! Acceptance (E2 decision (l)): compose median at 5 hosts is at most 1.15x
//! one host with the inactive groups collapsed, and at most 1.5x expanded —
//! drawing is O(visible rows), and 40 rows is 40 rows however many hosts
//! there are. `rebuild` is reported separately because it is per *change*,
//! never per frame.
//!
//! Ignored, like `render_scale_profile`: it is a measurement, not a gate.

use super::*;

use std::collections::HashSet;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::fleet::hosts::{HostId, HostKind, HostSpec};
use crate::fleet::sidebar::FleetSidebarModel;
use crate::fleet::state::{FleetState, HostEvent};
use crate::protocol::ClientShellAgent;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const SAMPLE_COUNT: usize = 40;
const WARMUP_COUNT: usize = 5;
const HOST_CARDINALITIES: [usize; 2] = [1, 5];
const AGENTS_PER_HOST: usize = 15;

#[derive(Clone, Copy)]
struct StageStats {
    median_ns: u128,
    p95_ns: u128,
    max_ns: u128,
}

fn summarize(mut samples: Vec<Duration>) -> StageStats {
    samples.sort_unstable();
    StageStats {
        median_ns: samples[samples.len() / 2].as_nanos(),
        p95_ns: samples[(samples.len() - 1) * 95 / 100].as_nanos(),
        max_ns: samples[samples.len() - 1].as_nanos(),
    }
}

fn host(index: usize) -> HostId {
    HostId::new(&format!("lab-{index}")).expect("valid host id")
}

/// The frozen generation-1 snapshot with a realistic agent spread.
fn snapshot_with_agents(index: usize) -> Box<ClientShellSnapshot> {
    let mut snapshot: ClientShellSnapshot = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/endpoint-snapshot-v1.json"
    )))
    .expect("frozen snapshot decodes");
    snapshot.boot_id = format!("boot-{index}");
    snapshot.revision = 1;
    let workspace_id = snapshot
        .workspaces
        .first()
        .map(|workspace| workspace.workspace_id.clone())
        .unwrap_or_else(|| "w1".to_string());
    let tab_id = snapshot
        .tabs
        .first()
        .map(|tab| tab.tab_id.clone())
        .unwrap_or_else(|| "w1:t1".to_string());
    let statuses = [
        AgentStatus::Blocked,
        AgentStatus::Working,
        AgentStatus::Done,
        AgentStatus::Idle,
        AgentStatus::Unknown,
    ];
    snapshot.agents = (0..AGENTS_PER_HOST)
        .map(|agent_index| ClientShellAgent {
            pane_id: format!("{workspace_id}:p{}", agent_index + 1),
            workspace_id: workspace_id.clone(),
            tab_id: tab_id.clone(),
            name: Some(format!("lab-{index}-agent-{agent_index}")),
            display_agent: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: statuses[agent_index % statuses.len()],
            state_change_seq: agent_index as u64,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        })
        .collect();
    Box::new(snapshot)
}

fn fleet_with(hosts: usize) -> FleetState {
    let specs = (1..=hosts)
        .map(|index| HostSpec {
            id: host(index),
            kind: HostKind::Local {
                session: Some(format!("lab-{index}")),
            },
            enabled: true,
        })
        .collect::<Vec<_>>();
    let mut state = FleetState::new(specs);
    for index in 1..=hosts {
        let id = host(index);
        state.apply(
            &id,
            HostEvent::Connected {
                server_version: "0.8.2-fork".into(),
                methods: Vec::new(),
            },
        );
        state.apply(&id, HostEvent::Snapshot(snapshot_with_agents(index)));
    }
    state.set_active_host(Some(host(1)));
    state
}

/// Every host but the active one, when the profile asks for collapsed groups.
fn collapsed_set(hosts: usize, collapsed: bool) -> HashSet<HostId> {
    if !collapsed {
        return HashSet::new();
    }
    (2..=hosts).map(host).collect()
}

fn console(hosts: usize, collapsed: bool) -> (ClientShellState, FleetState, HashSet<HostId>) {
    let fleet = fleet_with(hosts);
    let collapsed = collapsed_set(hosts, collapsed);
    let mut model = FleetSidebarModel::default();
    model.rebuild(
        &fleet,
        &collapsed,
        crate::config::AgentPanelSortConfig::Priority,
    );

    let mut shell = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut snapshot = snapshot_with_agents(1);
    snapshot.boot_id = "boot-1".into();
    shell.set_snapshot(snapshot);
    let mut surface = surface();
    surface.boot_id = "boot-1".into();
    surface.projection_revision = 1;
    shell.set_pane_surface(surface);
    shell.fleet_sidebar_update(model, host(1), None);
    (shell, fleet, collapsed)
}

fn profile_compose(hosts: usize, collapsed: bool) -> StageStats {
    let (mut shell, _fleet, _collapsed) = console(hosts, collapsed);
    for _ in 0..WARMUP_COUNT {
        black_box(shell.compose(COLS, ROWS));
    }
    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        // `compose` caches nothing that would make a second call cheaper: the
        // surface and the snapshot are unchanged, exactly as they are between
        // two frames of an idle console.
        let started = Instant::now();
        black_box(shell.compose(COLS, ROWS));
        samples.push(started.elapsed());
    }
    summarize(samples)
}

fn profile_rebuild(hosts: usize) -> StageStats {
    let fleet = fleet_with(hosts);
    let collapsed = HashSet::new();
    let mut model = FleetSidebarModel::default();
    for _ in 0..WARMUP_COUNT {
        model.rebuild(
            &fleet,
            &collapsed,
            crate::config::AgentPanelSortConfig::Priority,
        );
    }
    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let started = Instant::now();
        model.rebuild(
            &fleet,
            &collapsed,
            crate::config::AgentPanelSortConfig::Priority,
        );
        black_box(&model);
        samples.push(started.elapsed());
    }
    summarize(samples)
}

/// One measurement table, in microseconds with three decimals so a rebuild
/// that costs a fraction of one still shows a ratio.
fn print_stage(label: &str, rows: &[(usize, StageStats)]) {
    let baseline = rows[0].1;
    println!("  {label}");
    println!("       hosts   median_us     p95_us     max_us  median_vs_1x  p95_vs_1x");
    for (hosts, stats) in rows {
        println!(
            "  {hosts:>10}  {:>10.3}  {:>9.3}  {:>9.3}  {:>12.2}  {:>9.2}",
            stats.median_ns as f64 / 1000.0,
            stats.p95_ns as f64 / 1000.0,
            stats.max_ns as f64 / 1000.0,
            stats.median_ns as f64 / baseline.median_ns.max(1) as f64,
            stats.p95_ns as f64 / baseline.p95_ns.max(1) as f64,
        );
    }
}

#[test]
#[ignore = "scaling profile, not a gate: run with just bench-fleet-scale"]
fn fleet_sidebar_scale_profile() {
    println!("fleet console sidebar ({COLS}x{ROWS}, {AGENTS_PER_HOST} agents per host)");
    let collapsed = HOST_CARDINALITIES.map(|hosts| (hosts, profile_compose(hosts, true)));
    print_stage(
        "client shell composition (inactive groups collapsed)",
        &collapsed,
    );
    let expanded = HOST_CARDINALITIES.map(|hosts| (hosts, profile_compose(hosts, false)));
    print_stage("client shell composition (every group expanded)", &expanded);
    let rebuild = HOST_CARDINALITIES.map(|hosts| (hosts, profile_rebuild(hosts)));
    print_stage("FleetSidebarModel::rebuild (per fleet change)", &rebuild);
    println!(
        "  acceptance: compose median 5 vs 1 hosts <= 1.15x collapsed, <= 1.5x expanded (E2 decision (l))"
    );
}

/// The acceptance the profile above measures, as a cheap deterministic check:
/// a frame's *drawn* rows never scale with the number of hosts, because the
/// body is 40 rows whatever the fleet holds.
#[test]
fn drawn_fleet_rows_do_not_scale_with_the_number_of_hosts() {
    let mut counts = Vec::new();
    for hosts in HOST_CARDINALITIES {
        let (mut shell, _fleet, _collapsed) = console(hosts, false);
        shell.compose(COLS, ROWS).expect("the console composes");
        counts.push(shell.hits.fleet_rows.len() + shell.hits.agents.len());
    }
    let (one, five) = (counts[0], counts[1]);
    // 5 hosts x (15 agents + a workspace + two headers) is ~90 rows of model;
    // what a frame draws is bounded by the two sidebar bodies, not by that.
    assert!(
        five <= usize::from(ROWS),
        "drawn rows must stay bounded by the viewport, not the fleet: {one} -> {five}"
    );
    assert!(
        five >= one,
        "more hosts do fill more of the sidebar: {one} -> {five}"
    );
}
