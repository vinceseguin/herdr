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

    // A different name pointing at an existing profile's directory would be
    // two accounts on one login. The spelling must not decide it: `..` and a
    // symlinked ancestor are the same directory.
    let sneaky = lab
        .root
        .join("profiles")
        .join("third")
        .join("..")
        .join(SECOND_PROFILE);
    let refused = lab.herdr(&[
        "account",
        "add",
        "sneaky",
        "--config-dir",
        sneaky.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}{}",
        stdout_of(&refused),
        stderr_of(&refused)
    );
    assert!(
        stderr_of(&refused).contains("must not share a directory"),
        "{}",
        stderr_of(&refused)
    );

    std::os::unix::fs::symlink(lab.root.join("profiles"), lab.root.join("linked-profiles"))
        .expect("symlink the profiles directory");
    let linked = lab.root.join("linked-profiles").join(SECOND_PROFILE);
    let refused = lab.herdr(&[
        "account",
        "add",
        "linked",
        "--config-dir",
        linked.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(
        refused.status.code(),
        Some(1),
        "{}{}",
        stdout_of(&refused),
        stderr_of(&refused)
    );
    assert!(
        stderr_of(&refused).contains("must not share a directory"),
        "{}",
        stderr_of(&refused)
    );
    let after_refusals = stdout_of(&lab.herdr(&["account", "list", "--json"]));
    let listed_after: Vec<serde_json::Value> =
        serde_json::from_str(&after_refusals).expect("account list --json is JSON");
    assert_eq!(
        listed_after.len(),
        3,
        "a refused add records nothing: {after_refusals}"
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

    // Seeded from the default profile, so its transcripts are that profile's.
    let shared = lab.profile_dir(DEFAULT_PROFILE).join("projects");
    std::fs::write(shared.join("a.jsonl"), "{}").expect("transcript");
    assert!(target.join("projects").join("a.jsonl").is_file());

    // The store profile can go, directory and all.
    let removed = lab.herdr(&["account", "remove", "third", "--delete-dir"]);
    assert!(
        removed.status.success(),
        "remove failed: {}{}",
        stdout_of(&removed),
        stderr_of(&removed)
    );
    assert!(!target.exists(), "--delete-dir removes the directory");

    // Deleting a profile deletes its own directory and nothing through its
    // symlinks: the transcripts and the login it shared are the other
    // profile's.
    assert!(
        shared.join("a.jsonl").is_file(),
        "--delete-dir followed a shared symlink"
    );
    assert!(lab
        .profile_dir(DEFAULT_PROFILE)
        .join(".credentials.json")
        .is_file());

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

// ---------------------------------------------------------------------------
// PR 4 — `herdr agent start --account`
//
// The launch is a two-step: the client types `export CLAUDE_CONFIG_DIR=…` into
// the pane's shell, then runs the stock `agent.start`. These tests assert the
// end of that chain — what the *launched process* actually got — rather than
// what herdr says it sent, because "reported ok without evidence" is the one
// failure mode that would silently bill the wrong Claude account.
// ---------------------------------------------------------------------------

fn json_of(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_str(&stdout_of(output)).unwrap_or_else(|err| {
        panic!(
            "expected JSON: {err}\nstdout: {}\nstderr: {}",
            stdout_of(output),
            stderr_of(output)
        )
    })
}

/// What the fake `claude` recorded about its own launch.
fn last_launch(lab: &Lab, profile: &str) -> serde_json::Value {
    let path = lab.profile_dir(profile).join("last-launch.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("no last-launch.json at {path:?}: {err}"));
    serde_json::from_str(&text).expect("last-launch.json is JSON")
}

fn start_agent(lab: &Lab, name: &str, extra: &[&str]) -> std::process::Output {
    let pane = lab.pane_id();
    let mut args = vec!["agent", "start", name, "--kind", "claude", "--pane"];
    args.push(&pane);
    args.extend_from_slice(extra);
    lab.herdr(&args)
}

#[test]
fn agent_start_with_account_exports_the_profile_dir() {
    let mut lab = Lab::new("start-account");
    assert!(lab.up().status.success());

    let output = start_agent(&lab, "a1", &["--account", SECOND_PROFILE]);
    assert!(
        output.status.success(),
        "agent start failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );

    let response = json_of(&output);
    assert_eq!(response["result"]["account"], SECOND_PROFILE);
    assert_eq!(
        response["result"]["account_state"], "ok",
        "on Linux the launched process's environment is readable, so the state \
         must be evidence-backed: {response:#?}"
    );
    assert_eq!(response["result"]["agent"]["name"], "a1");

    // The launched process itself, not herdr's account of it.
    let launch = last_launch(&lab, SECOND_PROFILE);
    assert_eq!(
        launch["config_dir"].as_str(),
        lab.profile_dir(SECOND_PROFILE).to_str()
    );
    assert!(
        !lab.profile_dir(DEFAULT_PROFILE)
            .join("last-launch.json")
            .exists(),
        "the default profile must not have been used"
    );

    // And the token every other surface reads.
    let agent = json_of(&lab.herdr(&["agent", "get", "a1"]));
    assert_eq!(
        agent["result"]["agent"]["tokens"]["account"],
        SECOND_PROFILE
    );
    assert_eq!(agent["result"]["agent"]["tokens"]["account_state"], "ok");
    assert!(
        agent["result"]["agent"]["agent_session"]["value"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "the stub reported its session id like the real hook: {agent:#?}"
    );
}

#[test]
fn agent_start_uses_the_default_profile() {
    let mut lab = Lab::new("start-default");
    assert!(lab.up().status.success());

    let output = start_agent(&lab, "a1", &[]);
    assert!(
        output.status.success(),
        "agent start failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let response = json_of(&output);
    assert_eq!(response["result"]["account"], DEFAULT_PROFILE);
    assert_eq!(response["result"]["account_state"], "ok");
    assert_eq!(
        last_launch(&lab, DEFAULT_PROFILE)["config_dir"].as_str(),
        lab.profile_dir(DEFAULT_PROFILE).to_str()
    );
}

#[test]
fn agent_start_account_none_is_the_stock_launch() {
    let mut lab = Lab::new("start-none");
    assert!(lab.up().status.success());

    let output = start_agent(&lab, "a1", &["--account", "none"]);
    assert!(
        output.status.success(),
        "agent start failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let response = json_of(&output);
    assert!(
        response["result"]["account"].is_null(),
        "--account none must add no keys: {response:#?}"
    );
    assert!(
        response["result"]["account_state"].is_null(),
        "{response:#?}"
    );

    for profile in [DEFAULT_PROFILE, SECOND_PROFILE] {
        assert!(
            !lab.profile_dir(profile).join("last-launch.json").exists(),
            "{profile} must not have been launched into"
        );
    }
    // The lab pins the ambient directory inside its own root, so a launch that
    // applied no profile lands there and never near a real ~/.claude.
    assert!(
        lab.ambient_dir().join("last-launch.json").is_file(),
        "the stock launch inherits the pane's own CLAUDE_CONFIG_DIR"
    );

    let agent = json_of(&lab.herdr(&["agent", "get", "a1"]));
    assert!(
        agent["result"]["agent"]["tokens"]["account"].is_null(),
        "no profile was applied, so no account may be claimed: {agent:#?}"
    );
}

#[test]
fn agent_start_refuses_an_unknown_account_without_touching_the_pane() {
    let mut lab = Lab::new("start-unknown");
    assert!(lab.up().status.success());

    let output = start_agent(&lab, "a1", &["--account", "nosuch"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("unknown account profile"),
        "{}",
        stderr_of(&output)
    );

    let pane = lab.pane_id();
    let screen = stdout_of(&lab.herdr(&["pane", "read", &pane, "--source", "recent"]));
    assert!(
        !screen.contains("CLAUDE_CONFIG_DIR"),
        "nothing may be typed for a profile that does not exist: {screen}"
    );
}

/// A pane with something else in the foreground must be refused *before* a
/// byte is typed: an `export` line sent there would land in that program, and
/// the agent would then start under whatever account the shell already had.
#[test]
fn agent_start_refuses_a_busy_pane_before_typing_anything() {
    let mut lab = Lab::new("start-busy");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    let busy = lab.herdr(&["pane", "send-text", &pane, "sleep 30\r"]);
    assert!(busy.status.success(), "{}", stderr_of(&busy));

    // Wait for `sleep` to actually take the foreground.
    let mut taken = false;
    for _ in 0..40 {
        let info = json_of(&lab.herdr(&["pane", "process-info", "--pane", &pane]));
        let group = info["result"]["process_info"]["foreground_process_group_id"].as_u64();
        let shell = info["result"]["process_info"]["shell_pid"].as_u64();
        if group.is_some() && group != shell {
            taken = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(taken, "the pane never became busy");

    let output = start_agent(&lab, "a1", &["--account", SECOND_PROFILE]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("not at its shell prompt"),
        "{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("nothing was typed"),
        "{}",
        stderr_of(&output)
    );
    assert!(
        !lab.profile_dir(SECOND_PROFILE)
            .join("last-launch.json")
            .exists(),
        "nothing may have been launched"
    );
}

/// The contract that makes the whole epic trustworthy: when the environment
/// line does *not* reach the launched agent, herdr says so and fails, instead
/// of reporting the requested account and billing another one.
///
/// The pane is put at a `read` builtin, which leaves the shell itself in the
/// foreground — so the pane still looks idle to every gate herdr has — but
/// makes it swallow the next line typed at it. `claude` then starts without
/// the profile, under whatever directory the pane already had.
#[test]
fn agent_start_reports_a_mismatch_when_the_shell_swallows_the_environment_line() {
    let mut lab = Lab::new("swallow");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    // `printf` and `read` are both builtins, so nothing but the shell is ever
    // in the pane's foreground; the marker tells us `read` is now running.
    // The markers are assembled by `printf` so the shell's echo of the typed
    // line cannot be mistaken for its output: the screen only ever shows
    // `SWALLOWREADY` or `ATEIT=` once the command has actually run.
    let sent = lab.herdr(&[
        "pane",
        "send-text",
        &pane,
        "printf 'SWALLOW%s\\n' READY; read swallowed; printf 'ATE%s=%s\\n' IT \"$swallowed\"\r",
    ]);
    assert!(sent.status.success(), "{}", stderr_of(&sent));

    let mut ready = false;
    for _ in 0..80 {
        let screen = stdout_of(&lab.herdr(&["pane", "read", &pane, "--source", "recent"]));
        if screen.contains("SWALLOWREADY") {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "the pane never reached the read builtin");

    let output = start_agent(&lab, "a1", &["--account", SECOND_PROFILE]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a launch that missed its profile must fail: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let response = json_of(&output);
    assert_eq!(
        response["result"]["account_state"], "mismatch",
        "{response:#?}"
    );
    assert_eq!(response["result"]["account"], SECOND_PROFILE);
    assert!(
        stderr_of(&output).contains("is not running under account"),
        "{}",
        stderr_of(&output)
    );

    // The shell really did eat the line, and the agent really did run
    // somewhere else — the lab's ambient directory, not the profile.
    let screen = stdout_of(&lab.herdr(&["pane", "read", &pane, "--source", "recent"]));
    assert!(
        screen.contains("ATEIT="),
        "the read builtin should have consumed the export line: {screen}"
    );
    assert!(
        !lab.profile_dir(SECOND_PROFILE)
            .join("last-launch.json")
            .exists(),
        "the profile must not have been launched into"
    );
    assert!(
        lab.ambient_dir().join("last-launch.json").is_file(),
        "the agent ran under the pane's ambient directory"
    );

    // And the mismatch is recorded where every other surface reads it.
    let agent = json_of(&lab.herdr(&["agent", "get", "a1"]));
    assert_eq!(
        agent["result"]["agent"]["tokens"]["account_state"], "mismatch",
        "{agent:#?}"
    );
}

// ---------------------------------------------------------------------------
// PR 3 — `herdr account status` and `herdr account login`
//
// `status` is a read: it must show who a profile is logged in as and which
// agents claim it, without ever opening the credentials file, and it must
// still answer when no server is running. `login` types two lines into a pane
// — the profile's export and `claude auth login` — so the credentials land in
// that profile's directory instead of the ambient one.
// ---------------------------------------------------------------------------

/// A logged-out profile, registered in the store so `status` and `login` can
/// address it by name. The lab's own two profiles both ship logged in, so a
/// login that really flips `logged_in` needs a fresh one.
fn add_logged_out_profile(lab: &Lab, name: &str) -> std::path::PathBuf {
    let dir = lab.root.join("profiles").join(name);
    let added = lab.herdr(&[
        "account",
        "add",
        name,
        "--config-dir",
        dir.to_str().expect("utf-8 lab path"),
        "--no-hook",
    ]);
    assert!(
        added.status.success(),
        "account add {name} failed: {}{}",
        stdout_of(&added),
        stderr_of(&added)
    );
    assert!(
        !dir.join(".credentials.json").exists(),
        "a seeded profile is logged out"
    );
    dir
}

/// Poll a pane until its screen contains `needle`.
fn wait_for_pane_text(lab: &Lab, pane: &str, needle: &str) -> String {
    let mut screen = String::new();
    for _ in 0..100 {
        screen = stdout_of(&lab.herdr(&["pane", "read", pane, "--source", "recent"]));
        if screen.contains(needle) {
            return screen;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("pane {pane} never showed {needle:?}: {screen}");
}

#[test]
fn account_status_reports_identity_without_reading_a_secret() {
    let mut lab = Lab::new("status");
    assert!(lab.up().status.success());

    let output = lab.herdr(&["account", "status", "--json"]);
    assert!(
        output.status.success(),
        "account status failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let statuses = rows(&output);
    assert_eq!(statuses.len(), 2, "{statuses:#?}");

    let perso = &statuses[0];
    assert_eq!(perso["name"], DEFAULT_PROFILE);
    assert_eq!(perso["default"], true);
    assert_eq!(perso["origin"], "config");
    assert_eq!(perso["dir_exists"], true);
    assert_eq!(perso["logged_in"], true);
    assert_eq!(
        perso["credentials_mode_ok"], true,
        "the lab writes its credentials 0600: {perso:#?}"
    );
    // Identity is display-only, out of `.claude.json`'s oauthAccount.
    assert_eq!(perso["identity"]["email"], "perso@example.test");
    assert_eq!(perso["identity"]["organization"], "Example Org");
    assert_eq!(perso["identity"]["plan"], "max");
    assert_eq!(
        perso["agents"],
        serde_json::json!([]),
        "the lab starts no agent: {perso:#?}"
    );

    let work = &statuses[1];
    assert_eq!(work["name"], SECOND_PROFILE);
    assert_eq!(work["default"], false);
    assert_eq!(work["identity"]["email"], "work@example.test");

    // The credentials file itself is never opened, printed, or named.
    let printed = stdout_of(&output).to_ascii_lowercase();
    for forbidden in ["credentials.json", "claudeaioauth", "password", "secret"] {
        assert!(
            !printed.contains(forbidden),
            "{forbidden} leaked into `account status`: {printed}"
        );
    }

    // A named report is the same row on its own; an unknown name is an error,
    // never an empty report.
    let named = lab.herdr(&["account", "status", SECOND_PROFILE, "--json"]);
    assert!(named.status.success(), "{}", stderr_of(&named));
    let named = rows(&named);
    assert_eq!(named.len(), 1);
    assert_eq!(named[0]["name"], SECOND_PROFILE);

    let unknown = lab.herdr(&["account", "status", "nope", "--json"]);
    assert_eq!(unknown.status.code(), Some(1), "{}", stdout_of(&unknown));
    assert!(
        stderr_of(&unknown).contains("unknown account profile"),
        "{}",
        stderr_of(&unknown)
    );
}

#[test]
fn account_status_shows_the_agents_running_on_a_profile() {
    let mut lab = Lab::new("status-agents");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    let started = lab.herdr(&[
        "agent",
        "start",
        "a1",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--account",
        SECOND_PROFILE,
    ]);
    assert!(
        started.status.success(),
        "agent start failed: {}{}",
        stdout_of(&started),
        stderr_of(&started)
    );

    let statuses = rows(&lab.herdr(&["account", "status", "--json"]));
    let perso = &statuses[0];
    let work = &statuses[1];
    assert_eq!(
        perso["agents"],
        serde_json::json!([]),
        "the agent is not on perso: {perso:#?}"
    );
    let agents = work["agents"].as_array().expect("work agents");
    assert_eq!(agents.len(), 1, "{work:#?}");
    assert_eq!(agents[0]["name"], "a1");
    assert_eq!(agents[0]["pane_id"], pane);
    assert_eq!(agents[0]["account_state"], "ok");
    assert_eq!(agents[0]["account_state_known"], true);

    // The text report names the same agent.
    let text = stdout_of(&lab.herdr(&["account", "status", SECOND_PROFILE]));
    assert!(text.contains("a1 on"), "{text}");
    assert!(text.contains("work@example.test"), "{text}");
}

/// `status` is a read, and a machine whose profiles someone is checking may
/// well have no server running yet. That is not an error, and it must not be
/// reported as "no agents".
#[test]
fn account_status_still_answers_without_a_server() {
    let mut lab = Lab::new("status-offline");
    assert!(lab.up().status.success());
    // Stop the server but keep the lab root: the profiles and the config the
    // report reads from the filesystem are exactly what must still work.
    let stopped = lab.herdr(&["server", "stop"]);
    assert!(
        stopped.status.success(),
        "server stop failed: {}{}",
        stdout_of(&stopped),
        stderr_of(&stopped)
    );
    for _ in 0..100 {
        if lab.herdr(&["agent", "list"]).status.code() != Some(0) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let output = lab.herdr(&["account", "status", "--json"]);
    assert!(
        output.status.success(),
        "status must survive a stopped server: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let statuses = rows(&output);
    assert_eq!(statuses.len(), 2);
    for status in &statuses {
        assert!(
            status["agents"].is_null(),
            "unknown placement is null, not []: {status:#?}"
        );
        assert_eq!(status["logged_in"], true, "{status:#?}");
    }
    assert!(
        stderr_of(&output).contains("no herdr server is running"),
        "the reason is stated: {}",
        stderr_of(&output)
    );

    let text = stdout_of(&lab.herdr(&["account", "status"]));
    assert!(text.contains("no herdr server answered"), "{text}");
}

#[test]
fn account_login_types_the_profile_into_the_pane() {
    let mut lab = Lab::new("login");
    assert!(lab.up().status.success());

    let dir = add_logged_out_profile(&lab, "fresh");
    let before = rows(&lab.herdr(&["account", "status", "fresh", "--json"]));
    assert_eq!(before[0]["logged_in"], false, "{before:#?}");
    assert!(before[0]["identity"].is_null(), "{before:#?}");

    let pane = lab.pane_id();
    let output = lab.herdr(&["account", "login", "fresh", "--pane", &pane]);
    assert!(
        output.status.success(),
        "account login failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let printed = stdout_of(&output);
    assert!(
        printed.contains("export CLAUDE_CONFIG_DIR="),
        "the command says exactly what it typed: {printed}"
    );
    assert!(printed.contains("claude auth login"), "{printed}");
    assert!(printed.contains(&pane), "{printed}");

    // The stub really ran, under the new profile's directory.
    let screen = wait_for_pane_text(&lab, &pane, "fake-claude: auth login");
    assert!(
        screen.contains(dir.to_str().expect("utf-8 lab path")),
        "the stub echoed the profile directory it logged into: {screen}"
    );

    // …and wrote a private credentials file there, which `status` now sees
    // without opening it.
    let credentials = dir.join(".credentials.json");
    assert!(credentials.is_file(), "the stub wrote credentials");
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&credentials)
        .expect("credentials metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "credentials must be owner-only");

    let after = rows(&lab.herdr(&["account", "status", "fresh", "--json"]));
    assert_eq!(after[0]["logged_in"], true, "{after:#?}");
    assert_eq!(after[0]["identity"]["email"], "fresh@example.test");
    assert_eq!(after[0]["credentials_mode_ok"], true);

    // Nothing landed in the other profiles or in the ambient directory.
    for other in [DEFAULT_PROFILE, SECOND_PROFILE] {
        let claude_json = lab.profile_dir(other).join(".claude.json");
        let text = std::fs::read_to_string(&claude_json).expect("lab .claude.json");
        assert!(
            text.contains(&format!("{other}@example.test")),
            "{other}'s identity must be untouched: {text}"
        );
    }
    assert!(
        !lab.ambient_dir().join(".credentials.json").exists(),
        "the ambient directory must not have been logged into"
    );
}

#[test]
fn account_login_refuses_a_busy_pane_without_typing_anything() {
    let mut lab = Lab::new("login-busy");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    let sent = lab.herdr(&[
        "pane",
        "send-text",
        &pane,
        "printf 'BUSY%s\\n' READY; sleep 30\r",
    ]);
    assert!(sent.status.success(), "{}", stderr_of(&sent));
    wait_for_pane_text(&lab, &pane, "BUSYREADY");

    let output = lab.herdr(&["account", "login", SECOND_PROFILE, "--pane", &pane]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a busy pane must be refused: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("not at its shell prompt"),
        "{}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).is_empty(),
        "nothing was typed, so nothing is reported: {}",
        stdout_of(&output)
    );

    let screen = stdout_of(&lab.herdr(&["pane", "read", &pane, "--source", "recent"]));
    assert!(
        !screen.contains("CLAUDE_CONFIG_DIR"),
        "the export line must not have reached the sleeping job: {screen}"
    );
}

#[test]
fn account_login_refuses_a_profile_it_cannot_address() {
    let mut lab = Lab::new("login-refusals");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();

    let unknown = lab.herdr(&["account", "login", "nope", "--pane", &pane]);
    assert_eq!(unknown.status.code(), Some(1), "{}", stdout_of(&unknown));
    assert!(
        stderr_of(&unknown).contains("unknown account profile"),
        "{}",
        stderr_of(&unknown)
    );

    // A profile whose directory is gone: logging in would create an unseeded,
    // world-readable one that no other herdr command knows how to repair.
    let dir = add_logged_out_profile(&lab, "gone");
    std::fs::remove_dir_all(&dir).expect("remove the profile directory");
    let missing = lab.herdr(&["account", "login", "gone", "--pane", &pane]);
    assert_eq!(missing.status.code(), Some(1), "{}", stdout_of(&missing));
    assert!(
        stderr_of(&missing).contains("does not exist"),
        "{}",
        stderr_of(&missing)
    );

    let screen = stdout_of(&lab.herdr(&["pane", "read", &pane, "--source", "recent"]));
    assert!(
        !screen.contains("CLAUDE_CONFIG_DIR"),
        "no refusal may type anything: {screen}"
    );
    assert!(!dir.exists(), "the missing directory was not created");

    // Usage errors exit 2, before any server call.
    let usage = lab.herdr(&["account", "login"]);
    assert_eq!(usage.status.code(), Some(2), "{}", stderr_of(&usage));
}
