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
    let version = support::build_version();
    if is_fork_build() {
        assert!(
            version.starts_with(&format!("{}-fork", env!("CARGO_PKG_VERSION"))),
            "version: {version}"
        );
    } else {
        // Someone compiled this checkout with an explicit non-fork channel.
        assert!(!version.contains("-fork"), "version: {version}");
    }
}

#[test]
fn update_refuses_on_a_fork_build() {
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
    if is_fork_build() {
        assert!(
            stderr.starts_with(FORK_REFUSAL),
            "`herdr update` must refuse with the fork message first; stderr: {stderr}"
        );
    } else {
        assert!(
            !stderr.contains("fork builds"),
            "non-fork build must not print the fork refusal; stderr: {stderr}"
        );
    }
    assert!(
        !stderr.contains("checking"),
        "no update check may start; stderr: {stderr}"
    );
}

/// `herdr channel set` writes the config and then, on a stock build, hands
/// off to `self_update`. A fork build must print the fork guidance instead and
/// exit 0 without ever reaching the update machinery.
fn assert_channel_set_never_self_updates(channel: &str) {
    let home = IsolatedConfigHome::new(&format!("channel-set-{channel}"));
    let config_path = home.0.join("config.toml");
    let output = home
        .herdr()
        .args(["channel", "set", channel])
        // Backstop only, as in `update_refuses_on_a_fork_build`.
        .env("HERDR_ENV", "1")
        .env("HERDR_CONFIG_PATH", &config_path)
        .output()
        .expect("run `herdr channel set`");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let config = std::fs::read_to_string(&config_path).expect("channel set writes the config");
    assert!(
        config.contains(&format!("channel = \"{channel}\"")),
        "config: {config}"
    );
    assert!(
        stdout.contains(&format!("Herdr update channel set to {channel}")),
        "stdout: {stdout}"
    );
    assert!(
        !stderr.contains("checking") && !stderr.contains("downloading"),
        "no update check may start; stderr: {stderr}"
    );

    if is_fork_build() {
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout: {stdout}\nstderr: {stderr}"
        );
        assert!(
            stdout.contains(FORK_REFUSAL),
            "fork build must print the fork guidance; stdout: {stdout}"
        );
        assert!(
            !stderr.contains("update failed"),
            "fork build must not reach self_update; stderr: {stderr}"
        );
    } else {
        // A stock build hands off to `self_update`, which the `HERDR_ENV`
        // backstop refuses before any network access.
        assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
        assert!(
            !stdout.contains("fork builds") && !stderr.contains("fork builds"),
            "non-fork build must not print the fork guidance; stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn channel_set_stable_writes_config_without_self_updating_on_a_fork_build() {
    assert_channel_set_never_self_updates("stable");
}

#[test]
fn channel_set_preview_writes_config_without_self_updating_on_a_fork_build() {
    assert_channel_set_never_self_updates("preview");
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
