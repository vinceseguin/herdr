#![cfg(unix)]
//! `scripts/fork/ssh-lab.sh` makes the fleet lab reachable over SSH.
//!
//! The script starts a user-space `sshd` — no root, no system service — whose
//! sessions run a wrapper around the fleet lab's own herdr binary. E1's SSH
//! transport work validates `herdr --remote` and `kind = "ssh"` fleet hosts
//! against it, so this test drives the real script against a real fleet lab.
//!
//! There are exactly two acceptable outcomes, and the test asserts one of them:
//! either the lab comes up and serves the lab's herdr end to end, or this
//! machine has no `sshd` and `up` exits 3 with `sshd not found`.

pub mod support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use support::fleet_lab::{stderr_of, stdout_of, Lab};

/// `ssh-lab.sh` reserves this exit code for "this machine has no sshd".
const EXIT_NO_SSHD: i32 = 3;
const STEP_TIMEOUT_MS: &str = "20000";
const SSHD_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/fork/ssh-lab.sh")
}

/// A port nothing else on this machine is listening on, so two concurrent
/// nextest processes (or a stray sshd) cannot collide on the default 2299.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("reserve a loopback port")
}

/// Runs `ssh-lab.sh` against one fleet lab root, and always tears the ssh lab
/// down — a panicking assertion must not leave an sshd listening.
struct SshLab {
    lab_root: PathBuf,
    port: u16,
}

impl SshLab {
    fn new(lab_root: PathBuf) -> Self {
        Self {
            lab_root,
            port: free_port(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new("bash")
            .arg(script_path())
            .args(args)
            .env("HERDR_BIN", env!("CARGO_BIN_EXE_herdr"))
            .env("HERDR_FLEET_LAB_ROOT", &self.lab_root)
            .env("HERDR_FLEET_LAB_TIMEOUT_MS", STEP_TIMEOUT_MS)
            .env("HERDR_SSH_LAB_PORT", self.port.to_string())
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .output()
            .expect("run ssh-lab.sh")
    }

    fn root(&self) -> PathBuf {
        self.lab_root.join("ssh")
    }

    /// `ssh` pinned to the lab's fake `HOME` and its own client config, so the
    /// caller's `~/.ssh` is never opened.
    fn ssh(&self, remote_command: &str) -> Output {
        let home = self.root().join("home");
        Command::new("ssh")
            .arg("-F")
            .arg(home.join(".ssh/config"))
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("herdr-ssh-lab")
            .arg(remote_command)
            .env("HOME", &home)
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .output()
            .expect("run ssh against the ssh lab")
    }
}

impl Drop for SshLab {
    fn drop(&mut self) {
        let _ = self.run(&["down"]);
    }
}

fn status_json(ssh_lab: &SshLab) -> serde_json::Value {
    let output = ssh_lab.run(&["status", "--json"]);
    assert!(
        output.status.success(),
        "status --json failed: {}",
        stderr_of(&output)
    );
    serde_json::from_str(&stdout_of(&output)).expect("status --json emits one JSON object")
}

#[cfg(target_os = "linux")]
fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !PathBuf::from(format!("/proc/{pid}")).exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(target_os = "linux"))]
fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn ssh_lab_reaches_the_debug_herdr_of_the_fleet_lab() {
    let mut lab = Lab::new("sshlab");
    let up = lab.up("1");
    assert!(
        up.status.success(),
        "fleet lab up 1 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );

    let ssh_lab = SshLab::new(lab.root.clone());
    let up = ssh_lab.run(&["up"]);

    // Outcome 2 of 2: no sshd on this machine. Assert the contract E1's SSH
    // validations degrade on, and say so out loud rather than passing silently.
    if up.status.code() == Some(EXIT_NO_SSHD) {
        assert!(
            stderr_of(&up).contains("sshd not found"),
            "exit {EXIT_NO_SSHD} must say why: {}",
            stderr_of(&up)
        );
        assert!(
            !ssh_lab.root().exists(),
            "a refused up must not leave {} behind",
            ssh_lab.root().display()
        );
        eprintln!(
            "ssh_lab_reaches_the_debug_herdr_of_the_fleet_lab: no sshd on this machine; \
             asserted the documented exit {EXIT_NO_SSHD} contract instead of the live lab"
        );
        return;
    }

    // Outcome 1 of 2: the lab is live end to end.
    assert!(
        up.status.success(),
        "ssh-lab.sh up failed (and did not report a missing sshd): {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );

    let status = status_json(&ssh_lab);
    assert_eq!(
        status["root"].as_str(),
        Some(ssh_lab.root().to_string_lossy().as_ref()),
        "status: {status}"
    );
    assert_eq!(status["running"].as_bool(), Some(true), "status: {status}");
    assert_eq!(
        status["port"].as_u64(),
        Some(u64::from(ssh_lab.port)),
        "status: {status}"
    );
    assert_eq!(
        status["target"].as_str(),
        Some("herdr-ssh-lab"),
        "status: {status}"
    );
    let sshd_pid = status["pid"]
        .as_u64()
        .expect("status --json reports the sshd pid") as u32;

    // `env` is a published output shape: E1's SSH validations eval it.
    let env_output = ssh_lab.run(&["env"]);
    assert!(env_output.status.success(), "{}", stderr_of(&env_output));
    let env_text = stdout_of(&env_output);
    for export in [
        format!(
            r#"export HERDR_SSH_LAB_ROOT="{}""#,
            ssh_lab.root().display()
        ),
        format!(
            r#"export HERDR_SSH_LAB_HOME="{}/home""#,
            ssh_lab.root().display()
        ),
        r#"export HERDR_SSH_LAB_TARGET="herdr-ssh-lab""#.to_string(),
        format!(r#"export HERDR_SSH_LAB_PORT="{}""#, ssh_lab.port),
        format!(
            r#"export HERDR_SSH_LAB_SSH_CONFIG="{}/home/.ssh/config""#,
            ssh_lab.root().display()
        ),
    ] {
        assert!(
            env_text.contains(&export),
            "env output is missing `{export}`: {env_text}"
        );
    }

    // The remote side must resolve herdr to the lab's wrapper, not to anything
    // the invoking user has installed.
    let which = ssh_lab.ssh("command -v herdr");
    assert!(
        which.status.success(),
        "remote `command -v herdr` failed: {}",
        stderr_of(&which)
    );
    assert_eq!(
        stdout_of(&which).trim(),
        ssh_lab
            .root()
            .join("home/.local/bin/herdr")
            .to_string_lossy(),
        "remote herdr must be the lab wrapper"
    );

    // …and that wrapper must reach the fleet lab's own session, running the
    // binary this test built.
    let remote_status = ssh_lab.ssh("herdr --session lab-1 status server --json");
    assert!(
        remote_status.status.success(),
        "remote `herdr status server --json` failed: {}",
        stderr_of(&remote_status)
    );
    let remote_status = stdout_of(&remote_status);
    assert!(
        remote_status.contains(&format!(r#""version":"{}""#, support::build_version())),
        "remote herdr is not the build under test: {remote_status}"
    );
    assert!(
        remote_status.contains(
            lab.root
                .join("xdg/herdr-dev/sessions/lab-1")
                .to_string_lossy()
                .as_ref()
        ),
        "remote herdr did not use the fleet lab's session dir: {remote_status}"
    );

    // A second `up` must refuse rather than start a second sshd.
    let again = ssh_lab.run(&["up"]);
    assert!(!again.status.success(), "second up should fail");
    assert!(
        stderr_of(&again).contains("already up"),
        "second up stderr: {}",
        stderr_of(&again)
    );

    let down = ssh_lab.run(&["down"]);
    assert!(down.status.success(), "down: {}", stderr_of(&down));
    assert!(
        !ssh_lab.root().exists(),
        "down removed the ssh lab root: {}",
        ssh_lab.root().display()
    );
    assert!(
        wait_for_pid_exit(sshd_pid, SSHD_EXIT_TIMEOUT),
        "down left sshd {sshd_pid} running"
    );

    // Tearing down an ssh lab that is already gone is a no-op, not an error.
    let down_again = ssh_lab.run(&["down"]);
    assert!(
        down_again.status.success(),
        "second down: {}",
        stderr_of(&down_again)
    );

    // The fleet lab underneath is untouched: `ssh-lab.sh down` deletes only its
    // own subdirectory.
    let fleet_status = lab.run(&["status", "--json"]);
    assert!(
        fleet_status.status.success(),
        "fleet status after ssh-lab down: {}",
        stderr_of(&fleet_status)
    );
    let fleet_status: serde_json::Value =
        serde_json::from_str(&stdout_of(&fleet_status)).expect("fleet status --json");
    let sessions = fleet_status["sessions"]
        .as_array()
        .expect("fleet status carries a sessions array");
    assert_eq!(sessions.len(), 1, "fleet status: {fleet_status}");
    assert_eq!(
        sessions[0]["running"].as_bool(),
        Some(true),
        "fleet status: {fleet_status}"
    );
}

#[test]
fn ssh_lab_refuses_without_the_fleet_lab() {
    let root = support::fleet_lab::unique_root("nofleet");
    std::fs::create_dir_all(&root).expect("create a root without a fleet lab marker");
    let ssh_lab = SshLab::new(root.clone());

    let up = ssh_lab.run(&["up"]);
    assert!(
        !up.status.success(),
        "up must refuse a root that is not a fleet lab"
    );
    assert!(
        stderr_of(&up).contains(".herdr-fleet-lab"),
        "up stderr: {}",
        stderr_of(&up)
    );
    assert!(
        !ssh_lab.root().exists(),
        "a refused up must not create {}",
        ssh_lab.root().display()
    );

    let _ = std::fs::remove_dir_all(&root);
}
