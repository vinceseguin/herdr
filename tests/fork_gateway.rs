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

// ---- events (PR 5) ----

/// `GET /api/events` against the real lab: the opening pair, then a real host
/// going away arriving as a delta on an already-open socket.
///
/// One test for both because the second half needs the first half's lab, and a
/// second `Lab::up` would double a 30-second setup for no extra coverage.
#[test]
fn events_stream_sends_report_then_host_delta() {
    let mut lab = Lab::new("gw-events");
    let up = lab.up("2");
    assert!(
        up.status.success(),
        "up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );

    let gateway = Gateway::spawn(&lab, "");

    // Wait for both hosts, so the opening `fleet` message is the interesting
    // one rather than a race with the connector's first snapshot.
    let connected = support::wait_until(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(250),
        || {
            let response =
                gateway.http_get("/api/fleet", &[("Authorization", &gateway.authorization())]);
            response.status == 200
                && response.json()["hosts"].as_array().is_some_and(|hosts| {
                    hosts.len() == 2
                        && hosts
                            .iter()
                            .all(|host| host["connection"]["state"].as_str() == Some("connected"))
                })
        },
    );
    assert!(connected, "hosts never all connected");

    // 1. The opening pair: identity, then the whole fleet.
    let opening = gateway.ws("/api/events", &["--max-messages", "2", "--timeout", "30"]);
    let lines = support::gateway::ws_lines(&opening);
    assert_eq!(opening.status.code(), Some(0), "{lines:?}");
    assert_eq!(lines.len(), 2, "{lines:?}");

    let hello = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(hello["kind"].as_str(), Some("hello"), "{hello}");
    assert_eq!(
        hello["schema"].as_str(),
        Some("herdr.fleet.events.v1"),
        "{hello}"
    );
    assert_eq!(hello["scope"].as_str(), Some("read"), "{hello}");

    let fleet = support::gateway::ws_text_json(&lines[1]);
    assert_eq!(fleet["kind"].as_str(), Some("fleet"), "{fleet}");
    assert_eq!(
        fleet["report"]["schema"].as_str(),
        Some("herdr.fleet.status.v1"),
        "{fleet}"
    );
    let hosts: Vec<&str> = fleet["report"]["hosts"]
        .as_array()
        .expect("hosts")
        .iter()
        .filter_map(|host| host["id"].as_str())
        .collect();
    assert_eq!(hosts, vec!["lab-1", "lab-2"], "{fleet}");

    // The gateway advertises the capability it now serves.
    let info = gateway.http_get(
        "/api/gateway",
        &[("Authorization", &gateway.authorization())],
    );
    let features: Vec<String> = info.json()["features"]
        .as_array()
        .map(|features| {
            features
                .iter()
                .filter_map(|feature| feature.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(features.contains(&"events".to_string()), "{features:?}");

    // 2. A delta on an open socket: stop one lab server and watch the
    //    `host_connection` change arrive without re-polling `/api/fleet`.
    let watcher = gateway.ws_spawn(
        "/api/events",
        &[("Authorization", &gateway.authorization())],
        &["--max-messages", "3", "--timeout", "60"],
    );
    // The socket is opened by the child; give it the two opening messages
    // before changing the world, so the delta cannot land in the report.
    std::thread::sleep(std::time::Duration::from_secs(2));

    let stop = lab.herdr("lab-2", &["session", "stop", "lab-2"]);
    assert!(
        stop.status.success(),
        "stopping lab-2 failed: {}{}",
        stdout_of(&stop),
        stderr_of(&stop)
    );

    let observed = watcher
        .wait_with_output()
        .expect("wait for the events watcher");
    let lines = support::gateway::ws_lines(&observed);
    assert_eq!(observed.status.code(), Some(0), "{lines:?}");
    assert_eq!(lines.len(), 3, "{lines:?}");
    let delta = support::gateway::ws_text_json(&lines[2]);
    assert_eq!(delta["kind"].as_str(), Some("host_connection"), "{delta}");
    assert_eq!(delta["host"].as_str(), Some("lab-2"), "{delta}");
    assert_ne!(
        delta["connection"]["state"].as_str(),
        Some("connected"),
        "a stopped host must not still read as connected: {delta}"
    );
}

/// The socket is behind exactly the same auth as the JSON routes: no
/// credential is a 401 at the HTTP handshake, before any upgrade, and a
/// foreign browser origin is a 403 even with a good token.
#[test]
fn events_refuses_a_missing_token_and_a_foreign_origin() {
    let (_home, env) = hostless_env("events-auth");
    let gateway = Gateway::spawn_in(env);

    let anonymous = gateway.ws_with("/api/events", &[], &["--timeout", "20"]);
    let lines = support::gateway::ws_lines(&anonymous);
    assert_eq!(anonymous.status.code(), Some(2), "{lines:?}");
    assert_eq!(lines, vec!["handshake 401".to_string()], "{lines:?}");

    let foreign = gateway.ws_with(
        "/api/events",
        &[
            ("Authorization", &gateway.authorization()),
            ("Origin", "https://evil.example"),
        ],
        &["--timeout", "20"],
    );
    let lines = support::gateway::ws_lines(&foreign);
    assert_eq!(foreign.status.code(), Some(2), "{lines:?}");
    assert_eq!(lines, vec!["handshake 403".to_string()], "{lines:?}");

    // And a good token on a hostless gateway still gets a well-formed stream:
    // an empty fleet is a fleet.
    let opening = gateway.ws("/api/events", &["--max-messages", "2", "--timeout", "20"]);
    let lines = support::gateway::ws_lines(&opening);
    assert_eq!(opening.status.code(), Some(0), "{lines:?}");
    let fleet = support::gateway::ws_text_json(&lines[1]);
    assert_eq!(fleet["kind"].as_str(), Some("fleet"), "{fleet}");
    assert_eq!(
        fleet["report"]["hosts"].as_array().map(Vec::len),
        Some(0),
        "{fleet}"
    );
}

/// A stopping gateway says goodbye rather than dropping the TCP connection, so
/// a client can tell "the server went away" from "the network blipped".
#[test]
fn events_closes_with_going_away_when_the_gateway_stops() {
    let (_home, env) = hostless_env("events-shutdown");
    let mut gateway = Gateway::spawn_in(env);

    let watcher = gateway.ws_spawn(
        "/api/events",
        &[("Authorization", &gateway.authorization())],
        &["--max-messages", "10", "--timeout", "30"],
    );
    std::thread::sleep(std::time::Duration::from_secs(1));

    assert_eq!(gateway.stop(), Some(0));

    let observed = watcher.wait_with_output().expect("wait for the watcher");
    let lines = support::gateway::ws_lines(&observed);
    assert_eq!(observed.status.code(), Some(0), "{lines:?}");
    assert_eq!(
        lines.last().map(String::as_str),
        Some("close 1001"),
        "{lines:?}"
    );
}

/// The feed is a feed: a `read` client that talks anyway is ignored, and a
/// client that talks *too much* takes down only its own socket.
///
/// Both halves are one test because they share a gateway and neither needs a
/// lab: an empty fleet still exercises the whole handshake and the inbound cap.
#[test]
fn events_ignores_client_chatter_and_survives_an_oversized_message() {
    let (_home, env) = hostless_env("events-inbound");
    let gateway = Gateway::spawn_in(env);

    // Anything a client says on this socket is dropped — including a message
    // shaped like the terminal input a `read` token may never send.
    let chatty = gateway.ws(
        "/api/events",
        &[
            "--send",
            r#"{"type":"terminal.input","data":"rm -rf /"}"#,
            "--max-messages",
            "2",
            "--timeout",
            "20",
        ],
    );
    let lines = support::gateway::ws_lines(&chatty);
    assert_eq!(chatty.status.code(), Some(0), "{lines:?}");
    let kinds: Vec<Option<String>> = lines
        .iter()
        .map(|line| {
            support::gateway::ws_text_json(line)["kind"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        kinds,
        vec![Some("hello".to_string()), Some("fleet".to_string())],
        "{lines:?}"
    );

    // A message past the 4 KiB inbound cap ends that socket. The exit code is
    // deliberately not asserted (the peer may see a close frame or a reset);
    // what matters is that the *gateway* is unharmed.
    let oversized = format!(r#"{{"pad":"{}"}}"#, "x".repeat(8 * 1024));
    let _ = gateway.ws(
        "/api/events",
        &[
            "--send",
            &oversized,
            "--max-messages",
            "2",
            "--timeout",
            "20",
        ],
    );

    assert_eq!(gateway.http_get("/health", &[]).status, 200);
    let again = gateway.ws("/api/events", &["--max-messages", "2", "--timeout", "20"]);
    let lines = support::gateway::ws_lines(&again);
    assert_eq!(again.status.code(), Some(0), "{lines:?}");
    assert_eq!(
        support::gateway::ws_text_json(&lines[1])["kind"].as_str(),
        Some("fleet"),
        "{lines:?}"
    );
}

// ---- terminal (PR 6) ----

/// The header of a `binary <hex>` line: `[kind][seq u64 LE][w u16][h u16][full]`.
#[derive(Debug, PartialEq, Eq)]
struct FrameHeader {
    kind: u8,
    seq: u64,
    width: u16,
    height: u16,
    full: bool,
}

/// Split a `binary <hex>` line into its header and its payload.
///
/// This is the decoder E4's browser code implements in TypeScript, written out
/// once here so the layout is asserted end to end rather than in a unit test
/// alone.
fn decode_binary_line(line: &str) -> (FrameHeader, Vec<u8>) {
    let hex = line
        .strip_prefix("binary ")
        .unwrap_or_else(|| panic!("not a binary message: {line}"));
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&hex[index..index + 2], 16)
                .unwrap_or_else(|err| panic!("not hex ({err}): {line}"))
        })
        .collect();
    assert!(bytes.len() >= 14, "frame shorter than its header: {line}");
    let header = FrameHeader {
        kind: bytes[0],
        seq: u64::from_le_bytes(bytes[1..9].try_into().expect("8 bytes")),
        width: u16::from_le_bytes(bytes[9..11].try_into().expect("2 bytes")),
        height: u16::from_le_bytes(bytes[11..13].try_into().expect("2 bytes")),
        full: bytes[13] != 0,
    };
    (header, bytes[14..].to_vec())
}

/// The pane id every lab session's marker pane has.
const LAB_PANE: &str = "w1:p1";

/// Wait until every configured host is `connected`, so a terminal open is not
/// racing the connector's first handshake.
fn wait_for_connected_hosts(gateway: &Gateway, count: usize) {
    let mut report = serde_json::Value::Null;
    let ready = support::wait_until(
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
                hosts.len() == count
                    && hosts
                        .iter()
                        .all(|host| host["connection"]["state"].as_str() == Some("connected"))
            })
        },
    );
    assert!(ready, "hosts never all connected: {report}");
}

/// The whole read path against real servers: a `read` token observes lab-2's
/// marker pane, receives a full frame carrying that pane's text, and cannot
/// type into it.
#[test]
fn terminal_observe_streams_marker_pane_frames() {
    let mut lab = Lab::new("gw-term");
    let up = lab.up("2");
    assert!(
        up.status.success(),
        "up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );
    let gateway = Gateway::spawn(&lab, "");
    wait_for_connected_hosts(&gateway, 2);

    let observed = gateway.ws(
        &format!("/api/terminal/lab-2/{LAB_PANE}"),
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24}"#,
            "--max-messages",
            "2",
            "--timeout",
            "30",
            "--binary",
            "hex",
        ],
    );
    let lines = support::gateway::ws_lines(&observed);
    assert_eq!(observed.status.code(), Some(0), "{lines:?}");
    assert!(lines.len() >= 2, "{lines:?}");

    let ready = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(ready["type"].as_str(), Some("terminal.ready"), "{lines:?}");
    assert_eq!(ready["schema"].as_str(), Some("herdr.fleet.terminal.v1"));
    assert_eq!(ready["mode"].as_str(), Some("observe"));
    assert_eq!(ready["host"].as_str(), Some("lab-2"));
    assert_eq!(ready["pane"].as_str(), Some(LAB_PANE));
    assert_eq!(ready["ref"].as_str(), Some("lab-2/w1:p1"));
    assert_eq!(ready["cols"].as_u64(), Some(80));
    assert_eq!(ready["rows"].as_u64(), Some(24));

    // The first frame is a full redraw at the geometry this session asked for,
    // and it carries the pane's own text — which is how we know the stream is
    // wired to lab-2's pane and not to some other host's.
    let (header, body) = decode_binary_line(&lines[1]);
    assert_eq!(header.kind, 0x01, "{lines:?}");
    assert!(
        header.full,
        "the first frame must be a full redraw: {header:?}"
    );
    assert_eq!((header.width, header.height), (80, 24), "{header:?}");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("herdr-fleet-lab:lab-2"),
        "the frame does not carry lab-2's marker: {text:?}"
    );

    // A read credential in observe mode is refused, by code, and the socket
    // survives the refusal.
    let refused = gateway.ws(
        &format!("/api/terminal/lab-2/{LAB_PANE}"),
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24}"#,
            "--send",
            r#"{"type":"terminal.input","text":"echo INJECTED\n"}"#,
            "--max-messages",
            "3",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&refused);
    assert_eq!(refused.status.code(), Some(0), "{lines:?}");
    let error = lines
        .iter()
        .filter(|line| line.starts_with("text "))
        .map(|line| support::gateway::ws_text_json(line))
        .find(|value| value["type"].as_str() == Some("terminal.error"))
        .unwrap_or_else(|| panic!("no terminal.error in {lines:?}"));
    assert_eq!(error["code"].as_str(), Some("forbidden"), "{lines:?}");

    // Nothing reached the pane.
    let read = lab.herdr("lab-2", &["pane", "read", LAB_PANE, "--source", "recent"]);
    let pane_text = stdout_of(&read);
    assert!(
        !pane_text.contains("INJECTED"),
        "an observer typed into the pane: {pane_text}"
    );

    // A `read` credential asking for control is told it lacks the scope, before
    // a host is ever touched. What a *control* credential may do in that mode
    // is the PR 7 section below.
    let unscoped = gateway.ws(
        &format!("/api/terminal/lab-2/{LAB_PANE}"),
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"control","cols":80,"rows":24}"#,
            "--max-messages",
            "1",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&unscoped);
    let answer = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(answer["code"].as_str(), Some("forbidden"), "{lines:?}");
}

/// A pane the host cannot resolve, and a host the gateway does not have: two
/// different codes, both stream-local, neither a 5xx.
#[test]
fn terminal_open_reports_a_missing_pane_and_an_unknown_host() {
    let mut lab = Lab::new("gw-term-miss");
    let up = lab.up("1");
    assert!(
        up.status.success(),
        "up 1 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );
    let gateway = Gateway::spawn(&lab, "");
    wait_for_connected_hosts(&gateway, 1);

    // The host is up and answers the terminal hello, then refuses the target.
    let missing_pane = gateway.ws(
        "/api/terminal/lab-1/w9:p9",
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24}"#,
            "--max-messages",
            "3",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&missing_pane);
    let error = lines
        .iter()
        .filter(|line| line.starts_with("text "))
        .map(|line| support::gateway::ws_text_json(line))
        .find(|value| value["type"].as_str() == Some("terminal.error"))
        .unwrap_or_else(|| panic!("no terminal.error in {lines:?}"));
    assert_eq!(error["code"].as_str(), Some("pane_not_found"), "{lines:?}");

    // A host that is not in `[fleet]` at all never reaches a socket.
    let unknown_host = gateway.ws(
        &format!("/api/terminal/lab-404/{LAB_PANE}"),
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24}"#,
            "--max-messages",
            "1",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&unknown_host);
    let answer = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(answer["type"].as_str(), Some("terminal.error"), "{lines:?}");
    assert_eq!(
        answer["code"].as_str(),
        Some("host_unavailable"),
        "{lines:?}"
    );

    // A target that could not be a fleet reference never becomes a socket at
    // all: the handshake itself answers 404, so a client learns it spelled the
    // pane wrong without an upgrade it would have to tear down.
    let not_found = gateway.ws(
        "/api/terminal/lab-1/w1%2Fp1",
        &["--max-messages", "1", "--timeout", "30"],
    );
    let lines = support::gateway::ws_lines(&not_found);
    assert_eq!(not_found.status.code(), Some(2), "{lines:?}");
    assert_eq!(
        lines.first().map(String::as_str),
        Some("handshake 404"),
        "{lines:?}"
    );
}

/// A terminal on a host that has gone away is that socket's problem and
/// nobody else's: the fleet report still answers and the gateway still stops
/// cleanly.
#[test]
fn terminal_open_on_a_stopped_host_fails_without_touching_the_gateway() {
    let mut lab = Lab::new("gw-term-down");
    let up = lab.up("2");
    assert!(
        up.status.success(),
        "up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );
    let mut gateway = Gateway::spawn(&lab, "");
    wait_for_connected_hosts(&gateway, 2);

    let stopped = lab.herdr("lab-2", &["server", "stop"]);
    assert!(
        stopped.status.success(),
        "could not stop lab-2: {}{}",
        stdout_of(&stopped),
        stderr_of(&stopped)
    );
    let down = support::wait_until(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_millis(250),
        || {
            let response =
                gateway.http_get("/api/fleet", &[("Authorization", &gateway.authorization())]);
            response.status == 200
                && response.json()["hosts"].as_array().is_some_and(|hosts| {
                    hosts.iter().any(|host| {
                        host["id"].as_str() == Some("lab-2")
                            && host["connection"]["state"].as_str() != Some("connected")
                    })
                })
        },
    );
    assert!(down, "lab-2 never left the connected state");

    let refused = gateway.ws(
        &format!("/api/terminal/lab-2/{LAB_PANE}"),
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24}"#,
            "--max-messages",
            "1",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&refused);
    let answer = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(
        answer["code"].as_str(),
        Some("host_unavailable"),
        "{lines:?}"
    );

    // The other host is untouched, and so is the gateway.
    let alive = gateway.ws(
        &format!("/api/terminal/lab-1/{LAB_PANE}"),
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24}"#,
            "--max-messages",
            "2",
            "--timeout",
            "30",
            "--binary",
            "hex",
        ],
    );
    let lines = support::gateway::ws_lines(&alive);
    assert_eq!(alive.status.code(), Some(0), "{lines:?}");
    let (_, body) = decode_binary_line(&lines[1]);
    assert!(
        String::from_utf8_lossy(&body).contains("herdr-fleet-lab:lab-1"),
        "{lines:?}"
    );

    assert_eq!(gateway.http_get("/health", &[]).status, 200);
    assert_eq!(gateway.stop(), Some(0));
}

// ---- terminal control (PR 7) ----

/// `terminal.open` for a control session at the lab's standard geometry.
const OPEN_CONTROL: &str = r#"{"type":"terminal.open","mode":"control","cols":80,"rows":24}"#;
/// The same, taking the pane from whoever holds it.
const OPEN_CONTROL_TAKEOVER: &str =
    r#"{"type":"terminal.open","mode":"control","cols":80,"rows":24,"takeover":true}"#;

/// The JSON of every `text` line in a `ws-client.py` run.
fn ws_texts(output: &std::process::Output) -> Vec<serde_json::Value> {
    support::gateway::ws_lines(output)
        .iter()
        .filter(|line| line.starts_with("text "))
        .map(|line| support::gateway::ws_text_json(line))
        .collect()
}

/// Whether a pane's recent text contains `needle`, polled: a keystroke takes a
/// moment to be echoed by the tty and rendered by the host.
fn wait_for_pane_text(lab: &Lab, session: &str, pane: &str, needle: &str) -> String {
    let mut text = String::new();
    let found = support::wait_until(
        std::time::Duration::from_secs(20),
        std::time::Duration::from_millis(200),
        || {
            text = stdout_of(&lab.herdr(session, &["pane", "read", pane, "--source", "recent"]));
            text.contains(needle)
        },
    );
    assert!(found, "{session}/{pane} never showed {needle:?}: {text}");
    text
}

/// The whole write path against real servers: a `control` token opens lab-1's
/// marker pane, types, releases — and exactly one machine's pane changed.
#[test]
fn terminal_control_types_into_the_right_pane() {
    let mut lab = Lab::new("gw-term-ctl");
    let up = lab.up("2");
    assert!(
        up.status.success(),
        "up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );
    let mut gateway = Gateway::spawn(&lab, "");
    wait_for_connected_hosts(&gateway, 2);

    let typed = gateway.ws_with(
        &format!("/api/terminal/lab-1/{LAB_PANE}"),
        &[("Authorization", &gateway.control_authorization())],
        &[
            "--send",
            OPEN_CONTROL,
            "--send",
            r#"{"type":"terminal.input","text":"echo E3-CONTROL-lab-1\n"}"#,
            "--send",
            r#"{"type":"terminal.release"}"#,
            "--max-messages",
            "8",
            "--timeout",
            "30",
            "--binary",
            "len",
        ],
    );
    let lines = support::gateway::ws_lines(&typed);
    assert_eq!(typed.status.code(), Some(0), "{lines:?}");
    let ready = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(ready["type"].as_str(), Some("terminal.ready"), "{lines:?}");
    assert_eq!(ready["mode"].as_str(), Some("control"), "{lines:?}");
    assert_eq!(ready["ref"].as_str(), Some("lab-1/w1:p1"), "{lines:?}");
    // A release is the client's own doing and says so, rather than arriving as
    // the host's "detached".
    let closed = ws_texts(&typed)
        .into_iter()
        .find(|value| value["type"].as_str() == Some("terminal.closed"))
        .unwrap_or_else(|| panic!("no terminal.closed in {lines:?}"));
    assert_eq!(closed["reason"].as_str(), Some("released"), "{lines:?}");

    // The bytes reached lab-1's pty — and only lab-1's.
    wait_for_pane_text(&lab, "lab-1", LAB_PANE, "E3-CONTROL-lab-1");
    let other = stdout_of(&lab.herdr("lab-2", &["pane", "read", LAB_PANE, "--source", "recent"]));
    assert!(
        !other.contains("E3-CONTROL"),
        "input reached lab-2 as well: {other}"
    );

    assert_eq!(gateway.stop(), Some(0));
}

/// The scope gate, where it actually has to hold: inside the session, not at
/// the route. A read credential cannot open a control session, and the pane it
/// asked for is untouched.
#[test]
fn terminal_control_is_refused_for_the_read_scope() {
    let mut lab = Lab::new("gw-term-scope");
    let up = lab.up("1");
    assert!(
        up.status.success(),
        "up 1 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );
    let gateway = Gateway::spawn(&lab, "");
    wait_for_connected_hosts(&gateway, 1);

    // The read token reaches the route (it is authenticated) and is refused by
    // the session, with the scope named and a policy close.
    let refused = gateway.ws(
        &format!("/api/terminal/lab-1/{LAB_PANE}"),
        &[
            "--send",
            OPEN_CONTROL,
            "--send",
            r#"{"type":"terminal.input","text":"echo E3-READ-SCOPE\n"}"#,
            "--max-messages",
            "3",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&refused);
    let answer = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(answer["type"].as_str(), Some("terminal.error"), "{lines:?}");
    assert_eq!(answer["code"].as_str(), Some("forbidden"), "{lines:?}");
    assert!(
        lines.iter().any(|line| line == "close 1008"),
        "a refused control open must close with a policy code: {lines:?}"
    );
    // Nothing after the refusal was forwarded either: the session is over.
    assert!(
        !lines.iter().any(|line| line.starts_with("binary ")),
        "a refused session streamed frames: {lines:?}"
    );

    // Give the host a moment to be wrong, then prove it was not.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let pane = stdout_of(&lab.herdr("lab-1", &["pane", "read", LAB_PANE, "--source", "recent"]));
    assert!(
        !pane.contains("E3-READ-SCOPE"),
        "a read credential typed into the pane: {pane}"
    );
}

/// One pane, two controllers. The second is told `busy` rather than silently
/// sharing the keyboard; with `takeover` it wins and the first is told so.
#[test]
fn terminal_control_second_owner_needs_takeover() {
    use std::io::{BufRead, BufReader, Read};

    let mut lab = Lab::new("gw-term-take");
    let up = lab.up("1");
    assert!(
        up.status.success(),
        "up 1 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );
    let gateway = Gateway::spawn(&lab, "");
    wait_for_connected_hosts(&gateway, 1);
    let path = format!("/api/terminal/lab-1/{LAB_PANE}");
    let control = gateway.control_authorization();

    // The first controller holds the pane until somebody takes it. Reading its
    // `terminal.ready` is what makes this deterministic: the gateway only sends
    // it once the host has confirmed the attach.
    let mut first = gateway.ws_spawn(
        &path,
        &[("Authorization", &control)],
        &["--send", OPEN_CONTROL, "--timeout", "60", "--binary", "len"],
    );
    let mut first_out = BufReader::new(first.stdout.take().expect("piped stdout"));
    let mut ready_line = String::new();
    first_out
        .read_line(&mut ready_line)
        .expect("read the first controller's ready line");
    let ready = support::gateway::ws_text_json(ready_line.trim_end());
    assert_eq!(ready["type"].as_str(), Some("terminal.ready"), "{ready}");
    assert_eq!(ready["mode"].as_str(), Some("control"), "{ready}");

    // A second controller without `takeover` is refused by name, and the first
    // one keeps the pane.
    let busy = gateway.ws_with(
        &path,
        &[("Authorization", &control)],
        &[
            "--send",
            OPEN_CONTROL,
            "--max-messages",
            "1",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&busy);
    let answer = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(answer["type"].as_str(), Some("terminal.error"), "{lines:?}");
    assert_eq!(answer["code"].as_str(), Some("busy"), "{lines:?}");

    // With `takeover` it wins: it is ready, and it gets the host's redraw.
    let taken = gateway.ws_with(
        &path,
        &[("Authorization", &control)],
        &[
            "--send",
            OPEN_CONTROL_TAKEOVER,
            "--max-messages",
            "2",
            "--timeout",
            "30",
            "--binary",
            "len",
        ],
    );
    let lines = support::gateway::ws_lines(&taken);
    let ready = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(ready["type"].as_str(), Some("terminal.ready"), "{lines:?}");
    assert_eq!(ready["mode"].as_str(), Some("control"), "{lines:?}");
    assert!(
        lines.iter().any(|line| line.starts_with("binary ")),
        "the new owner never got a redraw: {lines:?}"
    );

    // And the evicted controller is told why, by a stable token rather than the
    // host's sentence.
    let mut rest = String::new();
    first_out
        .read_to_string(&mut rest)
        .expect("read the evicted controller's remaining output");
    let closed = rest
        .lines()
        .filter(|line| line.starts_with("text "))
        .map(support::gateway::ws_text_json)
        .find(|value| value["type"].as_str() == Some("terminal.closed"))
        .unwrap_or_else(|| panic!("the evicted controller was never told: {rest}"));
    assert_eq!(closed["reason"].as_str(), Some("taken_over"), "{rest}");
    let _ = first.kill();
    let _ = first.wait();
}

// ---- pairing (PR 8) ----

/// A pairing URL, split into the part a test can send and the part it must
/// never print. Nothing here ever asserts on the code itself.
fn pair_path(url: &str) -> String {
    let start = url
        .find("/pair?")
        .unwrap_or_else(|| panic!("a pairing URL contains /pair?; got {} bytes", url.len()));
    url[start..].to_string()
}

/// `herdr gateway pair --json` in the gateway's own environment.
fn pair_json(env: &GatewayEnv, args: &[&str]) -> serde_json::Value {
    let mut arguments = vec!["pair", "--json"];
    arguments.extend_from_slice(args);
    let output = env.run_gateway(&arguments);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("`pair --json` printed no JSON object ({err})"))
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .expect("a cookie name=value pair")
        .to_string()
}

/// The whole operator loop for a read device: `pair` prints a URL, opening it
/// once mints a cookie that authorizes the API, and opening it again does not.
#[test]
fn pairing_url_exchanges_into_a_device_cookie() {
    let (_home, env) = hostless_env("pair-exchange");
    let mut gateway = Gateway::spawn_in(env.clone());

    let pairing = pair_json(&env, &["--label", "test phone"]);
    let mut keys: Vec<&str> = pairing
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["expires_unix", "scope", "url"], "{pairing}");
    assert_eq!(pairing["scope"].as_str(), Some("read"));
    let url = pairing["url"].as_str().expect("a url").to_string();
    // The CLI learned the ephemeral port from the running gateway's marker.
    assert!(
        url.starts_with(&format!("http://{}/pair?code=", gateway.addr)),
        "the pairing URL must name the bound address"
    );

    let paired = gateway.http_get(&pair_path(&url), &[]);
    assert_eq!(paired.status, 303, "{}", paired.body);
    assert_eq!(paired.header("location"), Some("/"));
    let set_cookie = paired.header("set-cookie").expect("a Set-Cookie header");
    for attribute in [
        "herdr_gateway_device=",
        "Path=/",
        "HttpOnly",
        "SameSite=Strict",
        "Max-Age=31536000",
    ] {
        assert!(
            set_cookie.contains(attribute),
            "the cookie must carry {attribute}"
        );
    }
    assert!(
        !set_cookie.contains("Secure"),
        "a plain-http gateway must not mint a Secure cookie"
    );

    let cookie = cookie_pair(set_cookie);
    let fleet = gateway.http_get("/api/fleet", &[("Cookie", &cookie)]);
    assert_eq!(fleet.status, 200, "{}", fleet.body);

    let info = gateway.http_get("/api/gateway", &[("Cookie", &cookie)]);
    let body = info.json();
    assert_eq!(body["scope"].as_str(), Some("read"));
    assert_eq!(body["via"].as_str(), Some("device"));
    assert_eq!(body["device"]["label"].as_str(), Some("test phone"));
    assert!(
        body["features"]
            .as_array()
            .is_some_and(|features| features.iter().any(|name| name == "pairing")),
        "{body}"
    );

    // One time only.
    let again = gateway.http_get(&pair_path(&url), &[]);
    assert_eq!(again.status, 403, "{}", again.body);
    assert_eq!(again.json()["error"].as_str(), Some("pairing_invalid"));
    assert!(again.header("set-cookie").is_none());

    assert_eq!(gateway.stop(), Some(0));
}

/// The text form is what an operator actually sees: a sentence, the URL, then
/// a QR code that fits a terminal. Only its shape is asserted; the URL itself
/// is never printed by the test.
#[test]
fn pair_prints_a_url_and_a_scannable_qr_code() {
    let (_home, env) = hostless_env("pair-text");
    let mut gateway = Gateway::spawn_in(env.clone());

    let output = env.run_gateway(&["pair"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].starts_with("Pair this device with Herdr Fleet (read scope, valid "),
        "{:?}",
        lines[0]
    );
    assert!(
        lines[1].trim_start().starts_with("http://"),
        "{:?}",
        lines[1]
    );
    let qr: Vec<&&str> = lines[2..]
        .iter()
        .filter(|line| {
            line.chars()
                .all(|ch| " \u{2580}\u{2584}\u{2588}".contains(ch))
        })
        .collect();
    assert!(qr.len() >= 20, "a QR code of {} lines", qr.len());
    let width = qr
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    assert!(
        width <= 80,
        "a QR {width} columns wide does not fit a terminal"
    );

    // `--no-qr` is the same output without those lines.
    let plain = env.run_gateway(&["pair", "--no-qr"]);
    let plain = String::from_utf8_lossy(&plain.stdout);
    assert!(!plain.contains('\u{2588}'), "--no-qr still drew a QR code");
    assert_eq!(plain.lines().count(), 3, "{}", plain.lines().count());

    // A ttl outside the documented bounds is a usage error, not a short link.
    let refused = env.run_gateway(&["pair", "--ttl-secs", "5"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--ttl-secs"));

    assert_eq!(gateway.stop(), Some(0));
}

/// Rotating a token revokes the devices it granted **and** the codes that
/// would have granted it, while the gateway keeps running.
#[test]
fn pair_control_then_rotate_revokes_the_device() {
    let (_home, env) = hostless_env("pair-rotate");
    let mut gateway = Gateway::spawn_in(env.clone());

    let pairing = pair_json(&env, &["--control"]);
    assert_eq!(pairing["scope"].as_str(), Some("control"));
    let url = pairing["url"].as_str().expect("a url").to_string();
    let paired = gateway.http_get(&pair_path(&url), &[]);
    assert_eq!(paired.status, 303, "{}", paired.body);
    let cookie = cookie_pair(paired.header("set-cookie").expect("a Set-Cookie header"));
    assert_eq!(
        gateway
            .http_get("/api/gateway", &[("Cookie", &cookie)])
            .json()["scope"]
            .as_str(),
        Some("control")
    );

    // A second, unredeemed control code must not survive the rotation either.
    let pending = pair_json(&env, &["--control"]);
    let pending_url = pending["url"].as_str().expect("a url").to_string();

    let rotate = env.run_gateway(&["rotate-token", "control"]);
    let rotate_out = String::from_utf8_lossy(&rotate.stdout);
    assert_eq!(
        rotate.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&rotate.stderr)
    );
    assert!(
        rotate_out.contains("rotated the control token")
            && rotate_out.contains("1 device")
            && rotate_out.contains("1 pending pairing code"),
        "{rotate_out}"
    );

    // No restart happened: the running gateway refuses the cookie, the old
    // bearer token and the code that was still outstanding.
    let refused = gateway.http_get("/api/fleet", &[("Cookie", &cookie)]);
    assert_eq!(refused.status, 401, "{}", refused.body);
    let stale_bearer = gateway.http_get(
        "/api/fleet",
        &[("Authorization", &gateway.control_authorization())],
    );
    assert_eq!(stale_bearer.status, 401, "{}", stale_bearer.body);
    let stale_code = gateway.http_get(&pair_path(&pending_url), &[]);
    assert_eq!(stale_code.status, 403, "{}", stale_code.body);

    // The token file that replaced it works at once.
    let rotated = std::fs::read_to_string(gateway.gateway_dir().join("control.token"))
        .expect("read the rotated control token");
    let accepted = gateway.http_get(
        "/api/fleet",
        &[("Authorization", &format!("Bearer {}", rotated.trim()))],
    );
    assert_eq!(accepted.status, 200, "{}", accepted.body);
    // The read scope was not touched.
    assert_eq!(
        gateway
            .http_get("/api/fleet", &[("Authorization", &gateway.authorization())])
            .status,
        200
    );

    assert_eq!(gateway.stop(), Some(0));
}

/// `status` is the operator's "is it up, and who is paired": exit 0 and a live
/// health probe while it runs, exit 3 once it is gone.
#[test]
fn gateway_status_reports_running_and_devices() {
    let (_home, env) = hostless_env("status");

    let before = env.run_gateway(&["status", "--json"]);
    assert_eq!(before.status.code(), Some(3), "no gateway is running yet");
    let before: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&before.stdout).trim()).expect("json");
    assert_eq!(before["running"].as_bool(), Some(false));
    assert!(
        before["pid"].is_null() && before["listen"].is_null(),
        "{before}"
    );
    assert_eq!(before["devices"]["read"].as_u64(), Some(0));

    let mut gateway = Gateway::spawn_in(env.clone());

    // One code outstanding, then one device paired from a second code.
    let _pending = pair_json(&env, &[]);
    let url = pair_json(&env, &["--label", "desk"])["url"]
        .as_str()
        .expect("a url")
        .to_string();
    assert_eq!(gateway.http_get(&pair_path(&url), &[]).status, 303);

    let running = env.run_gateway(&["status", "--json"]);
    assert_eq!(running.status.code(), Some(0));
    let json: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&running.stdout).trim()).expect("json");
    assert_eq!(json["schema"].as_str(), Some("herdr.gateway.status.v1"));
    assert_eq!(json["running"].as_bool(), Some(true));
    assert_eq!(json["healthy"].as_bool(), Some(true), "{json}");
    assert_eq!(
        json["listen"].as_str(),
        Some(gateway.addr.to_string().as_str())
    );
    assert!(json["pid"].as_u64().is_some_and(|pid| pid > 0), "{json}");
    assert_eq!(json["devices"]["read"].as_u64(), Some(1), "{json}");
    assert_eq!(json["devices"]["control"].as_u64(), Some(0), "{json}");
    assert_eq!(json["pairings_pending"].as_u64(), Some(1), "{json}");

    let text = env.run_gateway(&["status"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("gateway:  running (pid "), "{text}");
    assert!(text.contains("health:   ok"), "{text}");
    assert!(text.contains("devices:  read 1, control 0"), "{text}");
    assert!(text.contains("pairings: 1 pending"), "{text}");

    assert_eq!(gateway.stop(), Some(0));

    // A stopped gateway removes its marker; the paired device is still on file.
    let after = env.run_gateway(&["status", "--json"]);
    assert_eq!(after.status.code(), Some(3));
    let after: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&after.stdout).trim()).expect("json");
    assert_eq!(after["running"].as_bool(), Some(false));
    assert_eq!(after["devices"]["read"].as_u64(), Some(1), "{after}");
}

/// A crash leaves `gateway.json` behind. `status` must not believe it, and
/// `pair` must not build a URL from a port nothing is listening on.
#[test]
fn a_stale_runtime_marker_is_not_a_running_gateway() {
    let (_home, env) = hostless_env("stale-marker");
    let mut gateway = Gateway::spawn_in(env.clone());
    let marker = gateway.gateway_dir().join("gateway.json");
    let recorded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker).expect("read the marker")).expect("json");
    assert_eq!(gateway.stop(), Some(0));

    // Put it back with a pid that cannot be running.
    let stale = serde_json::json!({
        "pid": 0,
        "listen": recorded["listen"],
        "started_unix": recorded["started_unix"],
    });
    std::fs::write(&marker, stale.to_string()).expect("write a stale marker");
    // `0600` like the gateway's own writer: otherwise the marker is refused for
    // its mode and this test would never reach the pid check it is about.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600))
            .expect("make the stale marker private");
    }

    let status = env.run_gateway(&["status", "--json"]);
    assert_eq!(
        status.status.code(),
        Some(3),
        "a stale marker is not running"
    );
    let json: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&status.stdout).trim()).expect("json");
    assert_eq!(json["running"].as_bool(), Some(false), "{json}");

    // `pair` falls back to the configured bind, which is the default port —
    // not the ephemeral one the dead process had.
    let pairing = pair_json(&env, &[]);
    let url = pairing["url"].as_str().expect("a url");
    assert!(
        url.starts_with("http://127.0.0.1:7788/pair?code="),
        "wrong base"
    );
}
