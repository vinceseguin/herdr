//! `herdr gateway` — the fork's headless HTTP/WebSocket surface over the fleet.
//!
//! The whole module lives behind the `gateway` cargo feature (on by default in
//! fork builds). A `--no-default-features` build has no `mod gateway`, no CLI
//! arm and no `gateway` entry in the command spec, so it is upstream-shaped and
//! links none of the feature's optional dependencies.
//!
//! This file is the single CLI entry point for the epic: later PRs add the
//! `pair`/`status`/`rotate-token` subcommands here rather than a second
//! dispatch in [`crate::cli`]. Anything that is not a known subcommand word is
//! the run path's own argument list, so `herdr gateway`, `herdr gateway --bind
//! ADDR` and `herdr gateway --config PATH` all start a server.
//!
//! Exit codes: 0 for a clean stop or printed help, 1 when the gateway refuses
//! to start (bad config, a bind the policy rejects, an untrusted token file),
//! 2 for a usage error.

mod assets;
mod auth;
// PR 4 consumes `FleetRuntime::{start, handle, shutdown}` and
// `FleetHandle::report`, but the module's streaming half is still unreached:
// `ChangeStream`/`ChangeItem` and `FleetHandle::subscribe_with_report` are
// PR 5's (`/api/events`), and `host_connection`/`host_spec` are PR 6's
// (`/api/terminal/{host}/{pane}`). Those items live in `fleet.rs`, which PR 4
// does not otherwise touch, so the allow stays on this declaration until PR 6
// has landed rather than becoming a scatter of per-item attributes there.
#[allow(dead_code)]
mod fleet;
mod http;
mod middleware;
mod paths;
mod policy;
mod run;
mod server;

/// The invocation line, shared with `herdr --help` so the two never drift.
pub(crate) const GATEWAY_COMMAND_LINE: &str = "herdr gateway [--bind ADDR] [--config PATH]";

pub(crate) fn run_gateway_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("help" | "--help" | "-h") if args.len() == 1 => {
            crate::platform::begin_cli_output();
            std::print!("{}", gateway_help());
            Ok(0)
        }
        // Everything else is the run path's argument list. `run` reports its
        // own usage errors, so an unknown option or word still exits 2.
        _ => run::run(args),
    }
}

fn gateway_help() -> String {
    let mut help = String::new();
    help.push_str("Serve the fleet over HTTP and WebSocket\n\n");
    help.push_str(&format!("usage: {GATEWAY_COMMAND_LINE}\n\n"));
    help.push_str("Options:\n");
    help.push_str("  --bind ADDR         Listen on ADDR instead of the configured address\n");
    help.push_str("  --config PATH       Read configuration from PATH\n");
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

    /// The bare command runs a server, so it is not exercised here; what this
    /// pins is that anything the run path cannot parse still exits 2 rather
    /// than starting one.
    #[test]
    fn unknown_subcommands_and_options_are_usage_errors() {
        assert_eq!(run_gateway_command(&args(&["bogus"])).expect("usage"), 2);
        assert_eq!(run_gateway_command(&args(&["--nope"])).expect("usage"), 2);
        // `help` only stands alone; `help bogus` is still a usage error.
        assert_eq!(
            run_gateway_command(&args(&["help", "bogus"])).expect("usage"),
            2
        );
        // A malformed `--bind` never reaches a listener.
        assert_eq!(
            run_gateway_command(&args(&["--bind", "nope"])).expect("usage"),
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
}
