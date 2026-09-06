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

    // A control mode is refused too: this build serves observers only, and the
    // refusal is `unsupported`, not a silent downgrade to observe.
    let control = gateway.ws_with(
        &format!("/api/terminal/lab-2/{LAB_PANE}"),
        &[("Authorization", &gateway.control_authorization())],
        &[
            "--send",
            r#"{"type":"terminal.open","mode":"control","cols":80,"rows":24}"#,
            "--max-messages",
            "1",
            "--timeout",
            "30",
        ],
    );
    let lines = support::gateway::ws_lines(&control);
    let answer = support::gateway::ws_text_json(&lines[0]);
    assert_eq!(answer["type"].as_str(), Some("terminal.error"), "{lines:?}");
    assert_eq!(answer["code"].as_str(), Some("unsupported"), "{lines:?}");

    // And a `read` credential asking for control is told it lacks the scope,
    // before the build's own limitation is ever consulted.
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
