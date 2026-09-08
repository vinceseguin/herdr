//! Moving a running Claude agent to another account, as a state machine (fork).
//!
//! Decision (i) of the E9 plan. The dangerous part of an account switch is not
//! the environment variable — that is [`super::launch`] — it is that the agent
//! has to stop and start again *without losing its conversation*. Claude keeps
//! the conversation in a session id, and only a clean exit followed by
//! `claude --resume <id>` gets it back. So the protocol is:
//!
//! 1. **Preflight.** Refuse unless the target really is a Claude agent with a
//!    session id herdr can resume. An agent with no session id is refused
//!    *before anything is typed*: exiting it would destroy the conversation
//!    this command exists to preserve.
//! 2. **Confirm.** Always. Nothing here happens implicitly — read is safe,
//!    control is explicit. Then look again: a person may take minutes over
//!    the question, and the agent that was idle may be working by the time
//!    they answer, so the pinned pane is re-read and anything other than the
//!    confirmed agent, in the confirmed state, is a refusal.
//! 3. **Ask it to leave.** `Escape` first when the agent is blocked (or when
//!    a working agent is being interrupted on purpose), then `/exit`. Never a
//!    signal, never a kill: a Claude in the middle of a tool call is left to
//!    finish, which is why a `working` agent is refused without `--interrupt`.
//! 4. **Wait for the pane's own shell.** Bounded, and on timeout the machine
//!    stops and says so — it never escalates.
//! 5. **Relaunch** through the same two-step launch as `herdr agent start
//!    --account`, with `--resume <id>`.
//! 6. **Prove it.** The switch is only a success once the hook reports back
//!    the *same* session id (the server clears the persisted session when the
//!    process exits, so a report after the relaunch is the new process's) and
//!    the relaunched process's own environment names the new profile.
//!
//! The module is pure: it observes and decides, and every side effect is an
//! [`Action`] for a driver to carry out. That is what makes each failure path
//! testable without a server, and what lets PR 8's TUI drive exactly the same
//! protocol from a background thread.

use std::path::PathBuf;

use crate::accounts::layout::ProfileInspection;
use crate::accounts::profile::AccountProfile;
use crate::accounts::tokens::{AccountState, AGENT_LABEL, APPLIES_TO_SOURCE};
use crate::agent_resume::{AgentSessionRef, AgentSessionRefKind};
use crate::api::schema::{AgentInfo, AgentSessionInfo, AgentStatus};

/// Milliseconds on some monotonic clock the driver owns. The machine only ever
/// compares them, so its tests can hand it plain numbers.
pub type Millis = u64;

/// How long each waiting stage gets by default: long enough for a real Claude
/// to finish writing its transcript and exit, short enough that a wedged agent
/// is reported rather than waited on forever.
pub const DEFAULT_TIMEOUT_MS: Millis = 20_000;

/// The largest `--timeout` accepted, so a typo cannot park the command for a
/// day holding a half-switched pane.
pub const MAX_TIMEOUT_MS: Millis = 600_000;

/// How often the pane and the agent are re-read while waiting.
pub const POLL_INTERVAL_MS: Millis = 250;

/// How long `Escape` is given to take effect before `/exit` is submitted.
pub const ESCAPE_SETTLE_MS: Millis = 300;

/// How many times `/exit` may be preceded by another `Escape` when the server
/// still reports the agent as blocked. Bounded so a permanently blocked agent
/// fails with a message instead of being poked forever.
const MAX_EXIT_ATTEMPTS: u8 = 3;

/// The one thing this protocol ever submits to an agent. A constant, never
/// anything a caller supplied.
const EXIT_COMMAND: &str = "/exit";

/// The rejection code a driver reports when a send never got an answer. A
/// server that *refused* a send wrote nothing to the pane; a request that was
/// lost in transit may or may not have, so the machine counts the pane as
/// touched from then on.
pub const TRANSPORT_ERROR_CODE: &str = "transport_error";

/// The knobs the user turned. `--yes` is deliberately absent: whether a
/// confirmation can be answered is the driver's business, so the machine
/// always asks and can never be built in a mode that does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchOptions {
    /// Allow moving an agent that is currently `working`, interrupting it.
    pub interrupt: bool,
    /// Proceed even when the agent already claims the target account, or the
    /// target profile has no credentials.
    pub force: bool,
    /// Budget for each waiting stage.
    pub timeout_ms: Millis,
}

impl Default for SwitchOptions {
    fn default() -> Self {
        Self {
            interrupt: false,
            force: false,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// The facts about a running agent the protocol reasons over.
///
/// A narrow copy of [`AgentInfo`] rather than the whole thing, so a test can
/// build the exact situation it wants to drive without inventing thirty
/// unrelated fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub pane_id: String,
    pub terminal_id: String,
    pub name: Option<String>,
    pub agent: Option<String>,
    pub status: AgentStatus,
    pub session: Option<AgentSessionInfo>,
    /// The account this agent currently claims (`tokens.account`).
    pub account: Option<String>,
}

impl AgentSnapshot {
    pub fn from_agent_info(info: &AgentInfo) -> Self {
        Self {
            pane_id: info.pane_id.clone(),
            terminal_id: info.terminal_id.clone(),
            name: info.name.clone(),
            agent: info.agent.clone(),
            status: info.agent_status,
            session: info.agent_session.clone(),
            account: info
                .tokens
                .get(crate::accounts::tokens::ACCOUNT_TOKEN)
                .cloned(),
        }
    }
}

/// What the pane looks like between the agent's exit and its relaunch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneReading {
    /// The pane's own shell holds the foreground *and* the server no longer
    /// holds an agent on that terminal. Both halves matter: the first says the
    /// Claude process is gone, the second that `agent.start` will accept the
    /// pane again. `terminal_id` is the terminal the pane holds *now*; a
    /// driver that cannot read it reports [`PaneReading::Unreadable`] instead,
    /// because a launch is never allowed on a pane whose identity is unknown.
    Released { terminal_id: String },
    /// Something is still running there.
    Busy { detail: String },
    /// The pane could not be read at all.
    Unreadable { detail: String },
}

/// What a driver saw after carrying out the last [`Action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// The agent as it is now; `None` when the target no longer resolves.
    Agent(Box<Option<AgentSnapshot>>),
    Pane(PaneReading),
    /// The user answered the confirmation.
    Confirmed(bool),
    /// There is nobody to ask (not a terminal, and no `--yes`).
    ConfirmUnavailable,
    /// The keys or the prompt reached the agent.
    Sent,
    /// The server refused to deliver them.
    SendRejected {
        code: String,
        detail: String,
    },
    /// The relaunch either put a ready agent in the pane, or did not.
    Launched(Box<Result<(), SwitchError>>),
    /// The relaunched process's environment was read and graded.
    Graded {
        state: AccountState,
        detail: Option<String>,
    },
    /// A requested [`Action::Wait`] elapsed.
    Tick,
}

impl Observation {
    /// Convenience for the common `Observation::Agent(...)` construction.
    pub fn agent(snapshot: Option<AgentSnapshot>) -> Self {
        Self::Agent(Box::new(snapshot))
    }

    /// Convenience for the common `Observation::Launched(...)` construction.
    pub fn launched(result: Result<(), SwitchError>) -> Self {
        Self::Launched(Box::new(result))
    }
}

/// What the driver must do next. Every side effect the switch has is one of
/// these, which is the whole reason the protocol can be tested without a
/// server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Put this question to the user and answer with `Confirmed`.
    AskConfirm(String),
    /// `agent.send_keys` on the pinned pane.
    SendKeys(Vec<String>),
    /// `agent.prompt` on the pinned pane.
    Prompt(String),
    /// `pane.send_text` on the pinned pane, with a trailing carriage return.
    ///
    /// The last resort for a `/exit` the server will not accept as a prompt.
    /// See [`SwitchMachine::on_exit`]: it is only ever reached for an agent
    /// the preflight saw a usage limit on, and the text is always the
    /// constant `/exit`.
    SubmitText(String),
    /// Re-read the agent on the pinned pane.
    PollAgent,
    /// Re-read the pinned pane's processes and agent ownership.
    PollPane,
    /// Sleep this long, then answer with `Tick`.
    Wait(Millis),
    /// Run the two-step launch.
    Launch(LaunchRequest),
    /// Grade the relaunched process and record the account tokens.
    Grade,
    /// Nothing more to do.
    Finish(Box<Result<SwitchResult, SwitchError>>),
}

/// Everything the relaunch needs. The driver never invents any of it: the pane
/// is the one pinned at preflight, and the arguments come from
/// [`crate::agent_resume::plan`], so herdr resumes Claude exactly the way it
/// does after a server restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub pane_id: String,
    pub name: String,
    pub account: String,
    /// `["--resume", "<session id>"]`.
    pub args: Vec<String>,
    pub session_id: String,
}

/// A switch that happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchResult {
    pub pane_id: String,
    pub name: String,
    /// The account the agent claimed before, when it claimed one.
    pub from: Option<String>,
    pub to: String,
    /// The same id before and after — that is the point of the whole protocol.
    pub session_id: String,
    pub account_state: AccountState,
    /// What the relaunched process's environment said, when it disagreed.
    pub mismatch_detail: Option<String>,
}

/// Why a switch did not happen, or did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchError {
    AgentNotFound {
        target: String,
    },
    NotClaude {
        target: String,
        agent: Option<String>,
    },
    /// `agent.start` needs a name to register, and inventing one could collide
    /// with an agent somewhere else in the session.
    Unnamed {
        pane_id: String,
    },
    /// The refusal that protects the conversation: with no session id there is
    /// nothing to resume, so exiting Claude would throw the conversation away.
    NoSession {
        target: String,
        agent_hint: bool,
    },
    ForeignSession {
        source: String,
        agent: String,
    },
    UnusableSession {
        value: String,
    },
    AlreadyOnAccount {
        account: String,
    },
    ProfileDirectoryMissing {
        name: String,
        dir: PathBuf,
    },
    ProfileLoggedOut {
        name: String,
    },
    AgentWorking {
        name: String,
    },
    /// Between the first look and the moment something was about to be sent
    /// (a human may take minutes over the confirmation) the agent on the
    /// pinned pane stopped being the one the user confirmed. Nothing was sent.
    AgentChanged {
        pane_id: String,
        detail: String,
    },
    /// The caller named the agent it meant (a TUI pinned a pane *and* the
    /// managed name it showed the user), and the pane now holds another one.
    /// Nothing was sent.
    NotTheAgent {
        pane_id: String,
        expected: String,
        actual: Option<String>,
    },
    /// Not a terminal, and no `--yes`.
    ConfirmationRequired,
    Declined,
    /// `agent.send_keys` was refused, so the agent could not be unblocked.
    KeysRefused {
        code: String,
        detail: String,
    },
    /// `agent.prompt "/exit"` was refused.
    ExitRefused {
        code: String,
        detail: String,
    },
    /// The exit budget ran out. Nothing further was sent; the agent is still
    /// running and still has its conversation.
    AgentStillRunning {
        name: String,
        pane_id: String,
        seconds: u64,
    },
    /// The pane is no longer the terminal the switch started on. Claude had
    /// already exited when this was noticed, so the message says how to get
    /// the conversation back.
    PaneReplaced {
        pane_id: String,
        session_id: String,
    },
    PaneUnreadable {
        pane_id: String,
        detail: String,
    },
    /// Claude came back but never reported a session id.
    SessionNotReported {
        pane_id: String,
        expected: String,
        seconds: u64,
    },
    /// Claude came back under a *different* conversation: the resume failed.
    SessionMismatch {
        expected: String,
        actual: String,
    },
    /// The relaunch did not put a ready agent back. Claude had exited, so the
    /// pane is at its shell (or holds a Claude herdr could not see become
    /// ready) and the conversation is on disk.
    Launch {
        pane_id: String,
        session_id: String,
        detail: String,
    },
    Api {
        method: String,
        detail: String,
    },
}

impl std::fmt::Display for SwitchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentNotFound { target } => {
                write!(formatter, "no agent named {target:?}")
            }
            Self::NotClaude { target, agent } => write!(
                formatter,
                "{target:?} is running {} , not claude; account profiles apply to Claude only",
                agent.as_deref().unwrap_or("no known agent")
            ),
            Self::Unnamed { pane_id } => write!(
                formatter,
                "the agent in pane {pane_id} has no name, and herdr needs one to start it again; \
                 give it one with `herdr agent rename {pane_id} <name>` and retry"
            ),
            Self::NoSession { target, agent_hint } => write!(
                formatter,
                "agent {target:?} has no Claude session id, so its conversation could not be \
                 resumed after a switch; nothing was sent to the pane.{}",
                if *agent_hint {
                    " Claude reports its session id through the herdr hook: install it for this \
                     profile with `herdr integration install claude` and start the agent again."
                } else {
                    ""
                }
            ),
            Self::ForeignSession { source, agent } => write!(
                formatter,
                "the session on this pane was reported by {source:?} for {agent:?}, not by \
                 Claude's own herdr hook; refusing to resume it"
            ),
            Self::UnusableSession { value } => write!(
                formatter,
                "the reported Claude session id ({value:?}) cannot be passed to `claude --resume`"
            ),
            Self::AlreadyOnAccount { account } => write!(
                formatter,
                "the agent already runs under account {account:?}; pass --force to switch anyway"
            ),
            Self::ProfileDirectoryMissing { name, dir } => write!(
                formatter,
                "account profile {name:?} points at {}, which does not exist; nothing was sent",
                dir.display()
            ),
            Self::ProfileLoggedOut { name } => write!(
                formatter,
                "account profile {name:?} has no credentials, so the resumed Claude would stop at \
                 a login screen; run `herdr account login {name}` first, or pass --force"
            ),
            Self::AgentWorking { name } => write!(
                formatter,
                "agent {name:?} is working; it may be in the middle of a tool call. Wait for it, \
                 or pass --interrupt to send Escape first"
            ),
            Self::AgentChanged { pane_id, detail } => write!(
                formatter,
                "the agent in pane {pane_id} changed while the switch was being confirmed: \
                 {detail}; nothing was sent to the pane. Look at it and retry"
            ),
            Self::NotTheAgent {
                pane_id,
                expected,
                actual,
            } => write!(
                formatter,
                "pane {pane_id} now holds {}, not agent {expected:?}; nothing was sent to the \
                 pane. Look at it and start the switch again",
                match actual {
                    Some(actual) => format!("agent {actual:?}"),
                    None => "an agent with no name".to_string(),
                }
            ),
            Self::ConfirmationRequired => write!(
                formatter,
                "switching an account stops and restarts the agent, so it needs a confirmation; \
                 pass --yes when there is no terminal to ask"
            ),
            Self::Declined => write!(formatter, "switch declined; nothing was sent to the pane"),
            Self::KeysRefused { code, detail } => {
                write!(
                    formatter,
                    "could not interrupt the agent ({code}): {detail}"
                )
            }
            Self::ExitRefused { code, detail } => {
                write!(
                    formatter,
                    "could not ask the agent to exit ({code}): {detail}"
                )
            }
            Self::AgentStillRunning {
                name,
                pane_id,
                seconds,
            } => write!(
                formatter,
                "agent {name:?} did not exit within {seconds}s of `/exit`; nothing else was sent \
                 and nothing was killed. It is still running in pane {pane_id} with its \
                 conversation intact — finish what it is doing and retry"
            ),
            Self::PaneReplaced {
                pane_id,
                session_id,
            } => write!(
                formatter,
                "pane {pane_id} is no longer the terminal this switch started on; stopping rather \
                 than typing into it. Claude had already exited: its conversation is still on \
                 disk, and `claude --resume {session_id}` in a pane recovers it"
            ),
            Self::PaneUnreadable { pane_id, detail } => write!(
                formatter,
                "could not read pane {pane_id} while waiting for its shell: {detail}"
            ),
            Self::SessionNotReported {
                pane_id,
                expected,
                seconds,
            } => write!(
                formatter,
                "Claude was restarted in pane {pane_id} but did not report session {expected:?} \
                 within {seconds}s. Read the pane: if it is asking you to log in, run \
                 `herdr account login`; the conversation is still on disk and \
                 `claude --resume {expected}` in that pane recovers it"
            ),
            Self::SessionMismatch { expected, actual } => write!(
                formatter,
                "the restarted Claude reported session {actual:?}, not {expected:?}: the resume \
                 did not take. The original conversation is still on disk; exit this one and run \
                 `claude --resume {expected}` in the pane"
            ),
            Self::Launch {
                pane_id,
                session_id,
                detail,
            } => write!(
                formatter,
                "could not restart the agent in pane {pane_id}: {detail}. Claude had exited; read \
                 the pane, and if it is at its shell, `claude --resume {session_id}` there \
                 recovers the conversation, which is still on disk"
            ),
            Self::Api { method, detail } => write!(formatter, "{method} failed: {detail}"),
        }
    }
}

/// A failure plus the facts that decide how loud it is and what to say next:
/// whether anything had already been sent to the pane when it happened, and
/// which pane and conversation that was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchFailure {
    pub error: SwitchError,
    /// `false` means the agent was never touched, so it is exactly as it was.
    pub touched_pane: bool,
    /// The pinned pane, once preflight has run.
    pub pane_id: Option<String>,
    /// The conversation the switch was preserving, once preflight has run.
    pub session_id: Option<String>,
    /// Things the driver did on the way out that the user must hear about —
    /// above all, that a relaunched agent was recorded under the new account
    /// even though the protocol did not finish.
    pub warnings: Vec<String>,
}

impl std::fmt::Display for SwitchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl SwitchFailure {
    /// What to do about the pane, for the errors whose own message cannot say
    /// it. Only a failure that touched the pane needs one: before that the
    /// agent is exactly as it was. Errors that already spell out the pane's
    /// state and the recovery command (a timeout, a lost or mismatched
    /// session, a failed relaunch, a replaced pane) get `None`.
    pub fn recovery_hint(&self) -> Option<String> {
        if !self.touched_pane {
            return None;
        }
        let (Some(pane_id), Some(session_id)) = (&self.pane_id, &self.session_id) else {
            return None;
        };
        match &self.error {
            SwitchError::KeysRefused { .. }
            | SwitchError::ExitRefused { .. }
            | SwitchError::PaneUnreadable { .. }
            | SwitchError::Api { .. } => Some(format!(
                "read pane {pane_id} with `herdr pane read {pane_id}`: if Claude is still running \
                 there it still has its conversation; if the pane is at its shell, \
                 `claude --resume {session_id}` there recovers it"
            )),
            SwitchError::AgentNotFound { .. }
            | SwitchError::NotClaude { .. }
            | SwitchError::Unnamed { .. }
            | SwitchError::NoSession { .. }
            | SwitchError::ForeignSession { .. }
            | SwitchError::UnusableSession { .. }
            | SwitchError::AlreadyOnAccount { .. }
            | SwitchError::ProfileDirectoryMissing { .. }
            | SwitchError::ProfileLoggedOut { .. }
            | SwitchError::AgentWorking { .. }
            | SwitchError::AgentChanged { .. }
            | SwitchError::NotTheAgent { .. }
            | SwitchError::ConfirmationRequired
            | SwitchError::Declined
            | SwitchError::AgentStillRunning { .. }
            | SwitchError::PaneReplaced { .. }
            | SwitchError::SessionNotReported { .. }
            | SwitchError::SessionMismatch { .. }
            | SwitchError::Launch { .. } => None,
        }
    }
}

/// What the machine is doing. Deadlines are absolute, on the driver's clock.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Preflight,
    Confirm,
    /// Confirmed; the agent is being read once more before anything is sent.
    Recheck,
    /// `Escape` has been sent; `attempts` counts the `/exit` tries so far.
    Escape {
        attempts: u8,
    },
    EscapeSettle {
        attempts: u8,
    },
    Exit {
        attempts: u8,
    },
    /// The pinned agent is being read once more, because the next action
    /// writes raw bytes into the pane with none of `agent.prompt`'s checks.
    RecheckExitText,
    /// `/exit` typed into the pane, after the prompt path was refused for a
    /// usage-limited agent.
    ExitText,
    AwaitShell {
        deadline: Millis,
    },
    Relaunch,
    AwaitSession {
        deadline: Millis,
    },
    Grade,
    Done,
}

/// Everything decided before the protocol starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchInput {
    /// What the user named on the command line, kept for messages.
    pub target: String,
    /// The managed name the agent is expected to have, when the caller knows
    /// it independently of `target`. A TUI addresses the pane the user
    /// right-clicked by id — a name could have moved to another pane — but
    /// what the user chose is *the agent that was in it*, so the first read
    /// must find that name there or refuse before anything is asked or sent.
    /// The CLI leaves this `None`: a name given on the command line is what
    /// the server resolves, and a pane id names whatever runs there.
    pub expected_name: Option<String>,
    pub to: AccountProfile,
    pub to_inspection: ProfileInspection,
    pub options: SwitchOptions,
    /// What herdr's detector said about the source agent's account usage, read
    /// once before the protocol starts. Text only: it never gates a phase and
    /// never changes what is sent, it only tells the human answering the
    /// confirmation *why* they are being asked.
    pub limit: Option<crate::accounts::limit::UsageLimit>,
}

/// The switch protocol.
#[derive(Debug, Clone)]
pub struct SwitchMachine {
    input: SwitchInput,
    phase: Phase,
    /// Pinned at preflight and never re-read from a later observation: every
    /// key, prompt and launch afterwards addresses *this* pane, so a pane that
    /// changed underneath is a refusal rather than a redirect.
    pane_id: String,
    terminal_id: String,
    name: String,
    session_id: String,
    resume_args: Vec<String>,
    from: Option<String>,
    status: AgentStatus,
    touched_pane: bool,
}

impl SwitchMachine {
    pub fn new(input: SwitchInput) -> Self {
        Self {
            input,
            phase: Phase::Preflight,
            pane_id: String::new(),
            terminal_id: String::new(),
            name: String::new(),
            session_id: String::new(),
            resume_args: Vec::new(),
            from: None,
            status: AgentStatus::Unknown,
            touched_pane: false,
        }
    }

    /// The first thing to do: look at the agent.
    pub fn start(&self) -> Action {
        Action::PollAgent
    }

    /// Whether anything has been sent to the pane yet.
    pub fn touched_pane(&self) -> bool {
        self.touched_pane
    }

    /// The pane the switch is pinned to, once preflight has run.
    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }

    /// What [`Action::PollAgent`] should ask about.
    ///
    /// Before preflight that is whatever the user typed — a name, a pane id.
    /// Afterwards it is always the pinned pane, so every later read is about
    /// the terminal the switch is acting on and not about a name that could
    /// have moved to another pane in the meantime.
    pub fn agent_target(&self) -> &str {
        if self.pane_id.is_empty() {
            &self.input.target
        } else {
            &self.pane_id
        }
    }

    /// Feed one observation, get the next action.
    pub fn next(&mut self, now: Millis, observation: Observation) -> Action {
        match std::mem::replace(&mut self.phase, Phase::Done) {
            Phase::Preflight => self.on_preflight(now, observation),
            Phase::Confirm => self.on_confirm(observation),
            Phase::Recheck => self.on_recheck(observation),
            Phase::Escape { attempts } => self.on_escape(attempts, observation),
            Phase::EscapeSettle { attempts } => self.on_escape_settle(attempts, observation),
            Phase::Exit { attempts } => self.on_exit(now, attempts, observation),
            Phase::RecheckExitText => self.on_recheck_exit_text(observation),
            Phase::ExitText => self.on_exit_text(now, observation),
            Phase::AwaitShell { deadline } => self.on_await_shell(now, deadline, observation),
            Phase::Relaunch => self.on_relaunch(now, observation),
            Phase::AwaitSession { deadline } => self.on_await_session(now, deadline, observation),
            Phase::Grade => self.on_grade(observation),
            Phase::Done => self.fail(SwitchError::Api {
                method: "switch".into(),
                detail: "the switch has already finished".into(),
            }),
        }
    }

    /// Wrap an error with what the driver's caller needs to report it: whether
    /// the pane was touched, and which pane and conversation this was about.
    pub fn failure(&self, error: SwitchError) -> SwitchFailure {
        SwitchFailure {
            error,
            touched_pane: self.touched_pane(),
            pane_id: (!self.pane_id.is_empty()).then(|| self.pane_id.clone()),
            session_id: (!self.session_id.is_empty()).then(|| self.session_id.clone()),
            warnings: Vec::new(),
        }
    }

    fn fail(&mut self, error: SwitchError) -> Action {
        self.phase = Phase::Done;
        Action::Finish(Box::new(Err(error)))
    }

    /// An observation that does not belong to the current phase. A driver bug,
    /// not a user one — but it must still stop the protocol rather than fall
    /// through into typing something.
    fn out_of_order(&mut self, phase: &str) -> Action {
        self.fail(SwitchError::Api {
            method: "switch".into(),
            detail: format!("unexpected observation during {phase}"),
        })
    }

    fn on_preflight(&mut self, _now: Millis, observation: Observation) -> Action {
        let Observation::Agent(agent) = observation else {
            return self.out_of_order("preflight");
        };
        let Some(agent) = *agent else {
            return self.fail(SwitchError::AgentNotFound {
                target: self.input.target.clone(),
            });
        };

        // Before anything else: is this the agent the caller meant? A pane
        // id says where to look, not what was there when the user chose.
        if let Some(expected) = self.input.expected_name.as_deref() {
            if agent.name.as_deref() != Some(expected) {
                return self.fail(SwitchError::NotTheAgent {
                    pane_id: agent.pane_id.clone(),
                    expected: expected.to_string(),
                    actual: agent.name.clone(),
                });
            }
        }

        if agent.agent.as_deref() != Some(AGENT_LABEL) {
            return self.fail(SwitchError::NotClaude {
                target: self.input.target.clone(),
                agent: agent.agent.clone(),
            });
        }
        let Some(name) = agent.name.clone() else {
            return self.fail(SwitchError::Unnamed {
                pane_id: agent.pane_id.clone(),
            });
        };

        // The refusal that has to come before everything else: without a
        // session id, `/exit` would end a conversation nothing can bring back.
        let Some(session) = agent.session.clone() else {
            return self.fail(SwitchError::NoSession {
                target: self.input.target.clone(),
                agent_hint: true,
            });
        };
        if session.source != APPLIES_TO_SOURCE || session.agent != AGENT_LABEL {
            return self.fail(SwitchError::ForeignSession {
                source: session.source,
                agent: session.agent,
            });
        }
        if session.kind != AgentSessionRefKind::Id {
            return self.fail(SwitchError::UnusableSession {
                value: session.value,
            });
        }
        // An id that would be read as an option turns `--resume <id>` into a
        // different command; `crate::agent_resume` refuses the same shape for
        // Codex resumes.
        if session.value.starts_with('-') {
            return self.fail(SwitchError::UnusableSession {
                value: session.value,
            });
        }
        let Some(session_ref) = AgentSessionRef::id(session.value.clone()) else {
            return self.fail(SwitchError::UnusableSession {
                value: session.value,
            });
        };
        // Upstream owns how Claude is resumed; mirroring it here would be one
        // more copy to drift.
        let Some(plan) = crate::agent_resume::plan(&session.source, &session.agent, &session_ref)
        else {
            return self.fail(SwitchError::UnusableSession {
                value: session.value,
            });
        };
        let Some((_executable, resume_args)) = plan.argv.split_first() else {
            return self.fail(SwitchError::UnusableSession {
                value: session.value,
            });
        };

        if !self.input.to_inspection.dir_exists {
            return self.fail(SwitchError::ProfileDirectoryMissing {
                name: self.input.to.name.clone(),
                dir: self.input.to.config_dir.clone(),
            });
        }
        if !self.input.to_inspection.logged_in && !self.input.options.force {
            return self.fail(SwitchError::ProfileLoggedOut {
                name: self.input.to.name.clone(),
            });
        }
        if agent.account.as_deref() == Some(self.input.to.name.as_str())
            && !self.input.options.force
        {
            return self.fail(SwitchError::AlreadyOnAccount {
                account: self.input.to.name.clone(),
            });
        }
        if agent.status == AgentStatus::Working && !self.input.options.interrupt {
            return self.fail(SwitchError::AgentWorking { name });
        }

        self.pane_id = agent.pane_id.clone();
        self.terminal_id = agent.terminal_id.clone();
        self.name = name;
        self.session_id = session.value;
        self.resume_args = resume_args.to_vec();
        self.from = agent.account.clone();
        self.status = agent.status;

        self.phase = Phase::Confirm;
        Action::AskConfirm(self.confirmation())
    }

    /// The question the user answers. It names the pane, the agent, both
    /// accounts and the session id, because that is the whole of what is about
    /// to change and what will be preserved.
    fn confirmation(&self) -> String {
        let from = self.from.as_deref().unwrap_or("no recorded account");
        let mut text = format!(
            "Switch agent {:?} in pane {} from {from} to {:?}?\n  \
             Claude will be asked to exit (Escape if needed, then /exit) and started again with \
             `--resume {}` under the new profile. It is never killed, and the conversation is \
             kept.",
            self.name, self.pane_id, self.input.to.name, self.session_id,
        );
        if self.status == AgentStatus::Working {
            text.push_str(
                "\n  WARNING: this agent is working. Escape will interrupt whatever it is doing.",
            );
        }
        if !self.input.to_inspection.logged_in {
            text.push_str(
                "\n  WARNING: the target profile has no credentials; Claude will stop at a login \
                 screen.",
            );
        }
        // Why this switch is being asked for, when herdr can see it. Stated
        // after the warnings so the last thing read is still a warning when
        // there is one.
        if let Some(limit) = self.input.limit.as_ref() {
            let reset = match limit.reset_text.as_deref() {
                Some(reset) => format!(", which resets {reset}"),
                None => String::new(),
            };
            text.push_str(&format!("\n  herdr sees a usage limit on {from}{reset}."));
        }
        text
    }

    fn on_confirm(&mut self, observation: Observation) -> Action {
        match observation {
            // A person may have taken minutes over the question, and the
            // server accepts a prompt for a working agent. So the agent is
            // read once more, on the pinned pane, before a byte is sent.
            Observation::Confirmed(true) => {
                self.phase = Phase::Recheck;
                Action::PollAgent
            }
            Observation::Confirmed(false) => self.fail(SwitchError::Declined),
            Observation::ConfirmUnavailable => self.fail(SwitchError::ConfirmationRequired),
            _ => self.out_of_order("confirmation"),
        }
    }

    /// What "still the agent that was confirmed" means: same terminal, same
    /// name, same conversation. Shared, because it is checked once before the
    /// protocol sends anything and once more before the typed `/exit`, and the
    /// two must not be allowed to drift apart.
    fn pinned_identity_error(&self, agent: Option<&AgentSnapshot>) -> Option<SwitchError> {
        let changed = |detail: String| {
            Some(SwitchError::AgentChanged {
                pane_id: self.pane_id.clone(),
                detail,
            })
        };
        let Some(agent) = agent else {
            return changed("there is no agent on that pane any more".into());
        };
        if agent.terminal_id != self.terminal_id {
            return changed("the pane is now another terminal".into());
        }
        if agent.name.as_deref() != Some(self.name.as_str()) {
            return changed(format!(
                "it is now named {}, not {:?}",
                agent
                    .name
                    .as_ref()
                    .map(|name| format!("{name:?}"))
                    .unwrap_or_else(|| "nothing".to_string()),
                self.name
            ));
        }
        match agent.session.as_ref() {
            Some(session) if session.value == self.session_id => None,
            Some(session) => changed(format!(
                "its session id is now {:?}, not {:?}",
                session.value, self.session_id
            )),
            None => changed("it no longer reports a session id".into()),
        }
    }

    /// The agent must still be the one that was confirmed: same terminal, same
    /// name, same conversation, and not working unless that was agreed to.
    /// Its status is taken from this read, not the first one, because that is
    /// what decides whether `/exit` needs an `Escape` in front of it.
    fn on_recheck(&mut self, observation: Observation) -> Action {
        let Observation::Agent(agent) = observation else {
            return self.out_of_order("recheck");
        };
        let agent = *agent;
        if let Some(error) = self.pinned_identity_error(agent.as_ref()) {
            return self.fail(error);
        }
        let Some(agent) = agent else {
            // Unreachable: a missing agent is the first thing the check above
            // refuses. Spelled out rather than unwrapped.
            return self.fail(SwitchError::AgentChanged {
                pane_id: self.pane_id.clone(),
                detail: "there is no agent on that pane any more".into(),
            });
        };
        if agent.status == AgentStatus::Working && !self.input.options.interrupt {
            return self.fail(SwitchError::AgentWorking {
                name: self.name.clone(),
            });
        }
        self.status = agent.status;
        self.begin_exit()
    }

    /// Escape first when the agent cannot read a prompt (blocked), or when a
    /// working agent is being interrupted on purpose. Otherwise `/exit` goes
    /// straight in: an idle Claude needs no interruption, and sending Escape
    /// to one is input it did not ask for.
    ///
    /// The pane counts as touched once a send is *delivered* (or lost in
    /// transit), not when it is decided: a server that refuses the send wrote
    /// nothing, and the agent is exactly as it was.
    fn begin_exit(&mut self) -> Action {
        match self.status {
            AgentStatus::Blocked => {
                self.phase = Phase::Escape { attempts: 0 };
                Action::SendKeys(vec!["esc".to_string()])
            }
            AgentStatus::Working if self.input.options.interrupt => {
                self.phase = Phase::Escape { attempts: 0 };
                Action::SendKeys(vec!["esc".to_string()])
            }
            _ => {
                self.phase = Phase::Exit { attempts: 0 };
                Action::Prompt(EXIT_COMMAND.to_string())
            }
        }
    }

    fn on_escape(&mut self, attempts: u8, observation: Observation) -> Action {
        match observation {
            Observation::Sent => {
                self.touched_pane = true;
                self.phase = Phase::EscapeSettle { attempts };
                Action::Wait(ESCAPE_SETTLE_MS)
            }
            Observation::SendRejected { code, detail } => {
                self.touched_pane |= code == TRANSPORT_ERROR_CODE;
                self.fail(SwitchError::KeysRefused { code, detail })
            }
            _ => self.out_of_order("interrupt"),
        }
    }

    fn on_escape_settle(&mut self, attempts: u8, observation: Observation) -> Action {
        match observation {
            Observation::Tick => {
                self.phase = Phase::Exit { attempts };
                Action::Prompt(EXIT_COMMAND.to_string())
            }
            _ => self.out_of_order("interrupt"),
        }
    }

    fn on_exit(&mut self, now: Millis, attempts: u8, observation: Observation) -> Action {
        match observation {
            Observation::Sent => {
                self.touched_pane = true;
                self.phase = Phase::AwaitShell {
                    deadline: now.saturating_add(self.input.options.timeout_ms),
                };
                Action::PollPane
            }
            // The server refuses a prompt while the agent is blocked. That can
            // race with the Escape that was meant to unblock it, so a bounded
            // number of retries goes back through Escape rather than failing on
            // a timing artefact.
            //
            // A usage limit is the one blocked state Escape cannot clear: the
            // notice stays on screen until the account's window rolls over, so
            // `agent.prompt` would be refused for as long as the limit lasts —
            // and switching away from it is the entire point of the command.
            // For that case, and only that case, `/exit` is typed into the
            // pane instead. The agent is still at a prompt that accepts
            // typing; what it cannot do is work.
            Observation::SendRejected { code, detail } => {
                self.touched_pane |= code == TRANSPORT_ERROR_CODE;
                if code == "agent_blocked" && self.input.limit.is_some() {
                    self.phase = Phase::RecheckExitText;
                    return Action::PollAgent;
                }
                if code == "agent_blocked" && attempts + 1 < MAX_EXIT_ATTEMPTS {
                    self.phase = Phase::Escape {
                        attempts: attempts + 1,
                    };
                    Action::SendKeys(vec!["esc".to_string()])
                } else {
                    self.fail(SwitchError::ExitRefused { code, detail })
                }
            }
            _ => self.out_of_order("exit"),
        }
    }

    /// One more read before raw bytes reach the pty.
    ///
    /// `pane.send_text` writes to a *pane id* with none of `agent.prompt`'s
    /// checks: it does not care which terminal now holds the pane, which agent
    /// is in it, or what conversation that agent is running. The refusal that
    /// led here proved only that some blocked agent answered on this pane a
    /// round trip ago, so the pinned identity is re-established immediately
    /// before `/exit` is typed. A pane that changed hands in the meantime
    /// fails the switch instead of being typed into.
    fn on_recheck_exit_text(&mut self, observation: Observation) -> Action {
        let Observation::Agent(agent) = observation else {
            return self.out_of_order("exit");
        };
        if let Some(error) = self.pinned_identity_error((*agent).as_ref()) {
            return self.fail(error);
        }
        self.phase = Phase::ExitText;
        Action::SubmitText(EXIT_COMMAND.to_string())
    }

    /// The typed `/exit`. One attempt: it went into the pty or it did not, and
    /// a second copy of a command that may already have landed could be typed
    /// at whatever the pane holds next.
    fn on_exit_text(&mut self, now: Millis, observation: Observation) -> Action {
        match observation {
            Observation::Sent => {
                self.touched_pane = true;
                self.phase = Phase::AwaitShell {
                    deadline: now.saturating_add(self.input.options.timeout_ms),
                };
                Action::PollPane
            }
            Observation::SendRejected { code, detail } => {
                self.touched_pane |= code == TRANSPORT_ERROR_CODE;
                self.fail(SwitchError::ExitRefused { code, detail })
            }
            _ => self.out_of_order("exit"),
        }
    }

    fn on_await_shell(
        &mut self,
        now: Millis,
        deadline: Millis,
        observation: Observation,
    ) -> Action {
        let expired = now >= deadline;
        match observation {
            Observation::Pane(PaneReading::Released { terminal_id }) => {
                if terminal_id != self.terminal_id {
                    return self.fail(SwitchError::PaneReplaced {
                        pane_id: self.pane_id.clone(),
                        session_id: self.session_id.clone(),
                    });
                }
                self.phase = Phase::Relaunch;
                Action::Launch(LaunchRequest {
                    pane_id: self.pane_id.clone(),
                    name: self.name.clone(),
                    account: self.input.to.name.clone(),
                    args: self.resume_args.clone(),
                    session_id: self.session_id.clone(),
                })
            }
            Observation::Pane(PaneReading::Busy { .. }) if expired => {
                self.fail(SwitchError::AgentStillRunning {
                    name: self.name.clone(),
                    pane_id: self.pane_id.clone(),
                    seconds: seconds(self.input.options.timeout_ms),
                })
            }
            Observation::Pane(PaneReading::Unreadable { detail }) if expired => {
                self.fail(SwitchError::PaneUnreadable {
                    pane_id: self.pane_id.clone(),
                    detail,
                })
            }
            Observation::Pane(_) => {
                self.phase = Phase::AwaitShell { deadline };
                Action::Wait(POLL_INTERVAL_MS)
            }
            Observation::Tick => {
                self.phase = Phase::AwaitShell { deadline };
                Action::PollPane
            }
            _ => self.out_of_order("waiting for the shell"),
        }
    }

    fn on_relaunch(&mut self, now: Millis, observation: Observation) -> Action {
        let Observation::Launched(result) = observation else {
            return self.out_of_order("relaunch");
        };
        match *result {
            Ok(()) => {
                self.phase = Phase::AwaitSession {
                    deadline: now.saturating_add(self.input.options.timeout_ms),
                };
                Action::PollAgent
            }
            Err(error) => self.fail(error),
        }
    }

    fn on_await_session(
        &mut self,
        now: Millis,
        deadline: Millis,
        observation: Observation,
    ) -> Action {
        let expired = now >= deadline;
        match observation {
            Observation::Agent(agent) => {
                // The server clears the persisted session when the Claude
                // process exits (`TerminalState::set_detected_state_with_
                // screen_signals_at`, on `process_exited`, in the same mutation
                // that releases the agent name — which `AwaitShell` waited
                // for), and a Claude launch with `--resume` seeds nothing, so
                // any session visible now was reported by the process this
                // switch started. Only Claude's own hook counts as that
                // report: another integration's session on the pane is not
                // evidence either way.
                if let Some(agent) = agent.as_ref() {
                    if agent.terminal_id != self.terminal_id {
                        return self.fail(SwitchError::PaneReplaced {
                            pane_id: self.pane_id.clone(),
                            session_id: self.session_id.clone(),
                        });
                    }
                    if let Some(session) = agent.session.as_ref().filter(|session| {
                        session.source == APPLIES_TO_SOURCE && session.agent == AGENT_LABEL
                    }) {
                        if session.value == self.session_id {
                            self.phase = Phase::Grade;
                            return Action::Grade;
                        }
                        return self.fail(SwitchError::SessionMismatch {
                            expected: self.session_id.clone(),
                            actual: session.value.clone(),
                        });
                    }
                }
                if expired {
                    return self.fail(SwitchError::SessionNotReported {
                        pane_id: self.pane_id.clone(),
                        expected: self.session_id.clone(),
                        seconds: seconds(self.input.options.timeout_ms),
                    });
                }
                self.phase = Phase::AwaitSession { deadline };
                Action::Wait(POLL_INTERVAL_MS)
            }
            Observation::Tick => {
                self.phase = Phase::AwaitSession { deadline };
                Action::PollAgent
            }
            _ => self.out_of_order("waiting for the resumed session"),
        }
    }

    fn on_grade(&mut self, observation: Observation) -> Action {
        let Observation::Graded { state, detail } = observation else {
            return self.out_of_order("verification");
        };
        self.phase = Phase::Done;
        Action::Finish(Box::new(Ok(SwitchResult {
            pane_id: self.pane_id.clone(),
            name: self.name.clone(),
            from: self.from.clone(),
            to: self.input.to.name.clone(),
            session_id: self.session_id.clone(),
            account_state: state,
            mismatch_detail: detail,
        })))
    }
}

fn seconds(millis: Millis) -> u64 {
    millis.div_ceil(1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::layout::ProfileInspection;
    use crate::accounts::profile::{AccountAgent, ProfileOrigin};

    fn profile(name: &str) -> AccountProfile {
        AccountProfile {
            name: name.to_string(),
            agent: AccountAgent::Claude,
            config_dir: PathBuf::from(format!("/p/{name}")),
            default: false,
            origin: ProfileOrigin::Config,
        }
    }

    fn healthy() -> ProfileInspection {
        ProfileInspection {
            dir_exists: true,
            logged_in: true,
            hook_installed: true,
            ..ProfileInspection::default()
        }
    }

    fn session(value: &str) -> AgentSessionInfo {
        AgentSessionInfo {
            source: APPLIES_TO_SOURCE.to_string(),
            agent: AGENT_LABEL.to_string(),
            kind: AgentSessionRefKind::Id,
            value: value.to_string(),
        }
    }

    fn agent() -> AgentSnapshot {
        AgentSnapshot {
            pane_id: "1:2".to_string(),
            terminal_id: "t-1".to_string(),
            name: Some("a1".to_string()),
            agent: Some(AGENT_LABEL.to_string()),
            status: AgentStatus::Idle,
            session: Some(session("sess-1")),
            account: Some("perso".to_string()),
        }
    }

    fn switching(options: SwitchOptions) -> SwitchMachine {
        SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            expected_name: None,
            to: profile("work"),
            to_inspection: healthy(),
            options,
            limit: None,
        })
    }

    /// The reason for the switch, in the question the human answers. A TUI
    /// modal (PR 8) renders the same string, which is why it lives in the
    /// machine rather than in the CLI.
    #[test]
    fn a_usage_limit_is_named_in_the_confirmation() {
        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            to: profile("work"),
            to_inspection: healthy(),
            options: SwitchOptions::default(),
            limit: Some(crate::accounts::limit::UsageLimit {
                reset_text: Some("3pm".to_string()),
            }),
        });
        let action = machine.next(0, Observation::agent(Some(agent())));
        let Action::AskConfirm(text) = &action else {
            panic!("expected a confirmation, got {action:?}");
        };
        assert!(text.contains("usage limit on perso"), "{text}");
        assert!(text.contains("resets 3pm"), "{text}");

        // …and nothing is invented when the detector saw nothing.
        let mut quiet = switching(SwitchOptions::default());
        let action = quiet.next(0, Observation::agent(Some(agent())));
        let Action::AskConfirm(text) = &action else {
            panic!("expected a confirmation, got {action:?}");
        };
        assert!(!text.contains("usage limit"), "{text}");
    }

    fn finished(action: &Action) -> &Result<SwitchResult, SwitchError> {
        match action {
            Action::Finish(result) => result,
            other => panic!("expected the machine to finish, got {other:?}"),
        }
    }

    fn error_of(action: &Action) -> &SwitchError {
        match finished(action) {
            Err(error) => error,
            Ok(result) => panic!("expected a failure, got {result:?}"),
        }
    }

    /// Answer the confirmation, then pass the recheck with `snapshot` as what
    /// the pinned pane shows now.
    fn confirm_with(machine: &mut SwitchMachine, snapshot: AgentSnapshot) -> Action {
        assert_eq!(
            machine.next(0, Observation::Confirmed(true)),
            Action::PollAgent,
            "a confirmation is followed by a second look at the agent"
        );
        machine.next(0, Observation::agent(Some(snapshot)))
    }

    /// Preflight, confirm, recheck and `/exit` an idle agent, leaving the
    /// machine waiting for the pane's shell.
    fn exited(machine: &mut SwitchMachine) {
        assert!(matches!(
            machine.next(0, Observation::agent(Some(agent()))),
            Action::AskConfirm(_)
        ));
        assert_eq!(
            confirm_with(machine, agent()),
            Action::Prompt("/exit".to_string())
        );
        assert_eq!(machine.next(0, Observation::Sent), Action::PollPane);
    }

    fn released() -> Observation {
        Observation::Pane(PaneReading::Released {
            terminal_id: "t-1".to_string(),
        })
    }

    /// The whole happy path, one observation at a time, with the exact actions
    /// a driver would carry out.
    #[test]
    fn an_idle_agent_walks_exit_shell_relaunch_session_grade() {
        let mut machine = switching(SwitchOptions::default());
        assert_eq!(machine.start(), Action::PollAgent);
        assert!(!machine.touched_pane());

        let action = machine.next(0, Observation::agent(Some(agent())));
        let Action::AskConfirm(text) = &action else {
            panic!("expected a confirmation, got {action:?}");
        };
        assert!(text.contains("a1"), "{text}");
        assert!(text.contains("perso"), "{text}");
        assert!(text.contains("work"), "{text}");
        assert!(text.contains("sess-1"), "{text}");
        assert!(text.contains("never killed"), "{text}");
        assert!(
            !machine.touched_pane(),
            "asking is not touching the agent's pane"
        );

        // Confirmed: one more look, then — idle — no Escape, straight to /exit.
        assert_eq!(
            machine.next(1, Observation::Confirmed(true)),
            Action::PollAgent
        );
        assert!(!machine.touched_pane(), "looking again is not touching");
        assert_eq!(
            machine.next(1, Observation::agent(Some(agent()))),
            Action::Prompt("/exit".to_string())
        );
        assert!(
            !machine.touched_pane(),
            "deciding to send is not sending; the server may still refuse"
        );

        assert_eq!(machine.next(2, Observation::Sent), Action::PollPane);
        assert!(machine.touched_pane());
        assert_eq!(
            machine.next(
                3,
                Observation::Pane(PaneReading::Busy {
                    detail: "claude".into()
                })
            ),
            Action::Wait(POLL_INTERVAL_MS)
        );
        assert_eq!(machine.next(4, Observation::Tick), Action::PollPane);

        let action = machine.next(5, released());
        assert_eq!(
            action,
            Action::Launch(LaunchRequest {
                pane_id: "1:2".to_string(),
                name: "a1".to_string(),
                account: "work".to_string(),
                args: vec!["--resume".to_string(), "sess-1".to_string()],
                session_id: "sess-1".to_string(),
            })
        );

        assert_eq!(
            machine.next(6, Observation::launched(Ok(()))),
            Action::PollAgent
        );
        // The hook has not reported yet.
        let mut waiting = agent();
        waiting.session = None;
        assert_eq!(
            machine.next(7, Observation::agent(Some(waiting))),
            Action::Wait(POLL_INTERVAL_MS)
        );
        assert_eq!(machine.next(8, Observation::Tick), Action::PollAgent);

        let mut resumed = agent();
        resumed.account = Some("work".to_string());
        assert_eq!(
            machine.next(9, Observation::agent(Some(resumed))),
            Action::Grade
        );

        let action = machine.next(
            10,
            Observation::Graded {
                state: AccountState::Ok,
                detail: None,
            },
        );
        let result = finished(&action).as_ref().expect("a successful switch");
        assert_eq!(
            result,
            &SwitchResult {
                pane_id: "1:2".to_string(),
                name: "a1".to_string(),
                from: Some("perso".to_string()),
                to: "work".to_string(),
                session_id: "sess-1".to_string(),
                account_state: AccountState::Ok,
                mismatch_detail: None,
            }
        );
    }

    /// The refusal the whole command is built around.
    #[test]
    fn an_agent_without_a_session_id_is_refused_before_anything_is_sent() {
        let mut machine = switching(SwitchOptions::default());
        let mut sessionless = agent();
        sessionless.session = None;
        let action = machine.next(0, Observation::agent(Some(sessionless)));
        assert!(matches!(error_of(&action), SwitchError::NoSession { .. }));
        assert!(
            !machine.touched_pane(),
            "a sessionless agent must never be touched"
        );
        let message = error_of(&action).to_string();
        assert!(message.contains("nothing was sent"), "{message}");
    }

    #[test]
    fn a_session_reported_by_another_integration_is_not_resumed() {
        let mut machine = switching(SwitchOptions::default());
        let mut foreign = agent();
        foreign.session = Some(AgentSessionInfo {
            source: "herdr:codex".to_string(),
            agent: "codex".to_string(),
            kind: AgentSessionRefKind::Id,
            value: "sess-1".to_string(),
        });
        let action = machine.next(0, Observation::agent(Some(foreign)));
        assert!(matches!(
            error_of(&action),
            SwitchError::ForeignSession { .. }
        ));
        assert!(!machine.touched_pane());
    }

    #[test]
    fn a_session_id_that_would_be_read_as_an_option_is_refused() {
        for value in ["--dangerous", "-r"] {
            let mut machine = switching(SwitchOptions::default());
            let mut odd = agent();
            odd.session = Some(session(value));
            let action = machine.next(0, Observation::agent(Some(odd)));
            assert!(
                matches!(error_of(&action), SwitchError::UnusableSession { .. }),
                "{value}: {action:?}"
            );
            assert!(!machine.touched_pane());
        }
    }

    #[test]
    fn a_pane_running_another_agent_is_refused() {
        let mut machine = switching(SwitchOptions::default());
        let mut codex = agent();
        codex.agent = Some("codex".to_string());
        let action = machine.next(0, Observation::agent(Some(codex)));
        assert!(matches!(error_of(&action), SwitchError::NotClaude { .. }));
        assert!(!machine.touched_pane());
    }

    #[test]
    fn an_unnamed_agent_is_refused_because_it_could_not_be_started_again() {
        let mut machine = switching(SwitchOptions::default());
        let mut unnamed = agent();
        unnamed.name = None;
        let action = machine.next(0, Observation::agent(Some(unnamed)));
        assert!(matches!(error_of(&action), SwitchError::Unnamed { .. }));
        assert!(!machine.touched_pane());
    }

    /// A caller that pins a pane *and* the name it showed the user gets the
    /// agent it meant or a refusal — never whatever happens to run there now.
    #[test]
    fn an_expected_name_that_is_not_on_the_pane_is_refused_before_asking() {
        let expecting = |name: &str| {
            SwitchMachine::new(SwitchInput {
                target: "1:2".to_string(),
                expected_name: Some(name.to_string()),
                to: profile("work"),
                to_inspection: healthy(),
                options: SwitchOptions::default(),
            })
        };

        // The agent that was there is still there: the protocol proceeds.
        let mut machine = expecting("a1");
        assert!(matches!(
            machine.next(0, Observation::agent(Some(agent()))),
            Action::AskConfirm(_)
        ));

        // Another named agent took the pane, or one nobody named did.
        let mut renamed = agent();
        renamed.name = Some("b2".to_string());
        let mut unnamed = agent();
        unnamed.name = None;
        for (now_there, expected_detail) in [(renamed, "agent \"b2\""), (unnamed, "no name")] {
            let mut machine = expecting("a1");
            let action = machine.next(0, Observation::agent(Some(now_there)));
            let SwitchError::NotTheAgent {
                pane_id, expected, ..
            } = error_of(&action)
            else {
                panic!("expected NotTheAgent, got {action:?}");
            };
            assert_eq!(pane_id, "1:2");
            assert_eq!(expected, "a1");
            assert!(!machine.touched_pane());
            let message = error_of(&action).to_string();
            assert!(message.contains(expected_detail), "{message}");
            assert!(message.contains("nothing was sent"), "{message}");
            // No question was asked, and nothing can be squeezed out of it.
            assert!(matches!(
                machine.next(1, Observation::Confirmed(true)),
                Action::Finish(_)
            ));
        }

        // The name is checked before the kind, so a pane that now runs
        // another agent says which agent, not merely "not claude".
        let mut codex = agent();
        codex.name = Some("c1".to_string());
        codex.agent = Some("codex".to_string());
        let mut machine = expecting("a1");
        let action = machine.next(0, Observation::agent(Some(codex)));
        assert!(matches!(error_of(&action), SwitchError::NotTheAgent { .. }));
    }

    /// Before preflight the target is whatever the user typed; afterwards every
    /// read, key and prompt is addressed to the pinned pane.
    #[test]
    fn the_poll_target_becomes_the_pinned_pane_after_preflight() {
        let mut machine = switching(SwitchOptions::default());
        assert_eq!(machine.agent_target(), "a1");
        assert_eq!(machine.pane_id(), "");
        machine.next(0, Observation::agent(Some(agent())));
        assert_eq!(machine.agent_target(), "1:2");
        assert_eq!(machine.pane_id(), "1:2");
    }

    #[test]
    fn a_missing_agent_is_refused() {
        let mut machine = switching(SwitchOptions::default());
        let action = machine.next(0, Observation::agent(None));
        assert!(matches!(
            error_of(&action),
            SwitchError::AgentNotFound { .. }
        ));
    }

    #[test]
    fn a_working_agent_needs_interrupt_and_then_gets_escape_first() {
        let mut working = agent();
        working.status = AgentStatus::Working;

        let mut machine = switching(SwitchOptions::default());
        let action = machine.next(0, Observation::agent(Some(working.clone())));
        assert!(matches!(
            error_of(&action),
            SwitchError::AgentWorking { .. }
        ));
        assert!(
            !machine.touched_pane(),
            "an agent mid-tool-call must not be interrupted by default"
        );

        let mut machine = switching(SwitchOptions {
            interrupt: true,
            ..SwitchOptions::default()
        });
        let action = machine.next(0, Observation::agent(Some(working.clone())));
        let Action::AskConfirm(text) = &action else {
            panic!("expected a confirmation, got {action:?}");
        };
        assert!(
            text.contains("WARNING: this agent is working"),
            "an interrupt must be spelled out in the confirmation: {text}"
        );
        assert_eq!(
            confirm_with(&mut machine, working),
            Action::SendKeys(vec!["esc".to_string()])
        );
        assert!(!machine.touched_pane());
        assert_eq!(
            machine.next(2, Observation::Sent),
            Action::Wait(ESCAPE_SETTLE_MS)
        );
        assert!(machine.touched_pane(), "the Escape was delivered");
        assert_eq!(
            machine.next(3, Observation::Tick),
            Action::Prompt("/exit".to_string())
        );
    }

    #[test]
    fn a_blocked_agent_is_escaped_before_the_exit_prompt() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        let mut machine = switching(SwitchOptions::default());
        assert!(matches!(
            machine.next(0, Observation::agent(Some(blocked.clone()))),
            Action::AskConfirm(_)
        ));
        assert_eq!(
            confirm_with(&mut machine, blocked),
            Action::SendKeys(vec!["esc".to_string()])
        );
    }

    /// The status that decides between Escape and a bare `/exit` is the one
    /// seen *after* the confirmation, and anything that is not the confirmed
    /// agent in an agreed state is refused with nothing sent.
    #[test]
    fn the_agent_is_read_again_after_the_confirmation() {
        // Idle when asked, working by the time the answer came: refused.
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(agent())));
        let mut working = agent();
        working.status = AgentStatus::Working;
        let action = confirm_with(&mut machine, working.clone());
        assert!(matches!(
            error_of(&action),
            SwitchError::AgentWorking { .. }
        ));
        assert!(!machine.touched_pane());

        // …unless interrupting was agreed to, in which case Escape leads.
        let mut machine = switching(SwitchOptions {
            interrupt: true,
            ..SwitchOptions::default()
        });
        machine.next(0, Observation::agent(Some(agent())));
        assert_eq!(
            confirm_with(&mut machine, working),
            Action::SendKeys(vec!["esc".to_string()])
        );

        // Idle when asked, blocked now: Escape goes first.
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(agent())));
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        assert_eq!(
            confirm_with(&mut machine, blocked),
            Action::SendKeys(vec!["esc".to_string()])
        );

        // A different conversation, a renamed agent, another terminal, or no
        // agent at all: each is a refusal that names what changed.
        let mut other_session = agent();
        other_session.session = Some(session("sess-2"));
        let mut renamed = agent();
        renamed.name = Some("b2".to_string());
        let mut moved = agent();
        moved.terminal_id = "t-9".to_string();
        let mut silent = agent();
        silent.session = None;
        for (changed, expected) in [
            (Some(other_session), "sess-2"),
            (Some(renamed), "b2"),
            (Some(moved), "another terminal"),
            (Some(silent), "no longer reports"),
            (None, "no agent"),
        ] {
            let mut machine = switching(SwitchOptions::default());
            machine.next(0, Observation::agent(Some(agent())));
            assert_eq!(
                machine.next(0, Observation::Confirmed(true)),
                Action::PollAgent
            );
            let action = machine.next(0, Observation::agent(changed));
            let SwitchError::AgentChanged { pane_id, detail } = error_of(&action) else {
                panic!("expected AgentChanged, got {action:?}");
            };
            assert_eq!(pane_id, "1:2");
            assert!(detail.contains(expected), "{detail}");
            assert!(!machine.touched_pane());
            assert!(
                error_of(&action).to_string().contains("nothing was sent"),
                "{action:?}"
            );
        }
    }

    /// A server that refuses a send wrote nothing to the pane, so the agent is
    /// untouched; a request lost in transit may have, so it counts as touched.
    #[test]
    fn a_refused_send_leaves_the_pane_untouched_but_a_lost_one_does_not() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;

        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(blocked.clone())));
        confirm_with(&mut machine, blocked.clone());
        let action = machine.next(
            1,
            Observation::SendRejected {
                code: "agent_not_ready".to_string(),
                detail: "not the foreground".to_string(),
            },
        );
        assert!(matches!(error_of(&action), SwitchError::KeysRefused { .. }));
        assert!(!machine.touched_pane());

        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(blocked.clone())));
        confirm_with(&mut machine, blocked);
        let action = machine.next(
            1,
            Observation::SendRejected {
                code: TRANSPORT_ERROR_CODE.to_string(),
                detail: "connection reset".to_string(),
            },
        );
        assert!(matches!(error_of(&action), SwitchError::KeysRefused { .. }));
        assert!(machine.touched_pane());

        // The same for the exit prompt of an idle agent.
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(agent())));
        confirm_with(&mut machine, agent());
        let action = machine.next(
            1,
            Observation::SendRejected {
                code: "agent_not_ready".to_string(),
                detail: "launch pending".to_string(),
            },
        );
        assert!(matches!(error_of(&action), SwitchError::ExitRefused { .. }));
        assert!(!machine.touched_pane());
    }

    /// The server rejects a prompt while the agent is still blocked, which can
    /// race with the Escape meant to unblock it. Bounded retries, then a clear
    /// failure — never an unbounded poke loop.
    #[test]
    fn a_blocked_exit_prompt_is_retried_a_bounded_number_of_times() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(blocked.clone())));
        confirm_with(&mut machine, blocked);

        let mut escapes = 1;
        let mut action = machine.next(2, Observation::Sent);
        let mut clock = 3;
        loop {
            match action {
                Action::Wait(_) => action = machine.next(clock, Observation::Tick),
                Action::Prompt(_) => {
                    action = machine.next(
                        clock,
                        Observation::SendRejected {
                            code: "agent_blocked".to_string(),
                            detail: "agent is blocked".to_string(),
                        },
                    )
                }
                Action::SendKeys(_) => {
                    escapes += 1;
                    action = machine.next(clock, Observation::Sent);
                }
                Action::Finish(_) => break,
                other => panic!("unexpected action {other:?}"),
            }
            clock += 1;
            assert!(clock < 50, "the retry loop must be bounded");
        }
        assert_eq!(escapes, MAX_EXIT_ATTEMPTS, "one Escape per exit attempt");
        assert!(matches!(error_of(&action), SwitchError::ExitRefused { .. }));
    }

    /// The regression this PR would otherwise have shipped: the usage-limit
    /// rule makes a limited agent `blocked`, the stock server refuses
    /// `agent.prompt` for a blocked agent, and Escape cannot clear a limit —
    /// so `switch-account`, the one command that gets a human out of a limit,
    /// would refuse every limited agent. `/exit` is typed instead.
    #[test]
    fn a_usage_limited_agent_is_asked_to_exit_by_typing() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            to: profile("work"),
            to_inspection: healthy(),
            options: SwitchOptions::default(),
            limit: Some(crate::accounts::limit::UsageLimit { reset_text: None }),
        });
        let blocked_again = blocked.clone();
        machine.next(0, Observation::agent(Some(blocked.clone())));
        machine.next(1, Observation::Confirmed(true));
        let action = machine.next(2, Observation::agent(Some(blocked)));

        // Escape first, as for any blocked agent…
        assert!(matches!(action, Action::SendKeys(_)), "{action:?}");
        let action = machine.next(3, Observation::Sent);
        assert!(matches!(action, Action::Wait(_)), "{action:?}");
        let action = machine.next(4, Observation::Tick);
        assert_eq!(action, Action::Prompt("/exit".to_string()));

        // …and when the server refuses the prompt, the pinned agent is read
        // once more — `pane.send_text` checks nothing — and only then typed.
        let action = machine.next(
            5,
            Observation::SendRejected {
                code: "agent_blocked".to_string(),
                detail: "agent is blocked".to_string(),
            },
        );
        assert_eq!(action, Action::PollAgent);
        let action = machine.next(6, Observation::agent(Some(blocked_again)));
        assert_eq!(action, Action::SubmitText("/exit".to_string()));
        assert_eq!(machine.next(7, Observation::Sent), Action::PollPane);
        assert!(machine.touched_pane());
    }

    /// The guard on the one send that carries no server-side checks: a pane
    /// that changed hands between the refused prompt and the typed `/exit` is
    /// never typed into.
    #[test]
    fn a_pane_that_changed_hands_is_not_typed_into() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            to: profile("work"),
            to_inspection: healthy(),
            options: SwitchOptions::default(),
            limit: Some(crate::accounts::limit::UsageLimit { reset_text: None }),
        });
        machine.next(0, Observation::agent(Some(blocked.clone())));
        machine.next(1, Observation::Confirmed(true));
        machine.next(2, Observation::agent(Some(blocked.clone())));
        machine.next(3, Observation::Sent);
        machine.next(4, Observation::Tick);
        let action = machine.next(
            5,
            Observation::SendRejected {
                code: "agent_blocked".to_string(),
                detail: "agent is blocked".to_string(),
            },
        );
        assert_eq!(action, Action::PollAgent);

        let mut stranger = blocked;
        stranger.terminal_id = "another-terminal".to_string();
        let action = machine.next(6, Observation::agent(Some(stranger)));
        assert!(
            matches!(error_of(&action), SwitchError::AgentChanged { .. }),
            "{action:?}"
        );
        // The Escape that opened the exit did reach the pane, so the failure is
        // still reported as "touched" — the point is that no `/exit` was typed
        // into whatever now holds the pane.
        assert!(machine.touched_pane());
    }

    /// Only for a limit. Any other blocked agent keeps the old, guarded path,
    /// because `pane.send_text` carries none of `agent.prompt`'s checks.
    #[test]
    fn an_ordinary_blocked_agent_is_never_typed_into() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(blocked.clone())));
        machine.next(1, Observation::Confirmed(true));
        let mut action = machine.next(2, Observation::agent(Some(blocked.clone())));
        for step in 3..50 {
            action = match action {
                Action::Wait(_) => machine.next(step, Observation::Tick),
                Action::SendKeys(_) => machine.next(step, Observation::Sent),
                Action::PollAgent => machine.next(step, Observation::agent(Some(blocked.clone()))),
                Action::Prompt(_) => machine.next(
                    step,
                    Observation::SendRejected {
                        code: "agent_blocked".to_string(),
                        detail: "agent is blocked".to_string(),
                    },
                ),
                Action::SubmitText(_) => panic!("nothing may be typed into a blocked pane"),
                Action::Finish(_) => break,
                other => panic!("unexpected action {other:?}"),
            };
        }
        assert!(matches!(error_of(&action), SwitchError::ExitRefused { .. }));
    }

    /// One attempt only: a typed line that may already have landed is never
    /// sent twice.
    #[test]
    fn a_refused_typed_exit_is_not_retried() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            to: profile("work"),
            to_inspection: healthy(),
            options: SwitchOptions::default(),
            limit: Some(crate::accounts::limit::UsageLimit { reset_text: None }),
        });
        machine.next(0, Observation::agent(Some(blocked.clone())));
        machine.next(1, Observation::Confirmed(true));
        machine.next(2, Observation::agent(Some(blocked.clone())));
        machine.next(3, Observation::Sent);
        machine.next(4, Observation::Tick);
        let action = machine.next(
            5,
            Observation::SendRejected {
                code: "agent_blocked".to_string(),
                detail: "blocked".to_string(),
            },
        );
        assert_eq!(action, Action::PollAgent);
        let action = machine.next(6, Observation::agent(Some(blocked)));
        assert_eq!(action, Action::SubmitText("/exit".to_string()));
        let action = machine.next(
            7,
            Observation::SendRejected {
                code: "pane_not_found".to_string(),
                detail: "gone".to_string(),
            },
        );
        assert!(matches!(error_of(&action), SwitchError::ExitRefused { .. }));
    }

    #[test]
    fn a_declined_confirmation_sends_nothing() {
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(agent())));
        let action = machine.next(1, Observation::Confirmed(false));
        assert!(matches!(error_of(&action), SwitchError::Declined));
        assert!(!machine.touched_pane());
    }

    #[test]
    fn no_way_to_confirm_is_a_refusal_not_a_default_yes() {
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(agent())));
        let action = machine.next(1, Observation::ConfirmUnavailable);
        assert!(matches!(
            error_of(&action),
            SwitchError::ConfirmationRequired
        ));
        assert!(!machine.touched_pane());
    }

    #[test]
    fn a_target_profile_that_is_missing_or_logged_out_is_refused() {
        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            expected_name: None,
            to: profile("work"),
            to_inspection: ProfileInspection {
                dir_exists: false,
                ..healthy()
            },
            options: SwitchOptions::default(),
            limit: None,
        });
        let action = machine.next(0, Observation::agent(Some(agent())));
        assert!(matches!(
            error_of(&action),
            SwitchError::ProfileDirectoryMissing { .. }
        ));

        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            expected_name: None,
            to: profile("work"),
            to_inspection: ProfileInspection {
                logged_in: false,
                ..healthy()
            },
            options: SwitchOptions::default(),
            limit: None,
        });
        let action = machine.next(0, Observation::agent(Some(agent())));
        assert!(matches!(
            error_of(&action),
            SwitchError::ProfileLoggedOut { .. }
        ));

        // …but --force takes it, with the warning spelled out.
        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            expected_name: None,
            to: profile("work"),
            to_inspection: ProfileInspection {
                logged_in: false,
                ..healthy()
            },
            options: SwitchOptions {
                force: true,
                ..SwitchOptions::default()
            },
            limit: None,
        });
        let action = machine.next(0, Observation::agent(Some(agent())));
        let Action::AskConfirm(text) = &action else {
            panic!("expected a confirmation, got {action:?}");
        };
        assert!(text.contains("no credentials"), "{text}");
    }

    #[test]
    fn switching_to_the_account_the_agent_already_claims_needs_force() {
        let mut on_work = agent();
        on_work.account = Some("work".to_string());

        let mut machine = switching(SwitchOptions::default());
        let action = machine.next(0, Observation::agent(Some(on_work.clone())));
        assert!(matches!(
            error_of(&action),
            SwitchError::AlreadyOnAccount { .. }
        ));

        let mut machine = switching(SwitchOptions {
            force: true,
            ..SwitchOptions::default()
        });
        assert!(matches!(
            machine.next(0, Observation::agent(Some(on_work))),
            Action::AskConfirm(_)
        ));
    }

    /// The timeout the plan cares about most: it must stop, say what state the
    /// pane is in, and send nothing else.
    #[test]
    fn an_agent_that_never_exits_fails_without_escalating() {
        let mut machine = switching(SwitchOptions {
            timeout_ms: 1_000,
            ..SwitchOptions::default()
        });
        exited(&mut machine);

        let busy = || {
            Observation::Pane(PaneReading::Busy {
                detail: "claude".to_string(),
            })
        };
        assert_eq!(machine.next(500, busy()), Action::Wait(POLL_INTERVAL_MS));
        let action = machine.next(1_000, busy());
        let SwitchError::AgentStillRunning {
            name,
            pane_id,
            seconds,
        } = error_of(&action)
        else {
            panic!("expected AgentStillRunning, got {action:?}");
        };
        assert_eq!(name, "a1");
        assert_eq!(pane_id, "1:2");
        assert_eq!(*seconds, 1);
        let message = error_of(&action).to_string();
        assert!(message.contains("nothing was killed"), "{message}");
        assert!(message.contains("still running"), "{message}");

        // And it is finished: no further action can be squeezed out of it.
        let after = machine.next(2_000, Observation::Tick);
        assert!(matches!(after, Action::Finish(_)));
    }

    #[test]
    fn a_pane_that_became_another_terminal_is_never_typed_into() {
        let mut machine = switching(SwitchOptions::default());
        exited(&mut machine);
        let action = machine.next(
            1,
            Observation::Pane(PaneReading::Released {
                terminal_id: "t-9".to_string(),
            }),
        );
        assert!(matches!(
            error_of(&action),
            SwitchError::PaneReplaced { .. }
        ));
        let message = error_of(&action).to_string();
        assert!(message.contains("claude --resume sess-1"), "{message}");

        // The same after the relaunch, while waiting for the hook.
        let mut machine = switching(SwitchOptions::default());
        exited(&mut machine);
        machine.next(1, released());
        machine.next(2, Observation::launched(Ok(())));
        let mut moved = agent();
        moved.terminal_id = "t-9".to_string();
        let action = machine.next(3, Observation::agent(Some(moved)));
        assert!(matches!(
            error_of(&action),
            SwitchError::PaneReplaced { .. }
        ));
    }

    /// A failure after the pane was touched must say how to get the
    /// conversation back, even when the error itself is a bare API failure.
    #[test]
    fn a_failure_after_the_pane_was_touched_carries_a_recovery_hint() {
        let mut machine = switching(SwitchOptions::default());
        let api = || SwitchError::Api {
            method: "agent.get".to_string(),
            detail: "connection reset".to_string(),
        };

        // Before preflight nothing is known and nothing was touched.
        let failure = machine.failure(api());
        assert_eq!(failure.recovery_hint(), None);
        assert_eq!(failure.pane_id, None);

        // After the exit was delivered, a bare API failure names the pane and
        // the resume command.
        exited(&mut machine);
        let failure = machine.failure(api());
        assert!(failure.touched_pane);
        assert_eq!(failure.pane_id.as_deref(), Some("1:2"));
        assert_eq!(failure.session_id.as_deref(), Some("sess-1"));
        let hint = failure
            .recovery_hint()
            .expect("a hint after touching the pane");
        assert!(hint.contains("herdr pane read 1:2"), "{hint}");
        assert!(hint.contains("claude --resume sess-1"), "{hint}");

        // Errors that already say what the pane holds get no second hint.
        let failure = machine.failure(SwitchError::AgentStillRunning {
            name: "a1".to_string(),
            pane_id: "1:2".to_string(),
            seconds: 20,
        });
        assert_eq!(failure.recovery_hint(), None);
        let failure = machine.failure(SwitchError::Launch {
            pane_id: "1:2".to_string(),
            session_id: "sess-1".to_string(),
            detail: "pane busy".to_string(),
        });
        assert_eq!(failure.recovery_hint(), None);
        assert!(
            failure.to_string().contains("claude --resume sess-1"),
            "{failure}"
        );
    }

    #[test]
    fn a_pane_that_cannot_be_read_until_the_deadline_fails_with_the_reason() {
        let mut machine = switching(SwitchOptions {
            timeout_ms: 500,
            ..SwitchOptions::default()
        });
        exited(&mut machine);
        let unreadable = || {
            Observation::Pane(PaneReading::Unreadable {
                detail: "transport closed".to_string(),
            })
        };
        assert_eq!(
            machine.next(100, unreadable()),
            Action::Wait(POLL_INTERVAL_MS)
        );
        let action = machine.next(500, unreadable());
        assert!(matches!(
            error_of(&action),
            SwitchError::PaneUnreadable { .. }
        ));
    }

    #[test]
    fn a_failed_relaunch_ends_the_protocol() {
        let mut machine = switching(SwitchOptions::default());
        exited(&mut machine);
        machine.next(1, released());
        let action = machine.next(
            2,
            Observation::launched(Err(SwitchError::Launch {
                pane_id: "1:2".to_string(),
                session_id: "sess-1".to_string(),
                detail: "pane busy".to_string(),
            })),
        );
        assert!(matches!(error_of(&action), SwitchError::Launch { .. }));
    }

    /// The conversation was lost: Claude came back under a different id.
    #[test]
    fn a_resume_that_started_a_new_conversation_is_a_loud_failure() {
        let mut machine = switching(SwitchOptions::default());
        exited(&mut machine);
        machine.next(1, released());
        machine.next(2, Observation::launched(Ok(())));

        let mut fresh = agent();
        fresh.session = Some(session("sess-2"));
        let action = machine.next(3, Observation::agent(Some(fresh)));
        let SwitchError::SessionMismatch { expected, actual } = error_of(&action) else {
            panic!("expected SessionMismatch, got {action:?}");
        };
        assert_eq!(expected, "sess-1");
        assert_eq!(actual, "sess-2");
        let message = error_of(&action).to_string();
        assert!(message.contains("still on disk"), "{message}");
    }

    /// Only Claude's own hook proves the resume. A session another
    /// integration reported on the pane — even with the same value — is
    /// neither proof nor a mismatch; the machine keeps waiting.
    #[test]
    fn a_session_from_another_integration_is_not_taken_as_the_resume() {
        let mut machine = switching(SwitchOptions::default());
        exited(&mut machine);
        machine.next(1, released());
        machine.next(2, Observation::launched(Ok(())));

        let mut foreign = agent();
        foreign.session = Some(AgentSessionInfo {
            source: "herdr:codex".to_string(),
            agent: "codex".to_string(),
            kind: AgentSessionRefKind::Id,
            value: "sess-1".to_string(),
        });
        assert_eq!(
            machine.next(3, Observation::agent(Some(foreign))),
            Action::Wait(POLL_INTERVAL_MS)
        );
        assert_eq!(machine.next(4, Observation::Tick), Action::PollAgent);
        assert_eq!(
            machine.next(5, Observation::agent(Some(agent()))),
            Action::Grade
        );
    }

    #[test]
    fn a_resume_that_never_reports_fails_naming_what_to_do() {
        let mut machine = switching(SwitchOptions {
            timeout_ms: 1_000,
            ..SwitchOptions::default()
        });
        exited(&mut machine);
        machine.next(1, released());
        machine.next(2, Observation::launched(Ok(())));

        let mut silent = agent();
        silent.session = None;
        assert_eq!(
            machine.next(500, Observation::agent(Some(silent.clone()))),
            Action::Wait(POLL_INTERVAL_MS)
        );
        let action = machine.next(1_002, Observation::agent(Some(silent)));
        assert!(matches!(
            error_of(&action),
            SwitchError::SessionNotReported { .. }
        ));
        let message = error_of(&action).to_string();
        assert!(message.contains("--resume sess-1"), "{message}");
    }

    /// A mismatch is still a result, not an error: the token has to be written
    /// so every other surface shows it, and the caller decides the exit code.
    #[test]
    fn a_graded_mismatch_finishes_with_the_state_recorded() {
        let mut machine = switching(SwitchOptions::default());
        exited(&mut machine);
        machine.next(1, released());
        machine.next(2, Observation::launched(Ok(())));
        machine.next(3, Observation::agent(Some(agent())));
        let action = machine.next(
            4,
            Observation::Graded {
                state: AccountState::Mismatch,
                detail: Some("/p/perso".to_string()),
            },
        );
        let result = finished(&action).as_ref().expect("a finished switch");
        assert_eq!(result.account_state, AccountState::Mismatch);
        assert_eq!(result.mismatch_detail.as_deref(), Some("/p/perso"));
    }

    /// A driver that feeds the wrong observation must stop the protocol, never
    /// fall through into sending something.
    #[test]
    fn an_out_of_order_observation_stops_the_protocol() {
        let mut machine = switching(SwitchOptions::default());
        let action = machine.next(0, Observation::Tick);
        assert!(matches!(error_of(&action), SwitchError::Api { .. }));
        assert!(!machine.touched_pane());
    }
}
