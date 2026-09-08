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

    // A configuration diagnostic is exit 1 with the report still printed, the
    // same contract `account list` holds. Last, because it edits the config.
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

    let diagnosed = lab.herdr(&["account", "status", "--json"]);
    assert_eq!(
        diagnosed.status.code(),
        Some(1),
        "diagnostics must exit 1: {}{}",
        stdout_of(&diagnosed),
        stderr_of(&diagnosed)
    );
    assert!(
        stderr_of(&diagnosed).contains("duplicate account profile name"),
        "{}",
        stderr_of(&diagnosed)
    );
    assert_eq!(
        rows(&diagnosed).len(),
        2,
        "the report is still printed: {}",
        stdout_of(&diagnosed)
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

    // The stub really ran, and really ran under the new profile: the export
    // line is echoed by the pane whether or not it took effect, so the proof
    // is a line only the stub prints, plus the file it wrote (below).
    let screen = wait_for_pane_text(&lab, &pane, "logged in as fresh@example.test");
    assert!(
        screen.contains("fake-claude: auth login"),
        "the stub ran its login path: {screen}"
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
    let refusal = stderr_of(&output);
    assert!(refusal.contains("not at its shell prompt"), "{refusal}");
    assert!(
        refusal.contains("--pane") && !refusal.contains("--account none"),
        "the refusal points at this command's own way out: {refusal}"
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

// ---------------------------------------------------------------------------
// PR 5 — `herdr agent switch-account`.
//
// The dangerous command of the epic: it stops somebody's Claude and starts it
// again. Every test below asserts on two things the user would care about if
// this went wrong — the conversation (the session id must be the *same* one
// before and after) and the account (the relaunched process's own environment
// must name the new profile) — plus, for every refusal, that nothing at all
// was typed into the pane.
// ---------------------------------------------------------------------------

/// A second pane in the lab's workspace, at its own shell prompt.
fn split_pane(lab: &Lab) -> String {
    let pane = lab.pane_id();
    let output = lab.herdr(&["pane", "split", &pane, "--direction", "right", "--no-focus"]);
    assert!(
        output.status.success(),
        "pane split failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    json_of(&output)["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("pane split reported no pane id: {}", stdout_of(&output)))
        .to_string()
}

fn agent_of(lab: &Lab, target: &str) -> serde_json::Value {
    json_of(&lab.herdr(&["agent", "get", target]))["result"]["agent"].clone()
}

fn session_id_of(lab: &Lab, target: &str) -> String {
    agent_of(lab, target)["agent_session"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("agent {target} has no session id"))
        .to_string()
}

/// Wait until the pane's own shell holds the foreground again, so a line typed
/// into it has finished running before the next one is sent.
fn wait_for_prompt(lab: &Lab, pane: &str) {
    for _ in 0..80 {
        let info = json_of(&lab.herdr(&["pane", "process-info", "--pane", pane]))["result"]
            ["process_info"]
            .clone();
        let shell = info["shell_pid"].as_u64();
        let foreground = info["foreground_processes"]
            .as_array()
            .map(|processes| processes.len())
            .unwrap_or(0);
        if shell.is_some()
            && info["foreground_process_group_id"].as_u64() == shell
            && foreground == 1
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("pane {pane} never came back to its shell prompt");
}

/// Start an agent in `pane`, after optionally exporting knobs for the stub.
///
/// Each export is followed by a marker the shell only prints once it has run
/// the line, because polling the process table alone races: the pane still
/// looks idle in the moment between `pane.send_text` returning and the pty
/// delivering the line.
fn start_agent_in(lab: &Lab, name: &str, pane: &str, exports: &[&str], extra: &[&str]) {
    for (index, export) in exports.iter().enumerate() {
        let marker = format!("EXPORT{index}");
        let line = format!("export {export}; printf 'EXPORT%s\\n' {index}\r");
        let sent = lab.herdr(&["pane", "send-text", pane, &line]);
        assert!(sent.status.success(), "{}", stderr_of(&sent));
        let mut ran = false;
        for _ in 0..80 {
            if screen(lab, pane).contains(&marker) {
                ran = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(ran, "the pane never ran `export {export}`");
        wait_for_prompt(lab, pane);
    }
    let mut args = vec!["agent", "start", name, "--kind", "claude", "--pane", pane];
    args.extend_from_slice(extra);
    let output = lab.herdr(&args);
    assert!(
        output.status.success(),
        "agent start {name} failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
}

/// What the pane's screen shows, for the assertions about what was typed.
fn screen(lab: &Lab, pane: &str) -> String {
    stdout_of(&lab.herdr(&["pane", "read", pane, "--source", "recent"]))
}

/// The whole point of the command: the conversation survives.
#[test]
fn switch_account_resumes_the_same_session_under_the_new_profile() {
    let mut lab = Lab::new("switch-ok");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(&lab, "a1", &pane, &[], &["--account", DEFAULT_PROFILE]);
    let before = session_id_of(&lab, "a1");
    assert_eq!(agent_of(&lab, "a1")["tokens"]["account"], DEFAULT_PROFILE);

    let output = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "switch-account failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let result = json_of(&output);
    assert_eq!(result["name"], "a1");
    assert_eq!(result["from"], DEFAULT_PROFILE);
    assert_eq!(result["to"], SECOND_PROFILE);
    assert_eq!(
        result["session_id"].as_str(),
        Some(before.as_str()),
        "the switch must keep the conversation: {result:#?}"
    );
    assert_eq!(
        result["account_state"], "ok",
        "on Linux the relaunched process's environment is readable, so the \
         account must be evidence-backed: {result:#?}"
    );

    // The agent herdr reports, not the report herdr wrote.
    let after = agent_of(&lab, "a1");
    assert_eq!(
        after["agent_session"]["value"].as_str(),
        Some(before.as_str()),
        "the same session id before and after: {after:#?}"
    );
    assert_eq!(after["tokens"]["account"], SECOND_PROFILE);
    assert_eq!(after["tokens"]["account_state"], "ok");

    // And the launched process itself: it was resumed, into the new profile.
    let launch = last_launch(&lab, SECOND_PROFILE);
    assert_eq!(
        launch["config_dir"].as_str(),
        lab.profile_dir(SECOND_PROFILE).to_str()
    );
    assert_eq!(launch["session_start_source"], "resume");
    assert_eq!(launch["session_id"].as_str(), Some(before.as_str()));
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).expect("argv");
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == before),
        "the relaunch must pass --resume <id>: {argv:?}"
    );

    let screen = screen(&lab, &pane);
    assert!(screen.contains("/exit"), "{screen}");
    assert!(
        screen.contains(&format!("resumed {before}")),
        "the stub must report the resume on screen: {screen}"
    );
}

/// The refusal that protects a conversation herdr could not bring back.
#[test]
fn switch_account_refuses_an_agent_without_a_session_id() {
    let mut lab = Lab::new("switch-nosess");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(
        &lab,
        "a1",
        &pane,
        &["FAKE_CLAUDE_NO_SESSION=1"],
        &["--account", DEFAULT_PROFILE],
    );
    assert!(
        agent_of(&lab, "a1")["agent_session"].is_null(),
        "the stub was told not to report a session"
    );

    let before = screen(&lab, &pane);
    let output = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--json",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a refusal that touched nothing must exit 2: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("no Claude session id"),
        "{}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("nothing was sent"),
        "{}",
        stderr_of(&output)
    );

    // Nothing typed, nothing launched, nothing claimed.
    assert_eq!(
        screen(&lab, &pane),
        before,
        "a refused switch must not touch the pane"
    );
    assert!(
        !lab.profile_dir(SECOND_PROFILE)
            .join("last-launch.json")
            .exists(),
        "the target profile must not have been launched into"
    );
    assert_eq!(agent_of(&lab, "a1")["tokens"]["account"], DEFAULT_PROFILE);
}

/// A Claude that may be mid-tool-call is never interrupted by default.
#[test]
fn switch_account_refuses_a_working_agent_without_interrupt() {
    let mut lab = Lab::new("switch-working");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(&lab, "a1", &pane, &[], &["--account", DEFAULT_PROFILE]);

    // The stub sets the same braille-spinner OSC title a busy Claude sets, so
    // this goes through herdr's own screen detection rather than a faked state
    // report — `herdr:claude` is a reserved native state source and cannot
    // report a state at all.
    let prompted = lab.herdr(&["agent", "prompt", "a1", "/work"]);
    assert!(prompted.status.success(), "{}", stderr_of(&prompted));
    let mut working = false;
    for _ in 0..60 {
        if agent_of(&lab, "a1")["agent_status"] == "working" {
            working = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(working, "the stub never looked working to herdr");

    let before = screen(&lab, &pane);
    let output = lab.herdr(&["agent", "switch-account", "a1", SECOND_PROFILE, "--yes"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("is working"),
        "{}",
        stderr_of(&output)
    );
    assert_eq!(
        screen(&lab, &pane),
        before,
        "a working agent must not be interrupted without --interrupt"
    );
}

/// Automation has to say `--yes`; a pipe is not a person.
#[test]
fn switch_account_requires_yes_when_not_a_tty() {
    let mut lab = Lab::new("switch-tty");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(&lab, "a1", &pane, &[], &["--account", DEFAULT_PROFILE]);
    let before = screen(&lab, &pane);

    let output = lab.herdr(&["agent", "switch-account", "a1", SECOND_PROFILE]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("--yes"),
        "{}",
        stderr_of(&output)
    );
    assert_eq!(screen(&lab, &pane), before, "nothing may be sent");
}

/// A Claude that will not leave is left alone: the command gives up, says what
/// the pane holds, and kills nothing.
#[test]
fn switch_account_never_kills_an_agent_that_will_not_exit() {
    let mut lab = Lab::new("switch-stuck");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(
        &lab,
        "a1",
        &pane,
        &["FAKE_CLAUDE_BUSY=1"],
        &["--account", DEFAULT_PROFILE],
    );
    let session = session_id_of(&lab, "a1");

    let output = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--timeout",
        "2000",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "the protocol had started, so this is a 1: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let message = stderr_of(&output);
    assert!(message.contains("did not exit"), "{message}");
    assert!(message.contains("nothing was killed"), "{message}");

    // Still there, still the same conversation, still on the old account.
    let after = agent_of(&lab, "a1");
    assert_eq!(after["agent_session"]["value"].as_str(), Some(&*session));
    assert_eq!(after["tokens"]["account"], DEFAULT_PROFILE);
    assert!(
        screen(&lab, &pane).contains("refusing to exit"),
        "the stub is still running: {}",
        screen(&lab, &pane)
    );
    assert!(
        !lab.profile_dir(SECOND_PROFILE)
            .join("last-launch.json")
            .exists(),
        "nothing may have been launched under the target profile"
    );
}

/// Everything the switch does is addressed to one pane. A second agent, in a
/// second pane, must not see a single keystroke of it.
#[test]
fn switch_account_leaves_another_pane_untouched() {
    let mut lab = Lab::new("switch-other");
    assert!(lab.up().status.success());

    let first = lab.pane_id();
    let second = split_pane(&lab);
    start_agent_in(&lab, "a1", &first, &[], &["--account", DEFAULT_PROFILE]);
    start_agent_in(&lab, "a2", &second, &[], &["--account", DEFAULT_PROFILE]);

    let other_session = session_id_of(&lab, "a2");
    let other_before = screen(&lab, &second);

    let output = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "switch-account failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert_eq!(json_of(&output)["pane_id"].as_str(), Some(first.as_str()));

    let other = agent_of(&lab, "a2");
    assert_eq!(other["tokens"]["account"], DEFAULT_PROFILE);
    assert_eq!(
        other["agent_session"]["value"].as_str(),
        Some(&*other_session)
    );
    assert_eq!(
        screen(&lab, &second),
        other_before,
        "the other pane must not have been typed into"
    );
}

/// The target profile is checked before the agent is asked to leave, so a
/// broken target can never cost anybody a running Claude.
#[test]
fn switch_account_refuses_a_missing_target_profile_before_touching_the_pane() {
    let mut lab = Lab::new("switch-gone");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(&lab, "a1", &pane, &[], &["--account", DEFAULT_PROFILE]);
    let before = screen(&lab, &pane);

    std::fs::remove_dir_all(lab.profile_dir(SECOND_PROFILE)).expect("remove the target profile");
    let output = lab.herdr(&["agent", "switch-account", "a1", SECOND_PROFILE, "--yes"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("does not exist"),
        "{}",
        stderr_of(&output)
    );
    assert_eq!(screen(&lab, &pane), before);

    // …and an account nobody configured is refused the same way.
    let output = lab.herdr(&["agent", "switch-account", "a1", "nosuch", "--yes"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert!(
        stderr_of(&output).contains("unknown account profile"),
        "{}",
        stderr_of(&output)
    );
    assert_eq!(screen(&lab, &pane), before);
}

/// A `--resume` Claude could not honour starts a *new* conversation. The
/// switch must refuse to call that a success, tell the user how to get the
/// old conversation back, and still record where the agent now runs.
#[test]
fn switch_account_fails_loudly_when_the_resume_starts_a_new_conversation() {
    let mut lab = Lab::new("switch-new");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(
        &lab,
        "a1",
        &pane,
        &["FAKE_CLAUDE_RESUME=new"],
        &["--account", DEFAULT_PROFILE],
    );
    let before = session_id_of(&lab, "a1");

    let output = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--json",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a resume that did not take is a failure after the protocol started: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).is_empty(),
        "no result may be printed for a lost conversation: {}",
        stdout_of(&output)
    );
    let message = stderr_of(&output);
    assert!(message.contains("did not take"), "{message}");
    assert!(
        message.contains(&format!("claude --resume {before}")),
        "the message must name the recovery command: {message}"
    );
    assert!(
        message.contains(&format!("running under account {SECOND_PROFILE:?}")),
        "the relaunched agent's account must still be recorded and said: {message}"
    );

    // The agent that is there now: a different conversation, on the new
    // account, and the token says so.
    let after = agent_of(&lab, "a1");
    let now = after["agent_session"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("the fresh Claude reports a session: {after:#?}"))
        .to_string();
    assert_ne!(now, before, "{after:#?}");
    assert_eq!(after["tokens"]["account"], SECOND_PROFILE);
    assert_eq!(after["tokens"]["account_state"], "ok");
    let launch = last_launch(&lab, SECOND_PROFILE);
    assert_eq!(launch["session_start_source"], "startup");
    assert_eq!(launch["session_id"].as_str(), Some(now.as_str()));
}

/// The relaunch fails outright: Claude cannot find the transcript and exits.
/// The pane must be left at its shell with the message naming the recovery
/// command, the note about the exported profile, no agent and no account
/// claimed — and the recovery it names must actually work, landing on the
/// new profile because the shell still exports it.
#[test]
fn switch_account_leaves_the_shell_usable_when_the_relaunch_fails() {
    let mut lab = Lab::new("switch-rl");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(
        &lab,
        "a1",
        &pane,
        &["FAKE_CLAUDE_RESUME=fail"],
        &["--account", DEFAULT_PROFILE],
    );
    let before = session_id_of(&lab, "a1");

    let output = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--timeout",
        "5000",
    ]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let message = stderr_of(&output);
    assert!(message.contains("could not restart"), "{message}");
    assert!(
        message.contains(&format!("claude --resume {before}")),
        "{message}"
    );
    assert!(
        message.contains("was already exported"),
        "the note that the shell still exports the profile: {message}"
    );
    assert!(
        !lab.profile_dir(SECOND_PROFILE)
            .join("last-launch.json")
            .exists(),
        "the failed relaunch never got as far as running under the target profile"
    );

    // The pane is at its shell with no agent on it.
    wait_for_prompt(&lab, &pane);
    let mut released = false;
    for _ in 0..50 {
        let gone = lab.herdr(&["agent", "get", "a1"]);
        if !gone.status.success() && stderr_of(&gone).contains("agent_not_found") {
            released = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        released,
        "the failed relaunch must not leave a named agent behind"
    );

    // Recovery by hand, exactly as the message says. The shell still exports
    // the new profile, so the resumed Claude lands there.
    let line = format!("FAKE_CLAUDE_RESUME=ok claude --resume {before}\r");
    let sent = lab.herdr(&["pane", "send-text", &pane, &line]);
    assert!(sent.status.success(), "{}", stderr_of(&sent));
    let mut resumed = false;
    for _ in 0..80 {
        if screen(&lab, &pane).contains(&format!("resumed {before}")) {
            resumed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        resumed,
        "a hand-typed resume must recover the conversation: {}",
        screen(&lab, &pane)
    );
    let launch = last_launch(&lab, SECOND_PROFILE);
    assert_eq!(
        launch["config_dir"].as_str(),
        lab.profile_dir(SECOND_PROFILE).to_str(),
        "the export line typed by the switch is still in effect"
    );
    assert_eq!(launch["session_id"].as_str(), Some(before.as_str()));
}

/// With `--interrupt` a working agent is moved: Escape first, then `/exit`,
/// never a signal — and the conversation still comes back.
#[test]
fn switch_account_interrupts_a_working_agent_only_with_the_flag() {
    let mut lab = Lab::new("switch-int");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(&lab, "a1", &pane, &[], &["--account", DEFAULT_PROFILE]);
    let before = session_id_of(&lab, "a1");

    let prompted = lab.herdr(&["agent", "prompt", "a1", "/work"]);
    assert!(prompted.status.success(), "{}", stderr_of(&prompted));
    let mut working = false;
    for _ in 0..60 {
        if agent_of(&lab, "a1")["agent_status"] == "working" {
            working = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(working, "the stub never looked working to herdr");

    let output = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--interrupt",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "switch-account --interrupt failed: {}{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let result = json_of(&output);
    assert_eq!(result["session_id"].as_str(), Some(before.as_str()));
    assert_eq!(result["account_state"], "ok");

    let screen = screen(&lab, &pane);
    assert!(
        screen.contains("fake-claude: interrupted"),
        "Escape must have reached the agent before /exit: {screen}"
    );
    assert!(screen.contains("fake-claude: exiting"), "{screen}");
    assert!(screen.contains(&format!("resumed {before}")), "{screen}");
    assert_eq!(agent_of(&lab, "a1")["tokens"]["account"], SECOND_PROFILE);
}

// ---- PR 7: the TUI account picker, driven through a real pty ----

/// The client shell attached to the lab in a pty, with everything it wrote.
///
/// The picker is reached by right-clicking a pane, so the only honest test of
/// it goes through the same path a user does: a real terminal, real mouse
/// sequences, real keys. The reader thread keeps the pty drained so herdr
/// never blocks on a full buffer.
struct Tui {
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Tui {
    fn attach(lab: &Lab) -> Self {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 36,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
        command.arg("--session");
        command.arg(support::accounts_lab::SESSION);
        command.arg("client");
        command.env("XDG_CONFIG_HOME", lab.root.join("xdg"));
        command.env("XDG_RUNTIME_DIR", lab.runtime_dir());
        command.env("XDG_STATE_HOME", lab.root.join("state"));
        command.env("XDG_DATA_HOME", lab.root.join("data"));
        command.env("XDG_CACHE_HOME", lab.root.join("cache"));
        command.env(
            "PATH",
            format!(
                "{}:{}",
                lab.root.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        command.env("CLAUDE_CONFIG_DIR", lab.ambient_dir());
        command.env("HERDR_BIN", env!("CARGO_BIN_EXE_herdr"));
        command.env("HERDR_DISABLE_SOUND", "1");
        command.env("TERM", "xterm-256color");
        command.env_remove("HERDR_SOCKET_PATH");
        command.env_remove("HERDR_CLIENT_SOCKET_PATH");
        command.env_remove("HERDR_ENV");
        command.env_remove("HERDR_SESSION");
        let child = pair.slave.spawn_command(command).expect("spawn the client");
        support::register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);

        let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = std::sync::Arc::clone(&output);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut chunk = [0_u8; 8192];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                if let Ok(mut sink) = sink.lock() {
                    sink.push_str(&String::from_utf8_lossy(&chunk[..read]));
                }
            }
        });
        let writer = pair.master.take_writer().expect("pty writer");
        Self {
            _master: pair.master,
            writer,
            child,
            output,
        }
    }

    fn send(&mut self, bytes: &str) {
        use std::io::Write as _;
        self.writer
            .write_all(bytes.as_bytes())
            .expect("write to the client");
        self.writer.flush().expect("flush the client");
    }

    fn screen(&self) -> String {
        self.output
            .lock()
            .map(|output| output.clone())
            .unwrap_or_default()
    }

    /// What the client *wrote*, with the escape sequences that addressed it
    /// removed.
    ///
    /// A terminal draws a frame as runs of styled cells separated by cursor
    /// moves, and ratatui writes only the cells that changed, so a phrase that
    /// is one line on screen is several runs in the stream with escapes
    /// between them. Matching the raw bytes therefore misses text that is
    /// plainly visible, and *which* text it misses depends on what the
    /// previous frame happened to hold — which is how a raw match turns into a
    /// flake on a slower machine. Every assertion below matches this instead.
    fn visible_screen(&self) -> String {
        visible(&self.screen())
    }

    fn contains(&self, needle: &str) -> bool {
        self.visible_screen().contains(needle)
    }

    /// Wait until the client has shown `needle`, and say what it showed if it
    /// never does.
    fn wait_for(&self, needle: &str) {
        for _ in 0..150 {
            if self.contains(needle) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!(
            "the client never showed {needle:?}; it wrote:\n{}",
            self.visible_screen()
        );
    }
}

/// Strip ANSI escape sequences from a pty stream.
///
/// CSI (`ESC [ … final`), OSC (`ESC ] … BEL` or `ESC \`), and the two-byte
/// escapes in between. Everything else is text.
fn visible(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() || next == '@' || next == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        // `ESC \` terminates a string; anything else is the
                        // start of a new sequence this loop should not eat.
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some('P') | Some('X') | Some('^') | Some('_') => {
                while let Some(next) = chars.next() {
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

impl Drop for Tui {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        support::unregister_spawned_herdr_pid(pid);
    }
}

/// An SGR right-click, press and release, at 1-based `(column, row)`.
fn right_click(tui: &mut Tui, column: u16, row: u16) {
    tui.send(&format!("\x1b[<2;{column};{row}M"));
    tui.send(&format!("\x1b[<2;{column};{row}m"));
}

/// Right-click until herdr's pane menu is actually on screen.
///
/// The client draws its sidebar — session name included — before it has a
/// snapshot with a pane surface in it, so a right-click sent the moment the
/// session name appears can land on a client that has no pane at those
/// coordinates yet, and `open_pane_context_menu` returns without doing
/// anything. On a loaded machine that window is wide enough to lose the click.
/// Retrying is the honest fix: what a test wants is "the menu for this pane",
/// not "one click", and a right-click on the same spot is idempotent — it
/// re-opens the same menu.
fn open_pane_menu(tui: &mut Tui, column: u16, row: u16) {
    let menus = |tui: &Tui| tui.visible_screen().matches("Rename pane").count();
    let before = menus(tui);
    for _ in 0..20 {
        right_click(tui, column, row);
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if menus(tui) > before {
                return;
            }
        }
    }
    panic!(
        "the pane menu never opened at ({column},{row}); the client showed:\n{}",
        tui.visible_screen()
    );
}

/// Right-click the pane and open the account picker on it.
///
/// The pane menu's items are fixed for a focused, unlabelled pane with no
/// agent: `Rename pane` then the account item, so one `Down` highlights it.
fn open_account_picker(tui: &mut Tui) {
    tui.wait_for("accounts-lab");
    open_pane_menu(tui, 80, 10);
    tui.wait_for("Start Claude as account...");
    tui.send("\x1b[B");
    std::thread::sleep(std::time::Duration::from_millis(200));
    tui.send("\r");
    tui.wait_for("start claude as account");
}

#[test]
fn the_tui_picker_starts_claude_under_the_account_it_was_given() {
    let mut lab = Lab::new("tui-pick");
    assert!(lab.up().status.success());
    let pane = lab.pane_id();

    let mut tui = Tui::attach(&lab);
    open_account_picker(&mut tui);
    assert!(
        tui.contains(&format!("pane {pane}")),
        "the modal names the pane it was opened on: {}",
        tui.visible_screen()
    );
    assert!(tui.contains(&lab.profile_dir(SECOND_PROFILE).display().to_string()));

    // `perso` is the default and starts selected, so one `Down` picks `work`.
    tui.send("\x1b[B");
    std::thread::sleep(std::time::Duration::from_millis(200));
    tui.send("\r");

    // The agent the picker started, seen through the API rather than the screen.
    let mut agent = serde_json::Value::Null;
    for _ in 0..200 {
        let listed = json_of(&lab.herdr(&["agent", "list"]));
        if let Some(found) = listed["result"]["agents"]
            .as_array()
            .and_then(|agents| agents.first())
        {
            if found["tokens"]["account"].as_str().is_some() {
                agent = found.clone();
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !agent.is_null(),
        "the picker never started an agent; the client wrote:\n{}",
        tui.visible_screen()
    );
    assert_eq!(agent["name"], "claude", "{agent:#?}");
    assert_eq!(agent["pane_id"].as_str(), Some(pane.as_str()));
    assert_eq!(agent["tokens"]["account"], SECOND_PROFILE, "{agent:#?}");
    assert_eq!(agent["tokens"]["account_state"], "ok", "{agent:#?}");

    // And the launched process itself agrees.
    let launch = last_launch(&lab, SECOND_PROFILE);
    assert_eq!(
        launch["config_dir"].as_str(),
        lab.profile_dir(SECOND_PROFILE).to_str()
    );
    assert!(
        !lab.profile_dir(DEFAULT_PROFILE)
            .join("last-launch.json")
            .exists(),
        "the default profile must not have been launched"
    );
}

#[test]
fn the_pane_menu_hides_the_account_item_without_a_configured_profile() {
    let mut lab = Lab::new("tui-none");
    assert!(lab.up().status.success());

    // Take the profiles out of the lab's config before the client starts, so
    // the menu is built from a config with no `[[accounts]]` at all. The lab
    // writes one config per app directory (`herdr` and `herdr-dev`, because a
    // debug build reads the second); strip every one of them, or the answer
    // depends on which channel the test binary was built for.
    let mut stripped = 0;
    for app in ["herdr", "herdr-dev"] {
        let config = lab.root.join("xdg").join(app).join("config.toml");
        let Ok(text) = std::fs::read_to_string(&config) else {
            continue;
        };
        let trimmed = text
            .split("[[accounts]]")
            .next()
            .expect("config before the accounts section")
            .to_owned();
        assert_ne!(
            trimmed, text,
            "{app}/config.toml must have declared profiles"
        );
        std::fs::write(&config, &trimmed).expect("rewrite the lab config");
        stripped += 1;
    }
    assert!(stripped > 0, "the lab wrote no config to strip");

    let mut tui = Tui::attach(&lab);
    tui.wait_for("accounts-lab");
    open_pane_menu(&mut tui, 80, 10);
    // The rest of the pane menu is on screen, so the account item's absence is
    // a fact about this menu rather than about the menu not being drawn yet.
    tui.wait_for("Close pane");
    assert!(
        !tui.contains("Start Claude as account"),
        "with no profiles the item must be absent: {}",
        tui.visible_screen()
    );
}

/// The failure path through the same pty: a pane with something in the
/// foreground is refused before a byte is typed, the modal says so and stays
/// open, nothing is started, and Esc dismisses it.
#[test]
fn the_tui_picker_refuses_a_busy_pane_and_stays_open_to_say_so() {
    let mut lab = Lab::new("tui-busy");
    assert!(lab.up().status.success());
    let pane = lab.pane_id();

    let mut tui = Tui::attach(&lab);
    open_account_picker(&mut tui);

    // Occupy the pane under the modal, through the API: the picker must
    // re-check the pane when Enter lands, not when the menu opened.
    let busy = lab.herdr(&["pane", "send-text", &pane, "sleep 30\r"]);
    assert!(busy.status.success(), "{}", stderr_of(&busy));
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

    tui.send("\x1b[B");
    std::thread::sleep(std::time::Duration::from_millis(200));
    tui.send("\r");
    tui.wait_for("is not at its shell prompt");
    tui.wait_for("close");

    let listed = json_of(&lab.herdr(&["agent", "list"]));
    assert_eq!(
        listed["result"]["agents"]
            .as_array()
            .map(|agents| agents.len())
            .unwrap_or(0),
        0,
        "nothing may have been started: {listed:#?}"
    );
    assert!(
        !screen(&lab, &pane).contains("CLAUDE_CONFIG_DIR="),
        "nothing may have been typed into the pane"
    );

    // Esc dismisses the settled modal; the pane menu is reachable again.
    tui.send("\x1b");
    std::thread::sleep(std::time::Duration::from_millis(300));
    open_pane_menu(&mut tui, 80, 10);
}

// ---------------------------------------------------------------------------
// PR 9 — the usage-limit rule and the account limit hints
// ---------------------------------------------------------------------------

/// The reconstructed limit screen the manifest rule is written against.
///
/// The stub prints this file, so the rule is exercised through herdr's real
/// screen detection rather than through a reported state: `herdr:claude` is a
/// reserved state source and cannot report `blocked` (see PR 5's *As built*).
fn usage_limit_fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fork/claude-usage-limit.txt")
}

/// Poll `agent get` until the server reports `status`, or give up.
fn wait_for_agent_status(lab: &Lab, target: &str, status: &str) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for _ in 0..100 {
        last = agent_of(lab, target);
        if last["agent_status"].as_str() == Some(status) {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("agent {target} never reached status {status}; last: {last:#?}");
}

/// Poll `agent explain` until the detector settles on `rule`, or give up.
fn wait_for_matched_rule(lab: &Lab, target: &str, rule: &str) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for _ in 0..80 {
        last = json_of(&lab.herdr(&["agent", "explain", target, "--json"]));
        if last["matched_rule"]["id"].as_str() == Some(rule) {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("agent {target} never matched rule {rule}; last explain: {last:#?}");
}

/// The whole of PR 9 end to end: a limit screen is `blocked` by the
/// `usage_limit` rule, `account status` carries it, and the switch preflight
/// says why it is being asked.
#[test]
fn a_usage_limit_screen_is_detected_and_reported() {
    let mut lab = Lab::new("limit");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    let fixture = format!(
        "FAKE_CLAUDE_LIMIT_FILE='{}'",
        usage_limit_fixture().display()
    );
    start_agent_in(
        &lab,
        "a1",
        &pane,
        &[&fixture],
        &["--account", DEFAULT_PROFILE],
    );

    // The negative first, on the very same agent: a healthy Claude at its
    // prompt is not limited, and nothing claims it is.
    let healthy = json_of(&lab.herdr(&["agent", "explain", "a1", "--json"]));
    assert_ne!(
        healthy["matched_rule"]["id"].as_str(),
        Some("usage_limit"),
        "an agent that has not hit a limit must not match it: {healthy:#?}"
    );
    let before = json_of(&lab.herdr(&["account", "status", "--json"]));
    assert!(
        !stdout_of(&lab.herdr(&["account", "status", "--json"])).contains("\"limit\""),
        "no limit key before the limit: {before:#?}"
    );

    // Now the limit arrives, the way a real one does: mid-session.
    let prompted = lab.herdr(&["agent", "prompt", "a1", "/limit"]);
    assert!(prompted.status.success(), "{}", stderr_of(&prompted));

    let explain = wait_for_matched_rule(&lab, "a1", "usage_limit");
    assert_eq!(explain["state"], "blocked", "{explain:#?}");
    assert_eq!(explain["visible_blocker"], true, "{explain:#?}");
    assert_eq!(
        explain["manifest_source"].as_str(),
        Some("bundled"),
        "the fork's own manifest must be the one that matched: {explain:#?}"
    );

    // The server's own view of the agent, which follows pane output rather
    // than the on-demand `explain` above.
    let agent = wait_for_agent_status(&lab, "a1", "blocked");
    assert_eq!(agent["agent_status"], "blocked", "{agent:#?}");

    // The report a human reads.
    let status = lab.herdr(&["account", "status", "--json"]);
    assert!(status.status.success(), "{}", stderr_of(&status));
    let rows: Vec<serde_json::Value> = serde_json::from_str(&stdout_of(&status)).expect("json");
    let limited = rows
        .iter()
        .find(|row| row["name"] == DEFAULT_PROFILE)
        .expect("the default profile is reported");
    let on_account = limited["agents"]
        .as_array()
        .expect("agents were listed")
        .iter()
        .find(|agent| agent["name"] == "a1")
        .expect("a1 is on the default profile");
    assert_eq!(
        on_account["limit"]["reset_text"], "3pm",
        "the reset time comes off the detection screen: {on_account:#?}"
    );
    assert!(
        stderr_of(&status).contains(&format!("switch-account {pane} {SECOND_PROFILE}")),
        "the hint must name a way out: {}",
        stderr_of(&status)
    );

    // And the switch says why it is being asked.
    let switched = lab.herdr(&[
        "agent",
        "switch-account",
        "a1",
        SECOND_PROFILE,
        "--yes",
        "--json",
    ]);
    assert!(
        switched.status.success(),
        "switch-account failed: {}{}",
        stdout_of(&switched),
        stderr_of(&switched)
    );
    let result = json_of(&switched);
    assert_eq!(
        result["limit"]["reset_text"], "3pm",
        "the preflight records the limit it switched away from: {result:#?}"
    );
    assert_eq!(result["to"], SECOND_PROFILE);
}

/// The other half of the contract, and the failure path that matters most:
/// an agent that is genuinely `blocked` — so `account status` really does run
/// `agent.explain` and the detection read on it — but blocked by a dialog
/// drawn on top of a limited account. herdr must report the dialog the human
/// can act on, and must not answer "switch accounts" to someone who has to
/// answer a question.
#[test]
fn an_agent_blocked_for_another_reason_carries_no_limit() {
    let mut lab = Lab::new("limit-neg");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    let negative = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fork/claude-usage-limit-with-dialog.txt");
    start_agent_in(
        &lab,
        "a1",
        &pane,
        &[&format!("FAKE_CLAUDE_LIMIT_FILE='{}'", negative.display())],
        &["--account", DEFAULT_PROFILE],
    );

    // A blocking dialog over a limit footer, printed onto the screen.
    let prompted = lab.herdr(&["agent", "prompt", "a1", "/limit"]);
    assert!(prompted.status.success(), "{}", stderr_of(&prompted));

    let explain = wait_for_matched_rule(&lab, "a1", "live_blocked_form");
    assert_eq!(explain["state"], "blocked", "{explain:#?}");
    let agent = wait_for_agent_status(&lab, "a1", "blocked");
    assert_eq!(agent["agent_status"], "blocked", "{agent:#?}");

    let status = lab.herdr(&["account", "status", "--json"]);
    assert!(
        !stdout_of(&status).contains("\"limit\""),
        "no limit key: {}",
        stdout_of(&status)
    );
    assert!(
        !stderr_of(&status).contains("switch-account"),
        "no switch was suggested: {}",
        stderr_of(&status)
    );
}

// ---------------------------------------------------------------------------
// PR 8: the TUI switch action.
//
// The same protocol `herdr agent switch-account` runs, reached from the pane
// context menu and gated by a confirmation modal. Every test below drives a
// real client over a pty with real mouse and key bytes, and asserts on the
// server's answer rather than on the screen wherever the fact matters: the
// conversation must be the *same* one before and after, the relaunched
// process's own environment must name the new profile, a declined
// confirmation must leave the pane byte-for-byte as it was, and an agent the
// protocol refuses must never have been typed into.
// ---------------------------------------------------------------------------

/// Right-click the pane and open the switch picker on the agent running in it.
///
/// With an agent in the pane the menu is `Rename pane`, `Switch Claude
/// account...` (the start item is withheld — the pane is occupied), then the
/// splits, so one `Down` highlights it.
fn open_switch_picker(tui: &mut Tui, column: u16) {
    tui.wait_for("accounts-lab");
    open_pane_menu(tui, column, 10);
    tui.wait_for("Switch Claude account...");
    assert!(
        !tui.contains("Start Claude as account"),
        "a pane running an agent has nothing to start: {}",
        tui.visible_screen()
    );
    // The pane menu starts with `Rename pane` and grows `Swap with focused
    // pane` when the clicked pane is not the focused one, so the switch item
    // is one or two rows down. Counting what is on screen keeps the test
    // independent of which pane the click landed in.
    let mut downs = 1;
    if tui.contains("Swap with focused pane") {
        downs += 1;
    }
    for _ in 0..downs {
        tui.send("\x1b[B");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    tui.send("\r");
    // The lowercase title is the picker's, not the menu item's.
    tui.wait_for("switch claude account");
}

/// Wait until `agent get <name>` reports `tokens.account == account`.
///
/// Lenient about the read failing: between `/exit` and the relaunch the server
/// has released the managed name — that is exactly what `AwaitShell` waits
/// for — so `agent.get` answers `agent_not_found` for a moment in the middle
/// of every successful switch.
fn wait_for_account(lab: &Lab, name: &str, account: &str, tui: &Tui) -> serde_json::Value {
    for _ in 0..300 {
        let response: serde_json::Value =
            serde_json::from_str(&stdout_of(&lab.herdr(&["agent", "get", name])))
                .unwrap_or(serde_json::Value::Null);
        let agent = response["result"]["agent"].clone();
        if agent["tokens"]["account"].as_str() == Some(account) {
            return agent;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!(
        "agent {name} never moved to account {account}; the client wrote:\n{}",
        tui.visible_screen()
    );
}

/// The whole point of the action: the conversation survives, and only the
/// agent the user right-clicked is touched.
#[test]
fn the_tui_switch_keeps_the_conversation_and_flips_the_account() {
    let mut lab = Lab::new("tui-sw");
    assert!(lab.up().status.success());

    let first = lab.pane_id();
    let second = split_pane(&lab);
    start_agent_in(&lab, "a1", &first, &[], &["--account", DEFAULT_PROFILE]);
    start_agent_in(&lab, "a2", &second, &[], &["--account", DEFAULT_PROFILE]);
    let sessions = [
        (first.clone(), "a1", session_id_of(&lab, "a1")),
        (second.clone(), "a2", session_id_of(&lab, "a2")),
    ];

    let mut tui = Tui::attach(&lab);
    tui.wait_for("accounts-lab");
    open_switch_picker(&mut tui, 80);

    // Which pane the click landed in is a layout detail; which pane the modal
    // is *about* is the contract, and it says so on its own title bar.
    let visible = tui.visible_screen();
    let targets = sessions
        .iter()
        .filter(|(pane, _, _)| visible.contains(&format!("pane {pane}")))
        .collect::<Vec<_>>();
    assert_eq!(
        targets.len(),
        1,
        "the modal must name exactly one pane: {visible}"
    );
    let (target_pane, target_agent, target_session) = targets[0].clone();
    let (other_pane, other_agent, other_session) = sessions
        .iter()
        .find(|(pane, _, _)| pane != &target_pane)
        .cloned()
        .expect("the other agent");
    // Captured after the attach: a client attaching resizes the panes, and a
    // reflow would look exactly like something having been typed.
    let other_screen_before = screen(&lab, &other_pane);

    assert!(
        tui.contains("current"),
        "the picker says where the agent is now: {visible}"
    );

    // `perso` is where it is, so the picker opens on `work`; Enter submits.
    tui.send("\r");
    tui.wait_for("↵ confirm");
    assert!(
        tui.contains("--resume"),
        "the confirmation says the conversation is resumed: {}",
        tui.visible_screen()
    );
    // Nothing has reached the pane while the question is up.
    assert!(
        !screen(&lab, &target_pane).contains("/exit"),
        "the confirmation must be answered before anything is sent"
    );
    // An Enter on the heels of the one that submitted the picker — a double
    // tap, a key repeat — is not an answer: the question has to have been on
    // screen long enough to be read.
    tui.send("\r");
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert!(
        tui.contains("↵ confirm"),
        "a yes before the question could be read must be ignored: {}",
        tui.visible_screen()
    );
    assert!(
        !screen(&lab, &target_pane).contains("/exit"),
        "an ignored answer sends nothing"
    );
    std::thread::sleep(std::time::Duration::from_millis(600));
    tui.send("\r");

    let after = wait_for_account(&lab, target_agent, SECOND_PROFILE, &tui);
    assert_eq!(
        after["agent_session"]["value"].as_str(),
        Some(target_session.as_str()),
        "the same conversation before and after: {after:#?}"
    );
    assert_eq!(after["tokens"]["account_state"], "ok", "{after:#?}");
    assert_eq!(after["pane_id"].as_str(), Some(target_pane.as_str()));

    // The relaunched process itself agrees: new profile, resumed conversation.
    let launch = last_launch(&lab, SECOND_PROFILE);
    assert_eq!(
        launch["config_dir"].as_str(),
        lab.profile_dir(SECOND_PROFILE).to_str()
    );
    assert_eq!(launch["session_start_source"], "resume");
    assert_eq!(launch["session_id"].as_str(), Some(target_session.as_str()));

    let pane_screen = screen(&lab, &target_pane);
    assert!(pane_screen.contains("/exit"), "{pane_screen}");
    assert!(
        pane_screen.contains(&format!("resumed {target_session}")),
        "the stub reports the resume on screen: {pane_screen}"
    );

    // The agent next door never moved.
    let untouched = agent_of(&lab, other_agent);
    assert_eq!(untouched["tokens"]["account"], DEFAULT_PROFILE);
    assert_eq!(
        untouched["agent_session"]["value"].as_str(),
        Some(other_session.as_str())
    );
    assert_eq!(
        screen(&lab, &other_pane),
        other_screen_before,
        "nothing may have been typed into the other pane"
    );
}

/// Declining the confirmation is the negative the whole modal exists for:
/// nothing at all reaches the pane.
#[test]
fn the_tui_switch_confirmation_can_be_declined_without_typing_anything() {
    let mut lab = Lab::new("tui-no");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(&lab, "a1", &pane, &[], &["--account", DEFAULT_PROFILE]);
    let before = session_id_of(&lab, "a1");

    let mut tui = Tui::attach(&lab);
    // Captured after the attach: the client's own resize reflows the pane,
    // and only what happens *after* that is the modal's doing.
    tui.wait_for("accounts-lab");
    let screen_before = screen(&lab, &pane);

    open_switch_picker(&mut tui, 80);
    tui.send("\r");
    tui.wait_for("↵ confirm");
    tui.send("n");
    tui.wait_for("nothing was sent to the pane");

    // The pane is exactly as it was, and so is the agent.
    assert_eq!(
        screen(&lab, &pane),
        screen_before,
        "a declined switch must not type a byte"
    );
    let agent = agent_of(&lab, "a1");
    assert_eq!(agent["tokens"]["account"], DEFAULT_PROFILE);
    assert_eq!(
        agent["agent_session"]["value"].as_str(),
        Some(before.as_str())
    );
    assert!(
        !lab.profile_dir(SECOND_PROFILE)
            .join("last-launch.json")
            .exists(),
        "nothing may have been launched under the other profile"
    );

    // The picker is usable again: a refusal that reached nothing is not an
    // outcome, so another account can be chosen from the same modal.
    assert!(
        tui.contains("switch account"),
        "the picker stays on screen: {}",
        tui.visible_screen()
    );
}

/// An agent with no session id: `/exit` would throw the conversation away, so
/// the protocol refuses before a byte is typed and the modal says so.
#[test]
fn the_tui_switch_refuses_an_agent_with_no_session_id() {
    let mut lab = Lab::new("tui-nose");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_agent_in(
        &lab,
        "a1",
        &pane,
        &["FAKE_CLAUDE_NO_SESSION=1"],
        &["--account", DEFAULT_PROFILE],
    );
    assert!(
        agent_of(&lab, "a1")["agent_session"].is_null(),
        "the stub must have reported no session id"
    );

    let mut tui = Tui::attach(&lab);
    tui.wait_for("accounts-lab");
    let screen_before = screen(&lab, &pane);

    open_switch_picker(&mut tui, 80);
    tui.send("\r");
    tui.wait_for("has no Claude session id");

    // No question was ever put, and nothing reached the pane.
    assert!(
        !tui.contains("↵ confirm"),
        "a switch that cannot run must not ask: {}",
        tui.visible_screen()
    );
    assert_eq!(
        screen(&lab, &pane),
        screen_before,
        "a refusal must not type a byte"
    );
    assert_eq!(agent_of(&lab, "a1")["tokens"]["account"], DEFAULT_PROFILE);
}

// ---------------------------------------------------------------------------
// PR 10 — `herdr account watch`
//
// The watcher is a long-running process that writes metadata onto agents it did
// not start, so every test below asserts on both halves of that: what it puts
// on an agent it is allowed to write to, and what it does *not* put on one it
// is not. `--once` drives the deterministic assertions; the label lifecycle is
// driven through a real background watcher, because "the badge appears while
// you are looking at it and goes away again" is the whole feature.
// ---------------------------------------------------------------------------

/// A pane whose `claude` prints the reconstructed limit screen on `/limit`.
fn start_limitable_agent(lab: &Lab, name: &str, pane: &str, account: &[&str]) {
    let fixture = format!(
        "FAKE_CLAUDE_LIMIT_FILE='{}'",
        usage_limit_fixture().display()
    );
    start_agent_in(lab, name, pane, &[&fixture], account);
}

/// Drive the stub with a raw `pane.send_text`, not `agent.prompt`.
///
/// A limited agent is `blocked`, and the stock server refuses `agent.prompt` on
/// a blocked agent (`src/app/api/agents.rs`) — the same refusal PR 5's switch
/// protocol has to work around. Typing into the pane is what a human does.
fn type_into_pane(lab: &Lab, pane: &str, line: &str) {
    let sent = lab.herdr(&["pane", "send-text", pane, &format!("{line}\r")]);
    assert!(sent.status.success(), "{}", stderr_of(&sent));
}

/// The three facts every watch assertion is about.
fn label_of(lab: &Lab, target: &str) -> (Option<String>, Option<String>) {
    let agent = agent_of(lab, target);
    (
        agent["tokens"]["account_state"]
            .as_str()
            .map(str::to_string),
        agent["state_labels"]["blocked"]
            .as_str()
            .map(str::to_string),
    )
}

/// Poll until the pane carries (or stops carrying) the watcher's label.
fn wait_for_label(lab: &Lab, target: &str, want: Option<&str>) -> (Option<String>, Option<String>) {
    let mut last = (None, None);
    for _ in 0..120 {
        last = label_of(lab, target);
        let matches = match want {
            Some(label) => last.1.as_deref() == Some(label),
            None => last.1.is_none(),
        };
        if matches {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("agent {target} never reached state_labels.blocked = {want:?}; last: {last:?}");
}

/// Drain a running watcher's `--json` events into a channel as they arrive.
///
/// A watcher is a stream. `wait_with_output` can only say what it printed once
/// it is dead, which is no use to a test that has to know when the watcher has
/// *seen* something before it does the next thing — and a pipe nobody drains is
/// a pipe that can fill under a chatty run.
fn watcher_events(child: &mut std::process::Child) -> std::sync::mpsc::Receiver<serde_json::Value> {
    let stdout = child.stdout.take().expect("the watcher's stdout is piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if line.trim().is_empty() {
                continue;
            }
            let event: serde_json::Value =
                serde_json::from_str(&line).expect("one JSON object per line");
            if sender.send(event).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Wait until the watcher reports `event`, or give up.
fn next_event(
    events: &std::sync::mpsc::Receiver<serde_json::Value>,
    event: &str,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match events.recv_timeout(left) {
            Ok(value) if value["event"] == event => return value,
            Ok(_) => continue,
            Err(_) => panic!("the watcher never reported a {event:?} event"),
        }
    }
}

/// Ctrl-C the watcher and wait for it, the way a human ends one.
fn interrupt(child: &mut std::process::Child) -> std::process::ExitStatus {
    let sent = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT to the watcher");
    assert!(sent.success());
    child.wait().expect("the watcher exits")
}

/// The whole feature, through a watcher that is really running: a limit arrives
/// mid-session, the badge appears within one interval, and when the limit lifts
/// the badge goes away and the account state the launcher wrote comes back.
#[test]
fn account_watch_labels_a_limited_agent_and_clears_it_again() {
    let mut lab = Lab::new("watch");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_limitable_agent(&lab, "a1", &pane, &["--account", DEFAULT_PROFILE]);
    assert_eq!(
        label_of(&lab, "a1"),
        (Some("ok".to_string()), None),
        "the launcher's state, before any watcher"
    );

    let watcher = lab.herdr_spawn(&["account", "watch", "--interval", "500", "--json"]);

    type_into_pane(&lab, &pane, "/limit");
    wait_for_agent_status(&lab, "a1", "blocked");
    let labelled = wait_for_label(&lab, "a1", Some("usage limit"));
    assert_eq!(
        labelled,
        (Some("limited".to_string()), Some("usage limit".to_string())),
        "a limited agent carries both the token and the label"
    );

    // The limit lifts. On screen that is Claude redrawing its prompt box, which
    // is what pushes the notice out of `after_last_horizontal_rule`.
    type_into_pane(&lab, &pane, "/redraw");
    let cleared = wait_for_label(&lab, "a1", None);
    assert_eq!(
        cleared,
        (Some("ok".to_string()), None),
        "the clear restores the account state the label replaced"
    );

    // Interrupting the watcher is a clean exit, and it takes nothing with it
    // that was not already gone.
    let interrupted = std::process::Command::new("kill")
        .args(["-INT", &watcher.id().to_string()])
        .status()
        .expect("send SIGINT to the watcher");
    assert!(interrupted.success());
    let output = watcher.wait_with_output().expect("watcher exits");
    assert_eq!(
        output.status.code(),
        Some(0),
        "Ctrl-C is a clean exit: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect();
    assert!(!events.is_empty(), "the watcher reported nothing at all");
    assert_eq!(events[0]["event"], "limited", "{events:#?}");
    assert_eq!(events[0]["account"], DEFAULT_PROFILE, "{events:#?}");
    assert_eq!(events[0]["reset_text"], "3pm", "{events:#?}");
    assert_eq!(events[0]["pane_id"].as_str(), Some(pane.as_str()));
    assert!(
        events.iter().any(|event| event["event"] == "cleared"),
        "the watcher says when a limit lifted: {events:#?}"
    );
}

/// The rule that keeps the watcher honest against a real server: an agent whose
/// account herdr cannot name is never written to, however limited it looks.
#[test]
fn account_watch_leaves_an_agent_without_an_account_alone() {
    let mut lab = Lab::new("watch-none");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_limitable_agent(&lab, "a1", &pane, &["--account", "none"]);
    let before = agent_of(&lab, "a1");
    assert!(
        before["tokens"].get("account").is_none(),
        "the fixture must have no account token: {before:#?}"
    );

    type_into_pane(&lab, &pane, "/limit");
    wait_for_matched_rule(&lab, "a1", "usage_limit");
    wait_for_agent_status(&lab, "a1", "blocked");

    // A positive control in the very same pass, so this test cannot pass with
    // the watcher broken: an agent herdr *can* attribute, limited on the same
    // reconstructed screen, must come back labelled from the same `--once`.
    let other = split_pane(&lab);
    start_limitable_agent(&lab, "a2", &other, &["--account", DEFAULT_PROFILE]);
    type_into_pane(&lab, &other, "/limit");
    wait_for_agent_status(&lab, "a2", "blocked");

    let watched = lab.herdr(&["account", "watch", "--once", "--json"]);
    assert!(watched.status.success(), "{}", stderr_of(&watched));
    let events: Vec<serde_json::Value> = stdout_of(&watched)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect();
    assert_eq!(
        events.len(),
        1,
        "exactly one of the two limited agents is the watcher's business: {events:#?}"
    );
    assert_eq!(events[0]["event"], "limited", "{events:#?}");
    assert_eq!(
        events[0]["pane_id"].as_str(),
        Some(other.as_str()),
        "{events:#?}"
    );
    assert_eq!(
        label_of(&lab, "a2"),
        (Some("limited".to_string()), Some("usage limit".to_string())),
        "the agent whose account herdr knows is labelled"
    );
    assert_eq!(
        label_of(&lab, "a1"),
        (None, None),
        "a genuinely limited agent with no account token is left exactly as it was"
    );
}

/// The hand-over and the exit promise, on one pane.
///
/// A watcher does not always start on a clean machine: a killed one, a `--once`
/// pass or a second `watch` in another terminal leaves a *leased* `limited`
/// behind, and the next watcher's first sight of that pane must not read its
/// own label as the account state it is replacing — restoring that on the way
/// out writes `limited` with no TTL and turns an expiring badge into a
/// permanent one. Then the opposite promise, `--keep-labels`, on the same pane.
#[test]
fn account_watch_takes_over_a_label_and_takes_it_off_on_exit() {
    let mut lab = Lab::new("watch-2nd");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_limitable_agent(&lab, "a1", &pane, &["--account", DEFAULT_PROFILE]);
    type_into_pane(&lab, &pane, "/limit");
    wait_for_agent_status(&lab, "a1", "blocked");

    // Stand in for a watcher that stopped without clearing: a lease long enough
    // that nothing but another watcher can take this label off.
    let first = lab.herdr(&["account", "watch", "--once", "--interval", "60000"]);
    assert!(first.status.success(), "{}", stderr_of(&first));
    assert_eq!(
        label_of(&lab, "a1"),
        (Some("limited".to_string()), Some("usage limit".to_string()))
    );

    let mut second = lab.herdr_spawn(&["account", "watch", "--interval", "500", "--json"]);
    let events = watcher_events(&mut second);
    next_event(&events, "limited");
    assert_eq!(
        interrupt(&mut second).code(),
        Some(0),
        "Ctrl-C is a clean exit"
    );
    assert_eq!(
        label_of(&lab, "a1"),
        (None, None),
        "the label comes off, and the watcher's own `limited` is never restored          as the state it replaced — that would outlive every lease"
    );

    let mut kept = lab.herdr_spawn(&[
        "account",
        "watch",
        "--interval",
        "500",
        "--json",
        "--keep-labels",
    ]);
    let kept_events = watcher_events(&mut kept);
    next_event(&kept_events, "limited");
    assert_eq!(interrupt(&mut kept).code(), Some(0));
    assert_eq!(
        label_of(&lab, "a1"),
        (Some("limited".to_string()), Some("usage limit".to_string())),
        "--keep-labels leaves the badge to its lease instead"
    );
}

/// Tokens are in-memory only, so a server restart takes every label with it and
/// no status change follows to announce that. The watcher reconciles what the
/// server holds against what it believes on every pass, which is what repairs
/// that — driven here by stripping the label out from under it.
#[test]
fn account_watch_reports_a_label_that_went_missing_again() {
    let mut lab = Lab::new("watch-again");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_limitable_agent(&lab, "a1", &pane, &["--account", DEFAULT_PROFILE]);
    type_into_pane(&lab, &pane, "/limit");
    wait_for_agent_status(&lab, "a1", "blocked");

    let first = lab.herdr(&["account", "watch", "--once", "--json"]);
    assert!(first.status.success(), "{}", stderr_of(&first));
    assert_eq!(
        label_of(&lab, "a1"),
        (Some("limited".to_string()), Some("usage limit".to_string()))
    );

    // `--once` leaves what it labelled to its lease rather than clearing it,
    // which is what makes a one-shot pass useful at all.
    let stripped = lab.herdr(&[
        "pane",
        "report-metadata",
        &pane,
        "--source",
        "fork:accounts",
        "--agent",
        "claude",
        "--clear-token",
        "account_state",
        "--clear-state-labels",
    ]);
    assert!(stripped.status.success(), "{}", stderr_of(&stripped));
    assert_eq!(label_of(&lab, "a1"), (None, None));

    let second = lab.herdr(&["account", "watch", "--once", "--json"]);
    assert!(second.status.success(), "{}", stderr_of(&second));
    assert_eq!(
        label_of(&lab, "a1"),
        (Some("limited".to_string()), Some("usage limit".to_string())),
        "the next pass puts the label back"
    );
}

/// The safety property a long-running labeller owes: a watcher that stops —
/// killed, crashed, machine gone — cannot leave a `limited` badge behind. The
/// label is a lease the server expires on its own.
#[test]
fn account_watch_labels_expire_without_a_watcher() {
    let mut lab = Lab::new("watch-ttl");
    assert!(lab.up().status.success());

    let pane = lab.pane_id();
    start_limitable_agent(&lab, "a1", &pane, &["--account", DEFAULT_PROFILE]);
    type_into_pane(&lab, &pane, "/limit");
    wait_for_agent_status(&lab, "a1", "blocked");

    // The shortest interval the CLI accepts, so the lease is its floor.
    let watched = lab.herdr(&["account", "watch", "--once", "--interval", "500"]);
    assert!(watched.status.success(), "{}", stderr_of(&watched));
    assert_eq!(
        label_of(&lab, "a1"),
        (Some("limited".to_string()), Some("usage limit".to_string()))
    );

    // No watcher is running now, and the agent is still blocked on the limit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    loop {
        if label_of(&lab, "a1") == (None, None) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the label outlived its lease: {:?}",
            label_of(&lab, "a1")
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert_eq!(
        agent_of(&lab, "a1")["agent_status"],
        "blocked",
        "the agent is still limited; only the watcher's claim about it expired"
    );
    assert_eq!(
        agent_of(&lab, "a1")["tokens"]["account"],
        DEFAULT_PROFILE,
        "the launcher's untimed account token is untouched by the lease"
    );
}

/// A mistyped interval is a refusal, not a silently different watcher.
#[test]
fn account_watch_refuses_an_interval_it_cannot_honour() {
    let mut lab = Lab::new("watch-args");
    assert!(lab.up().status.success());

    for args in [
        vec!["account", "watch", "--interval", "1"],
        vec!["account", "watch", "--interval", "600000"],
        vec!["account", "watch", "--interval"],
        vec!["account", "watch", "--interval", "soon"],
        vec!["account", "watch", "--nope"],
    ] {
        let output = lab.herdr(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must be a usage error: {}{}",
            stdout_of(&output),
            stderr_of(&output)
        );
        assert!(
            stdout_of(&output).is_empty(),
            "a refused watch prints nothing on stdout: {}",
            stdout_of(&output)
        );
    }
}
