//! `herdr gateway` end-to-end CLI wiring, exercised through the real binary.
//!
//! The whole file is gated on the `gateway` feature so `cargo nextest run
//! --no-default-features` (fork CI's `check-no-default-features` job) compiles
//! it to an empty test binary instead of failing on a missing subcommand.
//! `unix` is the only platform gate: these tests run the binary and never boot
//! a server or a PTY, so unlike `tests/cli.rs` they have no reason to skip
//! macOS. Later E3 PRs append their gateway tests here.
#![cfg(all(unix, feature = "gateway"))]

pub mod support;

use std::path::PathBuf;
use std::process::Command;

use support::fleet_lab::{stderr_of, stdout_of, Lab};
use support::gateway::{Gateway, GatewayEnv};

/// A throwaway `XDG_CONFIG_HOME` so nothing here reads or writes the
/// developer's real herdr config.
struct IsolatedConfigHome(PathBuf);

impl IsolatedConfigHome {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("herdr-fork-gateway-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create isolated config home");
        Self(dir)
    }

    fn herdr(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_CONFIG_PATH")
            .env("XDG_CONFIG_HOME", &self.0);
        command
    }
}

impl Drop for IsolatedConfigHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn gateway_help_exits_zero() {
    let home = IsolatedConfigHome::new("help");
    let output = home
        .herdr()
        .args(["gateway", "help"])
        .output()
        .expect("run `herdr gateway help`");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("usage: herdr gateway"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

/// The command reaches `run_gateway_command` rather than main's `unknown
/// command` guard. The bare `herdr gateway` now starts a server, so the
/// allowlist wiring is pinned with an argument the run path refuses instead.
#[test]
fn gateway_with_an_unknown_option_is_a_usage_error() {
    let home = IsolatedConfigHome::new("usage");
    let output = home
        .herdr()
        .args(["gateway", "--nope"])
        .output()
        .expect("run `herdr gateway --nope`");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stdout: {stdout}");
    assert!(stderr.contains("usage: herdr gateway"), "stderr: {stderr}");
    assert!(
        !stderr.contains("unknown command"),
        "gateway must be in the bare-command allowlist; stderr: {stderr}"
    );
}

/// The spec renders `herdr gateway --help` through clap, and the top-level
/// `--help` advertises the command exactly once.
#[test]
fn gateway_is_advertised_in_help() {
    let home = IsolatedConfigHome::new("advertised");
    let output = home
        .herdr()
        .args(["gateway", "--help"])
        .output()
        .expect("run `herdr gateway --help`");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}");
    assert!(stdout.contains("Usage: herdr gateway"), "stdout: {stdout}");

    let output = home
        .herdr()
        .arg("--help")
        .output()
        .expect("run `herdr --help`");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}");
    assert_eq!(
        stdout.matches("herdr gateway").count(),
        1,
        "stdout: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// PR 4 — `herdr gateway` serving HTTP over a real fleet
// ---------------------------------------------------------------------------

/// A gateway environment with no lab behind it: `[fleet]` names no host, so the
/// runtime starts, connects to nothing and reports an empty fleet.
fn hostless_env(name: &str) -> (IsolatedConfigHome, GatewayEnv) {
    let home = IsolatedConfigHome::new(name);
    let env = GatewayEnv {
        config_home: home.0.clone(),
        runtime_dir: None,
    };
    env.append_config("[fleet]\ninclude_local = false\n");
    (home, env)
}

#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|err| panic!("stat {}: {err}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// The whole PR in one run against real servers: the lab's two sessions are
/// aggregated, `/health` needs nothing, `/api/fleet` needs a token, and the
/// body is exactly the `herdr fleet status --json` shape.
#[test]
fn gateway_serves_health_and_fleet_for_the_lab() {
    let mut lab = Lab::new("gw-fleet");
    let up = lab.up("2");
    assert!(
        up.status.success(),
        "up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );

    let gateway = Gateway::spawn(&lab, "");

    let health = gateway.http_get("/health", &[]);
    assert_eq!(health.status, 200, "{}", health.body);
    assert_eq!(health.json()["ok"].as_bool(), Some(true));

    let bare = gateway.http_get("/api/fleet", &[]);
    assert_eq!(bare.status, 401, "{}", bare.body);
    assert_eq!(bare.json()["error"].as_str(), Some("unauthorized"));

    // The connector needs a moment to hand every host its first snapshot.
    let mut report = serde_json::Value::Null;
    let connected = support::wait_until(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(250),
        || {
            let response =
                gateway.http_get("/api/fleet", &[("Authorization", &gateway.authorization())]);
            if response.status != 200 {
                return false;
            }
            report = response.json();
            report["hosts"].as_array().is_some_and(|hosts| {
                hosts.len() == 2
                    && hosts.iter().all(|host| {
                        host["connection"]["state"].as_str() == Some("connected")
                            && host["workspaces"]
                                .as_array()
                                .is_some_and(|workspaces| !workspaces.is_empty())
                    })
            })
        },
    );
    assert!(connected, "hosts never all connected: {report}");

    assert_eq!(
        report["schema"].as_str(),
        Some("herdr.fleet.status.v1"),
        "{report}"
    );
    let hosts: Vec<&str> = report["hosts"]
        .as_array()
        .expect("hosts")
        .iter()
        .filter_map(|host| host["id"].as_str())
        .collect();
    assert_eq!(hosts, vec!["lab-1", "lab-2"], "{report}");
    for host in report["hosts"].as_array().expect("hosts") {
        let id = host["id"].as_str().unwrap_or_default();
        assert_eq!(
            host["workspaces"][0]["label"].as_str(),
            Some(id),
            "the lab labels each workspace after its session: {host}"
        );
    }
    // The lab's marker panes run a sleep loop, not an agent.
    assert_eq!(
        report["agents"].as_array().map(Vec::len),
        Some(0),
        "{report}"
    );

    // The gateway info route reports the scope the token proved.
    let info = gateway.http_get(
        "/api/gateway",
        &[("Authorization", &gateway.authorization())],
    );
    assert_eq!(info.status, 200, "{}", info.body);
    assert_eq!(
        info.json()["schema"].as_str(),
        Some("herdr.gateway.info.v1")
    );
    assert_eq!(info.json()["scope"].as_str(), Some("read"));

    // A browser origin that is not the gateway's own is refused outright.
    let foreign = gateway.http_get(
        "/api/fleet",
        &[
            ("Authorization", &gateway.authorization()),
            ("Origin", "https://evil.example"),
        ],
    );
    assert_eq!(foreign.status, 403, "{}", foreign.body);
    assert_eq!(foreign.json()["error"].as_str(), Some("origin_not_allowed"));

    // The embedded shell is served at the root.
    let shell = gateway.http_get("/", &[]);
    assert_eq!(shell.status, 200, "{}", shell.body);
    assert_eq!(
        shell.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(shell.body.contains("Herdr Fleet"), "{}", shell.body);
}

/// A LAN bind with no configured origins is refused before anything is
/// created: no listener, and no token store either.
#[test]
fn gateway_refuses_non_loopback_bind_without_origins() {
    let (_home, env) = hostless_env("nonloopback");

    let output = env.run_gateway(&["--bind", "0.0.0.0:0"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("allowed_origins"), "stderr: {stderr}");
    assert!(stderr.contains("0.0.0.0:0"), "stderr: {stderr}");
    assert!(
        !env.gateway_dir().exists(),
        "a refused bind must not create the token store"
    );

    // The same config with an origin configured gets past the policy, so the
    // refusal really is about the allowlist and not about the address.
    env.append_config("[gateway]\nallowed_origins = [\"https://fleet.example\"]\n");
    let output = env.run_gateway(&["--bind", "not-an-address"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a malformed address is a usage error, not a refusal"
    );
}

/// The token store is owner-only, and a token that stops being owner-only stops
/// being trusted.
#[test]
fn gateway_token_files_are_private() {
    let (_home, env) = hostless_env("private");

    let mut gateway = Gateway::spawn_in(env.clone());
    let gateway_dir = gateway.gateway_dir();
    assert_eq!(gateway.stop(), Some(0));

    assert_eq!(mode_of(&gateway_dir), 0o700, "{}", gateway_dir.display());
    for name in ["read.token", "control.token"] {
        let path = gateway_dir.join(name);
        assert_eq!(mode_of(&path), 0o600, "{}", path.display());
    }

    let read_token = gateway_dir.join("read.token");
    let mut permissions = std::fs::metadata(&read_token)
        .expect("stat read.token")
        .permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o644);
    }
    std::fs::set_permissions(&read_token, permissions).expect("chmod 644 read.token");

    let output = env.run_gateway(&["--bind", "127.0.0.1:0"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("read.token"),
        "the message must name the file: {stderr}"
    );
}

/// `SIGTERM` is a clean stop: exit 0, and the runtime marker is gone.
#[test]
fn gateway_stops_cleanly_on_sigterm() {
    let (_home, env) = hostless_env("sigterm");

    let mut gateway = Gateway::spawn_in(env.clone());
    let marker = gateway.gateway_dir().join("gateway.json");
    assert!(
        marker.exists(),
        "a running gateway writes {}",
        marker.display()
    );
    assert_eq!(mode_of(&marker), 0o600, "{}", marker.display());
    let recorded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker).expect("read the marker")).expect("json");
    assert_eq!(
        recorded["listen"].as_str(),
        Some(gateway.addr.to_string().as_str())
    );

    assert_eq!(gateway.stop(), Some(0));
    assert!(
        !marker.exists(),
        "a clean stop removes {}",
        marker.display()
    );
}

/// A query-string token is not a credential, and a peer that keeps guessing is
/// refused for the rest of the window.
#[test]
fn bad_credentials_are_refused_and_then_rate_limited() {
    let (_home, env) = hostless_env("credentials");
    let gateway = Gateway::spawn_in(env);

    let with_query = gateway.http_get(&format!("/api/fleet?token={}", gateway.read_token()), &[]);
    assert_eq!(with_query.status, 401, "{}", with_query.body);

    // The request above presented nothing, so it is not a guess and does not
    // count; five presented-and-wrong tokens reach the configured limit.
    let mut statuses = Vec::new();
    for _ in 0..5 {
        statuses.push(
            gateway
                .http_get("/api/fleet", &[("Authorization", "Bearer 00")])
                .status,
        );
    }
    assert_eq!(statuses, vec![401, 401, 401, 401, 401], "{statuses:?}");

    // Even a valid token is refused while the peer is blocked, and `/health`
    // is not.
    let blocked = gateway.http_get("/api/fleet", &[("Authorization", &gateway.authorization())]);
    assert_eq!(blocked.status, 429, "{}", blocked.body);
    assert!(blocked.header("retry-after").is_some());
    assert_eq!(gateway.http_get("/health", &[]).status, 200);
}
