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
//!    when it really names the profile's directory. Where that environment
//!    cannot be read at all the answer is `unverified`; where it was read and
//!    names something else — including *nothing at all*, which means the agent
//!    is running against its own default directory — it is `mismatch`, and the
//!    command fails.
//!
//! Step 4 is why `AccountState::Ok` is never assumed from a successful send:
//! the export could have been swallowed by a shell that was not where herdr
//! thought it was (a `read` builtin, a continuation prompt), and billing the
//! wrong Claude account is silent.
//!
//! The probe reads `/proc` on the machine this CLI runs on, which is the
//! machine the pane lives on: `crate::cli::send_request` only ever speaks to
//! the local API socket, so the pids `pane.process_info` reports are local
//! pids.

use std::time::{Duration, Instant};

use crate::accounts::launch::{pane_shell_at_prompt, plan_launch, LaunchError, LaunchPlan};
use crate::accounts::layout::{inspect, InspectOptions};
use crate::accounts::profile::{AccountProfile, Choice, ChoiceError, Profiles};
use crate::accounts::switch::{
    Action, AgentSnapshot, LaunchRequest, Observation, PaneReading, SwitchError, SwitchFailure,
    SwitchInput, SwitchMachine, SwitchResult,
};
use crate::accounts::tokens::{
    AccountState, ACCOUNT_STATE_TOKEN, ACCOUNT_TOKEN, AGENT_LABEL, APPLIES_TO_SOURCE,
    METADATA_SOURCE,
};
use crate::api::schema::{
    AgentInfo, AgentPromptParams, AgentSendKeysParams, AgentTarget, Method, PaneProcessInfo,
    PaneProcessInfoParams, PaneReportMetadataParams, PaneSendTextParams, PaneTarget, Request,
};
use crate::platform::ProcessEnvVar;

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
    /// The variable the profile is applied through, so a message can name it
    /// without reaching back into the plan.
    pub variable: &'static str,
    /// The directory the launched process really names, when its environment
    /// was read and disagreed with the profile. `None` alongside
    /// [`AccountState::Mismatch`] means the environment was read and the
    /// variable was not there at all.
    pub actual_config_dir: Option<String>,
    pub warnings: Vec<String>,
}

impl LaunchOutcome {
    /// What the launched process's environment actually said, phrased for the
    /// error the CLI prints on a mismatch.
    ///
    /// The two cases read very differently to a user: a wrong directory is a
    /// misconfiguration, while no directory at all means the assignment never
    /// ran and the agent is on whatever account Claude picks by itself.
    pub fn mismatch_detail(&self) -> String {
        match self.actual_config_dir.as_deref() {
            Some(dir) => format!("its {} is {dir:?}", self.variable),
            None => format!(
                "it has no {} at all, so it is running against Claude's own default directory",
                self.variable
            ),
        }
    }
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

/// A pane that has had the environment line typed into it.
///
/// The exported directory outlives everything that happens next: whatever
/// `agent.start` makes of the pane, its shell keeps the variable until it
/// exits. So a launch that ends anywhere other than [`AppliedLine::finish`]
/// says so on the way out — a later `claude` started in that pane, by hand or
/// by a `--account none` start, would otherwise run under a profile nobody
/// mentioned, which is the same silent wrong-account failure this module
/// exists to prevent. `agent.start` can fail long after the line landed (busy
/// pane, lost terminal, readiness timeout, transport error), and each of those
/// returns from a different place, so the notice is tied to the value's
/// lifetime rather than repeated at every exit.
pub struct AppliedLine {
    plan: LaunchPlan,
    graded: bool,
}

impl AppliedLine {
    /// The plan behind the line, for a caller that needs to name it before
    /// [`AppliedLine::finish`] consumes the guard.
    pub fn plan(&self) -> &LaunchPlan {
        &self.plan
    }

    /// Let the guard go without grading, reporting or the notice, because the
    /// shell that held the line is gone: the pane was replaced by another
    /// terminal underneath. There is nothing left to warn about, and grading
    /// a stranger's processes or recording an account on that pane would both
    /// be claims about a shell that ran nothing of ours.
    pub fn abandon(mut self) {
        self.graded = true;
    }

    /// The note [`Drop`] would print, handed to a caller that has somewhere
    /// better to put it than stderr — a TUI modal, say, where an `eprintln!`
    /// would land on top of the rendered screen and be lost. Taking it
    /// disarms the print, so the warning is delivered exactly once, and the
    /// caller must show it: the pane is still holding the exported line.
    pub fn take_note(mut self) -> String {
        self.graded = true;
        stranded_line_note(&self.plan)
    }

    /// Grade the launch and record it. Called once `agent.start` has reported
    /// the agent ready; taking `self` is what disarms the notice above.
    pub fn finish(mut self) -> LaunchOutcome {
        self.graded = true;
        let plan = &self.plan;
        let (account_state, actual_config_dir) = verify(plan);
        let mut warnings = plan.warnings.clone();
        if let Err(detail) = report(plan, account_state) {
            warnings.push(format!(
                "could not record the account on pane {}: {detail}; the agent is running under \
                 {:?} but `herdr agent list` will not show it",
                plan.pane_id, plan.profile.name
            ));
        }
        LaunchOutcome {
            account: plan.profile.name.clone(),
            account_state,
            line: plan.line.clone(),
            variable: plan.profile.agent.config_dir_env_var(),
            actual_config_dir,
            warnings,
        }
    }
}

/// What a pane is left holding when a launch fails after the line landed.
///
/// One wording, whether it is printed to a CLI's stderr or folded into a TUI
/// modal: the shell keeps the variable until it exits, so the next `claude`
/// started there — by hand, or by a `--account none` start — uses it.
fn stranded_line_note(plan: &LaunchPlan) -> String {
    format!(
        "{} was already exported in pane {} when the start failed; that shell still points at \
         {:?} (account {:?}) until it exits, so anything started there — including a later \
         `--account none` start — will use it.",
        plan.profile.agent.config_dir_env_var(),
        plan.pane_id,
        plan.expected_config_dir,
        plan.profile.name,
    )
}

impl Drop for AppliedLine {
    fn drop(&mut self) {
        if self.graded {
            return;
        }
        eprintln!("note: {}", stranded_line_note(&self.plan));
    }
}

/// Type the assignment, then wait for the shell to be back at its prompt.
///
/// The wait is not the ordering guarantee — the pty delivers the export and
/// the agent's command line in order, and the shell reads them in order — it
/// only keeps `agent.start` from meeting a pane that is momentarily busy.
///
/// Consumes the plan and hands back the guard that owns it, so the pane's new
/// state cannot be forgotten by a caller that fails later.
pub fn apply_env(plan: LaunchPlan) -> Result<AppliedLine, AccountLaunchError> {
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
        // The server rejected the call (no such pane, bad text), so nothing
        // was written to the pty.
        return Err(AccountLaunchError::Api {
            method: "pane.send_text",
            detail: error.to_string(),
            typed: false,
        });
    }

    wait_for_prompt(&plan);
    Ok(AppliedLine {
        plan,
        graded: false,
    })
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
/// Every process in the pane's foreground job was execed by the pane shell
/// *after* the assignment was typed, so each one carries the environment the
/// shell had at that moment. Reading all of them and demanding they agree is
/// stricter than picking the one that looks like `claude`: a disagreement
/// anywhere is reported as a mismatch rather than resolved in the launch's
/// favour.
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
    let readings: Vec<ProcessEnvVar> = info
        .foreground_processes
        .iter()
        // The pane shell's `/proc` environment is its *exec-time* one, so it
        // never shows the assignment that was just typed. Reading it would
        // grade every launch against the wrong process.
        .filter(|process| process.pid != shell_pid)
        .map(|process| crate::platform::process_env_var(process.pid, variable))
        .collect();
    grade(&readings, &plan.expected_config_dir)
}

/// Turn what the probe read into a verdict, with no I/O, so the rule that
/// decides whether herdr claims an account is testable on its own.
///
/// The three outcomes:
///
/// * any reading that names a **different** directory is a mismatch, whatever
///   the others say — a launch is never resolved in its own favour;
/// * otherwise a reading that names the **expected** directory is the evidence
///   `ok` requires;
/// * otherwise, if any environment was read at all and **none** of them
///   carried the variable, the assignment did not reach the launched job: the
///   agent is running against the agent's own default directory, which is a
///   different account than the one asked for, so that is a mismatch too —
///   reporting it as merely unverified would hide a wrong account behind a
///   word that means "could not check";
/// * only when nothing could be read is the answer
///   [`AccountState::Unverified`].
fn grade(readings: &[ProcessEnvVar], expected_config_dir: &str) -> (AccountState, Option<String>) {
    let expected = crate::accounts::config::dir_key(std::path::Path::new(expected_config_dir));
    let mut read_any = false;
    let mut matched = false;
    for reading in readings {
        match reading {
            ProcessEnvVar::Unreadable => {}
            ProcessEnvVar::Unset => read_any = true,
            ProcessEnvVar::Set(value) => {
                read_any = true;
                if crate::accounts::config::dir_key(std::path::Path::new(value)) != expected {
                    return (AccountState::Mismatch, Some(value.clone()));
                }
                matched = true;
            }
        }
    }

    match (matched, read_any) {
        (true, _) => (AccountState::Ok, None),
        (false, true) => (AccountState::Mismatch, None),
        (false, false) => (AccountState::Unverified, None),
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

// ---------------------------------------------------------------------------
// The switch driver (PR 5).
//
// `crate::accounts::switch` decides *what* to do; this half carries it out
// against a stock server. Keeping the two apart is what makes every failure
// path of a command that can lose a conversation testable without a pty.
//
// Two steps are handed back to the caller as closures rather than done here:
// asking the user (only the CLI knows whether there is a terminal, and PR 8's
// TUI will ask in a modal) and the relaunch itself (which has to run the stock
// `agent.start` retry loop that lives in `crate::cli::agent`, rather than grow
// a second copy of it here).
// ---------------------------------------------------------------------------

/// What the user said when asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    Yes,
    No,
    /// Nobody could be asked: not a terminal, and no `--yes`.
    Unavailable,
}

/// A finished switch, plus anything worth telling the user that did not stop
/// it (a profile with no hook, a metadata report the server refused).
pub struct SwitchOutcome {
    pub result: SwitchResult,
    pub warnings: Vec<String>,
}

/// Run the switch protocol against the local server.
///
/// `confirm` is asked exactly once, before anything is sent. `launch` performs
/// the two-step relaunch and hands back the guard that owns the pane's new
/// environment; it is graded here, once the resumed session has been proven.
pub fn switch_account(
    input: SwitchInput,
    confirm: &mut dyn FnMut(&str) -> Confirmation,
    launch: &mut dyn FnMut(&LaunchRequest) -> Result<AppliedLine, SwitchError>,
    // Boxed: the failure carries the error, both ids and the warnings, and a
    // fat `Err` on every `Result` in the loop is what clippy's
    // `result_large_err` objects to.
) -> Result<SwitchOutcome, Box<SwitchFailure>> {
    let mut machine = SwitchMachine::new(input);
    let clock = Instant::now();
    let mut applied: Option<AppliedLine> = None;
    let mut warnings: Vec<String> = Vec::new();
    let mut action = machine.start();

    loop {
        let observation = match action {
            Action::Finish(result) => {
                return match *result {
                    Ok(result) => Ok(SwitchOutcome { result, warnings }),
                    Err(error) => Err(conclude(&machine, error, applied.take(), warnings)),
                };
            }
            Action::AskConfirm(text) => match confirm(&text) {
                Confirmation::Yes => Observation::Confirmed(true),
                Confirmation::No => Observation::Confirmed(false),
                Confirmation::Unavailable => Observation::ConfirmUnavailable,
            },
            Action::SendKeys(keys) => send_agent_keys(machine.pane_id(), keys),
            Action::Prompt(text) => submit_agent_prompt(machine.pane_id(), &text),
            Action::PollAgent => match read_agent(machine.agent_target()) {
                Ok(snapshot) => Observation::agent(snapshot),
                // A read that failed outright (not "no such agent", which is
                // an observation) ends the protocol the same way any other
                // failure does, so a relaunched agent is still graded and
                // recorded rather than forgotten with the guard's note.
                Err(error) => return Err(conclude(&machine, error, applied.take(), warnings)),
            },
            Action::PollPane => Observation::Pane(read_pane(machine.pane_id())),
            Action::Wait(millis) => {
                std::thread::sleep(Duration::from_millis(millis));
                Observation::Tick
            }
            Action::Launch(request) => match launch(&request) {
                Ok(line) => {
                    applied = Some(line);
                    Observation::launched(Ok(()))
                }
                Err(error) => Observation::launched(Err(error)),
            },
            Action::Grade => {
                let Some(line) = applied.take() else {
                    let error = SwitchError::Api {
                        method: "switch".into(),
                        detail: "nothing to verify: the relaunch left no record".into(),
                    };
                    return Err(conclude(&machine, error, None, warnings));
                };
                let outcome = line.finish();
                warnings.extend(outcome.warnings.clone());
                Observation::Graded {
                    state: outcome.account_state,
                    detail: match outcome.account_state {
                        AccountState::Mismatch => Some(outcome.mismatch_detail()),
                        _ => None,
                    },
                }
            }
        };
        let now = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        action = machine.next(now, observation);
    }
}

/// Turn a protocol failure into what the caller reports, settling the pane's
/// environment guard on the way.
///
/// When the relaunch had succeeded but the protocol did not (the hook never
/// reported, or reported another conversation), the agent really is running
/// under the new profile: it is graded and recorded exactly as a success would
/// be, and the failure carries a warning saying so. Saying nothing would leave
/// a running agent with no account at all, which reads as "unknown" when it is
/// in fact known. The one exception is a pane that is no longer the terminal
/// the switch started on: there is nothing of ours left to grade there, and
/// recording an account on it would claim one for a shell that runs nothing.
fn conclude(
    machine: &SwitchMachine,
    error: SwitchError,
    applied: Option<AppliedLine>,
    mut warnings: Vec<String>,
) -> Box<SwitchFailure> {
    if let Some(applied) = applied {
        if matches!(error, SwitchError::PaneReplaced { .. }) {
            applied.abandon();
        } else {
            let outcome = applied.finish();
            warnings.extend(outcome.warnings);
            warnings.push(format!(
                "the agent is running under account {:?} ({}); the switch failed after it \
                 started",
                outcome.account, outcome.account_state,
            ));
        }
    }
    let mut failure = machine.failure(error);
    failure.warnings = warnings;
    Box::new(failure)
}

/// Read one agent by target. `Ok(None)` means the target resolves to no agent.
fn read_agent(target: &str) -> Result<Option<AgentSnapshot>, SwitchError> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:switch:agent_get".into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    })
    .map_err(|err| SwitchError::Api {
        method: "agent.get".into(),
        detail: err.to_string(),
    })?;
    if let Some(error) = response.get("error") {
        if error["code"].as_str() == Some("agent_not_found") {
            return Ok(None);
        }
        return Err(SwitchError::Api {
            method: "agent.get".into(),
            detail: error.to_string(),
        });
    }
    let info: AgentInfo =
        serde_json::from_value(response["result"]["agent"].clone()).map_err(|err| {
            SwitchError::Api {
                method: "agent.get".into(),
                detail: format!("unreadable agent: {err}"),
            }
        })?;
    Ok(Some(AgentSnapshot::from_agent_info(&info)))
}

/// Is the pane back to being a plain shell herdr may start an agent in?
///
/// Two independent facts, and both are needed. The process table says the
/// Claude process is gone; `agent.get` says the *server* has released the
/// terminal, which is what `agent.start` checks before it will accept the pane
/// at all. Waiting on only the first would relaunch into a pane the server
/// still calls busy, after the environment line had already been typed.
fn read_pane(pane_id: &str) -> PaneReading {
    // `typed: false`: while the pane is being watched for its shell, nothing
    // has been typed into it yet, and an error here must not say otherwise.
    let info = match pane_process_info(
        pane_id,
        "cli:accounts:switch:process_info",
        "pane.process_info",
        false,
    ) {
        Ok(info) => info,
        Err(error) => {
            return PaneReading::Unreadable {
                detail: error.to_string(),
            }
        }
    };
    if let Err(error) = pane_shell_at_prompt(&info) {
        return PaneReading::Busy {
            detail: error.to_string(),
        };
    }
    match read_agent(pane_id) {
        Ok(Some(agent)) => PaneReading::Busy {
            detail: format!(
                "the server still holds a {} agent on this pane",
                agent.agent.as_deref().unwrap_or("running")
            ),
        },
        // Released only once the pane's identity is known too: a launch is
        // never allowed on a pane that cannot prove it is still the terminal
        // the switch started on.
        Ok(None) => match pane_terminal_id(pane_id) {
            Some(terminal_id) => PaneReading::Released { terminal_id },
            None => PaneReading::Unreadable {
                detail: "pane.get did not report the pane's terminal".to_string(),
            },
        },
        Err(error) => PaneReading::Unreadable {
            detail: error.to_string(),
        },
    }
}

/// The terminal currently attached to a pane, for the check that the switch is
/// still talking to the pane it started on. `None` when it cannot be read.
fn pane_terminal_id(pane_id: &str) -> Option<String> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:switch:pane_get".into(),
        method: Method::PaneGet(PaneTarget {
            pane_id: pane_id.to_owned(),
        }),
    })
    .ok()?;
    response["result"]["pane"]["terminal_id"]
        .as_str()
        .map(str::to_owned)
}

fn send_agent_keys(pane_id: &str, keys: Vec<String>) -> Observation {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:switch:send_keys".into(),
        method: Method::AgentSendKeys(AgentSendKeysParams {
            target: pane_id.to_owned(),
            keys,
        }),
    });
    observation_of_send(response, "agent.send_keys")
}

fn submit_agent_prompt(pane_id: &str, text: &str) -> Observation {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:switch:prompt".into(),
        method: Method::AgentPrompt(AgentPromptParams {
            target: pane_id.to_owned(),
            text: text.to_owned(),
            wait: None,
        }),
    });
    observation_of_send(response, "agent.prompt")
}

fn observation_of_send(response: std::io::Result<serde_json::Value>, method: &str) -> Observation {
    match response {
        // A transport failure is reported with a code of its own rather than
        // as a rejection to retry: retrying Escape at a server that is not
        // answering would only delay the failure.
        Err(err) => Observation::SendRejected {
            code: crate::accounts::switch::TRANSPORT_ERROR_CODE.to_string(),
            detail: format!("{method}: {err}"),
        },
        Ok(response) => match response.get("error") {
            Some(error) => Observation::SendRejected {
                code: error["code"].as_str().unwrap_or("error").to_string(),
                detail: error["message"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string()),
            },
            None => Observation::Sent,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: &str = "/p/work";

    fn set(dir: &str) -> ProcessEnvVar {
        ProcessEnvVar::Set(dir.to_string())
    }

    #[test]
    fn a_process_that_names_the_profile_directory_is_the_evidence_ok_requires() {
        assert_eq!(grade(&[set(WORK)], WORK), (AccountState::Ok, None));
        // Spelling is normalized the same way the profile's own directory is,
        // so a trailing slash is the same account.
        assert_eq!(grade(&[set("/p/work/")], WORK), (AccountState::Ok, None));
    }

    #[test]
    fn a_different_directory_anywhere_is_a_mismatch_naming_it() {
        assert_eq!(
            grade(&[set(WORK), set("/p/perso")], WORK),
            (AccountState::Mismatch, Some("/p/perso".to_string()))
        );
        assert_eq!(
            grade(&[set("/p/perso"), set(WORK)], WORK),
            (AccountState::Mismatch, Some("/p/perso".to_string()))
        );
    }

    /// The launch that would otherwise be graded most dangerously: the shell
    /// swallowed the assignment (a `read` builtin, a continuation prompt), so
    /// `claude` was execed without the variable and is billing the account it
    /// picks by itself. That is a wrong account, not an unknown one.
    #[test]
    fn an_environment_read_without_the_variable_is_a_mismatch_not_unverified() {
        assert_eq!(
            grade(&[ProcessEnvVar::Unset], WORK),
            (AccountState::Mismatch, None)
        );
        assert_eq!(
            grade(&[ProcessEnvVar::Unreadable, ProcessEnvVar::Unset], WORK),
            (AccountState::Mismatch, None)
        );
    }

    /// A child that scrubbed its own environment is not evidence against the
    /// process that does name the directory.
    #[test]
    fn one_reading_that_matches_outweighs_a_sibling_without_the_variable() {
        assert_eq!(
            grade(&[ProcessEnvVar::Unset, set(WORK)], WORK),
            (AccountState::Ok, None)
        );
    }

    /// Nothing read is nothing known: non-Linux targets, a process that has
    /// already exited, a refused `/proc` read, an empty foreground list.
    #[test]
    fn nothing_readable_is_unverified_and_never_ok() {
        assert_eq!(grade(&[], WORK), (AccountState::Unverified, None));
        assert_eq!(
            grade(
                &[ProcessEnvVar::Unreadable, ProcessEnvVar::Unreadable],
                WORK
            ),
            (AccountState::Unverified, None)
        );
    }

    /// A value that is not a directory at all still has to be *reported*, not
    /// swallowed: it is a different account than the one asked for.
    #[test]
    fn an_empty_value_is_a_mismatch_that_names_what_was_read() {
        assert_eq!(
            grade(&[set("")], WORK),
            (AccountState::Mismatch, Some(String::new()))
        );
    }

    #[test]
    fn a_mismatch_message_distinguishes_a_wrong_directory_from_no_directory() {
        let outcome = |actual: Option<&str>| LaunchOutcome {
            account: "work".to_string(),
            account_state: AccountState::Mismatch,
            line: " export CLAUDE_CONFIG_DIR='/p/work'".to_string(),
            variable: "CLAUDE_CONFIG_DIR",
            actual_config_dir: actual.map(str::to_string),
            warnings: Vec::new(),
        };
        let wrong = outcome(Some("/p/perso")).mismatch_detail();
        assert!(wrong.contains("CLAUDE_CONFIG_DIR"), "{wrong}");
        assert!(wrong.contains("/p/perso"), "{wrong}");

        let absent = outcome(None).mismatch_detail();
        assert!(absent.contains("no CLAUDE_CONFIG_DIR"), "{absent}");
        assert!(absent.contains("default directory"), "{absent}");
    }
}
