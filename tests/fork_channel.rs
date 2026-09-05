//! Fork build identity: the binary reports channel `fork` and refuses to
//! replace itself from upstream's release manifest.
//!
//! These run the real binary, so they cover the wiring in `src/main.rs`,
//! `src/update.rs` and `src/build_info.rs` — not just the pure guards.

pub mod support;

use std::path::PathBuf;
use std::process::Command;

const FORK_REFUSAL: &str = "self-update is disabled for fork builds; see docs/fork/README.md";

/// A throwaway `XDG_CONFIG_HOME` so nothing here can read or write the
/// developer's real herdr config.
struct IsolatedConfigHome(PathBuf);

impl IsolatedConfigHome {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("herdr-fork-channel-{}-{name}", std::process::id()));
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
            .env_remove("HERDR_FAKE_UPDATE_VERSION")
            .env("XDG_CONFIG_HOME", &self.0);
        command
    }
}

impl Drop for IsolatedConfigHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn is_fork_build() -> bool {
    option_env!("HERDR_BUILD_CHANNEL").map(str::trim) == Some("fork")
}

#[test]
fn version_flag_reports_the_build_channel() {
    let home = IsolatedConfigHome::new("version");
    let output = home
        .herdr()
        .arg("--version")
        .output()
        .expect("run `herdr --version`");
    assert!(output.status.success(), "status: {:?}", output.status);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        format!("herdr {}", support::build_version()),
        "stdout: {stdout}"
    );
}

#[test]
fn fork_builds_report_a_fork_version() {
    if !is_fork_build() {
        // Someone compiled this checkout with an explicit non-fork channel.
        return;
    }

    assert!(
        support::build_version().starts_with(&format!("{}-fork", env!("CARGO_PKG_VERSION"))),
        "version: {}",
        support::build_version()
    );
}

#[test]
fn update_refuses_on_a_fork_build() {
    if !is_fork_build() {
        return;
    }

    // `HERDR_ENV=1` is a backstop, not the behaviour under test: `herdr update`
    // refuses inside a herdr session too, so if the fork guard ever regressed
    // this test would still never reach the network or install anything — it
    // would just fail on the message below.
    let home = IsolatedConfigHome::new("update");
    let output = home
        .herdr()
        .arg("update")
        .env("HERDR_ENV", "1")
        .output()
        .expect("run `herdr update`");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.starts_with(FORK_REFUSAL),
        "`herdr update` must refuse with the fork message first; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("checking"),
        "fork build must not start an update check; stderr: {stderr}"
    );
}

#[test]
fn help_marks_the_disabled_update_on_fork_builds() {
    let home = IsolatedConfigHome::new("help");
    let output = home
        .herdr()
        .arg("--help")
        .output()
        .expect("run `herdr --help`");
    assert!(output.status.success(), "status: {:?}", output.status);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.contains("fork build; self-update disabled"),
        is_fork_build(),
        "stdout: {stdout}"
    );
}
