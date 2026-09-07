//! `herdr account` — the fork's Claude account profiles.
//!
//! PR 1 ships the read-only half. `list` never contacts a server and never
//! reads a secret: it merges `[[accounts]]` with the CLI-managed store and
//! reports each profile's health from the filesystem.
//!
//! Exit codes: 0 for a report, 1 when the configuration has diagnostics, 2 for
//! a usage error.

use serde::Serialize;

use crate::accounts::layout::{self, InspectOptions};
use crate::accounts::profile::{load_profiles, Profiles};

pub(super) const ACCOUNT_USAGE: &str = "Usage:
  herdr account list [--json]

An account is one Claude config directory (CLAUDE_CONFIG_DIR) with its own
credentials. Declare profiles as [[accounts]] in config.toml, or let
`herdr account add` manage them in <config>/accounts/profiles.toml.

list reads configuration and the profile directories only: it contacts no
server, and never reads or prints credentials.";

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

fn rows_for(profiles: &Profiles) -> Vec<AccountListRow> {
    let default = profiles
        .default_profile()
        .map(|profile| profile.name.clone());
    profiles
        .iter()
        .map(|profile| {
            // Health only: `list` must not parse a multi-megabyte
            // `.claude.json` for a name it does not print.
            let inspection = layout::inspect(profile, InspectOptions::health());
            AccountListRow {
                name: profile.name.clone(),
                agent: profile.agent.as_str(),
                config_dir: profile.config_dir.display().to_string(),
                default: default.as_deref() == Some(profile.name.as_str()),
                origin: profile.origin.as_str(),
                dir_exists: inspection.dir_exists,
                logged_in: inspection.logged_in,
                hook_installed: inspection.hook_installed,
            }
        })
        .collect()
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
    fn usage_documents_that_list_reads_no_secrets() {
        assert!(ACCOUNT_USAGE.contains("herdr account list"));
        assert!(ACCOUNT_USAGE.contains("credentials"));
    }
}
