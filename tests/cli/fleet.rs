//! `herdr fleet status` against real named servers.
//!
//! The unit tests drive the connector against a fake endpoint; these prove the
//! same code path against actual herdr servers, including the socket layout,
//! the generation-1 handshake and the JSON contract.

#![cfg(unix)]

use super::harness::*;
use crate::support;

/// The client socket a named session listens on.
fn named_client_socket(config_home: &Path, session: &str) -> PathBuf {
    config_home
        .join(app_dir_name())
        .join("sessions")
        .join(session)
        .join("herdr-client.sock")
}

fn write_config(config_home: &Path, extra: &str) {
    let dir = config_home.join(app_dir_name());
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        format!("onboarding = false\n{extra}"),
    )
    .unwrap();
}

const TWO_LOCAL_HOSTS: &str = r#"
[fleet]
include_local = false

[[fleet.hosts]]
name = "alpha"
kind = "local"
session = "alpha"

[[fleet.hosts]]
name = "beta"
kind = "local"
session = "beta"
"#;

fn total(counts: &serde_json::Value) -> u64 {
    ["blocked", "working", "done", "idle", "unknown"]
        .iter()
        .map(|status| counts[status].as_u64().unwrap_or_default())
        .sum()
}

#[test]
fn fleet_status_json_merges_two_named_sessions() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    let beta = spawn_named_server(&config_home, &runtime_dir, "beta");
    wait_for_socket(
        &named_client_socket(&config_home, "alpha"),
        Duration::from_secs(15),
    );
    wait_for_socket(
        &named_client_socket(&config_home, "beta"),
        Duration::from_secs(15),
    );
    write_config(&config_home, TWO_LOCAL_HOSTS);

    let report = run_named_cli_json(
        &config_home,
        &runtime_dir,
        &["fleet", "status", "--json", "--timeout-ms", "15000"],
    );

    assert_eq!(report["schema"], "herdr.fleet.status.v1");
    assert_eq!(report["client_version"], support::build_version());
    let hosts = report["hosts"].as_array().expect("hosts array");
    assert_eq!(hosts.len(), 2, "unexpected hosts: {report}");
    for (host, session) in hosts.iter().zip(["alpha", "beta"]) {
        assert_eq!(host["id"], session);
        assert_eq!(host["kind"], "local");
        assert_eq!(host["session"], session);
        assert_eq!(
            host["connection"]["state"], "connected",
            "host {session} did not connect: {host}"
        );
        assert_eq!(
            host["connection"]["server_version"],
            support::build_version()
        );
        assert!(
            host["boot_id"]
                .as_str()
                .is_some_and(|boot| !boot.is_empty()),
            "host {session} reported no boot id: {host}"
        );
        let workspaces = host["workspaces"].as_array().expect("workspaces array");
        assert!(
            !workspaces.is_empty(),
            "host {session} reported no workspaces: {host}"
        );
        for workspace in workspaces {
            let reference = workspace["ref"].as_str().expect("workspace ref");
            assert!(
                reference.starts_with(&format!("{session}/")),
                "workspace reference {reference} is not host-qualified"
            );
        }
    }
    // Ids are per server, so two hosts may both hold `w1`; the references must
    // still be distinct.
    let references = hosts
        .iter()
        .flat_map(|host| host["workspaces"].as_array().cloned().unwrap_or_default())
        .map(|workspace| workspace["ref"].as_str().unwrap_or_default().to_string())
        .collect::<std::collections::HashSet<_>>();
    let workspace_count: usize = hosts
        .iter()
        .map(|host| host["workspaces"].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(references.len(), workspace_count, "references collided");

    let agents = report["agents"].as_array().expect("agents array");
    for agent in agents {
        let reference = agent["ref"].as_str().expect("agent ref");
        assert!(
            reference.starts_with("alpha/") || reference.starts_with("beta/"),
            "agent reference {reference} is not host-qualified"
        );
        assert_eq!(
            reference,
            format!(
                "{}/{}",
                agent["host"].as_str().unwrap_or_default(),
                agent["pane_id"].as_str().unwrap_or_default()
            )
        );
    }
    assert_eq!(total(&report["counts"]), agents.len() as u64);

    // Stopping one host must not disturb the other.
    drop(beta);
    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &["fleet", "status", "--json", "--timeout-ms", "15000"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "an unreachable host is data, not a failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let hosts = report["hosts"].as_array().expect("hosts array");
    assert_eq!(hosts[0]["connection"]["state"], "connected");
    assert_eq!(
        hosts[1]["connection"]["state"], "unavailable",
        "the stopped host must be reported unavailable: {report}"
    );
    assert!(
        hosts[1]["connection"]["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "an unavailable host must carry a reason: {report}"
    );

    drop(alpha);
    cleanup_test_base(&base);
}

#[test]
fn fleet_status_text_lists_hosts() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");

    let alpha = spawn_named_server(&config_home, &runtime_dir, "alpha");
    wait_for_socket(
        &named_client_socket(&config_home, "alpha"),
        Duration::from_secs(15),
    );
    write_config(
        &config_home,
        r#"
[fleet]
include_local = false

[[fleet.hosts]]
name = "alpha"
kind = "local"
session = "alpha"

[[fleet.hosts]]
name = "gone"
kind = "local"
session = "herdr-fleet-cli-missing"
"#,
    );

    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &["fleet", "status", "--timeout-ms", "15000"],
    );
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("HOST") && text.contains("KIND") && text.contains("STATE"),
        "missing table header: {text}"
    );
    assert!(text.contains("alpha"), "missing the connected host: {text}");
    assert!(text.contains("gone"), "missing the missing host: {text}");
    assert!(
        text.contains("connected") && text.contains("unavailable"),
        "missing host states: {text}"
    );
    assert!(
        text.contains("! gone:"),
        "an unavailable host must show its reason: {text}"
    );

    drop(alpha);
    cleanup_test_base(&base);
}

#[test]
fn fleet_status_rejects_invalid_config() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    write_config(
        &config_home,
        r#"
[fleet]
include_local = true

[[fleet.hosts]]
name = "local"
kind = "local"
"#,
    );

    let output = run_named_cli(&config_home, &runtime_dir, &["fleet", "status", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "an invalid [fleet] section must exit 1: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fleet"),
        "the diagnostics must reach stderr: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "no report may be printed: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    cleanup_test_base(&base);
}

#[test]
fn fleet_usage_errors_exit_two() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    write_config(&config_home, "");

    for args in [
        vec!["fleet"],
        vec!["fleet", "stats"],
        vec!["fleet", "status", "--jsn"],
        vec!["fleet", "status", "--timeout-ms"],
    ] {
        let output = run_named_cli(&config_home, &runtime_dir, &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "expected a usage error for {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // `--help` is answered by the shared clap spec, `help` by the command.
    let output = run_named_cli(&config_home, &runtime_dir, &["fleet", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("status"));

    let output = run_named_cli(&config_home, &runtime_dir, &["fleet", "status", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    for flag in ["--json", "--timeout-ms", "--watch"] {
        assert!(help.contains(flag), "help is missing {flag}: {help}");
    }

    let output = run_named_cli(&config_home, &runtime_dir, &["fleet", "help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("--watch"));

    cleanup_test_base(&base);
}
