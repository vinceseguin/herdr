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

/// Where the switch has got to, for a caller that shows progress.
///
/// Derived from the action the driver is about to carry out rather than from
/// [`SwitchMachine`]'s private phase, so the machine keeps exactly one public
/// surface — the `Observation`/`Action` pair — and a progress line can never
/// disagree with what is actually being done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchPhase {
    /// Reading the agent for the first time and checking it can be switched.
    Preflight,
    /// Waiting for the user's answer.
    Confirm,
    /// Reading the agent again, after the answer, before anything is sent.
    Recheck,
    /// `Escape` and `/exit` on their way to Claude.
    Exit,
    /// Waiting for the pane to be a plain shell again.
    AwaitShell,
    /// The two-step relaunch under the new profile.
    Relaunch,
    /// Waiting for the hook to report the same conversation back.
    AwaitSession,
    /// Reading the relaunched process's environment and recording the account.
    Grade,
}

impl SwitchPhase {
    /// A line short enough for a modal, in the imperative present the rest of
    /// the account UI uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Preflight => "checking the agent...",
            Self::Confirm => "waiting for confirmation...",
            Self::Recheck => "re-checking the agent...",
            Self::Exit => "asking claude to exit...",
            Self::AwaitShell => "waiting for the shell...",
            Self::Relaunch => "resuming under the new account...",
            Self::AwaitSession => "waiting for the conversation...",
            Self::Grade => "verifying account...",
        }
    }
}

/// Which phase an action belongs to.
///
/// `PollAgent` is the only ambiguous one: the same action reads the agent at
/// preflight, again after the confirmation, and once more while waiting for
/// the resumed conversation, so the driver's own two flags disambiguate it.
fn phase_of(action: &Action, confirmed: bool, launched: bool) -> Option<SwitchPhase> {
    match action {
        Action::AskConfirm(_) => Some(SwitchPhase::Confirm),
        Action::SendKeys(_) | Action::Prompt(_) | Action::SubmitText(_) => Some(SwitchPhase::Exit),
        Action::PollPane => Some(SwitchPhase::AwaitShell),
        Action::Launch(_) => Some(SwitchPhase::Relaunch),
        Action::Grade => Some(SwitchPhase::Grade),
        Action::PollAgent => Some(match (launched, confirmed) {
            (true, _) => SwitchPhase::AwaitSession,
            (false, true) => SwitchPhase::Recheck,
            (false, false) => SwitchPhase::Preflight,
        }),
        // A sleep belongs to whatever it is waiting for, and the end of the
        // protocol is not a phase.
        Action::Wait(_) | Action::Finish(_) => None,
    }
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
    switch_account_with_progress(input, confirm, launch, &mut |_| {})
}

/// [`switch_account`], reporting each phase it enters.
///
/// The TUI shows these in its modal, because the protocol blocks for seconds
/// at a time and a modal that says nothing looks hung. `on_phase` is called
/// only when the phase *changes*, and never between a decision and a send: it
/// is a notification, not a hook a caller can act on.
pub fn switch_account_with_progress(
    input: SwitchInput,
    confirm: &mut dyn FnMut(&str) -> Confirmation,
    launch: &mut dyn FnMut(&LaunchRequest) -> Result<AppliedLine, SwitchError>,
    on_phase: &mut dyn FnMut(SwitchPhase),
) -> Result<SwitchOutcome, Box<SwitchFailure>> {
    let mut machine = SwitchMachine::new(input);
    let clock = Instant::now();
    let mut applied: Option<AppliedLine> = None;
    let mut warnings: Vec<String> = Vec::new();
    let mut action = machine.start();
    let mut confirmed = false;
    let mut launched = false;
    let mut reported: Option<SwitchPhase> = None;

    loop {
        if let Some(phase) = phase_of(&action, confirmed, launched) {
            if reported != Some(phase) {
                reported = Some(phase);
                on_phase(phase);
            }
        }
        let observation = match action {
            Action::Finish(result) => {
                return match *result {
                    Ok(result) => Ok(SwitchOutcome { result, warnings }),
                    Err(error) => Err(conclude(&machine, error, applied.take(), warnings)),
                };
            }
            Action::AskConfirm(text) => match confirm(&text) {
                Confirmation::Yes => {
                    confirmed = true;
                    Observation::Confirmed(true)
                }
                Confirmation::No => Observation::Confirmed(false),
                Confirmation::Unavailable => Observation::ConfirmUnavailable,
            },
            Action::SendKeys(keys) => send_agent_keys(machine.pane_id(), keys),
            Action::Prompt(text) => submit_agent_prompt(machine.pane_id(), &text),
            Action::SubmitText(text) => submit_pane_text(machine.pane_id(), &text),
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
                    launched = true;
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

/// `/exit` typed straight into the pane's pty.
///
/// The escape hatch for an agent the server will not accept a prompt for. It
/// carries none of `agent.prompt`'s guards, which is exactly why the machine
/// reaches it only for a usage-limited agent it has already pinned by pane and
/// terminal id, and why the text it submits is a constant rather than anything
/// a caller supplied. The trailing carriage return is what submits it:
/// `pane.send_text` sends raw bytes with no bracketed paste.
fn submit_pane_text(pane_id: &str, text: &str) -> Observation {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:switch:send_text".into(),
        method: Method::PaneSendText(PaneSendTextParams {
            pane_id: pane_id.to_owned(),
            text: format!("{text}\r"),
        }),
    });
    observation_of_send(response, "pane.send_text")
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

// ---------------------------------------------------------------------------
// The watcher (PR 10).
//
// `crate::accounts::watch` decides *what* to label; this half carries it out
// against a stock server and owns the two things a long-running process has to
// get right: it must survive the server going away and coming back, and it
// must not leave anything behind when it stops.
//
// Nothing here sends input to a pane. The only writes are
// `pane.report_metadata`, and the only reads are `agent.list`, `agent.explain`
// and `agent.read` — the same three `herdr account status` already makes, on
// the same terms.
// ---------------------------------------------------------------------------

/// How the watcher runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    /// How often the agent list is read.
    pub interval: Duration,
    /// Do one pass and stop, leaving whatever it labelled in place. The label
    /// is still leased, so a `--once` run that finds a limit leaves a badge
    /// that expires by itself rather than one that lasts forever.
    pub once: bool,
    /// One JSON object per line instead of one sentence per line. Newline
    /// delimited on purpose: a watcher is a stream, and a JSON array would
    /// never close.
    pub json: bool,
    /// Leave the labels in place on exit and let their leases expire instead.
    pub keep_labels: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            interval: DEFAULT_WATCH_INTERVAL,
            once: false,
            json: false,
            keep_labels: false,
        }
    }
}

/// Default poll interval: fast enough that a limit shows up while the human is
/// still looking at the screen, slow enough to be free on a machine with
/// dozens of panes (one `agent.list` per interval, whatever the pane count).
pub const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// The narrowest and widest intervals accepted.
///
/// The floor keeps a mistyped `--interval 1` from turning into a request loop;
/// the ceiling keeps the lease (four intervals) inside the server's own TTL
/// range and keeps `Ctrl-C` responsive.
pub const MIN_WATCH_INTERVAL: Duration = Duration::from_millis(500);
pub const MAX_WATCH_INTERVAL: Duration = Duration::from_secs(300);

/// How long the loop sleeps between interrupt checks.
///
/// The interval is slept in slices so `Ctrl-C` is answered in well under a
/// second even when someone asks for a five-minute poll.
const INTERRUPT_POLL: Duration = Duration::from_millis(100);

/// How long to wait after a failed poll before trying again, and the cap.
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Why the loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEnd {
    /// `--once` finished its pass.
    Once,
    /// `SIGINT`/`SIGTERM`.
    Interrupted,
    /// Something no amount of waiting fixes.
    Failed,
}

/// Run the watcher until it is interrupted, or once with
/// [`WatchOptions::once`].
///
/// Exit codes are the caller's, but the shape is: [`WatchEnd::Once`] and
/// [`WatchEnd::Interrupted`] are success, [`WatchEnd::Failed`] is not. A server
/// that is not running is *not* a failure — it is the ordinary state of a
/// machine whose herdr is being restarted, and the watcher waits for it.
pub fn watch(options: &WatchOptions) -> WatchEnd {
    let interrupted = install_interrupt_handler();

    let lease = crate::accounts::watch::lease_for(options.interval);
    let mut state = crate::accounts::watch::WatchState::new();
    let mut backoff = RECONNECT_BACKOFF_START;
    let mut connected = true;
    let mut warned_overflow = false;
    let mut end = WatchEnd::Interrupted;

    loop {
        if interrupted.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        match poll_agents() {
            Ok(agents) => {
                if !connected {
                    eprintln!("note: reconnected to the herdr server");
                    connected = true;
                }
                backoff = RECONNECT_BACKOFF_START;
                // A stdout that can no longer be written is a request to stop,
                // not a failure — the clears below still run.
                if !apply(&mut state, &agents, lease, options) {
                    break;
                }
                if state.overflowed() && !warned_overflow {
                    warned_overflow = true;
                    eprintln!(
                        "warning: more than {} panes with account agents; the rest are not \
                         watched",
                        crate::accounts::watch::MAX_TRACKED_PANES
                    );
                }
            }
            Err(PollError::Fatal(message)) => {
                eprintln!("herdr account watch: {message}");
                end = WatchEnd::Failed;
                break;
            }
            Err(PollError::Unavailable(message)) => {
                // Nothing is written and nothing is forgotten. A server that
                // restarted lost every token (`src/persist/snapshot.rs` carries
                // none) and a server that is merely unreachable kept them; the
                // watcher cannot tell the two apart from here and does not have
                // to, because the next successful poll compares what it
                // believes against what the server actually holds and re-labels
                // whatever went missing.
                if connected {
                    eprintln!("note: {message}; waiting for it to come back");
                    connected = false;
                }
                if options.once {
                    end = WatchEnd::Failed;
                    break;
                }
                if !sleep_interruptibly(backoff, &interrupted) {
                    break;
                }
                backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                continue;
            }
        }
        if options.once {
            end = WatchEnd::Once;
            break;
        }
        if !sleep_interruptibly(options.interval, &interrupted) {
            break;
        }
    }

    // Take the labels back off before leaving. Their leases would expire on
    // their own, but seconds of a stale `usage limit` badge on a recovered
    // agent is seconds of a reader being told the wrong thing.
    //
    // `--once` is the exception, and deliberately so: a one-shot pass has no
    // lifetime for a label to belong to, so it leaves what it found and lets
    // the lease expire it. Clearing on the way out would make `--once` a very
    // expensive no-op.
    //
    // A fatal end is the one case where they are not even attempted: the only
    // fatal today is a protocol the client cannot speak, so every clear would
    // fail — and `send_request` re-prints the mismatch on each one.
    if !options.keep_labels && !options.once && connected && end != WatchEnd::Failed {
        for action in state.drain_clears() {
            let crate::accounts::watch::WatchAction::Clear {
                pane_id,
                name,
                account,
                restore_state,
            } = &action
            else {
                continue;
            };
            match write_clear(pane_id, restore_state.as_deref()) {
                // Nobody may be reading stdout any more; the write that matters
                // on the way out is the one that already reached the server.
                Ok(()) => {
                    let _ = print_event(
                        options,
                        "cleared",
                        pane_id,
                        name.as_deref(),
                        account,
                        &crate::accounts::limit::UsageLimit::default(),
                    );
                }
                Err(err) => eprintln!(
                    "warning: could not clear the usage-limit label on pane {pane_id} \
                     (account {account}): {err}; it expires by itself within {}s",
                    lease.as_secs()
                ),
            }
        }
    }

    end
}

/// One poll: read the agents, act on what changed.
///
/// Returns false when stdout has gone away (`herdr account watch --json | head`
/// is an ordinary thing to type), which is a request to stop rather than a
/// failure — the same reading `herdr fleet watch` gives a closed pipe.
#[must_use]
fn apply(
    state: &mut crate::accounts::watch::WatchState,
    agents: &[crate::accounts::watch::WatchAgent],
    lease: Duration,
    options: &WatchOptions,
) -> bool {
    use crate::accounts::watch::WatchAction;

    let mut queue = state.observe(agents, lease, Instant::now());
    let mut guard = 0usize;
    let mut writable = true;
    while let Some(action) = queue.pop() {
        // A bound on the follow-up work one poll can generate. `Explain` is the
        // only action that produces another action, and it produces at most
        // one, so this can only trip if that ever changes.
        guard += 1;
        if guard > agents.len().saturating_mul(2) + 8 {
            break;
        }
        match action {
            WatchAction::Explain { pane_id } => {
                let Some(agent) = agents.iter().find(|agent| agent.pane_id == pane_id) else {
                    continue;
                };
                let limit = usage_limit_on(&pane_id);
                match state.explained(agent, limit, lease, Instant::now()) {
                    WatchAction::Nothing => {}
                    next => queue.push(next),
                }
            }
            WatchAction::Label {
                pane_id,
                name,
                account,
                limit,
                lease,
                announce,
            } => match write_label(&pane_id, lease) {
                Ok(()) => {
                    if announce {
                        writable &= print_event(
                            options,
                            "limited",
                            &pane_id,
                            name.as_deref(),
                            &account,
                            &limit,
                        );
                    }
                }
                Err(err) => {
                    state.label_failed(&pane_id);
                    eprintln!("warning: could not label pane {pane_id}: {err}");
                }
            },
            WatchAction::Clear {
                pane_id,
                name,
                account,
                restore_state,
            } => match write_clear(&pane_id, restore_state.as_deref()) {
                Ok(()) => {
                    writable &= print_event(
                        options,
                        "cleared",
                        &pane_id,
                        name.as_deref(),
                        &account,
                        &crate::accounts::limit::UsageLimit::default(),
                    );
                }
                Err(err) => {
                    if state.clear_failed(
                        &pane_id,
                        crate::accounts::limit::UsageLimit::default(),
                        Instant::now(),
                    ) {
                        eprintln!("warning: could not clear the label on pane {pane_id}: {err}");
                    } else {
                        eprintln!(
                            "warning: could not clear the label on pane {pane_id}: {err}; \
                             giving up on it — it expires by itself within {}s",
                            lease.as_secs()
                        );
                    }
                }
            },
            WatchAction::Nothing => {}
        }
    }
    writable
}

/// One line per label change, in the shape the caller asked for.
///
/// Returns false when the line could not be written, which stops the watcher
/// the way an interrupt does — clears included — instead of `println!`'s panic.
///
/// On unix a closed pipe usually never gets here: `begin_cli_output` puts
/// `SIGPIPE` back to `SIG_DFL` so `herdr account watch --json | head -3` ends by
/// signal like every other herdr CLI, and the labels it was holding are left to
/// their leases (which is what the leases are for). This is the rest of it —
/// Windows, where the write returns an error instead, and any other write
/// failure on a redirected stdout — where a panic would unwind straight past
/// the clears the exit path owes the server. `herdr fleet watch` draws the same
/// distinction (`src/cli/fleet.rs`).
#[must_use]
fn print_event(
    options: &WatchOptions,
    event: &str,
    pane_id: &str,
    name: Option<&str>,
    account: &str,
    limit: &crate::accounts::limit::UsageLimit,
) -> bool {
    use std::io::Write as _;

    let line = if options.json {
        let mut record = serde_json::Map::new();
        record.insert("event".into(), event.into());
        record.insert("pane_id".into(), pane_id.into());
        if let Some(name) = name {
            record.insert("name".into(), name.into());
        }
        record.insert("account".into(), account.into());
        if let Some(reset) = limit.reset_text.as_deref() {
            record.insert("reset_text".into(), reset.into());
        }
        serde_json::Value::Object(record).to_string()
    } else {
        let who = match name {
            Some(name) => format!("{name} ({pane_id})"),
            None => pane_id.to_string(),
        };
        match limit.reset_text.as_deref() {
            Some(reset) => format!("{event} {who} account={account} resets {reset}"),
            None => format!("{event} {who} account={account}"),
        }
    };
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
}

/// Why a poll did not produce an agent list.
enum PollError {
    /// No server, or one that went away. Worth waiting for.
    Unavailable(String),
    /// Nothing gets better by waiting.
    Fatal(String),
}

/// The running agents, reduced to what the fold needs.
fn poll_agents() -> Result<Vec<crate::accounts::watch::WatchAgent>, PollError> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:watch:agent_list".into(),
        method: Method::AgentList(crate::api::schema::EmptyParams::default()),
    })
    .map_err(|err| {
        if crate::cli::protocol_mismatch_was_reported(&err) {
            // The guard printed the mismatch already; waiting cannot fix a
            // server the client cannot speak to.
            PollError::Fatal("incompatible server protocol".to_string())
        } else if crate::cli::server_not_running_was_reported(&err) {
            PollError::Unavailable("no herdr server is running".to_string())
        } else {
            PollError::Unavailable(format!("could not list agents ({err})"))
        }
    })?;
    if let Some(error) = response.get("error") {
        return Err(PollError::Unavailable(format!(
            "could not list agents ({error})"
        )));
    }
    let agents: Vec<AgentInfo> = serde_json::from_value(response["result"]["agents"].clone())
        .map_err(|err| PollError::Unavailable(format!("could not read the agent list ({err})")))?;
    Ok(agents
        .iter()
        .map(crate::accounts::watch::WatchAgent::from_agent_info)
        .collect())
}

/// What herdr's detector says about one blocked pane's account usage.
///
/// The same two calls `herdr account status` makes, in the same order: the
/// verdict first, and the screen only once the rule has already matched. Every
/// failure answers `None`, which the fold reads as "no limit seen" and looks
/// again — never as "not limited".
fn usage_limit_on(pane_id: &str) -> Option<crate::accounts::limit::UsageLimit> {
    let explain = crate::cli::account::agent_explain(pane_id)?;
    // The agent list this pane came from is one round trip old, and the answer
    // is another. `explain.agent` is the detector's own view of what is in the
    // pane *now*, so it is the cheapest possible re-check that the evidence
    // being read still belongs to a Claude agent: a pane whose Claude exited
    // and whose next agent started in between is refused here rather than
    // labelled from a verdict about something else. It also pins the rule id to
    // the manifest that defines it, since `usage_limit` is a name another
    // agent's (possibly remotely fetched) manifest could reuse.
    if explain.get("agent").and_then(serde_json::Value::as_str) != Some(AGENT_LABEL) {
        return None;
    }
    if !crate::accounts::limit::matched_usage_limit(&explain) {
        return None;
    }
    let screen = crate::cli::account::detection_screen(pane_id).unwrap_or_default();
    crate::accounts::limit::classify(&explain, &screen)
}

/// Report the limit onto the pane, as a lease.
///
/// `ttl_ms` is the whole reason a stopped watcher is safe: the server drops
/// both the label and the token when the lease runs out
/// (`MetadataTokens::expire_at`, `TerminalState::agent_metadata_is_expired`),
/// so a watcher that was killed, crashed or lost its machine cannot strand a
/// `limited` badge on an agent that recovered hours ago.
fn write_label(pane_id: &str, lease: Duration) -> Result<(), String> {
    let mut state_labels = std::collections::HashMap::new();
    state_labels.insert(
        crate::accounts::watch::LIMIT_LABEL_STATE.to_string(),
        crate::accounts::watch::LIMIT_LABEL.to_string(),
    );
    let mut tokens = std::collections::HashMap::new();
    tokens.insert(
        ACCOUNT_STATE_TOKEN.to_string(),
        Some(AccountState::Limited.as_str().to_string()),
    );
    report_metadata(pane_id, state_labels, tokens, false, Some(lease))
}

/// Take the label back off, restoring the `account_state` it replaced.
///
/// `None` removes the token rather than writing `ok`: the watcher only knows
/// what it overwrote, and inventing a verified state for an agent it never
/// verified is exactly the silent wrong answer the account tooling exists to
/// avoid.
fn write_clear(pane_id: &str, restore_state: Option<&str>) -> Result<(), String> {
    let mut tokens = std::collections::HashMap::new();
    tokens.insert(
        ACCOUNT_STATE_TOKEN.to_string(),
        restore_state.map(str::to_string),
    );
    report_metadata(
        pane_id,
        std::collections::HashMap::new(),
        tokens,
        true,
        None,
    )
}

/// The one write the watcher makes, in the epic's own vocabulary.
///
/// The report is pinned with `agent = "claude"` and **not** with
/// `applies_to_source`, which is a correction to the plan and was measured in
/// the lab. `applies_to_source` gates a *presentation* report on the pane's
/// hook authority (`TerminalState::metadata_guards_match`): a state label
/// reported with it is silently dropped unless a `herdr:claude` hook has
/// already claimed the terminal, which the session-id report the Claude hook
/// makes does not do (`herdr:claude` is a reserved state source, PR 5). The
/// token half landed and the label half did not, which is the worst of both.
///
/// `agent = "claude"` buys everything the scoping was for. It is checked on the
/// same guard, so the label only shows while herdr still sees Claude in that
/// pane; `metadata_report_blocked_by_process_exit` refuses a report that races
/// the process exiting; and the exit sweep in `TerminalState` clears any
/// metadata whose `agent_label` is the agent that just left. Tokens were never
/// exit-scoped at all — they live in `TerminalState::metadata_tokens`, which
/// the exit path does not touch — which is precisely why the watcher leases
/// them.
fn report_metadata(
    pane_id: &str,
    state_labels: std::collections::HashMap<String, String>,
    tokens: std::collections::HashMap<String, Option<String>>,
    clear_state_labels: bool,
    lease: Option<Duration>,
) -> Result<(), String> {
    let response = crate::cli::send_request(&Request {
        id: "cli:accounts:watch:report_metadata".into(),
        method: Method::PaneReportMetadata(PaneReportMetadataParams {
            pane_id: pane_id.to_string(),
            source: METADATA_SOURCE.to_string(),
            agent: Some(AGENT_LABEL.to_string()),
            applies_to_source: None,
            title: None,
            display_agent: None,
            state_labels,
            tokens,
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels,
            seq: None,
            ttl_ms: lease.map(|lease| {
                u64::try_from(lease.as_millis())
                    .unwrap_or(u64::MAX)
                    .clamp(1, 86_400_000)
            }),
        }),
    })
    .map_err(|err| err.to_string())?;
    match response.get("error") {
        Some(error) => Err(error.to_string()),
        None => Ok(()),
    }
}

/// Catch `Ctrl-C` (and `SIGTERM`/`SIGHUP`, through ctrlc's `termination`
/// feature) so the watcher can take its labels off before it goes.
///
/// The same handler `src/client/mod.rs` and `src/server/headless.rs` install,
/// for the same reason: the process owns something a sudden exit would leave
/// behind. Failing to install one is a warning, not a refusal — the leases
/// still expire — and a *second* interrupt leaves immediately, because by then
/// the user has asked twice and the labels are the server's problem.
fn install_interrupt_handler() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&interrupted);
    if let Err(err) = ctrlc::set_handler(move || {
        if flag.swap(true, std::sync::atomic::Ordering::AcqRel) {
            std::process::exit(130);
        }
    }) {
        tracing::warn!(%err, "failed to install a termination handler for account watch");
        eprintln!(
            "warning: could not catch Ctrl-C ({err}); labels will expire on their own instead \
             of being cleared on exit"
        );
    }
    interrupted
}

/// Sleep `total`, in slices, answering an interrupt between them.
///
/// Returns false when the sleep was cut short by an interrupt.
fn sleep_interruptibly(total: Duration, interrupted: &std::sync::atomic::AtomicBool) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if interrupted.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep(INTERRUPT_POLL.min(deadline.saturating_duration_since(now)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watcher writes metadata onto agents it did not start, and the whole
    /// safety argument for that is that metadata is *all* it can write:
    /// decision (d) of the epic is "detect and suggest, never auto-switch".
    /// `crate::accounts::watch` pins the decision half by giving the action
    /// enum no vocabulary for typing; this pins the runtime half. The guard is
    /// scoped to the watcher's section rather than the file because `client.rs`
    /// legitimately types into panes for the launch and the switch — which is
    /// exactly why a driver that grew a `pane.send_text` here would look
    /// unremarkable in review.
    #[test]
    fn the_watch_driver_only_reads_and_reports_metadata() {
        const SOURCE: &str = include_str!("client.rs");
        const SECTION_START: &str = "// The watcher (PR 10).";

        let section = SOURCE
            .split_once(SECTION_START)
            .expect("the watcher section is still marked in this file")
            .1
            .split_once("#[cfg(test)]")
            .expect("the watcher section still ends where the tests begin")
            .0;
        assert!(
            section.contains("Method::PaneReportMetadata"),
            "the slice must really be the watcher"
        );
        for reaches_a_pane in [
            "PaneSendText",
            "AgentSendKeys",
            "AgentPrompt",
            "AgentStart",
            "submit_pane_text",
            "apply_env",
            "switch_account",
        ] {
            assert!(
                !section.contains(reaches_a_pane),
                "the watcher must never reach {reaches_a_pane}: it looks and labels, \
                 nothing else"
            );
        }
    }

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
