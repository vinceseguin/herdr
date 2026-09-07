//! `herdr fleet status` — the first user-visible view of the fork's fleet.
//!
//! Read-only by construction: it starts the connector, folds what the hosts
//! say into a [`crate::fleet::state::FleetState`], prints it and stops. It
//! never sends input to a host.
//!
//! Exit codes follow the plan's decision (h): 0 whenever a report was produced
//! (an unreachable host is data, not a failure), 1 for an invalid `[fleet]`
//! section, 2 for a usage error.

use std::io::Write as _;
use std::time::Duration;

use crate::api::schema::AgentStatus;
use crate::config::Config;
use crate::fleet::oneshot;
use crate::fleet::report::FleetStatusReport;
use crate::fleet::state::FleetChange;

const FLEET_USAGE: &str = "usage: herdr fleet status [--json] [--timeout-ms MS] [--watch]";
/// How long a one-shot waits for hosts that have not answered yet.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Upper bound on `--timeout-ms`, so a typo cannot hang a script for a day.
const MAX_TIMEOUT_MS: u64 = 600_000;

pub(super) fn run_fleet_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("status") => run_status_command(&args[1..]),
        Some("help" | "--help" | "-h") if args.len() == 1 => {
            print_fleet_help();
            Ok(0)
        }
        _ => {
            print_fleet_help_to_stderr();
            Ok(2)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusArgs {
    json: bool,
    watch: bool,
    timeout: Duration,
}

impl Default for StatusArgs {
    fn default() -> Self {
        Self {
            json: false,
            watch: false,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }
    }
}

fn parse_status_args(args: &[String]) -> Result<StatusArgs, String> {
    let mut parsed = StatusArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => parsed.json = true,
            "--watch" => parsed.watch = true,
            "--timeout-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--timeout-ms needs a value".to_string());
                };
                parsed.timeout = parse_timeout(value)?;
                index += 1;
            }
            other => {
                if let Some(value) = other.strip_prefix("--timeout-ms=") {
                    parsed.timeout = parse_timeout(value)?;
                } else {
                    return Err(format!("unknown option: {other}"));
                }
            }
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let millis = value
        .parse::<u64>()
        .map_err(|_| format!("--timeout-ms must be a whole number of milliseconds: {value}"))?;
    if millis > MAX_TIMEOUT_MS {
        return Err(format!("--timeout-ms must be at most {MAX_TIMEOUT_MS}"));
    }
    Ok(Duration::from_millis(millis))
}

fn run_status_command(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_status_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error}");
            eprintln!("{FLEET_USAGE}");
            return Ok(2);
        }
    };

    let config = Config::load().config;
    crate::platform::begin_cli_output();

    if parsed.watch {
        return run_watch(&config, &parsed);
    }

    let report = match oneshot::collect_status(&config, parsed.timeout) {
        Ok(report) => report,
        Err(diagnostics) => return Ok(report_config_diagnostics(&diagnostics)),
    };
    let mut out = std::io::stdout().lock();
    write_report(&mut out, &report, parsed.json)?;
    out.flush()?;
    Ok(0)
}

fn run_watch(config: &Config, parsed: &StatusArgs) -> std::io::Result<i32> {
    let json = parsed.json;
    // The closures write straight to stdout and flush every line: `--watch`
    // is usually piped, where the default block buffering would hide changes
    // until the buffer filled.
    let mut failure: Option<std::io::Error> = None;
    let report_failure = &mut failure;
    let watched = oneshot::watch(
        config,
        parsed.timeout,
        |report| {
            let mut out = std::io::stdout().lock();
            match write_report(&mut out, report, json).and_then(|()| out.flush()) {
                Ok(()) => true,
                Err(error) => {
                    *report_failure = Some(error);
                    false
                }
            }
        },
        |change| {
            let mut out = std::io::stdout().lock();
            let written = if json {
                match serde_json::to_string(change) {
                    Ok(line) => writeln!(out, "{line}"),
                    Err(error) => {
                        tracing::warn!(error = %error, "could not encode a fleet change");
                        Ok(())
                    }
                }
            } else {
                writeln!(out, "{}", render_change(change))
            };
            written.and_then(|()| out.flush()).is_ok()
        },
    );
    if let Err(diagnostics) = watched {
        return Ok(report_config_diagnostics(&diagnostics));
    }
    match failure {
        // A closed pipe (`| head`) is a normal end of a watch, not a failure.
        Some(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(0),
        Some(error) => Err(error),
        None => Ok(0),
    }
}

fn report_config_diagnostics(diagnostics: &[String]) -> i32 {
    for diagnostic in diagnostics {
        eprintln!("{diagnostic}");
    }
    1
}

fn write_report(
    out: &mut impl std::io::Write,
    report: &FleetStatusReport,
    json: bool,
) -> std::io::Result<()> {
    if json {
        let encoded = serde_json::to_string_pretty(report)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        writeln!(out, "{encoded}")
    } else {
        write!(out, "{}", report.render_text())
    }
}

/// One line per delta, the text twin of the `--watch --json` stream.
fn render_change(change: &FleetChange) -> String {
    match change {
        FleetChange::HostConnection { host, connection } => {
            let mut line = format!("host {host} {}", connection.state_name());
            if let Some(reason) = connection.reason() {
                line.push_str(&format!(": {reason}"));
            }
            line
        }
        FleetChange::Snapshot {
            host,
            boot_id,
            revision,
        } => format!("snapshot {host} boot {boot_id} revision {revision}"),
        FleetChange::AgentAdded { agent } => format!(
            "agent + {} {} {}",
            agent.pane,
            status_name(agent.agent_status),
            agent.workspace_label
        ),
        FleetChange::AgentRemoved { pane } => format!("agent - {pane}"),
        FleetChange::AgentStatus { pane, from, to } => format!(
            "agent ~ {pane} {} -> {}",
            status_name(*from),
            status_name(*to)
        ),
        FleetChange::AgentMetadata {
            pane,
            tokens,
            state_labels,
        } => {
            let mut line = format!("agent * {pane}");
            for (key, value) in tokens {
                line.push_str(&format!(" {key}={value}"));
            }
            for (status, label) in state_labels {
                line.push_str(&format!(" {status}:{label}"));
            }
            if tokens.is_empty() && state_labels.is_empty() {
                line.push_str(" cleared");
            }
            line
        }
        FleetChange::ActiveHost { host } => match host {
            Some(host) => format!("active host {host}"),
            None => "active host none".to_string(),
        },
    }
}

fn status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
    }
}

fn fleet_help() -> String {
    let mut help = String::new();
    help.push_str("Inspect the configured fleet of herdr hosts\n\n");
    help.push_str(&format!("{FLEET_USAGE}\n\n"));
    help.push_str("Options:\n");
    help.push_str("  --json              Print the fleet status report as JSON\n");
    help.push_str(&format!(
        "  --timeout-ms MS     Wait at most MS for hosts to answer (default {DEFAULT_TIMEOUT_MS})\n"
    ));
    help.push_str("  --watch             Keep running and print one line per change\n");
    help
}

fn print_fleet_help() {
    print!("{}", fleet_help());
}

fn print_fleet_help_to_stderr() {
    eprint!("{}", fleet_help());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::hosts::HostId;
    use crate::fleet::refs::FleetPaneRef;
    use crate::fleet::state::HostConnection;

    fn host(name: &str) -> HostId {
        HostId::new(name).expect("valid host id")
    }

    #[test]
    fn status_args_default_to_a_five_second_text_report() {
        let parsed = parse_status_args(&[]).expect("no arguments");
        assert_eq!(parsed, StatusArgs::default());
        assert_eq!(parsed.timeout, Duration::from_secs(5));
        assert!(!parsed.json);
        assert!(!parsed.watch);
    }

    #[test]
    fn status_args_accept_every_flag() {
        let args = ["--json", "--watch", "--timeout-ms", "250"].map(String::from);
        let parsed = parse_status_args(&args).expect("valid arguments");
        assert!(parsed.json);
        assert!(parsed.watch);
        assert_eq!(parsed.timeout, Duration::from_millis(250));

        let joined = ["--timeout-ms=1200".to_string()];
        assert_eq!(
            parse_status_args(&joined).expect("valid arguments").timeout,
            Duration::from_millis(1200)
        );
    }

    #[test]
    fn status_args_reject_typos_and_bad_timeouts() {
        for args in [
            vec!["--jsn".to_string()],
            vec!["--timeout-ms".to_string()],
            vec!["--timeout-ms".to_string(), "soon".to_string()],
            vec!["--timeout-ms".to_string(), "-1".to_string()],
            vec!["--timeout-ms".to_string(), "600001".to_string()],
        ] {
            assert!(
                parse_status_args(&args).is_err(),
                "expected a usage error for {args:?}"
            );
        }
    }

    #[test]
    fn changes_render_one_line_each() {
        let pane = FleetPaneRef::new(host("lab-1"), "w1:p1".to_string());
        assert_eq!(
            render_change(&FleetChange::HostConnection {
                host: host("lab-1"),
                connection: HostConnection::Unavailable {
                    reason: "no herdr server".to_string(),
                    retry_in: None,
                },
            }),
            "host lab-1 unavailable: no herdr server"
        );
        assert_eq!(
            render_change(&FleetChange::AgentRemoved { pane: pane.clone() }),
            "agent - lab-1/w1:p1"
        );
        assert_eq!(
            render_change(&FleetChange::AgentStatus {
                pane,
                from: AgentStatus::Working,
                to: AgentStatus::Blocked,
            }),
            "agent ~ lab-1/w1:p1 working -> blocked"
        );
        assert_eq!(
            render_change(&FleetChange::ActiveHost { host: None }),
            "active host none"
        );
        let pane = FleetPaneRef::new(host("lab-1"), "w1:p1".to_string());
        assert_eq!(
            render_change(&FleetChange::AgentMetadata {
                pane: pane.clone(),
                tokens: std::collections::BTreeMap::from([
                    ("account".to_string(), "work".to_string()),
                    ("account_state".to_string(), "ok".to_string()),
                ]),
                state_labels: std::collections::BTreeMap::from([(
                    "blocked".to_string(),
                    "limit".to_string()
                )]),
            }),
            "agent * lab-1/w1:p1 account=work account_state=ok blocked:limit"
        );
        assert_eq!(
            render_change(&FleetChange::AgentMetadata {
                pane,
                tokens: std::collections::BTreeMap::new(),
                state_labels: std::collections::BTreeMap::new(),
            }),
            "agent * lab-1/w1:p1 cleared"
        );
    }

    #[test]
    fn the_help_names_every_flag() {
        let help = fleet_help();
        for flag in ["--json", "--timeout-ms", "--watch"] {
            assert!(help.contains(flag), "help is missing {flag}: {help}");
        }
    }
}
