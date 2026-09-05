#![cfg(unix)]
//! `scripts/fork/fleet-lab.sh` boots N isolated named herdr sessions.
//!
//! The lab is the fixture every later fork epic validates against, so this test
//! drives the real script against real servers: it must come up, report a
//! machine-readable status, keep every byte of state inside its own root, and
//! tear itself down without leaving a process or a directory behind.

pub mod support;

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Generous: the script boots real servers and CI runners are slow.
const STEP_TIMEOUT_MS: &str = "20000";
const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn script_path() -> PathBuf {
    repo_root().join("scripts/fork/fleet-lab.sh")
}

/// A lab root that no other test (or a concurrent `just ci`) can collide with.
fn unique_root(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    // Kept short on purpose: session sockets live under this root and unix
    // socket paths are capped at ~108 bytes.
    std::env::temp_dir().join(format!(
        "herdr-lab-{label}-{}-{}",
        std::process::id(),
        nanos % 1_000_000
    ))
}

struct Lab {
    root: PathBuf,
    registered: bool,
}

impl Lab {
    fn new(label: &str) -> Self {
        Self {
            root: unique_root(label),
            registered: false,
        }
    }

    fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_bin(env!("CARGO_BIN_EXE_herdr"), args)
    }

    fn run_with_bin(&self, bin: &str, args: &[&str]) -> Output {
        let mut command = Command::new("bash");
        command
            .arg(script_path())
            .args(args)
            .env("HERDR_BIN", bin)
            .env("HERDR_FLEET_LAB_ROOT", &self.root)
            .env("HERDR_FLEET_LAB_TIMEOUT_MS", STEP_TIMEOUT_MS)
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION");
        command.output().expect("run fleet-lab.sh")
    }

    /// Bring the lab up, then hand its runtime dir to the shared test watchdog
    /// so a leaked server is reaped even if this test panics.
    ///
    /// Registration has to happen *after* `up`: it creates the runtime dir with
    /// an owner marker, and `up` refuses (and later deletes) a root that does
    /// not carry the lab's own marker file.
    fn up(&mut self, count: &str) -> Output {
        let output = self.run(&["up", count]);
        if output.status.success() {
            support::register_runtime_dir(&self.runtime_dir());
            self.registered = true;
        }
        output
    }

    fn herdr(&self, session: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr"))
            .arg("--session")
            .arg(session)
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .output()
            .expect("run herdr against the lab")
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = self.run(&["down"]);
        if self.registered {
            support::unregister_runtime_dir(&self.runtime_dir());
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn status_json(lab: &Lab) -> serde_json::Value {
    let output = lab.run(&["status", "--json"]);
    assert!(
        output.status.success(),
        "status --json failed: {}",
        stderr_of(&output)
    );
    serde_json::from_str(&stdout_of(&output)).expect("status --json emits one JSON object")
}

#[test]
fn fleet_lab_boots_isolated_sessions_and_tears_them_down() {
    let mut lab = Lab::new("life");

    let up = lab.up("2");
    assert!(
        up.status.success(),
        "up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );

    let status = status_json(&lab);
    assert_eq!(
        status["root"].as_str(),
        Some(lab.root.to_string_lossy().as_ref()),
        "status reports the lab root"
    );
    let sessions = status["sessions"]
        .as_array()
        .expect("status --json carries a sessions array")
        .clone();
    assert_eq!(sessions.len(), 2, "status: {status}");

    for (index, session) in sessions.iter().enumerate() {
        let expected_name = format!("lab-{}", index + 1);
        assert_eq!(session["name"].as_str(), Some(expected_name.as_str()));
        assert_eq!(session["running"].as_bool(), Some(true), "{session}");
        assert!(session["pid"].as_u64().is_some(), "{session}");
        assert!(session["pane_id"].as_str().is_some(), "{session}");

        let client_socket = PathBuf::from(
            session["client_socket"]
                .as_str()
                .expect("client_socket path"),
        );
        assert!(
            client_socket.starts_with(&lab.root),
            "every socket stays inside the lab root: {}",
            client_socket.display()
        );
        support::wait_for_socket(&client_socket, SOCKET_TIMEOUT);
    }

    // The pane of lab-2 really runs the lab's marker process.
    let pane_id = sessions[1]["pane_id"]
        .as_str()
        .expect("lab-2 pane id")
        .to_string();
    let read = lab.herdr("lab-2", &["pane", "read", &pane_id, "--source", "recent"]);
    assert!(read.status.success(), "pane read: {}", stderr_of(&read));
    assert!(
        stdout_of(&read).contains("herdr-fleet-lab:lab-2"),
        "pane read did not show the lab marker: {}",
        stdout_of(&read)
    );

    // `env` is a published output shape: later epics eval it.
    let env_output = lab.run(&["env"]);
    assert!(env_output.status.success(), "{}", stderr_of(&env_output));
    let env_text = stdout_of(&env_output);
    assert!(
        env_text.contains(r#"export HERDR_FLEET_LAB_SESSIONS="lab-1 lab-2""#),
        "env output: {env_text}"
    );
    assert!(
        env_text.contains("export HERDR_FLEET_LAB_CLIENT_SOCKET_2="),
        "env output: {env_text}"
    );
    assert!(
        env_text.contains(&format!(
            r#"export XDG_CONFIG_HOME="{}/xdg""#,
            lab.root.display()
        )),
        "env output: {env_text}"
    );

    // A second `up` must refuse rather than start a second set of servers.
    let again = lab.run(&["up", "2"]);
    assert!(!again.status.success(), "second up should fail");
    assert!(
        stderr_of(&again).contains("already up"),
        "second up stderr: {}",
        stderr_of(&again)
    );

    let down = lab.run(&["down"]);
    assert!(down.status.success(), "down: {}", stderr_of(&down));
    assert!(!lab.root.exists(), "down removed the lab root");

    #[cfg(target_os = "linux")]
    {
        let leaked = support::herdr_server_pids_for_runtime_dir(&lab.runtime_dir())
            .expect("scan for leaked lab servers");
        assert!(leaked.is_empty(), "down left servers running: {leaked:?}");
    }

    // Tearing down a lab that is already gone is a no-op, not an error.
    let down_again = lab.run(&["down"]);
    assert!(
        down_again.status.success(),
        "second down: {}",
        stderr_of(&down_again)
    );
}

#[test]
fn fleet_lab_up_fails_without_leaving_a_root_when_the_binary_is_missing() {
    let lab = Lab::new("nobin");
    let missing = lab.root.join("nonexistent-herdr");

    let output = lab.run_with_bin(&missing.to_string_lossy(), &["up", "1"]);

    assert!(
        !output.status.success(),
        "up with a missing binary must fail"
    );
    assert!(
        !lab.root.exists(),
        "a failed up must not leave {} behind",
        lab.root.display()
    );
}

#[test]
fn fleet_lab_refuses_a_root_without_its_marker() {
    let lab = Lab::new("nomark");
    std::fs::create_dir_all(lab.root.join("decoy")).expect("create decoy root");

    let up = lab.run(&["up", "1"]);
    assert!(!up.status.success(), "up must refuse an unmarked root");
    assert!(
        stderr_of(&up).contains("marker"),
        "up stderr: {}",
        stderr_of(&up)
    );

    let down = lab.run(&["down"]);
    assert!(!down.status.success(), "down must refuse an unmarked root");
    assert!(
        lab.root.join("decoy").exists(),
        "down must not delete a directory it does not own"
    );
}
