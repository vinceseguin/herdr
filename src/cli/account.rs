//! `herdr account` — the fork's Claude account profiles.
//!
//! `list` and the seeding half of `add` never contact a server: they merge
//! `[[accounts]]` with the CLI-managed store, report each profile's health
//! from the filesystem, and build a new profile directory out of an existing
//! one.
//!
//! Credentials are the line this module does not cross. `.credentials.json` is
//! never read, copied, linked, printed or logged — a seeded profile is logged
//! out until `herdr account login` — and `.claude.json` is copied only after
//! its identity keys are removed. `src/accounts/layout.rs` enforces both.
//!
//! Exit codes: 0 for a report, 1 when the configuration has diagnostics or the
//! operation failed, 2 for a usage error.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::accounts::config::{AccountProfileConfig, DEFAULT_AGENT};
use crate::accounts::layout::{self, InspectOptions, SeedPlan, SeedReport};
use crate::accounts::profile::{
    self, load_profiles, validate_name, AccountAgent, AccountProfile, ProfileOrigin, Profiles,
};
use crate::accounts::store;

pub(super) const ACCOUNT_USAGE: &str = "Usage:
  herdr account list [--json]
  herdr account add <name> [--config-dir <path>] [--from <profile>]
                           [--dry-run] [--force] [--print-config]
                           [--no-hook] [--json]
  herdr account remove <name> [--delete-dir]
  herdr account default <name>

An account is one Claude config directory (CLAUDE_CONFIG_DIR) with its own
credentials. Declare profiles as [[accounts]] in config.toml, or let
`herdr account add` manage them in <config>/accounts/profiles.toml.

add seeds the new directory from an existing profile: transcripts and other
shared state are symlinked, settings and .claude.json are copied with the
identity keys removed, and credentials are never copied — the new profile is
logged out until `herdr account login <name>`.

These commands contact no server, and never read or print credentials.";

const ADD_USAGE: &str = "usage: herdr account add <name> [--config-dir <path>] [--from <profile>] \
[--dry-run] [--force] [--print-config] [--no-hook] [--json]";
const REMOVE_USAGE: &str = "usage: herdr account remove <name> [--delete-dir]";
const DEFAULT_USAGE: &str = "usage: herdr account default <name>";

#[derive(Debug, Serialize)]
struct AccountListRow {
    name: String,
    agent: &'static str,
    config_dir: String,
    default: bool,
    origin: &'static str,
    dir_exists: bool,
    logged_in: bool,
    hook_installed: bool,
}

pub(super) fn run_account_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("default") => set_default(&args[1..]),
        Some("help" | "--help" | "-h") if args.len() == 1 => {
            println!("{ACCOUNT_USAGE}");
            Ok(0)
        }
        _ => {
            eprintln!("{ACCOUNT_USAGE}");
            Ok(2)
        }
    }
}

fn list(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr account list [--json]");
            return Ok(2);
        }
    };

    let config = crate::config::Config::load().config;
    let (profiles, diagnostics) = load_profiles(&config);
    let rows = rows_for(&profiles);

    crate::platform::begin_cli_output();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
    } else {
        print_rows(&rows);
    }

    for diagnostic in &diagnostics {
        eprintln!("{diagnostic}");
    }
    Ok(i32::from(!diagnostics.is_empty()))
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct AddArgs {
    name: String,
    config_dir: Option<String>,
    from: Option<String>,
    dry_run: bool,
    force: bool,
    print_config: bool,
    no_hook: bool,
    json: bool,
}

fn parse_add(args: &[String]) -> Result<AddArgs, String> {
    let mut parsed = AddArgs::default();
    let mut name: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let value = |flag: &str| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg {
            "--config-dir" => {
                parsed.config_dir = Some(value("--config-dir")?);
                index += 2;
            }
            "--from" => {
                parsed.from = Some(value("--from")?);
                index += 2;
            }
            "--dry-run" => {
                parsed.dry_run = true;
                index += 1;
            }
            "--force" => {
                parsed.force = true;
                index += 1;
            }
            "--print-config" => {
                parsed.print_config = true;
                index += 1;
            }
            "--no-hook" => {
                parsed.no_hook = true;
                index += 1;
            }
            "--json" => {
                parsed.json = true;
                index += 1;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other:?}")),
            other => {
                if name.is_some() {
                    return Err(format!("unexpected argument {other:?}"));
                }
                name = Some(other.to_string());
                index += 1;
            }
        }
    }
    parsed.name = name.ok_or_else(|| "a profile name is required".to_string())?;
    Ok(parsed)
}

#[derive(Debug, Serialize)]
struct AddOutcome<'a> {
    profile: &'a AccountListRow,
    seeded_from: String,
    dry_run: bool,
    created: bool,
    linked: &'a [PathBuf],
    copied: &'a [PathBuf],
    scrubbed: &'a [PathBuf],
    hook_installed: bool,
    stored: bool,
    warnings: &'a [String],
}

fn add(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_add(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("herdr account add: {message}");
            eprintln!("{ADD_USAGE}");
            return Ok(2);
        }
    };
    if let Err(reason) = validate_name(&parsed.name) {
        eprintln!(
            "herdr account add: invalid profile name {:?}: {reason}",
            parsed.name
        );
        return Ok(2);
    }

    let config = crate::config::Config::load().config;
    let (profiles, diagnostics) = load_profiles(&config);
    for diagnostic in &diagnostics {
        eprintln!("{diagnostic}");
    }
    if profiles.get(&parsed.name).is_some() {
        eprintln!(
            "account profile {:?} already exists (see `herdr account list`)",
            parsed.name
        );
        return Ok(1);
    }

    let home = profile::home_dir();
    let raw_target = parsed
        .config_dir
        .clone()
        .unwrap_or_else(|| format!("~/.claude-{}", parsed.name));
    let target = match absolute_dir(&raw_target, home.as_deref()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("herdr account add: {message}");
            return Ok(1);
        }
    };
    if let Some(clash) = profiles.iter().find(|profile| profile.config_dir == target) {
        eprintln!(
            "account profile {:?} already uses {}; two profiles must not share a directory",
            clash.name,
            target.display()
        );
        return Ok(1);
    }

    let source = match seed_source(&profiles, parsed.from.as_deref(), home.as_deref()) {
        Ok(source) => source,
        Err(message) => {
            eprintln!("herdr account add: {message}");
            return Ok(1);
        }
    };

    let plan = match layout::plan_seed(&source, &target) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("herdr account add: {err}");
            return Ok(1);
        }
    };

    crate::platform::begin_cli_output();

    if parsed.dry_run {
        let row = pending_row(&parsed.name, &target);
        return print_add_outcome(
            &parsed,
            &row,
            &plan,
            &SeedReport::default(),
            false,
            false,
            &[],
        );
    }

    let report = match layout::apply_seed(&plan, parsed.force) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("herdr account add: {err}");
            return Ok(1);
        }
    };

    let mut warnings = report.warnings.clone();
    let mut hook_installed = false;
    if parsed.no_hook {
        warnings.push(format!(
            "--no-hook: no session-start hook installed; run `CLAUDE_CONFIG_DIR={} herdr integration install claude` before switching accounts",
            target.display()
        ));
    } else {
        match install_hook(&target) {
            Ok(()) => hook_installed = true,
            Err(message) => warnings.push(message),
        }
    }

    let mut stored = false;
    if !parsed.print_config {
        match record_profile(&parsed.name, &target) {
            Ok(()) => stored = true,
            Err(message) => {
                eprintln!("herdr account add: {message}");
                eprintln!(
                    "the profile directory {} was created; add it to config.toml or rerun once the store is readable",
                    target.display()
                );
                return Ok(1);
            }
        }
    }

    let row = AccountListRow {
        name: parsed.name.clone(),
        agent: DEFAULT_AGENT,
        config_dir: target.display().to_string(),
        default: false,
        origin: if stored {
            ProfileOrigin::Store.as_str()
        } else {
            "unregistered"
        },
        dir_exists: true,
        logged_in: false,
        hook_installed,
    };
    print_add_outcome(
        &parsed,
        &row,
        &plan,
        &report,
        hook_installed,
        stored,
        &warnings,
    )
}

fn print_add_outcome(
    parsed: &AddArgs,
    row: &AccountListRow,
    plan: &SeedPlan,
    report: &SeedReport,
    hook_installed: bool,
    stored: bool,
    warnings: &[String],
) -> std::io::Result<i32> {
    if parsed.json {
        let outcome = AddOutcome {
            profile: row,
            seeded_from: plan.source.display().to_string(),
            dry_run: parsed.dry_run,
            created: report.created,
            linked: &report.linked,
            copied: &report.copied,
            scrubbed: &report.scrubbed,
            hook_installed,
            stored,
            warnings,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&outcome).map_err(std::io::Error::other)?
        );
    } else {
        if parsed.dry_run {
            println!(
                "would create {} (0700), seeded from {}",
                plan.target.display(),
                plan.source.display()
            );
        } else {
            println!(
                "{} {} (0700), seeded from {}",
                if report.created {
                    "created"
                } else {
                    "seeded into"
                },
                plan.target.display(),
                plan.source.display()
            );
        }
        for entry in &plan.links {
            println!("  link  {} -> {}", entry.entry, entry.source.display());
        }
        for entry in &plan.copies {
            let scrubbed = plan.scrub.contains(&entry.target);
            if scrubbed {
                println!("  copy  {} (identity keys removed)", entry.entry);
            } else {
                println!("  copy  {}", entry.entry);
            }
        }
        for skipped in &plan.skipped {
            println!("  skip  {skipped}");
        }
        if parsed.print_config {
            println!();
            println!("[[accounts]]");
            println!("name = \"{}\"", row.name);
            println!("agent = \"{}\"", row.agent);
            println!("config_dir = \"{}\"", row.config_dir);
            println!();
            println!(
                "Add the block above to config.toml; nothing was written to the account store."
            );
        }
        if !parsed.dry_run {
            println!(
                "Run `herdr account login {}` to sign this profile in.",
                row.name
            );
        }
    }
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    Ok(0)
}

/// Where a new profile is seeded from: `--from`, else the default profile,
/// else the ambient Claude directory.
fn seed_source(
    profiles: &Profiles,
    from: Option<&str>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(name) = from {
        return match profiles.get(name) {
            Some(profile) => Ok(profile.config_dir.clone()),
            None => Err(format!(
                "unknown source profile {name:?}; configured profiles: {}",
                if profiles.is_empty() {
                    "none".to_string()
                } else {
                    profiles.names().join(", ")
                }
            )),
        };
    }
    if let Some(profile) = profiles
        .default_profile()
        .filter(|profile| profile.config_dir.is_dir())
    {
        return Ok(profile.config_dir.clone());
    }
    let ambient = ambient_claude_dir(home);
    match ambient {
        Some(dir) if dir.is_dir() => Ok(dir),
        Some(dir) => Err(format!(
            "no profile to seed from: {} does not exist; pass --from <profile>",
            dir.display()
        )),
        None => Err("no profile to seed from and no home directory; pass --from <profile>".into()),
    }
}

/// The directory Claude Code uses when herdr applies no profile:
/// `CLAUDE_CONFIG_DIR` when it is set, else `~/.claude`. Mirrors
/// `crate::integration::env::claude_dir`, which is private to a module fork
/// work must not edit.
fn ambient_claude_dir(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(AccountAgent::Claude.config_dir_env_var())
        .filter(|value| !value.is_empty())
    {
        return Some(crate::accounts::config::dir_key(Path::new(&dir)));
    }
    home.map(|home| crate::accounts::config::dir_key(&home.join(".claude")))
}

fn absolute_dir(raw: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("--config-dir must not be empty".to_string());
    }
    if trimmed.starts_with('~') && home.is_none() {
        return Err(format!(
            "cannot expand {trimmed:?}: no home directory; pass an absolute --config-dir"
        ));
    }
    let expanded = profile::expand(trimmed, home.unwrap_or(Path::new("")));
    let normalized = crate::accounts::config::dir_key(&expanded);
    if !normalized.is_absolute() {
        return Err(format!(
            "--config-dir must be an absolute path after ~ expansion: {trimmed:?}"
        ));
    }
    Ok(normalized)
}

/// Install the herdr session-start hook into the new profile by re-running
/// this executable with `CLAUDE_CONFIG_DIR` pointed at it.
///
/// A subprocess rather than a call: `crate::integration` reads the ambient
/// environment, and `src/integration/**` is off limits to fork work. A failure
/// names the command to rerun; it never rolls the profile back.
fn install_hook(target: &Path) -> Result<(), String> {
    let rerun = format!(
        "run `CLAUDE_CONFIG_DIR={} herdr integration install claude` once it is fixed",
        target.display()
    );
    let executable = std::env::current_exe()
        .map_err(|err| format!("could not locate the herdr executable ({err}); {rerun}"))?;
    let output = std::process::Command::new(&executable)
        .args(["integration", "install", "claude"])
        .env(AccountAgent::Claude.config_dir_env_var(), target)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|err| format!("could not install the session-start hook ({err}); {rerun}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if detail.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        detail
    };
    Err(format!(
        "the session-start hook was not installed ({detail}); session ids will not be reported and switching accounts will not work; {rerun}"
    ))
}

/// Load the store, refusing to write over one herdr could not read.
fn store_for_write() -> Result<crate::accounts::store::AccountsStore, String> {
    let (store, diagnostics) = store::load();
    if diagnostics.is_empty() {
        return Ok(store);
    }
    Err(format!(
        "refusing to rewrite {}: {}",
        store::store_path().display(),
        diagnostics.join("; ")
    ))
}

fn record_profile(name: &str, target: &Path) -> Result<(), String> {
    let mut store = store_for_write()?;
    store.add(AccountProfileConfig {
        name: name.to_string(),
        agent: DEFAULT_AGENT.to_string(),
        config_dir: target.display().to_string(),
        default: false,
    })?;
    store::save(&store)
        .map_err(|err| format!("could not write {}: {err}", store::store_path().display()))
}

fn pending_row(name: &str, target: &Path) -> AccountListRow {
    let inspection = layout::inspect_dir(target, InspectOptions::health());
    AccountListRow {
        name: name.to_string(),
        agent: DEFAULT_AGENT,
        config_dir: target.display().to_string(),
        default: false,
        origin: "unregistered",
        dir_exists: inspection.dir_exists,
        logged_in: inspection.logged_in,
        hook_installed: inspection.hook_installed,
    }
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

fn remove(args: &[String]) -> std::io::Result<i32> {
    let (name, delete_dir) = match args {
        [name] if !name.starts_with('-') => (name.clone(), false),
        [name, flag] if !name.starts_with('-') && flag == "--delete-dir" => (name.clone(), true),
        [flag, name] if flag == "--delete-dir" && !name.starts_with('-') => (name.clone(), true),
        _ => {
            eprintln!("{REMOVE_USAGE}");
            return Ok(2);
        }
    };

    let config = crate::config::Config::load().config;
    let (profiles, diagnostics) = load_profiles(&config);
    for diagnostic in &diagnostics {
        eprintln!("{diagnostic}");
    }
    let Some(profile) = profiles.get(&name) else {
        eprintln!("unknown account profile {name:?} (see `herdr account list`)");
        return Ok(1);
    };
    if profile.origin == ProfileOrigin::Config {
        eprintln!(
            "account profile {name:?} is declared as [[accounts]] in config.toml; remove the block there instead"
        );
        return Ok(1);
    }
    let config_dir = profile.config_dir.clone();

    if delete_dir {
        if let Err(message) = may_delete_dir(&config_dir, &profiles, &name) {
            eprintln!("herdr account remove: {message}");
            return Ok(1);
        }
    }

    let mut store = match store_for_write() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("herdr account remove: {message}");
            return Ok(1);
        }
    };
    if let Err(message) = store.remove(&name) {
        eprintln!("herdr account remove: {message}");
        return Ok(1);
    }
    if let Err(err) = store::save(&store) {
        eprintln!(
            "herdr account remove: could not write {}: {err}",
            store::store_path().display()
        );
        return Ok(1);
    }

    crate::platform::begin_cli_output();
    println!(
        "removed account profile {name} from {}",
        store::store_path().display()
    );
    if delete_dir {
        match std::fs::remove_dir_all(&config_dir) {
            Ok(()) => println!("deleted {}", config_dir.display()),
            Err(err) => {
                eprintln!(
                    "warning: the profile was removed but {} could not be deleted: {err}",
                    config_dir.display()
                );
                return Ok(1);
            }
        }
    } else {
        println!(
            "{} was left in place (pass --delete-dir to remove it)",
            config_dir.display()
        );
    }
    Ok(0)
}

/// Whether `--delete-dir` may remove this directory.
///
/// Deleting a profile directory deletes that account's login, so every way a
/// directory could be someone else's is refused: a shared directory, the
/// ambient Claude directory, a home directory, a filesystem root, or a
/// symlink pointing somewhere else entirely.
fn may_delete_dir(dir: &Path, profiles: &Profiles, name: &str) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|err| format!("cannot delete {}: {err}", dir.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing to delete {}: it is a symlink, not a profile directory",
            dir.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "refusing to delete {}: not a directory",
            dir.display()
        ));
    }
    if dir.parent().is_none() {
        return Err(format!(
            "refusing to delete a filesystem root: {}",
            dir.display()
        ));
    }
    let home = profile::home_dir().map(|home| crate::accounts::config::dir_key(&home));
    if home.as_deref() == Some(dir) {
        return Err(format!(
            "refusing to delete a home directory: {}",
            dir.display()
        ));
    }
    if ambient_claude_dir(home.as_deref()).as_deref() == Some(dir) {
        return Err(format!(
            "refusing to delete the ambient Claude directory: {}",
            dir.display()
        ));
    }
    if let Some(other) = profiles
        .iter()
        .find(|profile| profile.name != name && profile.config_dir == dir)
    {
        return Err(format!(
            "refusing to delete {}: account profile {:?} also uses it",
            dir.display(),
            other.name
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// default
// ---------------------------------------------------------------------------

fn set_default(args: &[String]) -> std::io::Result<i32> {
    let [name] = args else {
        eprintln!("{DEFAULT_USAGE}");
        return Ok(2);
    };
    if name.starts_with('-') {
        eprintln!("{DEFAULT_USAGE}");
        return Ok(2);
    }

    let config = crate::config::Config::load().config;
    let (profiles, diagnostics) = load_profiles(&config);
    for diagnostic in &diagnostics {
        eprintln!("{diagnostic}");
    }
    if profiles.get(name).is_none() {
        eprintln!("unknown account profile {name:?} (see `herdr account list`)");
        return Ok(1);
    }

    let mut store = match store_for_write() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("herdr account default: {message}");
            return Ok(1);
        }
    };
    if let Err(message) = store.set_default(name) {
        eprintln!("herdr account default: {message}");
        return Ok(1);
    }
    if let Err(err) = store::save(&store) {
        eprintln!(
            "herdr account default: could not write {}: {err}",
            store::store_path().display()
        );
        return Ok(1);
    }

    crate::platform::begin_cli_output();
    println!("default account profile is now {name}");
    Ok(0)
}

fn rows_for(profiles: &Profiles) -> Vec<AccountListRow> {
    let default = profiles
        .default_profile()
        .map(|profile| profile.name.clone());
    profiles
        .iter()
        .map(|profile| row_for(profile, default.as_deref() == Some(profile.name.as_str())))
        .collect()
}

fn row_for(profile: &AccountProfile, default: bool) -> AccountListRow {
    // Health only: `list` must not parse a multi-megabyte `.claude.json` for a
    // name it does not print.
    let inspection = layout::inspect(profile, InspectOptions::health());
    AccountListRow {
        name: profile.name.clone(),
        agent: profile.agent.as_str(),
        config_dir: profile.config_dir.display().to_string(),
        default,
        origin: profile.origin.as_str(),
        dir_exists: inspection.dir_exists,
        logged_in: inspection.logged_in,
        hook_installed: inspection.hook_installed,
    }
}

fn print_rows(rows: &[AccountListRow]) {
    if rows.is_empty() {
        println!("No account profiles configured.");
        println!("Add [[accounts]] to config.toml (see `herdr --default-config`).");
        return;
    }
    for row in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.name,
            row.agent,
            row.config_dir,
            if row.default { "default" } else { "-" },
            row.origin,
            if row.dir_exists { "dir" } else { "no-dir" },
            if row.logged_in {
                "logged-in"
            } else {
                "logged-out"
            },
            if row.hook_installed {
                "hook"
            } else {
                "no-hook"
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::accounts::profile::{AccountAgent, AccountProfile, ProfileOrigin};

    fn profile(name: &str, default: bool, origin: ProfileOrigin) -> AccountProfile {
        AccountProfile {
            name: name.to_string(),
            agent: AccountAgent::Claude,
            config_dir: PathBuf::from(format!("/nonexistent/herdr-accounts/{name}")),
            default,
            origin,
        }
    }

    #[test]
    fn rows_mark_exactly_one_default() {
        let profiles = Profiles::test_new(vec![
            profile("perso", true, ProfileOrigin::Config),
            profile("work", false, ProfileOrigin::Store),
        ]);
        let rows = rows_for(&profiles);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].default);
        assert!(!rows[1].default);
        assert_eq!(rows[0].origin, "config");
        assert_eq!(rows[1].origin, "store");
        assert!(
            !rows[0].dir_exists,
            "a missing directory is reported as missing"
        );
        assert!(!rows[0].logged_in);
        assert!(!rows[0].hook_installed);
    }

    #[test]
    fn rows_carry_no_credential_fields() {
        let profiles = Profiles::test_new(vec![profile("perso", true, ProfileOrigin::Config)]);
        let encoded = serde_json::to_string(&rows_for(&profiles)).expect("encode");
        for forbidden in ["credential", "token", "password", "secret", "oauth"] {
            assert!(
                !encoded.to_ascii_lowercase().contains(forbidden),
                "{forbidden} leaked into an account row: {encoded}"
            );
        }
    }

    #[test]
    fn an_empty_list_is_not_an_error() {
        assert!(rows_for(&Profiles::default()).is_empty());
    }

    #[test]
    fn add_parses_its_flags_and_refuses_the_rest() {
        let parse =
            |args: &[&str]| parse_add(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());

        let parsed = parse(&[
            "work",
            "--config-dir",
            "/p/work",
            "--from",
            "perso",
            "--json",
        ])
        .expect("parse");
        assert_eq!(parsed.name, "work");
        assert_eq!(parsed.config_dir.as_deref(), Some("/p/work"));
        assert_eq!(parsed.from.as_deref(), Some("perso"));
        assert!(parsed.json);
        assert!(!parsed.dry_run && !parsed.force && !parsed.print_config && !parsed.no_hook);

        let parsed = parse(&[
            "--dry-run",
            "--force",
            "--print-config",
            "--no-hook",
            "work",
        ])
        .expect("parse");
        assert!(parsed.dry_run && parsed.force && parsed.print_config && parsed.no_hook);

        for bad in [
            vec![],
            vec!["work", "extra"],
            vec!["work", "--unknown"],
            vec!["work", "--config-dir"],
            vec!["--from"],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_config_dir_is_expanded_and_must_end_up_absolute() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(
            absolute_dir("~/.claude-work", Some(&home)).expect("expand"),
            home.join(".claude-work")
        );
        assert_eq!(
            absolute_dir("/p/./work/", Some(&home)).expect("normalize"),
            PathBuf::from("/p/work")
        );
        assert!(absolute_dir("relative", Some(&home)).is_err());
        assert!(absolute_dir("   ", Some(&home)).is_err());
        assert!(absolute_dir("~/.claude", None).is_err());
    }

    #[test]
    fn the_seed_source_falls_back_from_explicit_to_default_to_ambient() {
        let profiles = Profiles::test_new(vec![
            profile("perso", true, ProfileOrigin::Config),
            profile("work", false, ProfileOrigin::Store),
        ]);
        // The lab and the tests point CLAUDE_CONFIG_DIR at a directory that
        // does not exist here, so the fallback is an error rather than a
        // silent seed from the wrong place.
        let missing_home = PathBuf::from("/nonexistent/herdr-accounts-home");

        assert!(matches!(
            seed_source(&profiles, Some("nope"), Some(&missing_home)),
            Err(message) if message.contains("unknown source profile")
        ));
        assert_eq!(
            seed_source(&profiles, Some("work"), Some(&missing_home)).expect("explicit"),
            profiles.get("work").expect("work").config_dir
        );
        // `perso`'s directory does not exist either, so neither it nor the
        // ambient directory can be used: never guess a source.
        assert!(seed_source(&profiles, None, Some(&missing_home)).is_err());
    }

    #[test]
    fn delete_dir_refuses_shared_missing_home_and_symlinked_directories() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "herdr-account-delete-{}-{nanos}",
            std::process::id()
        ));
        let shared = root.join("shared");
        std::fs::create_dir_all(&shared).expect("shared dir");

        let mut perso = profile("perso", true, ProfileOrigin::Config);
        perso.config_dir = crate::accounts::config::dir_key(&shared);
        let mut work = profile("work", false, ProfileOrigin::Store);
        work.config_dir = crate::accounts::config::dir_key(&shared);
        let profiles = Profiles::test_new(vec![perso, work]);

        // `work` is asked to delete a directory `perso` also uses.
        let error = may_delete_dir(
            &crate::accounts::config::dir_key(&shared),
            &profiles,
            "work",
        )
        .expect_err("shared");
        assert!(error.contains("also uses it"), "{error}");

        // A directory that is not there at all.
        let error = may_delete_dir(&root.join("gone"), &profiles, "work").expect_err("missing");
        assert!(error.contains("cannot delete"), "{error}");

        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&shared, &link).expect("symlink");
            let error = may_delete_dir(&link, &profiles, "work").expect_err("symlink");
            assert!(error.contains("symlink"), "{error}");
        }

        if let Some(home) = profile::home_dir() {
            let key = crate::accounts::config::dir_key(&home);
            let error = may_delete_dir(&key, &profiles, "work").expect_err("home");
            assert!(error.contains("home directory"), "{error}");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_documents_every_subcommand() {
        for line in [
            "herdr account list",
            "herdr account add",
            "herdr account remove",
            "herdr account default",
        ] {
            assert!(ACCOUNT_USAGE.contains(line), "{line}");
        }
        assert!(ACCOUNT_USAGE.contains("credentials are never copied"));
    }

    #[test]
    fn usage_documents_that_list_reads_no_secrets() {
        assert!(ACCOUNT_USAGE.contains("herdr account list"));
        assert!(ACCOUNT_USAGE.contains("credentials"));
    }
}
