//! `herdr gateway` — the fork's headless HTTP/WebSocket surface over the fleet.
//!
//! The whole module lives behind the `gateway` cargo feature (on by default in
//! fork builds). A `--no-default-features` build has no `mod gateway`, no CLI
//! arm and no `gateway` entry in the command spec, so it is upstream-shaped and
//! links none of the feature's optional dependencies.
//!
//! This file is the single CLI entry point for the epic: later PRs add the
//! `pair`/`status`/`rotate-token` subcommands and the bare `herdr gateway` run
//! here rather than a second dispatch in [`crate::cli`]. Today only `help` is
//! implemented, so every other invocation is a usage error.
//!
//! Exit codes follow the fleet CLI's convention: 0 when help was printed,
//! 2 for a usage error.

mod auth;
mod paths;
mod policy;

/// The invocation line, shared with `herdr --help` so the two never drift.
pub(crate) const GATEWAY_COMMAND_LINE: &str = "herdr gateway [--bind ADDR] [--config PATH]";

/// Both help surfaces carry this while the run path is unbuilt, so the options
/// they advertise never read as working ones. Delete it, and
/// `the_staging_note_matches_the_dispatch`, in the PR that serves requests.
pub(crate) const GATEWAY_STAGING_NOTE: &str =
    "Not implemented yet: this build accepts only `herdr gateway help`.";

pub(crate) fn run_gateway_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("help" | "--help" | "-h") if args.len() == 1 => {
            crate::platform::begin_cli_output();
            std::print!("{}", gateway_help());
            Ok(0)
        }
        _ => {
            crate::platform::begin_cli_output();
            std::eprint!("{}", gateway_help());
            Ok(2)
        }
    }
}

fn gateway_help() -> String {
    let mut help = String::new();
    help.push_str("Serve the fleet over HTTP and WebSocket\n\n");
    help.push_str(&format!("usage: {GATEWAY_COMMAND_LINE}\n\n"));
    help.push_str("Options:\n");
    help.push_str("  --bind ADDR         Listen on ADDR instead of the configured address\n");
    help.push_str("  --config PATH       Read configuration from PATH\n");
    help.push_str(&format!("\n{GATEWAY_STAGING_NOTE}\n"));
    help
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn help_exits_zero() {
        assert_eq!(run_gateway_command(&args(&["help"])).expect("help"), 0);
        assert_eq!(run_gateway_command(&args(&["--help"])).expect("help"), 0);
        assert_eq!(run_gateway_command(&args(&["-h"])).expect("help"), 0);
    }

    #[test]
    fn no_subcommand_is_a_usage_error() {
        assert_eq!(run_gateway_command(&[]).expect("usage"), 2);
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        assert_eq!(run_gateway_command(&args(&["bogus"])).expect("usage"), 2);
        // `help` only stands alone; `help bogus` is still a usage error.
        assert_eq!(
            run_gateway_command(&args(&["help", "bogus"])).expect("usage"),
            2
        );
    }

    #[test]
    fn help_text_mentions_the_usage_line_and_every_option() {
        let help = gateway_help();
        assert!(
            help.contains(&format!("usage: {GATEWAY_COMMAND_LINE}")),
            "help: {help}"
        );
        assert!(help.contains("--bind ADDR"), "help: {help}");
        assert!(help.contains("--config PATH"), "help: {help}");
    }

    /// The staging note is only honest while nothing but `help` succeeds, so it
    /// is pinned to the dispatch: the PR that makes `--bind` run a server fails
    /// here and must remove the note from both help surfaces.
    #[test]
    fn the_staging_note_matches_the_dispatch() {
        assert!(gateway_help().contains(GATEWAY_STAGING_NOTE));
        assert_eq!(
            run_gateway_command(&args(&["--bind", "127.0.0.1:7788"])).expect("usage"),
            2
        );
    }
}
