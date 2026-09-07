use std::time::{Duration, Instant};

use crate::api::schema::{
    AgentPromptParams, AgentPromptWaitOptions, AgentReadParams, AgentRenameParams,
    AgentSendKeysParams, AgentStartParams, AgentTarget, AgentWaitParams, EmptyParams, ErrorBody,
    ErrorResponse, Method, PaneProcessInfoParams, PaneTarget, ReadFormat, ReadSource, Request,
};

const AGENT_START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PANE_SHELL_READINESS_RETRY_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn run_agent_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_agent_help();
        return Ok(2);
    };

    match subcommand {
        "list" => agent_list(&args[1..]),
        "get" => agent_get(&args[1..]),
        "read" => agent_read(&args[1..]),
        "send-keys" => agent_send_keys(&args[1..]),
        "prompt" => agent_prompt(&args[1..]),
        "rename" => agent_rename(&args[1..]),
        "focus" => agent_focus(&args[1..]),
        "wait" => agent_wait(&args[1..]),
        "attach" => agent_attach(&args[1..]),
        "start" => agent_start(&args[1..]),
        // Fork (E9): move a running Claude agent to another account profile.
        "switch-account" => agent_switch_account(&args[1..]),
        "explain" => agent_explain(&args[1..]),
        "help" | "--help" | "-h" => {
            print_agent_help();
            Ok(0)
        }
        _ => {
            print_agent_help();
            Ok(2)
        }
    }
}

fn agent_explain(args: &[String]) -> std::io::Result<i32> {
    let mut file = None;
    let mut agent = None;
    let mut json = false;
    let mut verbose = false;
    let mut target = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --file");
                    return Ok(2);
                };
                file = Some(value.clone());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent");
                    return Ok(2);
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --format");
                    return Ok(2);
                };
                match value.as_str() {
                    "json" => json = true,
                    "text" => json = false,
                    other => {
                        eprintln!("invalid --format: {other} (expected text or json)");
                        return Ok(2);
                    }
                }
                index += 2;
            }
            "--verbose" | "-v" => {
                verbose = true;
                index += 1;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: herdr agent explain <target> [--json|--verbose]");
                eprintln!(
                    "usage: herdr agent explain --file PATH --agent LABEL [--json|--verbose]"
                );
                return Ok(0);
            }
            value if value.starts_with('-') => {
                eprintln!("unknown option: {value}");
                return Ok(2);
            }
            value => {
                if target.is_some() {
                    eprintln!("usage: herdr agent explain <target> [--json]");
                    return Ok(2);
                }
                target = Some(value.to_string());
                index += 1;
            }
        }
    }

    let explain = if let Some(path) = file {
        if target.is_some() {
            eprintln!("usage: herdr agent explain --file PATH --agent LABEL [--json]");
            return Ok(2);
        }
        let Some(agent_label) = agent else {
            eprintln!("herdr agent explain --file requires --agent LABEL");
            return Ok(2);
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                let response = ErrorResponse {
                    id: "cli:agent:explain".into(),
                    error: ErrorBody {
                        code: "agent_explain_file_read_failed".into(),
                        message: format!("failed to read agent explain file {path}: {err}"),
                    },
                };
                let response = serde_json::to_string(&response).map_err(std::io::Error::other)?;
                eprintln!("{response}");
                return Ok(1);
            }
        };
        crate::detect::manifest::explain_to_json_value(&crate::detect::manifest::explain_for_label(
            &agent_label,
            &content,
        ))
    } else {
        let Some(target) = target else {
            eprintln!("usage: herdr agent explain <target> [--json]");
            eprintln!("usage: herdr agent explain --file PATH --agent LABEL [--json]");
            return Ok(2);
        };
        if agent.is_some() {
            eprintln!("--agent is only valid with --file");
            return Ok(2);
        }

        let response = super::send_request(&Request {
            id: "cli:agent:explain".into(),
            method: Method::AgentExplain(AgentTarget {
                target: target.to_owned(),
            }),
        })?;
        if response.get("error").is_some() {
            eprintln!("{}", serde_json::to_string(&response).unwrap());
            return Ok(1);
        }
        response["result"]["explain"].clone()
    };

    if json {
        println!("{explain}");
    } else {
        print_agent_explain_text(&explain, verbose);
    }
    Ok(0)
}

fn print_agent_explain_text(explain: &serde_json::Value, verbose: bool) {
    println!("agent: {}", explain["agent"].as_str().unwrap_or("unknown"));
    println!("state: {}", explain["state"].as_str().unwrap_or("unknown"));
    println!(
        "manifest: {} {}",
        explain["manifest_source"].as_str().unwrap_or("none"),
        explain["manifest_version"].as_str().unwrap_or("unknown")
    );
    if let Some(rule) = explain["matched_rule"].as_object() {
        let rule_id = rule
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("-");
        println!(
            "rule: {} (region={} priority={})",
            rule_id,
            rule.get("region")
                .and_then(|value| value.as_str())
                .unwrap_or("-"),
            rule.get("priority")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
        );
        if let Some(preview) = matched_rule_region_preview(explain, rule_id) {
            println!("evidence: {preview:?}");
        }
    } else {
        println!("rule: none");
    }
    if let Some(reason) = explain["fallback_reason"].as_str() {
        println!("fallback_reason: {reason}");
    }
    if let Some(reason) = explain["screen_detection_skip_reason"].as_str() {
        println!("screen_detection_skip_reason: {reason}");
    }
    if let Some(reason) = explain["skipped_update_reason"].as_str() {
        println!("skipped_update_reason: {reason}");
    }
    if let Some(warning) = explain["warning"].as_str() {
        println!("warning: {warning}");
    }

    if !verbose {
        return;
    }

    println!(
        "visible: idle={} blocker={} working={}",
        explain["visible_idle"].as_bool().unwrap_or(false),
        explain["visible_blocker"].as_bool().unwrap_or(false),
        explain["visible_working"].as_bool().unwrap_or(false)
    );
    println!(
        "cached_remote_version: {}",
        explain["cached_remote_version"].as_str().unwrap_or("none")
    );
    println!(
        "local_override_shadowing_remote: {}",
        explain["local_override_shadowing_remote"]
            .as_bool()
            .unwrap_or(false)
    );
    if let Some(status) = explain["remote_update_status"].as_str() {
        println!("remote_update_status: {status}");
    }
    if let Some(error) = explain["remote_update_error"].as_str() {
        println!("remote_update_error: {error}");
    }
    if let Some(evaluated_rules) = explain["evaluated_rules"]
        .as_array()
        .filter(|rules| !rules.is_empty())
    {
        println!("evaluated_rules:");
        for rule in evaluated_rules {
            println!(
                "  {} {} priority={} region={} state={}",
                if rule["matched"].as_bool().unwrap_or(false) {
                    "✓"
                } else {
                    "✗"
                },
                rule["id"].as_str().unwrap_or("-"),
                rule["priority"].as_i64().unwrap_or(0),
                rule["region"].as_str().unwrap_or("-"),
                rule["state"].as_str().unwrap_or("unknown")
            );
            let evidence = &rule["evidence"];
            println!(
                "    matchers: contains={:?} regex={:?} line_regex={:?} all={} any={} not={}",
                evidence["contains"],
                evidence["regex"],
                evidence["line_regex"],
                evidence["all_count"].as_u64().unwrap_or(0),
                evidence["any_count"].as_u64().unwrap_or(0),
                evidence["not_count"].as_u64().unwrap_or(0)
            );
            println!(
                "    region: bytes={} preview={:?}",
                evidence["region_bytes"].as_u64().unwrap_or(0),
                evidence["region_preview"].as_str().unwrap_or("")
            );
        }
    }
}

fn matched_rule_region_preview<'a>(
    explain: &'a serde_json::Value,
    rule_id: &str,
) -> Option<&'a str> {
    explain["evaluated_rules"]
        .as_array()?
        .iter()
        .find(|rule| rule["id"].as_str() == Some(rule_id))?["evidence"]["region_preview"]
        .as_str()
        .filter(|preview| !preview.is_empty())
}

fn agent_start(args: &[String]) -> std::io::Result<i32> {
    let Some(name) = args.first() else {
        eprintln!("usage: herdr agent start <name> --kind KIND --pane ID [--account NAME|none] [--timeout MS] [-- <agent-args...>]");
        return Ok(2);
    };
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut kind = None;
    let mut pane_id = None;
    let mut timeout_ms = None;
    let mut account = None;
    let mut index = 1;
    while index < separator {
        match args[index].as_str() {
            "--kind" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --kind");
                    return Ok(2);
                };
                kind = Some(value.clone());
                index += 2;
            }
            "--pane" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = match parse_timeout(value) {
                    Ok(timeout_ms) => Some(timeout_ms),
                    Err(exit_code) => return Ok(exit_code),
                };
                index += 2;
            }
            // Fork (E9): which Claude account profile the agent runs under.
            // `none` opts out and gives back byte-for-byte stock behaviour.
            "--account" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --account");
                    return Ok(2);
                };
                account = Some(value.clone());
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    let Some(kind) = kind else {
        eprintln!("missing required --kind");
        return Ok(2);
    };
    let Some(pane_id) = pane_id else {
        eprintln!("missing required --pane");
        return Ok(2);
    };
    let Some(expected_kind) = crate::detect::parse_agent_label(&kind) else {
        eprintln!("unsupported interactive agent kind: {kind}");
        return Ok(2);
    };
    let expected_kind = crate::detect::agent_label(expected_kind).to_string();
    let agent_args = if separator < args.len() {
        args[separator + 1..].to_vec()
    } else {
        Vec::new()
    };

    // Fork (E9): decide and apply the Claude account profile *before* the
    // stock start below types its command line. This is the whole of decision
    // (b) at the call site: the pane shell is given `CLAUDE_CONFIG_DIR`, then
    // the unmodified `agent.start` runs and the `claude` it types inherits it.
    // Both lines reach the same pty in order, so the shell executes the
    // assignment before it reads the command. `None` means no profile applies
    // and the rest of this function is byte-for-byte the stock behaviour.
    let account_plan = match prepare_account_launch(
        &expected_kind,
        account.as_deref(),
        &pane_id,
        name,
        &agent_args,
    ) {
        Ok(plan) => plan,
        Err(exit_code) => return Ok(exit_code),
    };

    let started = start_managed_agent(
        name,
        &kind,
        &expected_kind,
        &pane_id,
        &agent_args,
        timeout_ms,
    )?;
    match started {
        Ok(mut response) => {
            // Fork (E9): grade the launch against the process's own
            // environment and record `tokens.account`, then add the two
            // contract keys to the stock response.
            let Some(applied) = account_plan else {
                return super::print_response(&response);
            };
            let agent_name = applied.plan().name.clone();
            let outcome = applied.finish();
            response["result"]["account"] = serde_json::Value::String(outcome.account.clone());
            response["result"]["account_state"] =
                serde_json::Value::String(outcome.account_state.as_str().to_string());
            let exit_code = super::print_response(&response)?;
            for warning in &outcome.warnings {
                eprintln!("warning: {warning}");
            }
            if outcome.account_state == crate::accounts::tokens::AccountState::Mismatch {
                eprintln!(
                    "error: agent {agent_name:?} is not running under account {:?}: {}. \
                     The account token records the mismatch; stop the agent and start it again.",
                    outcome.account,
                    outcome.mismatch_detail(),
                );
                return Ok(1);
            }
            Ok(exit_code)
        }
        Err(AgentStartRefusal::Response(response)) => super::print_response(&response),
        Err(AgentStartRefusal::Transport(err)) => {
            print_agent_transport_error(err, "cli:agent:start", "agent_start_transport_failed")
        }
    }
}

const SWITCH_ACCOUNT_USAGE: &str = "usage: herdr agent switch-account <target> <account> [--yes] [--interrupt] [--force] [--timeout MS] [--json]";

/// Fork (E9): move a running Claude agent to another account profile, keeping
/// its conversation.
///
/// The protocol lives in `crate::accounts::switch`; this function is the
/// command line around it. Two things it owns and the machine deliberately
/// does not: whether there is a human to confirm with, and how the relaunch is
/// carried out (through the same `start_managed_agent` a stock `agent start`
/// uses).
///
/// Exit codes are the contract that matters when this fails:
///
/// * `0` — the agent is running under the new profile with the same session;
/// * `2` — refused before a single byte reached the pane, so the agent is
///   exactly as it was;
/// * `1` — the protocol had started; the message says what the pane holds now.
fn agent_switch_account(args: &[String]) -> std::io::Result<i32> {
    let mut positional: Vec<&String> = Vec::new();
    let mut yes = false;
    let mut interrupt = false;
    let mut force = false;
    let mut json = false;
    let mut timeout_ms = crate::accounts::switch::DEFAULT_TIMEOUT_MS;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--yes" | "-y" => {
                yes = true;
                index += 1;
            }
            "--interrupt" => {
                interrupt = true;
                index += 1;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = match parse_timeout(value) {
                    Ok(millis) => millis,
                    Err(exit_code) => return Ok(exit_code),
                };
                if timeout_ms == 0 || timeout_ms > crate::accounts::switch::MAX_TIMEOUT_MS {
                    eprintln!(
                        "--timeout must be between 1 and {} ms",
                        crate::accounts::switch::MAX_TIMEOUT_MS
                    );
                    return Ok(2);
                }
                index += 2;
            }
            "help" | "--help" | "-h" => {
                eprintln!("{SWITCH_ACCOUNT_USAGE}");
                return Ok(0);
            }
            value if value.starts_with('-') => {
                eprintln!("unknown option: {value}");
                return Ok(2);
            }
            _ => {
                positional.push(&args[index]);
                index += 1;
            }
        }
    }

    let [target, account] = positional.as_slice() else {
        eprintln!("{SWITCH_ACCOUNT_USAGE}");
        return Ok(2);
    };
    let (target, account) = ((*target).clone(), (*account).clone());
    if account == crate::accounts::profile::NO_ACCOUNT {
        eprintln!(
            "switch-account needs a profile to switch to; `none` only means \"apply no profile\" \
             at start time"
        );
        return Ok(2);
    }

    let config = crate::config::Config::load().config;
    let (profiles, diagnostics) = crate::accounts::profile::load_profiles(&config);
    for diagnostic in &diagnostics {
        eprintln!("warning: {diagnostic}");
    }
    let Some(profile) = profiles.get(&account).cloned() else {
        eprintln!(
            "unknown account profile {account:?}; configured: {}",
            if profiles.is_empty() {
                "none".to_string()
            } else {
                profiles.names().join(", ")
            }
        );
        return Ok(2);
    };
    let inspection = crate::accounts::layout::inspect(
        &profile,
        crate::accounts::layout::InspectOptions::health(),
    );

    // The readiness wait of the relaunch follows --timeout when the server
    // would accept it, so one flag governs the whole command.
    let start_timeout_ms = (timeout_ms > crate::app::AGENT_START_SETTLE_DELAY.as_millis() as u64
        && timeout_ms <= crate::app::MAX_AGENT_START_TIMEOUT.as_millis() as u64)
        .then_some(timeout_ms);

    let input = crate::accounts::switch::SwitchInput {
        target: target.clone(),
        to: profile.clone(),
        to_inspection: inspection,
        options: crate::accounts::switch::SwitchOptions {
            interrupt,
            force,
            timeout_ms,
        },
    };

    let mut confirm = |question: &str| confirm_switch(question, yes);
    let mut launch = |request: &crate::accounts::switch::LaunchRequest| {
        relaunch_under_account(&profile, request, start_timeout_ms)
    };

    let outcome = crate::accounts::client::switch_account(input, &mut confirm, &mut launch);
    match outcome {
        Ok(outcome) => {
            let result = &outcome.result;
            if json {
                let rendered = serde_json::json!({
                    "pane_id": result.pane_id,
                    "name": result.name,
                    "from": result.from,
                    "to": result.to,
                    "session_id": result.session_id,
                    "account_state": result.account_state.as_str(),
                });
                println!("{rendered}");
            } else {
                println!(
                    "switched {} in pane {} from {} to {} ({}), same session {}",
                    result.name,
                    result.pane_id,
                    result.from.as_deref().unwrap_or("no recorded account"),
                    result.to,
                    result.account_state,
                    result.session_id,
                );
            }
            for warning in &outcome.warnings {
                eprintln!("warning: {warning}");
            }
            if result.account_state == crate::accounts::tokens::AccountState::Mismatch {
                eprintln!(
                    "error: agent {:?} resumed session {} but is not running under account {:?}: \
                     {}. The account token records the mismatch.",
                    result.name,
                    result.session_id,
                    result.to,
                    result
                        .mismatch_detail
                        .as_deref()
                        .unwrap_or("its environment disagrees"),
                );
                return Ok(1);
            }
            Ok(0)
        }
        Err(failure) => {
            eprintln!("error: {failure}");
            // The one fact a caller needs to act on: whether the agent was
            // touched at all.
            Ok(if failure.touched_pane { 1 } else { 2 })
        }
    }
}

/// Ask before stopping somebody's agent. `--yes` answers for automation; with
/// no terminal and no `--yes` the answer is "there is nobody to ask", never
/// an assumed yes.
fn confirm_switch(question: &str, yes: bool) -> crate::accounts::client::Confirmation {
    use std::io::{IsTerminal as _, Write as _};

    if yes {
        return crate::accounts::client::Confirmation::Yes;
    }
    if !std::io::stdin().is_terminal() {
        return crate::accounts::client::Confirmation::Unavailable;
    }
    eprint!("{question}\nProceed? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return crate::accounts::client::Confirmation::Unavailable;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => crate::accounts::client::Confirmation::Yes,
        _ => crate::accounts::client::Confirmation::No,
    }
}

/// The relaunch half of a switch: the same two-step launch `herdr agent start
/// --account` performs, with `--resume <id>` in the arguments.
///
/// The returned guard owns the fact that the pane's shell now exports the new
/// profile; the switch driver grades it once the resumed session is proven.
/// Dropping it on any failure here is deliberate — it prints what the pane is
/// left holding.
fn relaunch_under_account(
    profile: &crate::accounts::profile::AccountProfile,
    request: &crate::accounts::switch::LaunchRequest,
    timeout_ms: Option<u64>,
) -> Result<crate::accounts::client::AppliedLine, crate::accounts::switch::SwitchError> {
    let launch_error = |detail: String| crate::accounts::switch::SwitchError::Launch { detail };

    let plan =
        crate::accounts::client::prepare(profile, &request.pane_id, &request.name, &request.args)
            .map_err(|error| launch_error(error.to_string()))?;
    let applied = crate::accounts::client::apply_env(plan)
        .map_err(|error| launch_error(error.to_string()))?;

    match start_managed_agent(
        &request.name,
        crate::accounts::tokens::AGENT_LABEL,
        crate::accounts::tokens::AGENT_LABEL,
        &request.pane_id,
        &request.args,
        timeout_ms,
    ) {
        Ok(Ok(_response)) => Ok(applied),
        Ok(Err(AgentStartRefusal::Response(response))) => Err(launch_error(
            response["error"]["message"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| response["error"].to_string()),
        )),
        Ok(Err(AgentStartRefusal::Transport(err))) => Err(launch_error(err.to_string())),
        Err(err) => Err(launch_error(err.to_string())),
    }
}

/// Why a managed start did not produce a ready agent.
pub(crate) enum AgentStartRefusal {
    /// A response to print; its `error` body says what the server refused.
    Response(serde_json::Value),
    /// The readiness wait lost the transport.
    Transport(std::io::Error),
}

/// Start a managed agent in a pane and wait for it to become interactive.
///
/// This is the stock `agent.start` sequence — including the `agent_pane_busy`
/// retry that covers a shell still running its rc files, and the terminal-id
/// pinning that catches a pane replaced underneath — factored out so the
/// fork's account switch (`herdr agent switch-account`) relaunches an agent
/// through exactly this code instead of a second copy of the subtlest part of
/// starting one.
pub(crate) fn start_managed_agent(
    name: &str,
    kind: &str,
    expected_kind: &str,
    pane_id: &str,
    agent_args: &[String],
    timeout_ms: Option<u64>,
) -> std::io::Result<Result<serde_json::Value, AgentStartRefusal>> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
    let retryable_timeout = timeout > crate::app::AGENT_START_SETTLE_DELAY
        && timeout <= crate::app::MAX_AGENT_START_TIMEOUT;
    let pinned_terminal_id = pane_terminal_id(pane_id)?;
    let mut retry_deadline = None;
    let mut previous_busy_response: Option<serde_json::Value> = None;
    let mut response = loop {
        if let Some(previous_busy_response) = previous_busy_response.as_ref() {
            let retry_expired = retry_deadline.is_some_and(|deadline| Instant::now() >= deadline);
            if retry_expired
                || pane_terminal_id(pane_id)? != pinned_terminal_id
                || !pane_shell_is_initializing(pane_id)?
            {
                return Ok(Err(AgentStartRefusal::Response(
                    previous_busy_response.clone(),
                )));
            }
        }

        let response = super::send_request(&Request {
            id: "cli:agent:start".into(),
            method: Method::AgentStart(AgentStartParams {
                name: name.to_owned(),
                kind: kind.to_owned(),
                pane_id: pane_id.to_owned(),
                args: agent_args.to_vec(),
                timeout_ms,
            }),
        })?;
        if response.get("error").is_none() {
            break response;
        }
        if response["error"]["code"].as_str() != Some("agent_pane_busy")
            || !retryable_timeout
            || pinned_terminal_id.is_none()
            || pane_terminal_id(pane_id)? != pinned_terminal_id
            || !pane_shell_is_initializing(pane_id)?
        {
            return Ok(Err(AgentStartRefusal::Response(response)));
        }

        let deadline = *retry_deadline
            .get_or_insert_with(|| Instant::now() + PANE_SHELL_READINESS_RETRY_TIMEOUT);
        previous_busy_response = Some(response);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            if let Some(previous_busy_response) = previous_busy_response.as_ref() {
                return Ok(Err(AgentStartRefusal::Response(
                    previous_busy_response.clone(),
                )));
            }
        }
        std::thread::sleep(AGENT_START_POLL_INTERVAL.min(remaining));
    };

    let Some(expected_terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        return Ok(Err(AgentStartRefusal::Response(cli_agent_error(
            "cli:agent:start",
            "agent_start_failed",
            "agent start response did not include terminal_id",
        ))));
    };
    if pinned_terminal_id
        .as_deref()
        .is_some_and(|pinned| pinned != expected_terminal_id)
    {
        return Ok(Err(AgentStartRefusal::Response(agent_name_lost_error(
            "cli:agent:start",
            name,
        ))));
    }
    let waited = wait_for_named_agent(name, pane_id, timeout, expected_kind, expected_terminal_id);
    match waited {
        Ok(Ok(agent)) => {
            response["result"]["agent"] = agent;
            Ok(Ok(response))
        }
        Ok(Err(error)) => Ok(Err(AgentStartRefusal::Response(error))),
        Err(err) => Ok(Err(AgentStartRefusal::Transport(err))),
    }
}

/// Fork (E9): resolve `--account` and put the profile's environment into the
/// pane, returning the plan the launch must be graded against.
///
/// `Ok(None)` means no profile applies and nothing was typed. Every refusal
/// happens before a byte reaches the pane, and every one of them is an error:
/// falling back to "start it anyway" is how an agent ends up billing the wrong
/// Claude account without anyone noticing.
///
/// `Ok(Some(_))` hands back a guard that owns the fact that the pane's shell
/// now carries the variable, so every way this command can fail afterwards
/// says so without the stock code paths having to know about accounts.
fn prepare_account_launch(
    expected_kind: &str,
    account: Option<&str>,
    pane_id: &str,
    name: &str,
    agent_args: &[String],
) -> Result<Option<crate::accounts::client::AppliedLine>, i32> {
    if expected_kind != crate::accounts::tokens::AGENT_LABEL {
        // Decision (c): profiles configure Claude only for now. Silently
        // ignoring `--account` on another kind would look like it worked.
        if let Some(requested) =
            account.filter(|value| *value != crate::accounts::profile::NO_ACCOUNT)
        {
            eprintln!(
                "--account {requested:?} applies to --kind claude only (got {expected_kind:?})"
            );
            return Err(2);
        }
        return Ok(None);
    }

    let config = crate::config::Config::load().config;
    let (profiles, diagnostics) = crate::accounts::profile::load_profiles(&config);
    for diagnostic in &diagnostics {
        eprintln!("warning: {diagnostic}");
    }

    let profile = match crate::accounts::client::choose_profile(&profiles, account) {
        Ok(Some(profile)) => profile.clone(),
        Ok(None) => return Ok(None),
        Err(error) => {
            eprintln!("{error}");
            return Err(2);
        }
    };

    let plan = match crate::accounts::client::prepare(&profile, pane_id, name, agent_args) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("{error}");
            return Err(1);
        }
    };
    match crate::accounts::client::apply_env(plan) {
        Ok(applied) => Ok(Some(applied)),
        Err(error) => {
            eprintln!("{error}");
            Err(1)
        }
    }
}

fn agent_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr agent list");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:list".into(),
        method: Method::AgentList(EmptyParams::default()),
    })?)
}

fn agent_get(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent get <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: herdr agent get <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:get".into(),
        method: Method::AgentGet(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_focus(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent focus <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: herdr agent focus <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:focus".into(),
        method: Method::AgentFocus(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_attach(args: &[String]) -> std::io::Result<i32> {
    let (target, takeover) =
        match super::parse_attach_target(args, "usage: herdr agent attach <target> [--takeover]") {
            Ok(parsed) => parsed,
            Err(code) => return Ok(code),
        };

    let response = resolve_agent_target(&target, "cli:agent:attach:resolve")?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }
    let Some(terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        eprintln!("agent attach failed: response did not include terminal_id");
        return Ok(1);
    };
    crate::client::run_terminal_attach(terminal_id.to_owned(), takeover)?;
    Ok(0)
}

fn agent_wait(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent wait <target> [--until STATUS]... [--timeout MS]");
        return Ok(2);
    };
    let mut until = Vec::new();
    let mut timeout_ms = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--until" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("--until requires at least one status");
                    return Ok(2);
                };
                let status = match super::parse_agent_status(value) {
                    Ok(status) => status,
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                };
                until.push(status);
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = match parse_timeout(value) {
                    Ok(timeout_ms) => Some(timeout_ms),
                    Err(exit_code) => return Ok(exit_code),
                };
                index += 2;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: herdr agent wait <target> [--until STATUS]... [--timeout MS]");
                return Ok(0);
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:wait".into(),
        method: Method::AgentWait(AgentWaitParams {
            target: target.clone(),
            until,
            timeout_ms,
        }),
    })?)
}

fn wait_for_named_agent(
    name: &str,
    fallback_pane_id: &str,
    timeout: Duration,
    expected_kind: &str,
    expected_terminal_id: &str,
) -> std::io::Result<Result<serde_json::Value, serde_json::Value>> {
    let deadline = Instant::now().checked_add(timeout);
    let mut first_poll = true;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Let the server reconcile its matching startup deadline before
            // returning so the pending name is immediately reusable.
            let _ = resolve_agent_target_unchecked(name, "cli:agent:start:timeout");
            return Ok(Err(agent_wait_timeout()));
        }
        let poll_id = "cli:agent:start";
        let mut response = if first_poll {
            first_poll = false;
            resolve_agent_target(name, poll_id)?
        } else {
            resolve_agent_target_unchecked(name, poll_id)?
        };
        if response.get("error").is_some() {
            response = resolve_agent_target_unchecked(fallback_pane_id, poll_id)?;
            if response.get("error").is_some() {
                std::thread::sleep(AGENT_START_POLL_INTERVAL);
                continue;
            }
        }
        let agent = &response["result"]["agent"];
        let outcome = if agent["terminal_id"].as_str() != Some(expected_terminal_id) {
            Some(Err(agent_name_lost_error("cli:agent:start", name)))
        } else if let Some(actual) = agent["agent"]
            .as_str()
            .filter(|actual| *actual != expected_kind)
        {
            Some(Err(cli_agent_error(
                "cli:agent:start",
                "agent_kind_mismatch",
                format!("expected {expected_kind}, detected {actual}"),
            )))
        } else if agent["name"].as_str() != Some(name) {
            Some(Err(agent_name_lost_error("cli:agent:start", name)))
        } else {
            match agent["agent_status"].as_str() {
                Some("blocked") => Some(Err(cli_agent_error(
                    "cli:agent:start",
                    "agent_not_ready",
                    format!("agent {name} is blocked during startup and is not ready for prompts"),
                ))),
                Some("working" | "unknown") => None,
                Some("idle" | "done") if agent["interactive_ready"].as_bool() == Some(true) => {
                    Some(Ok(agent.clone()))
                }
                Some("idle" | "done") if !agent["launch_pending"].as_bool().unwrap_or(false) => {
                    Some(Err(cli_agent_error(
                        "cli:agent:start",
                        "agent_start_failed",
                        "agent process exited before becoming interactive",
                    )))
                }
                _ => None,
            }
        };
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
        std::thread::sleep(AGENT_START_POLL_INTERVAL);
    }
}

fn pane_terminal_id(pane_id: &str) -> std::io::Result<Option<String>> {
    let response = super::send_request(&Request {
        id: "cli:agent:start:pane".into(),
        method: Method::PaneGet(PaneTarget {
            pane_id: pane_id.to_owned(),
        }),
    })?;
    Ok(response["result"]["pane"]["terminal_id"]
        .as_str()
        .map(str::to_owned))
}

fn pane_shell_is_initializing(pane_id: &str) -> std::io::Result<bool> {
    let response = super::send_request(&Request {
        id: "cli:agent:start:process_info".into(),
        method: Method::PaneProcessInfo(PaneProcessInfoParams {
            pane_id: Some(pane_id.to_owned()),
        }),
    })?;
    Ok(process_info_shows_shell_initialization(
        &response["result"]["process_info"],
    ))
}

#[cfg(unix)]
fn process_info_shows_shell_initialization(process_info: &serde_json::Value) -> bool {
    let Some(shell_pid) = process_info["shell_pid"].as_u64() else {
        return false;
    };
    if process_info["foreground_process_group_id"].as_u64() != Some(shell_pid) {
        return false;
    }
    process_info["foreground_processes"]
        .as_array()
        .is_some_and(|processes| {
            processes.iter().any(|process| {
                process["pid"].as_u64() == Some(shell_pid)
                    && (process["name"]
                        .as_str()
                        .is_some_and(crate::platform::is_pane_shell_process_name)
                        || process["argv"]
                            .as_array()
                            .and_then(|argv| argv.first())
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(crate::platform::is_pane_shell_process_name))
            })
        })
}

// Windows exposes no foreground process group, so shell initialization is not
// observable and a busy `agent.start` is not retried there.
#[cfg(not(unix))]
fn process_info_shows_shell_initialization(_process_info: &serde_json::Value) -> bool {
    false
}

fn agent_name_lost_error(request_id: &str, expected_name: &str) -> serde_json::Value {
    cli_agent_error(
        request_id,
        "agent_name_not_found",
        format!("named agent {expected_name} no longer owns the target terminal"),
    )
}

fn print_agent_transport_error(
    err: std::io::Error,
    request_id: &str,
    code: &str,
) -> std::io::Result<i32> {
    if super::protocol_mismatch_was_reported(&err) {
        return Ok(1);
    }
    // A dead-server marker reaches here from `send_request` in the agent
    // startup path; surface its deferred response exactly once instead of
    // printing a second, generic transport-error line.
    if let Some(response) = super::server_not_running_reported_response(&err) {
        let value = serde_json::to_value(response).map_err(std::io::Error::other)?;
        return super::print_response(&value);
    }
    super::print_response(&cli_agent_error(request_id, code, err.to_string()))
}

fn agent_wait_timeout() -> serde_json::Value {
    cli_agent_error(
        "cli:agent:start",
        "timeout",
        "timed out waiting for agent startup",
    )
}

fn cli_agent_error(id: &str, code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn resolve_agent_target(target: &str, request_id: &str) -> std::io::Result<serde_json::Value> {
    super::send_request(&agent_get_request(target, request_id))
}

fn resolve_agent_target_unchecked(
    target: &str,
    request_id: &str,
) -> std::io::Result<serde_json::Value> {
    super::send_request_unchecked(&agent_get_request(target, request_id))
}

fn agent_get_request(target: &str, request_id: &str) -> Request {
    Request {
        id: request_id.into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    }
}

fn agent_rename(args: &[String]) -> std::io::Result<i32> {
    let [target, value] = args else {
        eprintln!("usage: herdr agent rename <target> <name>|--clear");
        return Ok(2);
    };
    let name = if value == "--clear" {
        None
    } else {
        Some(value.clone())
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:rename".into(),
        method: Method::AgentRename(AgentRenameParams {
            target: target.clone(),
            name,
        }),
    })?)
}

fn agent_prompt(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!(
            "usage: herdr agent prompt <target> <text> [--wait] [--until STATUS]... [--timeout MS]"
        );
        return Ok(2);
    };
    let Some(text) = args.get(1) else {
        eprintln!("agent prompt requires text");
        return Ok(2);
    };
    let mut wait = false;
    let mut until = Vec::new();
    let mut timeout_ms = None;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--wait" => {
                wait = true;
                index += 1;
            }
            "--until" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("--until requires at least one status");
                    return Ok(2);
                };
                let status = match super::parse_agent_status(value) {
                    Ok(status) => status,
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                };
                until.push(status);
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = match parse_timeout(value) {
                    Ok(timeout_ms) => Some(timeout_ms),
                    Err(exit_code) => return Ok(exit_code),
                };
                index += 2;
            }
            option => {
                eprintln!("unknown option: {option}");
                return Ok(2);
            }
        }
    }
    if !until.is_empty() && !wait {
        eprintln!("--until requires --wait");
        return Ok(2);
    }
    if timeout_ms.is_some() && !wait {
        eprintln!("--timeout requires --wait");
        return Ok(2);
    }
    let response = super::send_request(&Request {
        id: "cli:agent:prompt".into(),
        method: Method::AgentPrompt(AgentPromptParams {
            target: target.clone(),
            text: text.clone(),
            wait: wait.then_some(AgentPromptWaitOptions {
                until,
                timeout_ms,
                submission_deadline: None,
            }),
        }),
    })?;
    super::print_response(&response)
}

fn agent_send_keys(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: herdr agent send-keys <target> <key> [key ...]");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:send-keys".into(),
        method: Method::AgentSendKeys(AgentSendKeysParams {
            target: args[0].clone(),
            keys: args[1..].to_vec(),
        }),
    })?)
}

fn agent_read(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
        return Ok(2);
    };

    let mut source = ReadSource::Recent;
    let mut lines = None;
    let mut format = ReadFormat::Text;
    let mut strip_ansi = true;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --source");
                    return Ok(2);
                };
                source = super::parse_read_source(value)?;
                index += 2;
            }
            "--lines" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --lines");
                    return Ok(2);
                };
                lines = Some(super::parse_u32_flag("--lines", value)?);
                index += 2;
            }
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --format");
                    return Ok(2);
                };
                format = super::parse_read_format(value)?;
                strip_ansi = !matches!(format, ReadFormat::Ansi);
                index += 2;
            }
            "--ansi" => {
                format = ReadFormat::Ansi;
                strip_ansi = false;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let response = super::send_request(&Request {
        id: "cli:agent:read".into(),
        method: Method::AgentRead(AgentReadParams {
            target: target.clone(),
            source,
            lines,
            format,
            strip_ansi,
        }),
    })?;
    super::print_read_response(&response)
}

fn print_agent_help() {
    eprintln!("herdr agent commands:");
    eprintln!("  herdr agent list");
    eprintln!("  herdr agent get <target>");
    eprintln!("  herdr agent read <target> [--source visible|recent|recent-unwrapped|detection] [--lines N] [--format text|ansi] [--ansi]");
    eprintln!("  herdr agent send-keys <target> <key> [key ...]");
    eprintln!("  herdr agent prompt <target> <text> [--wait] [--until STATUS]... [--timeout MS]");
    eprintln!("  herdr agent rename <target> <name>|--clear");
    eprintln!("  herdr agent focus <target>");
    eprintln!("  herdr agent wait <target> [--until STATUS]... [--timeout MS]");
    eprintln!("  herdr agent attach <target> [--takeover]");
    eprintln!(
        "  herdr agent start <name> --kind KIND --pane ID [--timeout MS] [-- <agent-args...>]"
    );
    eprintln!("  herdr agent switch-account <target> <account> [--yes] [--interrupt] [--force] [--timeout MS] [--json]");
    eprintln!("  herdr agent explain <target> [--json|--format text|json] [--verbose]");
    eprintln!(
        "  herdr agent explain --file PATH --agent LABEL [--json|--format text|json] [--verbose]"
    );
    eprintln!("  targets accept unique agent names and pane ids that currently host agents");
    eprintln!("  kinds: {}", super::spec::agent_kind_values().join("|"));
}

fn parse_timeout(value: &str) -> Result<u64, i32> {
    super::parse_u64_flag("--timeout", value).map_err(|err| {
        eprintln!("{err}");
        2
    })
}
