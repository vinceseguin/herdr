//! `herdr account` against a real running herdr server (fork).
//!
//! Every test drives `scripts/fork/accounts-lab.sh`: one isolated server, two
//! seeded Claude profile directories under a throwaway `XDG_CONFIG_HOME`, and
//! a fake `claude` first on `PATH`. The real Claude Code is never involved,
//! and `~/.claude`, `~/.claude.json` and `~/.config/herdr` are never touched.
//!
//! `unix` only: the lab script needs a POSIX shell and the server's unix
//! sockets. Later E9 PRs append their account tests here.
#![cfg(unix)]

pub mod support;

use support::accounts_lab::{stderr_of, stdout_of, Lab, DEFAULT_PROFILE, SECOND_PROFILE};

fn rows(output: &std::process::Output) -> Vec<serde_json::Value> {
    serde_json::from_str(&stdout_of(output)).expect("account list --json is JSON")
}

#[test]
fn account_list_reports_the_lab_profiles() {
    let mut lab = Lab::new("list");
    let up = lab.up();
    assert!(
        up.status.success(),
        "accounts-lab up failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );

    let output = lab.herdr(&["account", "list", "--json"]);
    assert!(
        output.status.success(),
        "account list failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).is_empty(),
        "a valid config must report no diagnostics: {}",
        stderr_of(&output)
    );

    let rows = rows(&output);
    assert_eq!(rows.len(), 2, "{rows:#?}");

    let perso = &rows[0];
    assert_eq!(perso["name"], DEFAULT_PROFILE);
    assert_eq!(perso["agent"], "claude");
    assert_eq!(perso["origin"], "config");
    assert_eq!(perso["default"], true);
    assert_eq!(perso["dir_exists"], true);
    assert_eq!(perso["logged_in"], true);
    assert_eq!(
        perso["hook_installed"], false,
        "the lab seeds no hook; `herdr account add` (PR 2) installs it"
    );
    assert_eq!(
        perso["config_dir"].as_str(),
        lab.profile_dir(DEFAULT_PROFILE).to_str()
    );

    let work = &rows[1];
    assert_eq!(work["name"], SECOND_PROFILE);
    assert_eq!(work["default"], false);
    assert_eq!(work["logged_in"], true);
    assert_eq!(
        work["config_dir"].as_str(),
        lab.profile_dir(SECOND_PROFILE).to_str()
    );
}

#[test]
fn account_list_never_prints_a_secret() {
    let mut lab = Lab::new("quiet");
    assert!(lab.up().status.success());

    let output = lab.herdr(&["account", "list", "--json"]);
    let printed = stdout_of(&output).to_ascii_lowercase();
    for forbidden in ["credential", "oauth", "token", "password", "secret"] {
        assert!(
            !printed.contains(forbidden),
            "{forbidden} leaked into `account list`: {printed}"
        );
    }

    // The seeded credentials file exists and stays private, and nothing herdr
    // ran opened it.
    let credentials = lab.profile_dir(DEFAULT_PROFILE).join(".credentials.json");
    assert!(credentials.is_file(), "the lab seeds a credentials file");
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&credentials)
        .expect("credentials metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the lab's credentials file must stay 0600");
}

#[test]
fn a_duplicate_profile_name_is_a_diagnostic_and_a_nonzero_exit() {
    let mut lab = Lab::new("dupes");
    assert!(lab.up().status.success());

    let config = lab.root.join("xdg").join("herdr-dev").join("config.toml");
    let original = std::fs::read_to_string(&config).expect("lab config");
    std::fs::write(
        &config,
        format!(
            "{original}\n[[accounts]]\nname = \"{DEFAULT_PROFILE}\"\nagent = \"claude\"\nconfig_dir = \"{}/profiles/dup\"\n",
            lab.root.display()
        ),
    )
    .expect("append a duplicate profile");

    let output = lab.herdr(&["account", "list", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "diagnostics must exit 1: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let diagnostics = stderr_of(&output);
    assert_eq!(
        diagnostics.lines().count(),
        1,
        "exactly one diagnostic: {diagnostics}"
    );
    assert!(
        diagnostics.contains("duplicate account profile name"),
        "{diagnostics}"
    );
    assert_eq!(
        rows(&output).len(),
        2,
        "the duplicate is dropped, the report still stands"
    );

    std::fs::write(&config, original).expect("restore the lab config");
}

#[test]
fn the_lab_puts_its_fake_claude_first_on_path() {
    let mut lab = Lab::new("stub");
    assert!(lab.up().status.success());

    let stub = lab.claude_stub();
    assert!(stub.is_file(), "the lab installs a fake claude at {stub:?}");

    let version = std::process::Command::new(&stub)
        .arg("--version")
        .env("CLAUDE_CONFIG_DIR", lab.profile_dir(SECOND_PROFILE))
        .output()
        .expect("run the stub");
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("fake-claude"),
        "{version:?}"
    );

    // The pane the lab left at a shell prompt is what PR 4 launches into.
    let pane = lab.pane_id();
    assert!(!pane.is_empty(), "the lab reports a pane id");
}
