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
//! Exit codes: 0 for a report or a completed operation, 1 when a read-only
//! report found configuration diagnostics or when an operation failed, 2 for a
//! usage error. A completed operation that degraded — an entry the platform
//! could not share, a hook that did not install — still exits 0 and says so on
//! stderr, because the profile it just created is real and usable.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::accounts::config::{AccountProfileConfig, DEFAULT_AGENT};
use crate::accounts::launch::{env_assignment_line, pane_shell_at_prompt, LaunchError};
use crate::accounts::layout::{self, InspectOptions, SeedPlan, SeedReport};
use crate::accounts::profile::{
    self, load_profiles, validate_name, AccountAgent, AccountProfile, ProfileOrigin, Profiles,
};
use crate::accounts::store;
use crate::api::schema::{
    AgentInfo, AgentReadParams, AgentStatus, AgentTarget, EmptyParams, Method, PaneCurrentParams,
    PaneProcessInfo, PaneProcessInfoParams, PaneSendTextParams, ReadFormat, ReadSource, Request,
};

pub(super) const ACCOUNT_USAGE: &str = "Usage:
  herdr account list [--json]
  herdr account add <name> [--config-dir <path>] [--from <profile>]
                           [--dry-run] [--force] [--print-config]
                           [--no-hook] [--json]
  herdr account remove <name> [--delete-dir]
  herdr account default <name>
  herdr account status [<name>] [--json]
  herdr account login <name> [--pane <id>]

An account is one Claude config directory (CLAUDE_CONFIG_DIR) with its own
credentials. Declare profiles as [[accounts]] in config.toml, or let
`herdr account add` manage them in <config>/accounts/profiles.toml.

add seeds the new directory from an existing profile: transcripts and other
shared state are symlinked, settings and .claude.json are copied with the
identity keys removed, and credentials are never copied — the new profile is
logged out until `herdr account login <name>`.

status and login talk to the local herdr server; the rest contact none. No
command ever reads or prints the contents of a credentials file.";

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
        Some("status") => status(&args[1..]),
        Some("login") => login(&args[1..]),
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
        // A flag where a value belongs is a typo, never a value: `--from
        // --json` must not seed from a profile called "--json", and
        // `--config-dir --force` must not create a directory called
        // "--force".
        let value = |flag: &str| -> Result<String, String> {
            match args.get(index + 1) {
                Some(value) if !value.starts_with('-') => Ok(value.clone()),
                Some(value) => Err(format!(
                    "{flag} needs a value, but the next argument is {value:?}"
                )),
                None => Err(format!("{flag} needs a value")),
            }
        };
        match arg {
            "--config-dir" => {
                if parsed.config_dir.is_some() {
                    return Err("--config-dir was given more than once".to_string());
                }
                parsed.config_dir = Some(value("--config-dir")?);
                index += 2;
            }
            "--from" => {
                if parsed.from.is_some() {
                    return Err("--from was given more than once".to_string());
                }
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
    // Lexically *and* as the filesystem resolves it: `config_dir` comparison
    // alone cannot see `..` or a symlink, and a second profile pointing at an
    // existing profile's directory is a second name for one login.
    let target_key = layout::resolved_key(&target);
    if let Some(clash) = profiles.iter().find(|profile| {
        profile.config_dir == target || layout::resolved_key(&profile.config_dir) == target_key
    }) {
        eprintln!(
            "account profile {:?} already uses {}; two profiles must not share a directory",
            clash.name,
            clash.config_dir.display()
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
            // Seeding writes entry by entry and never rolls back, so say so
            // rather than let the user assume nothing happened.
            if plan.target.exists() {
                eprintln!(
                    "{} may be partially seeded; nothing was recorded in the account store",
                    plan.target.display()
                );
            }
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
        // Trust the exit code only as far as the evidence: the child reads
        // `CLAUDE_CONFIG_DIR` itself, so a success that landed the hook
        // somewhere else must not be reported as a hook in this profile.
        if target
            .join(layout::HOOK_DIR)
            .join(layout::HOOK_FILE)
            .is_file()
        {
            return Ok(());
        }
        return Err(format!(
            "`herdr integration install claude` reported success but left no hook in {}; \
             session ids will not be reported and switching accounts will not work; {rerun}",
            target.display()
        ));
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
    // Every comparison below is made on both spellings: `config_dir` is only
    // lexically normalized, so `..` and a symlinked ancestor would otherwise
    // walk a delete straight into `~/.claude` — which `remove_dir_all`
    // resolves even though this path never looked like it.
    let resolved = layout::resolved_key(dir);
    let same = |other: &Path| other == dir || layout::resolved_key(other) == resolved;
    let inside =
        |other: &Path| other.starts_with(dir) || layout::resolved_key(other).starts_with(&resolved);

    let home = profile::home_dir().map(|home| crate::accounts::config::dir_key(&home));
    if let Some(home) = home.as_deref() {
        if inside(home) {
            return Err(format!(
                "refusing to delete a home directory: {}",
                dir.display()
            ));
        }
    }
    if let Some(ambient) = ambient_claude_dir(home.as_deref()) {
        if inside(&ambient) {
            return Err(format!(
                "refusing to delete the ambient Claude directory: {}",
                dir.display()
            ));
        }
    }
    if let Some(other) = profiles
        .iter()
        .find(|profile| profile.name != name && same(&profile.config_dir))
    {
        return Err(format!(
            "refusing to delete {}: account profile {:?} also uses it",
            dir.display(),
            other.name
        ));
    }
    if let Some(other) = profiles
        .iter()
        .find(|profile| profile.name != name && inside(&profile.config_dir))
    {
        return Err(format!(
            "refusing to delete {}: account profile {:?} lives inside it ({})",
            dir.display(),
            other.name,
            other.config_dir.display()
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

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

const STATUS_USAGE: &str = "usage: herdr account status [<name>] [--json]";
const LOGIN_USAGE: &str = "usage: herdr account login <name> [--pane <id>]";

fn parse_status(args: &[String]) -> Result<(Option<String>, bool), String> {
    let mut name: Option<String> = None;
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => {
                if json {
                    return Err("--json was given more than once".to_string());
                }
                json = true;
            }
            other if other.starts_with('-') => return Err(format!("unknown flag {other:?}")),
            other => {
                if name.is_some() {
                    return Err(format!("unexpected argument {other:?}"));
                }
                name = Some(other.to_string());
            }
        }
    }
    Ok((name, json))
}

fn status(args: &[String]) -> std::io::Result<i32> {
    let (name, json) = match parse_status(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("herdr account status: {message}");
            eprintln!("{STATUS_USAGE}");
            return Ok(2);
        }
    };

    let config = crate::config::Config::load().config;
    let (profiles, diagnostics) = load_profiles(&config);

    // A name that is not a profile is an error, never an empty report: a typo
    // must not read as "this account is fine and idle".
    if let Some(name) = name.as_deref() {
        if profiles.get(name).is_none() {
            for diagnostic in &diagnostics {
                eprintln!("{diagnostic}");
            }
            eprintln!("unknown account profile {name:?} (see `herdr account list`)");
            return Ok(1);
        }
    }

    // Read-only and offline-tolerant: without a server the profile half of the
    // report is still complete and correct, so the agents column says it does
    // not know rather than the command failing.
    let (agents, agents_note) = agent_facts();

    let statuses = crate::accounts::status::assemble(
        &profiles,
        name.as_deref(),
        |profile| layout::inspect(profile, InspectOptions::with_identity()),
        agents.as_deref(),
    );

    crate::platform::begin_cli_output();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&statuses).map_err(std::io::Error::other)?
        );
    } else {
        print!("{}", crate::accounts::status::render_text(&statuses));
    }

    if let Some(note) = agents_note {
        eprintln!("note: {note}");
    }
    // The one blocked reason this command can offer a way out of. On stderr,
    // beside the warnings, so `--json` stays a clean document on stdout.
    let names = profiles.names();
    for status in &statuses {
        for agent in status.agents.iter().flatten() {
            let Some(limit) = agent.limit.as_ref() else {
                continue;
            };
            let others = crate::accounts::limit::alternatives(
                &names.iter().map(String::as_str).collect::<Vec<_>>(),
                Some(status.name.as_str()),
            );
            eprintln!(
                "{}: {}",
                agent.name.as_deref().unwrap_or("an agent"),
                crate::accounts::limit::hint(
                    limit,
                    Some(status.name.as_str()),
                    &agent.pane_id,
                    &others,
                )
            );
        }
    }
    // Reported against the full merged view, not the selected profile: an
    // agent stranded on a profile that no longer exists is exactly what a
    // single-profile `status` would otherwise hide.
    if let Some(agents) = agents.as_deref() {
        for orphan in crate::accounts::status::agents_on_unknown_profiles(&profiles, agents) {
            eprintln!(
                "warning: {} on pane {} claims account {:?}, which is not configured",
                orphan.name.as_deref().unwrap_or("an agent"),
                orphan.pane_id,
                orphan.account.as_deref().unwrap_or_default()
            );
        }
    }
    for diagnostic in &diagnostics {
        eprintln!("{diagnostic}");
    }
    Ok(i32::from(!diagnostics.is_empty()))
}

/// The running agents, or why they could not be listed.
///
/// Every failure degrades to "unknown": `status` is a read, and a herdr that
/// is not running is the normal state of a machine whose profiles someone is
/// checking before starting anything.
fn agent_facts() -> (
    Option<Vec<crate::accounts::status::AgentFact>>,
    Option<String>,
) {
    let response = match crate::cli::send_request(&Request {
        id: "cli:accounts:status:agent_list".into(),
        method: Method::AgentList(EmptyParams::default()),
    }) {
        Ok(response) => response,
        Err(err) if crate::cli::server_not_running_was_reported(&err) => {
            return (
                None,
                Some("no herdr server is running, so no agent placement is known".to_string()),
            );
        }
        Err(err) if crate::cli::protocol_mismatch_was_reported(&err) => {
            // The guard already printed the mismatch; do not say it twice.
            return (None, None);
        }
        Err(err) => {
            return (
                None,
                Some(format!(
                    "could not list agents ({err}); placement is unknown"
                )),
            );
        }
    };
    if let Some(error) = response.get("error") {
        return (
            None,
            Some(format!(
                "could not list agents ({error}); placement is unknown"
            )),
        );
    }
    match serde_json::from_value::<Vec<AgentInfo>>(response["result"]["agents"].clone()) {
        Ok(agents) => (
            Some(
                agents
                    .iter()
                    .map(|agent| {
                        crate::accounts::status::AgentFact::from_agent_info(agent)
                            .with_limit(usage_limit_of(agent))
                    })
                    .collect(),
            ),
            None,
        ),
        Err(err) => (
            None,
            Some(format!(
                "could not read the agent list ({err}); placement is unknown"
            )),
        ),
    }
}

/// What herdr's detector says about a blocked Claude agent's account usage.
///
/// Asked **only** for a Claude agent that is already blocked. `agent.explain`
/// is a per-agent round trip, and a status run on a busy machine must not turn
/// into one explain per pane: an idle or working agent is not waiting on a
/// usage limit, and an agent that is not Claude has no account to be limited.
///
/// Every failure answers `None` — "herdr did not see a limit", never "there is
/// no limit". `status` is a read that already degrades gracefully when no
/// server answers, and a limit hint is the least of what it owes the caller.
fn usage_limit_of(agent: &AgentInfo) -> Option<crate::accounts::limit::UsageLimit> {
    if agent.agent_status != AgentStatus::Blocked {
        return None;
    }
    if agent.agent.as_deref() != Some(crate::accounts::tokens::AGENT_LABEL) {
        return None;
    }
    let explain = agent_explain(&agent.pane_id)?;
    // Check the verdict before paying for the screen: the reset time is the
    // only thing the read contributes, and most blocked agents are blocked for
    // some other reason.
    if !crate::accounts::limit::matched_usage_limit(&explain) {
        return None;
    }
    let screen = detection_screen(&agent.pane_id).unwrap_or_default();
    crate::accounts::limit::classify(&explain, &screen)
}

/// One `agent.explain`, or `None` if anything at all went wrong.
pub(crate) fn agent_explain(target: &str) -> Option<serde_json::Value> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:explain".into(),
        method: Method::AgentExplain(AgentTarget {
            target: target.to_string(),
        }),
    })
    .ok()?;
    if response.get("error").is_some() {
        return None;
    }
    Some(response["result"]["explain"].clone())
}

/// The same bottom-buffer text the detector matched against, so the reset time
/// is read from the screen that produced the verdict rather than from the
/// user-visible viewport (which the user can scroll).
pub(crate) fn detection_screen(target: &str) -> Option<String> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:read".into(),
        method: Method::AgentRead(AgentReadParams {
            target: target.to_string(),
            source: ReadSource::Detection,
            lines: None,
            format: ReadFormat::Text,
            strip_ansi: true,
        }),
    })
    .ok()?;
    if response.get("error").is_some() {
        return None;
    }
    response["result"]["read"]["text"]
        .as_str()
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

/// How long to wait for the pane's shell to come back to its prompt after the
/// environment line.
///
/// Unlike `crate::accounts::client`, this wait is load-bearing rather than
/// politeness. The launch driver hands its second line to `agent.start`, which
/// refuses a busy pane itself; `login` submits its second line with a raw
/// `pane.send_text` that nothing on the server stands in front of, so a pane
/// that has not come back is a refusal. The budget is generous because a slow
/// prompt command is the ordinary reason to miss it.
const LOGIN_SETTLE_GRACE: std::time::Duration = std::time::Duration::from_millis(150);
const LOGIN_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(5_000);
const LOGIN_SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

fn parse_login(args: &[String]) -> Result<(String, Option<String>), String> {
    let mut name: Option<String> = None;
    let mut pane: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                if pane.is_some() {
                    return Err("--pane was given more than once".to_string());
                }
                // A flag where a pane id belongs is a typo. Typing into the
                // pane herdr happens to focus instead would send an export
                // line to whatever is running there.
                match args.get(index + 1) {
                    Some(value) if !value.starts_with('-') => {
                        pane = Some(crate::cli::normalize_pane_id(value));
                    }
                    Some(value) => {
                        return Err(format!(
                            "--pane needs a pane id, but the next argument is {value:?}"
                        ))
                    }
                    None => return Err("--pane needs a pane id".to_string()),
                }
                index += 2;
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
    let name = name.ok_or_else(|| "a profile name is required".to_string())?;
    Ok((name, pane))
}

fn login(args: &[String]) -> std::io::Result<i32> {
    let (name, pane) = match parse_login(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("herdr account login: {message}");
            eprintln!("{LOGIN_USAGE}");
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

    // Logging in creates this profile's credentials, so the directory has to
    // be the one `herdr account add` built. Letting Claude create a missing
    // one would produce an unseeded, world-readable directory that no other
    // herdr command knows how to repair.
    let inspection = layout::inspect(profile, InspectOptions::health());
    if !inspection.dir_exists {
        eprintln!(
            "account profile {name:?} points at {}, which does not exist; \
             run `herdr account add {name}` or fix its config_dir",
            profile.config_dir.display()
        );
        return Ok(1);
    }
    let Some(config_dir) = profile.config_dir.to_str() else {
        eprintln!(
            "account profile {name:?} has a config_dir that is not valid UTF-8 ({:?}) \
             and cannot be typed at a shell prompt",
            profile.config_dir
        );
        return Ok(1);
    };

    let pane_id = match resolve_pane(pane) {
        Ok(pane_id) => pane_id,
        Err(message) => {
            eprintln!("herdr account login: {message}");
            return Ok(1);
        }
    };

    // Everything that can refuse refuses before a byte reaches the pane.
    let info = match pane_process_info(&pane_id) {
        Ok(info) => info,
        Err(message) => {
            eprintln!("herdr account login: {message}");
            return Ok(1);
        }
    };
    let shell = match pane_shell_at_prompt(&info) {
        Ok(shell) => shell,
        // The shared refusal ends by offering `--account none`, which belongs
        // to `herdr agent start`. Point at this command's own way out instead.
        Err(LaunchError::PaneNotAtPrompt { pane_id, detail }) => {
            eprintln!(
                "pane {pane_id} is not at its shell prompt ({detail}); nothing was typed. \
                 Wait for the pane to return to its prompt, or name another one with --pane"
            );
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let line =
        match env_assignment_line(shell.family, profile.agent.config_dir_env_var(), config_dir) {
            Ok(line) => line,
            Err(error) => {
                eprintln!("{error}");
                return Ok(1);
            }
        };
    // Matched, never assumed: a second `AccountAgent` has to be a compile error
    // here rather than a `claude auth login` typed at a profile that is not
    // Claude's.
    let command = match profile.agent {
        AccountAgent::Claude => format!(
            "{} auth login",
            crate::detect::interactive_agent_executable(crate::detect::Agent::Claude)
        ),
    };
    let variable = profile.agent.config_dir_env_var();

    // Both warnings are about what completing this login will do, so they are
    // said while the user can still stop rather than after the fact.
    if inspection.logged_in {
        eprintln!(
            "warning: account profile {name:?} already has credentials; \
             completing this login replaces them"
        );
    }
    if !inspection.hook_installed {
        eprintln!(
            "warning: account profile {name:?} has no herdr session hook, so Claude will not \
             report its session id; run `CLAUDE_CONFIG_DIR={config_dir} herdr integration install claude`"
        );
    }

    crate::platform::begin_cli_output();
    if let Err(error) = send_line(&pane_id, &line) {
        eprintln!("herdr account login: {error}");
        if error.may_have_landed {
            // The request left this process and no reply came back, so the
            // pane's shell may already export the variable. Saying otherwise
            // would leave a shell pointing at a profile nobody mentioned.
            eprintln!(
                "note: pane {pane_id} may already export {variable}={config_dir:?}; check it \
                 before starting anything there"
            );
        }
        return Ok(1);
    }
    if !wait_for_prompt(&pane_id, shell.pid) {
        // `pane.send_text` has no busy check of its own, so typing the second
        // line now would feed `claude auth login` to whatever took the
        // foreground instead of to the shell.
        eprintln!(
            "herdr account login: pane {pane_id} did not come back to its shell prompt after \
             the environment line, so `{command}` was not typed. {variable} is exported in that \
             shell now, so running `{command}` there logs {name:?} in."
        );
        return Ok(1);
    }
    if let Err(error) = send_line(&pane_id, &command) {
        eprintln!(
            "herdr account login: {error}; {variable} was already exported in pane {pane_id}, \
             so running `{command}` there by hand logs the right profile in"
        );
        return Ok(1);
    }

    println!("typed into pane {pane_id}:");
    println!("  {}", line.trim_start());
    println!("  {command}");
    println!("Follow the login in pane {pane_id}, then run `herdr account status {name}`.");
    Ok(0)
}

/// The pane `login` types into: `--pane`, else the pane this command runs in,
/// else the focused one (`pane.current`).
fn resolve_pane(explicit: Option<String>) -> Result<String, String> {
    if let Some(pane_id) = explicit {
        return Ok(pane_id);
    }
    let caller_pane_id = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| crate::cli::normalize_pane_id(&value));
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:login:pane_current".into(),
        method: Method::PaneCurrent(PaneCurrentParams { caller_pane_id }),
    })
    .map_err(|err| format!("could not resolve the current pane: {err}"))?;
    if let Some(error) = response.get("error") {
        return Err(format!("could not resolve the current pane: {error}"));
    }
    response["result"]["pane"]["pane_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "the server named no current pane; pass --pane <id>".to_string())
}

/// Read a pane's process table.
///
/// Kept here rather than shared with `crate::accounts::client`: that module's
/// helpers carry launch-specific error context (whether the environment line
/// had already been typed) that `login` has no use for.
fn pane_process_info(pane_id: &str) -> Result<PaneProcessInfo, String> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:login:process_info".into(),
        method: Method::PaneProcessInfo(PaneProcessInfoParams {
            pane_id: Some(pane_id.to_owned()),
        }),
    })
    .map_err(|err| format!("pane.process_info failed: {err}"))?;
    if let Some(error) = response.get("error") {
        return Err(format!("pane.process_info failed: {error}"));
    }
    serde_json::from_value(response["result"]["process_info"].clone())
        .map_err(|err| format!("pane.process_info returned an unreadable process table: {err}"))
}

/// Why a line could not be submitted, and whether it may have reached the pane.
///
/// The same distinction `crate::accounts::client::apply_env` draws: a request
/// the server rejected wrote nothing to the pty, while a transport failure may
/// have delivered the text and lost only the reply. Only the caller can say
/// what a pane that may be holding an export line means for the user.
struct SendLineError {
    detail: String,
    may_have_landed: bool,
}

impl std::fmt::Display for SendLineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "pane.send_text failed: {}", self.detail)
    }
}

/// Submit one line at the pane's prompt.
fn send_line(pane_id: &str, line: &str) -> Result<(), SendLineError> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:login:send_text".into(),
        method: Method::PaneSendText(PaneSendTextParams {
            pane_id: pane_id.to_owned(),
            text: format!("{line}\r"),
        }),
    })
    .map_err(|err| SendLineError {
        detail: err.to_string(),
        // The request never reached the server, or its reply never came back.
        // Assuming nothing was typed would understate what the pane holds.
        may_have_landed: true,
    })?;
    match response.get("error") {
        // The server rejected the call (no such pane, bad text), so nothing
        // was written to the pty.
        Some(error) => Err(SendLineError {
            detail: error.to_string(),
            may_have_landed: false,
        }),
        None => Ok(()),
    }
}

/// Poll until the pane's own shell holds the foreground again.
///
/// `false` means the budget ran out; the caller refuses rather than typing into
/// whatever is there.
fn wait_for_prompt(pane_id: &str, shell_pid: u32) -> bool {
    std::thread::sleep(LOGIN_SETTLE_GRACE);
    let deadline = std::time::Instant::now() + LOGIN_SETTLE_TIMEOUT;
    loop {
        let settled = pane_process_info(pane_id)
            .ok()
            .and_then(|info| pane_shell_at_prompt(&info).ok())
            .is_some_and(|shell| shell.pid == shell_pid);
        if settled {
            return true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(LOGIN_SETTLE_POLL.min(remaining));
    }
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
            // A flag swallowed as a value would seed from — or create — a
            // directory named after the flag.
            vec!["work", "--config-dir", "--json"],
            vec!["work", "--from", "--dry-run"],
            // Two answers to one question: the silent last-wins would decide
            // which account's credentials directory this is.
            vec!["work", "--config-dir", "/a", "--config-dir", "/b"],
            vec!["work", "--from", "perso", "--from", "other"],
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

    /// `config_dir` is only lexically normalized, so two profiles can name one
    /// directory without looking alike. `--delete-dir` deletes credentials, so
    /// it has to see through both spellings.
    #[cfg(unix)]
    #[test]
    fn delete_dir_sees_through_dot_dot_and_symlinked_ancestors() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "herdr-account-delete-resolved-{}-{nanos}",
            std::process::id()
        ));
        let real = root.join("real");
        std::fs::create_dir_all(real.join("perso")).expect("perso dir");
        std::os::unix::fs::symlink(&real, root.join("link")).expect("symlink");

        let mut perso = profile("perso", true, ProfileOrigin::Config);
        perso.config_dir = crate::accounts::config::dir_key(&real.join("perso"));
        let mut work = profile("work", false, ProfileOrigin::Store);
        // The same directory, reached through a symlinked ancestor.
        work.config_dir = crate::accounts::config::dir_key(&root.join("link").join("perso"));
        let profiles = Profiles::test_new(vec![perso, work.clone()]);

        let error =
            may_delete_dir(&work.config_dir, &profiles, "work").expect_err("same real directory");
        assert!(error.contains("also uses it"), "{error}");

        // And a directory that merely *contains* another profile.
        let mut parent = profile("parent", false, ProfileOrigin::Store);
        parent.config_dir = crate::accounts::config::dir_key(&real);
        let profiles = Profiles::test_new(vec![
            parent.clone(),
            profiles.get("perso").expect("perso").clone(),
        ]);
        let error = may_delete_dir(&parent.config_dir, &profiles, "parent")
            .expect_err("contains a profile");
        assert!(error.contains("lives inside it"), "{error}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn status_parses_its_optional_name_and_flag() {
        let parse =
            |args: &[&str]| parse_status(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());

        assert_eq!(parse(&[]).expect("bare"), (None, false));
        assert_eq!(parse(&["--json"]).expect("json"), (None, true));
        assert_eq!(
            parse(&["work", "--json"]).expect("named"),
            (Some("work".to_string()), true)
        );
        assert_eq!(
            parse(&["--json", "work"]).expect("either order"),
            (Some("work".to_string()), true)
        );

        for bad in [
            vec!["work", "extra"],
            vec!["--unknown"],
            vec!["--json", "--json"],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn login_parses_its_name_and_pane() {
        let parse =
            |args: &[&str]| parse_login(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());

        assert_eq!(parse(&["work"]).expect("bare"), ("work".to_string(), None));
        assert_eq!(
            parse(&["work", "--pane", "%1.1"]).expect("pane"),
            ("work".to_string(), Some("%1.1".to_string()))
        );
        assert_eq!(
            parse(&["--pane", "%1.1", "work"]).expect("either order"),
            ("work".to_string(), Some("%1.1".to_string()))
        );

        for bad in [
            // No name: `login` must never guess which account to sign in.
            vec![],
            vec!["--pane", "%1.1"],
            vec!["work", "other"],
            vec!["work", "--unknown"],
            // A flag swallowed as a pane id would send the export line to
            // whichever pane herdr happens to focus.
            vec!["work", "--pane"],
            vec!["work", "--pane", "--json"],
            vec!["work", "--pane", "%1.1", "--pane", "%1.2"],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn usage_documents_every_subcommand() {
        for line in [
            "herdr account list",
            "herdr account add",
            "herdr account remove",
            "herdr account default",
            "herdr account status",
            "herdr account login",
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
