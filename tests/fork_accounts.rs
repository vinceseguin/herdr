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

/// The stub is the only thing E9 ever runs in place of Claude Code, so its
/// refusal to touch a real installation is a tested property, not a comment.
#[test]
fn the_fake_claude_never_touches_a_real_claude_directory() {
    use std::process::Stdio;

    let mut lab = Lab::new("guard");
    assert!(lab.up().status.success());
    let stub = lab.claude_stub();

    // A home that looks real, and a CLAUDE_CONFIG_DIR pointing into it.
    let home = lab.root.join("fake-home");
    let claude = home.join(".claude");
    std::fs::create_dir_all(&home).expect("fake home");

    for (label, dir) in [("~/.claude", claude.clone()), ("$HOME", home.clone())] {
        let output = std::process::Command::new(&stub)
            .arg("auth")
            .arg("login")
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &dir)
            .stdin(Stdio::null())
            .output()
            .expect("run the stub");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{label} must be refused: {}{}",
            stdout_of(&output),
            stderr_of(&output)
        );
        assert!(
            stderr_of(&output).contains("refusing to run against the real"),
            "{label}: {}",
            stderr_of(&output)
        );
    }
    assert!(
        !claude.exists(),
        "the stub must not create the directory it refused"
    );

    // Without a directory it guesses none: `auth login` refuses, and an
    // ordinary launch writes nothing anywhere.
    let login = std::process::Command::new(&stub)
        .arg("auth")
        .arg("login")
        .env("HOME", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null())
        .output()
        .expect("run the stub");
    assert_eq!(login.status.code(), Some(3), "{}", stderr_of(&login));

    let launch = std::process::Command::new(&stub)
        .env("HOME", &home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("HERDR_PANE_ID")
        .stdin(Stdio::null())
        .output()
        .expect("run the stub");
    assert!(launch.status.success(), "{}", stderr_of(&launch));
    assert!(
        stdout_of(&launch).contains("no profile state written"),
        "{}",
        stdout_of(&launch)
    );
    assert!(!claude.exists(), "still nothing under the fake home");
}

/// Everything the lab starts writes inside the lab root, including a launch
/// that applied no profile at all.
#[test]
fn the_lab_pins_the_ambient_claude_directory_inside_its_root() {
    let mut lab = Lab::new("ambient");
    assert!(lab.up().status.success());
    assert!(
        lab.ambient_dir().is_dir(),
        "the lab seeds {:?}",
        lab.ambient_dir()
    );
    assert!(
        !lab.ambient_dir().join(".credentials.json").exists(),
        "the ambient directory stays logged out"
    );

    let env = lab.run(&["env"]);
    assert!(env.status.success(), "{}", stderr_of(&env));
    let exports = stdout_of(&env);
    assert!(
        exports.contains("HERDR_ACCOUNTS_LAB_PROFILE_AMBIENT="),
        "{exports}"
    );
    assert!(
        !exports.contains("XDG_RUNTIME_DIR="),
        "eval-ing XDG_RUNTIME_DIR would hijack the caller's session: {exports}"
    );
}

/// A section herdr threw away must say so. Reporting "No account profiles
/// configured" for a config that plainly declares some would send someone
/// looking for the mistake in the wrong file.
#[test]
fn a_section_of_the_wrong_shape_is_reported_not_silently_empty() {
    let mut lab = Lab::new("shape");
    assert!(lab.up().status.success());

    let config = lab.root.join("xdg").join("herdr-dev").join("config.toml");
    let original = std::fs::read_to_string(&config).expect("lab config");

    for (label, body, expected) in [
        (
            "reserved table",
            "onboarding = false\n\n[accounts.defaults]\nworkspace = \"x\"\n",
            "[accounts.defaults] is reserved",
        ),
        (
            "scalar",
            "onboarding = false\naccounts = 3\n",
            "accounts must be an array of tables",
        ),
    ] {
        std::fs::write(&config, body).expect("write the lab config");
        let output = lab.herdr(&["account", "list", "--json"]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{label}: {}{}",
            stdout_of(&output),
            stderr_of(&output)
        );
        assert!(
            stderr_of(&output).contains(expected),
            "{label}: {}",
            stderr_of(&output)
        );
        assert!(rows(&output).is_empty(), "{label}: {}", stdout_of(&output));
    }

    std::fs::write(&config, original).expect("restore the lab config");
}

/// The credential hazard of the whole epic, asserted end to end: a new profile
/// is seeded from an existing one, shares its transcripts, carries its
/// settings *without* the source's identity, and is logged out.
#[test]
fn account_add_seeds_shared_transcripts_and_private_identity() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut lab = Lab::new("add");
    assert!(lab.up().status.success());

    let source = lab.profile_dir(DEFAULT_PROFILE);
    let credentials = source.join(".credentials.json");
    let before = std::fs::metadata(&credentials)
        .expect("source credentials")
        .modified()
        .expect("mtime");
    let target = lab.root.join("profiles").join("third");

    let dry = lab.herdr(&[
        "account",
        "add",
        "third",
        "--config-dir",
        target.to_str().expect("utf-8 path"),
        "--dry-run",
    ]);
    assert!(
        dry.status.success(),
        "dry run failed: {}{}",
        stdout_of(&dry),
        stderr_of(&dry)
    );
    let plan = stdout_of(&dry);
    assert!(plan.contains("would create"), "{plan}");
    assert!(plan.contains("link  projects"), "{plan}");
    assert!(
        plan.contains(".credentials.json: private to each account"),
        "{plan}"
    );
    assert!(!target.exists(), "--dry-run must write nothing");

    let added = lab.herdr(&[
        "account",
        "add",
        "third",
        "--config-dir",
        target.to_str().expect("utf-8 path"),
        "--json",
    ]);
    assert!(
        added.status.success(),
        "add failed: {}{}",
        stdout_of(&added),
        stderr_of(&added)
    );
    let outcome: serde_json::Value =
        serde_json::from_str(&stdout_of(&added)).expect("account add --json is JSON");
    assert_eq!(outcome["profile"]["name"], "third");
    assert_eq!(outcome["profile"]["logged_in"], false);
    assert_eq!(outcome["profile"]["hook_installed"], true);
    assert_eq!(outcome["stored"], true);
    assert_eq!(
        outcome["seeded_from"].as_str(),
        source.to_str(),
        "the default profile is the seed source"
    );

    // Nothing that could be a credential is printed.
    let printed = format!("{}{}", stdout_of(&added), stderr_of(&added)).to_ascii_lowercase();
    for forbidden in ["oauth", "credentials.json", "accesstoken", "secret"] {
        assert!(
            !printed.contains(forbidden),
            "{forbidden} was printed: {printed}"
        );
    }

    // Directory: 0700, transcripts shared, identity scrubbed, logged out.
    assert_eq!(
        std::fs::metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let projects = target.join("projects");
    assert!(
        std::fs::symlink_metadata(&projects)
            .expect("projects")
            .file_type()
            .is_symlink(),
        "transcripts are shared by symlink"
    );
    assert_eq!(
        std::fs::canonicalize(&projects).expect("canonical"),
        std::fs::canonicalize(source.join("projects")).expect("canonical source"),
    );

    let seeded: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(target.join(".claude.json")).expect("seeded .claude.json"),
    )
    .expect("json");
    assert_eq!(
        seeded.get("oauthAccount"),
        None,
        "the source account's identity must not follow the seed"
    );
    assert_eq!(seeded["hasCompletedOnboarding"], serde_json::json!(true));

    assert!(
        !target.join(".credentials.json").exists(),
        "a seeded profile is logged out until `herdr account login`"
    );
    assert_eq!(
        std::fs::metadata(&credentials)
            .expect("source credentials")
            .modified()
            .expect("mtime"),
        before,
        "the source profile's credentials file was touched"
    );

    // The hook landed in the new profile, not the ambient directory.
    assert!(target.join("hooks").join("herdr-agent-state.sh").is_file());
    let settings = std::fs::read_to_string(target.join("settings.json")).expect("settings.json");
    assert!(settings.contains("SessionStart"), "{settings}");
    assert!(
        !lab.ambient_dir().join("hooks").exists(),
        "the hook must not land in the ambient directory"
    );

    // And it is a real profile now.
    let listed = lab.herdr(&["account", "list", "--json"]);
    let rows = rows(&listed);
    let third = rows
        .iter()
        .find(|row| row["name"] == "third")
        .expect("third is listed");
    assert_eq!(third["origin"], "store");
    assert_eq!(third["logged_in"], false);
    assert_eq!(third["default"], false);

    // A second add of the same name changes nothing.
    let again = lab.herdr(&[
        "account",
        "add",
        "third",
        "--config-dir",
        target.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(again.status.code(), Some(1), "{}", stdout_of(&again));
    assert!(
        stderr_of(&again).contains("already exists"),
        "{}",
        stderr_of(&again)
    );
}

#[test]
fn account_default_prefers_store_and_remove_refuses_config_profiles() {
    let mut lab = Lab::new("default");
    assert!(lab.up().status.success());

    let target = lab.root.join("profiles").join("third");
    let added = lab.herdr(&[
        "account",
        "add",
        "third",
        "--config-dir",
        target.to_str().expect("utf-8 path"),
        "--no-hook",
    ]);
    assert!(
        added.status.success(),
        "add failed: {}{}",
        stdout_of(&added),
        stderr_of(&added)
    );

    // The config marks `perso` default; the store's choice beats the flag.
    let chosen = lab.herdr(&["account", "default", "third"]);
    assert!(
        chosen.status.success(),
        "default failed: {}{}",
        stdout_of(&chosen),
        stderr_of(&chosen)
    );
    let listed = rows(&lab.herdr(&["account", "list", "--json"]));
    let defaults: Vec<&str> = listed
        .iter()
        .filter(|row| row["default"] == true)
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(defaults, vec!["third"], "{listed:#?}");

    let unknown = lab.herdr(&["account", "default", "nope"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(stderr_of(&unknown).contains("unknown account profile"));

    // A [[accounts]] profile is the user's to remove, not herdr's.
    let refused = lab.herdr(&["account", "remove", DEFAULT_PROFILE]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}{}",
        stdout_of(&refused),
        stderr_of(&refused)
    );
    assert!(
        stderr_of(&refused).contains("config.toml"),
        "{}",
        stderr_of(&refused)
    );
    assert!(
        lab.profile_dir(DEFAULT_PROFILE).is_dir(),
        "a refused remove must not touch the directory"
    );

    // The store profile can go, directory and all.
    let removed = lab.herdr(&["account", "remove", "third", "--delete-dir"]);
    assert!(
        removed.status.success(),
        "remove failed: {}{}",
        stdout_of(&removed),
        stderr_of(&removed)
    );
    assert!(!target.exists(), "--delete-dir removes the directory");

    // The stored default named the profile that just left, so the config
    // default is in charge again — and nothing reports an unknown default.
    let output = lab.herdr(&["account", "list", "--json"]);
    assert!(stderr_of(&output).is_empty(), "{}", stderr_of(&output));
    let remaining = rows(&output);
    assert_eq!(remaining.len(), 2, "{remaining:#?}");
    assert_eq!(remaining[0]["name"], DEFAULT_PROFILE);
    assert_eq!(remaining[0]["default"], true);
}

/// `--delete-dir` deletes credentials, so every way the directory could be
/// somebody else's is refused. The ambient Claude directory is the one a user
/// can realistically point a profile at by mistake.
#[test]
fn account_remove_refuses_to_delete_the_ambient_directory() {
    let mut lab = Lab::new("ambient-rm");
    assert!(lab.up().status.success());

    let ambient = lab.ambient_dir();
    let added = lab.herdr(&[
        "account",
        "add",
        "amb",
        "--config-dir",
        ambient.to_str().expect("utf-8 path"),
        "--no-hook",
    ]);
    assert!(
        added.status.success(),
        "add failed: {}{}",
        stdout_of(&added),
        stderr_of(&added)
    );

    let removed = lab.herdr(&["account", "remove", "amb", "--delete-dir"]);
    assert_eq!(
        removed.status.code(),
        Some(1),
        "{}{}",
        stdout_of(&removed),
        stderr_of(&removed)
    );
    assert!(
        stderr_of(&removed).contains("ambient Claude directory"),
        "{}",
        stderr_of(&removed)
    );
    assert!(ambient.is_dir(), "a refused delete leaves the directory");

    // Without --delete-dir the profile still goes away, directory intact.
    let removed = lab.herdr(&["account", "remove", "amb"]);
    assert!(
        removed.status.success(),
        "{}{}",
        stdout_of(&removed),
        stderr_of(&removed)
    );
    assert!(ambient.is_dir());
    assert_eq!(rows(&lab.herdr(&["account", "list", "--json"])).len(), 2);
}
