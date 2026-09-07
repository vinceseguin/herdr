//! The runtime half of account launches (fork).
//!
//! Everything in the rest of `src/accounts/` is data. This module is the only
//! part that talks to a server, and it does it exclusively through stock API
//! methods — `pane.process_info`, `pane.send_text`, `pane.report_metadata` —
//! so a LAN host running upstream herdr behaves identically. Decision (b) of
//! the E9 plan: no server change, no new `agent.start` field an old server
//! would accept and quietly ignore.
//!
//! The order matters and is the whole safety argument:
//!
//! 1. read the pane's shell and **refuse unless it is idle at its prompt**, so
//!    the assignment can only ever reach a shell;
//! 2. type the assignment and wait for the shell to come back to its prompt,
//!    so `agent.start` finds the pane the way it expects it;
//! 3. let the caller run the stock `agent.start` — the pty delivers the two
//!    lines in order, so the agent's command line is read after the export has
//!    been executed;
//! 4. read the launched process's **own** environment and report `ok` only
//!    when it really names the profile's directory. Where that cannot be read
//!    the answer is `unverified`; where it names something else it is
//!    `mismatch`, and the command fails.
//!
//! Step 4 is why `AccountState::Ok` is never assumed from a successful send:
//! the export could have been swallowed by a shell that was not where herdr
//! thought it was, and billing the wrong Claude account is silent.

use std::time::{Duration, Instant};

use crate::accounts::launch::{pane_shell_at_prompt, plan_launch, LaunchError, LaunchPlan};
use crate::accounts::layout::{inspect, InspectOptions};
use crate::accounts::profile::{AccountProfile, Choice, ChoiceError, Profiles};
use crate::accounts::tokens::{
    AccountState, ACCOUNT_STATE_TOKEN, ACCOUNT_TOKEN, AGENT_LABEL, APPLIES_TO_SOURCE,
    METADATA_SOURCE,
};
use crate::api::schema::{
    Method, PaneProcessInfo, PaneProcessInfoParams, PaneReportMetadataParams, PaneSendTextParams,
    Request,
};

/// How long to wait for the shell to finish running the assignment before
/// starting the agent anyway. `agent.start` has its own busy retry, and the
/// verification step catches a launch that missed the export, so this is a
/// politeness budget rather than a correctness one.
const SHELL_SETTLE_TIMEOUT: Duration = Duration::from_millis(2_000);
const SHELL_SETTLE_POLL: Duration = Duration::from_millis(50);

/// Give the pty time to deliver the line before the first poll; a poll that
/// races ahead of it would see the shell still at its prompt and conclude the
/// assignment had already run.
const SHELL_SETTLE_GRACE: Duration = Duration::from_millis(150);

/// What a launch under a profile ended up doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutcome {
    /// The profile name, as reported in `tokens.account`.
    pub account: String,
    pub account_state: AccountState,
    /// The line that was typed, for the record and for a failure message that
    /// tells the user what to rerun by hand.
    pub line: String,
    /// The directory the launched process really names, when it could be read
    /// and differs from the profile's. Only ever set for `mismatch`.
    pub actual_config_dir: Option<String>,
    pub warnings: Vec<String>,
}

/// Why an account launch could not be carried out.
///
/// Split from [`LaunchError`] because a caller has to be able to say whether
/// anything was typed into the pane yet.
#[derive(Debug)]
pub enum AccountLaunchError {
    /// The plan was refused. Nothing was typed.
    Plan(LaunchError),
    /// A server call failed. `typed` says whether the assignment had already
    /// been sent, because that changes what the pane is left holding.
    Api {
        method: &'static str,
        detail: String,
        typed: bool,
    },
}

impl std::fmt::Display for AccountLaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(error) => write!(formatter, "{error}"),
            Self::Api {
                method,
                detail,
                typed: false,
            } => write!(formatter, "{method} failed: {detail}"),
            Self::Api {
                method,
                detail,
                typed: true,
            } => write!(
                formatter,
                "{method} failed: {detail}; the environment line was already typed into the pane, \
                 so a `claude` started there by hand would still use the requested profile"
            ),
        }
    }
}

/// Resolve `--account` against the configured profiles.
///
/// `Ok(None)` is "run exactly as a stock herdr would": no profile applies, so
/// nothing is typed and no token is reported.
pub fn choose_profile<'a>(
    profiles: &'a Profiles,
    requested: Option<&str>,
) -> Result<Option<&'a AccountProfile>, ChoiceError> {
    match profiles.choose(requested)? {
        Choice::Profile(profile) => Ok(Some(profile)),
        Choice::None => Ok(None),
    }
}

/// Read a pane's process table.
fn pane_process_info(
    pane_id: &str,
    request_id: &'static str,
    method: &'static str,
    typed: bool,
) -> Result<PaneProcessInfo, AccountLaunchError> {
    let response = crate::cli::send_request(&Request {
        id: request_id.into(),
        method: Method::PaneProcessInfo(PaneProcessInfoParams {
            pane_id: Some(pane_id.to_owned()),
        }),
    })
    .map_err(|err| AccountLaunchError::Api {
        method,
        detail: err.to_string(),
        typed,
    })?;
    if let Some(error) = response.get("error") {
        return Err(AccountLaunchError::Api {
            method,
            detail: error.to_string(),
            typed,
        });
    }
    serde_json::from_value(response["result"]["process_info"].clone()).map_err(|err| {
        AccountLaunchError::Api {
            method,
            detail: format!("unreadable process_info: {err}"),
            typed,
        }
    })
}

/// Decide the launch: which shell the pane is at, and the line to type.
///
/// Everything that can refuse, refuses here — before a byte reaches the pane.
pub fn prepare(
    profile: &AccountProfile,
    pane_id: &str,
    name: &str,
    args: &[String],
) -> Result<LaunchPlan, AccountLaunchError> {
    let info = pane_process_info(
        pane_id,
        "cli:accounts:launch:process_info",
        "pane.process_info",
        false,
    )?;
    let shell = pane_shell_at_prompt(&info).map_err(AccountLaunchError::Plan)?;
    let inspection = inspect(profile, InspectOptions::health());
    plan_launch(profile, &inspection, pane_id, name, args, shell).map_err(AccountLaunchError::Plan)
}

/// Type the assignment, then wait for the shell to be back at its prompt.
///
/// The wait is not the ordering guarantee — the pty delivers the export and
/// the agent's command line in order, and the shell reads them in order — it
/// only keeps `agent.start` from meeting a pane that is momentarily busy.
pub fn apply_env(plan: &LaunchPlan) -> Result<(), AccountLaunchError> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:launch:send_text".into(),
        method: Method::PaneSendText(PaneSendTextParams {
            pane_id: plan.pane_id.clone(),
            text: format!("{}\r", plan.line),
        }),
    })
    .map_err(|err| AccountLaunchError::Api {
        method: "pane.send_text",
        detail: err.to_string(),
        // The request never reached the server, or its reply never came back.
        // Treat it as typed: assuming otherwise would understate what the pane
        // may be holding.
        typed: true,
    })?;
    if let Some(error) = response.get("error") {
        return Err(AccountLaunchError::Api {
            method: "pane.send_text",
            detail: error.to_string(),
            typed: false,
        });
    }

    wait_for_prompt(plan);
    Ok(())
}

/// Poll until the pane's shell holds the foreground again, or the budget runs
/// out. Best effort on purpose: a pane that never settles is handled by
/// `agent.start`'s own busy path and, ultimately, by verification.
fn wait_for_prompt(plan: &LaunchPlan) {
    std::thread::sleep(SHELL_SETTLE_GRACE);
    let deadline = Instant::now() + SHELL_SETTLE_TIMEOUT;
    loop {
        let settled = pane_process_info(
            &plan.pane_id,
            "cli:accounts:launch:settle",
            "pane.process_info",
            true,
        )
        .map(|info| pane_shell_at_prompt(&info).is_ok_and(|shell| shell.pid == plan.shell.pid))
        .unwrap_or(false);
        if settled {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::debug!(
                pane_id = %plan.pane_id,
                "pane did not return to its shell prompt after the account environment line"
            );
            return;
        }
        std::thread::sleep(SHELL_SETTLE_POLL.min(remaining));
    }
}

/// Read the launched process's own environment and grade the launch.
///
/// Every process in the pane's foreground job was execed by the pane shell, so
/// each one carries the environment the shell had at that moment. Reading all
/// of them and demanding they agree is stricter than picking the one that
/// looks like `claude`: a disagreement anywhere is reported as a mismatch
/// rather than resolved in the launch's favour.
///
/// Nothing readable at all — a platform without `/proc`, a process that has
/// already exited, a permission error — is [`AccountState::Unverified`]. It is
/// never [`AccountState::Ok`].
pub fn verify(plan: &LaunchPlan) -> (AccountState, Option<String>) {
    let Ok(info) = pane_process_info(
        &plan.pane_id,
        "cli:accounts:launch:verify",
        "pane.process_info",
        true,
    ) else {
        return (AccountState::Unverified, None);
    };
    let Some(shell_pid) = info.shell_pid else {
        // Without knowing which process is the pane's shell, its exec-time
        // environment cannot be excluded — and that one legitimately holds a
        // different directory. Reporting a mismatch from it would be as wrong
        // as reporting ok, so the honest answer is that nothing was verified.
        return (AccountState::Unverified, None);
    };
    let variable = plan.profile.agent.config_dir_env_var();
    let expected =
        crate::accounts::config::dir_key(std::path::Path::new(&plan.expected_config_dir));

    let mut read_any = false;
    for process in &info.foreground_processes {
        if process.pid == shell_pid {
            // The pane shell's `/proc` environment is its *exec-time* one, so
            // it never shows the assignment that was just typed. Reading it
            // would produce a mismatch on every launch.
            continue;
        }
        let Some(value) = crate::platform::process_env_var(process.pid, variable) else {
            continue;
        };
        read_any = true;
        if crate::accounts::config::dir_key(std::path::Path::new(&value)) != expected {
            return (AccountState::Mismatch, Some(value));
        }
    }

    if read_any {
        (AccountState::Ok, None)
    } else {
        (AccountState::Unverified, None)
    }
}

/// Record the account on the pane so every reader — `agent list`, the sidebar,
/// the fleet report — sees the same fact.
///
/// Scoped to the Claude integration's source, so the server drops it when the
/// Claude process exits and a stale account never outlives its agent. A server
/// that rejects the report is a warning, not a failed launch: the agent is
/// already running under the right profile.
pub fn report(plan: &LaunchPlan, state: AccountState) -> Result<(), String> {
    let mut tokens = std::collections::HashMap::new();
    tokens.insert(ACCOUNT_TOKEN.to_string(), Some(plan.profile.name.clone()));
    tokens.insert(
        ACCOUNT_STATE_TOKEN.to_string(),
        Some(state.as_str().to_string()),
    );

    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:launch:report_metadata".into(),
        method: Method::PaneReportMetadata(PaneReportMetadataParams {
            pane_id: plan.pane_id.clone(),
            source: METADATA_SOURCE.to_string(),
            agent: Some(AGENT_LABEL.to_string()),
            applies_to_source: Some(APPLIES_TO_SOURCE.to_string()),
            title: None,
            display_agent: None,
            state_labels: std::collections::HashMap::new(),
            tokens,
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels: false,
            seq: None,
            ttl_ms: None,
        }),
    })
    .map_err(|err| err.to_string())?;
    match response.get("error") {
        Some(error) => Err(error.to_string()),
        None => Ok(()),
    }
}

/// Grade the launch and record it. Called once `agent.start` has reported the
/// agent ready.
pub fn finish(plan: &LaunchPlan) -> LaunchOutcome {
    let (account_state, actual_config_dir) = verify(plan);
    let mut warnings = plan.warnings.clone();
    if let Err(detail) = report(plan, account_state) {
        warnings.push(format!(
            "could not record the account on pane {}: {detail}; the agent is running under {:?} \
             but `herdr agent list` will not show it",
            plan.pane_id, plan.profile.name
        ));
    }
    LaunchOutcome {
        account: plan.profile.name.clone(),
        account_state,
        line: plan.line.clone(),
        actual_config_dir,
        warnings,
    }
}
