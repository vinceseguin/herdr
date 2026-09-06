//! `herdr gateway` end-to-end CLI wiring, exercised through the real binary.
//!
//! The whole file is gated on the `gateway` feature so `cargo nextest run
//! --no-default-features` (fork CI's `check-no-default-features` job) compiles
//! it to an empty test binary instead of failing on a missing subcommand.
//! Later E3 PRs append their gateway tests here.
#![cfg(all(unix, not(target_os = "macos"), feature = "gateway"))]

use std::path::PathBuf;
use std::process::Command;

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

/// The bare command reaches `run_gateway_command` rather than main's
/// `unknown command` guard, which is the wiring this PR adds.
#[test]
fn gateway_without_a_subcommand_is_a_usage_error() {
    let home = IsolatedConfigHome::new("usage");
    let output = home
        .herdr()
        .arg("gateway")
        .output()
        .expect("run `herdr gateway`");

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
