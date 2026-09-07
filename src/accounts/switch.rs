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
//!    control is explicit.
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
    /// pane again.
    Released { terminal_id: Option<String> },
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
    /// The pane is no longer the pane the switch started on.
    PaneReplaced {
        pane_id: String,
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
    Launch {
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
            Self::PaneReplaced { pane_id } => write!(
                formatter,
                "pane {pane_id} is no longer the terminal this switch started on; stopping rather \
                 than typing into it"
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
            Self::Launch { detail } => write!(formatter, "could not restart the agent: {detail}"),
            Self::Api { method, detail } => write!(formatter, "{method} failed: {detail}"),
        }
    }
}

/// A failure plus the one fact that decides how loud it is: whether anything
/// had already been sent to the pane when it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchFailure {
    pub error: SwitchError,
    /// `false` means the agent was never touched, so it is exactly as it was.
    pub touched_pane: bool,
}

impl std::fmt::Display for SwitchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

/// What the machine is doing. Deadlines are absolute, on the driver's clock.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Preflight,
    Confirm,
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
    pub to: AccountProfile,
    pub to_inspection: ProfileInspection,
    pub options: SwitchOptions,
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
            Phase::Escape { attempts } => self.on_escape(attempts, observation),
            Phase::EscapeSettle { attempts } => self.on_escape_settle(attempts, observation),
            Phase::Exit { attempts } => self.on_exit(now, attempts, observation),
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
        text
    }

    fn on_confirm(&mut self, observation: Observation) -> Action {
        match observation {
            Observation::Confirmed(true) => self.begin_exit(),
            Observation::Confirmed(false) => self.fail(SwitchError::Declined),
            Observation::ConfirmUnavailable => self.fail(SwitchError::ConfirmationRequired),
            _ => self.out_of_order("confirmation"),
        }
    }

    /// Escape first when the agent cannot read a prompt (blocked), or when a
    /// working agent is being interrupted on purpose. Otherwise `/exit` goes
    /// straight in: an idle Claude needs no interruption, and sending Escape
    /// to one is input it did not ask for.
    fn begin_exit(&mut self) -> Action {
        self.touched_pane = true;
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
                Action::Prompt("/exit".to_string())
            }
        }
    }

    fn on_escape(&mut self, attempts: u8, observation: Observation) -> Action {
        match observation {
            Observation::Sent => {
                self.phase = Phase::EscapeSettle { attempts };
                Action::Wait(ESCAPE_SETTLE_MS)
            }
            Observation::SendRejected { code, detail } => {
                self.fail(SwitchError::KeysRefused { code, detail })
            }
            _ => self.out_of_order("interrupt"),
        }
    }

    fn on_escape_settle(&mut self, attempts: u8, observation: Observation) -> Action {
        match observation {
            Observation::Tick => {
                self.phase = Phase::Exit { attempts };
                Action::Prompt("/exit".to_string())
            }
            _ => self.out_of_order("interrupt"),
        }
    }

    fn on_exit(&mut self, now: Millis, attempts: u8, observation: Observation) -> Action {
        match observation {
            Observation::Sent => {
                self.phase = Phase::AwaitShell {
                    deadline: now.saturating_add(self.input.options.timeout_ms),
                };
                Action::PollPane
            }
            // The server refuses a prompt while the agent is blocked. That can
            // race with the Escape that was meant to unblock it, so a bounded
            // number of retries goes back through Escape rather than failing on
            // a timing artefact.
            Observation::SendRejected { code, detail } => {
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

    fn on_await_shell(
        &mut self,
        now: Millis,
        deadline: Millis,
        observation: Observation,
    ) -> Action {
        let expired = now >= deadline;
        match observation {
            Observation::Pane(PaneReading::Released { terminal_id }) => {
                if terminal_id.is_some_and(|id| id != self.terminal_id) {
                    return self.fail(SwitchError::PaneReplaced {
                        pane_id: self.pane_id.clone(),
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
                // process exits, so any session visible now was reported by the
                // process this switch started.
                if let Some(session) = agent.as_ref().as_ref().and_then(|a| a.session.as_ref()) {
                    if session.value == self.session_id {
                        self.phase = Phase::Grade;
                        return Action::Grade;
                    }
                    return self.fail(SwitchError::SessionMismatch {
                        expected: self.session_id.clone(),
                        actual: session.value.clone(),
                    });
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
            to: profile("work"),
            to_inspection: healthy(),
            options,
        })
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

        // Idle: no Escape, straight to /exit.
        assert_eq!(
            machine.next(1, Observation::Confirmed(true)),
            Action::Prompt("/exit".to_string())
        );
        assert!(machine.touched_pane());

        assert_eq!(machine.next(2, Observation::Sent), Action::PollPane);
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

        let action = machine.next(
            5,
            Observation::Pane(PaneReading::Released {
                terminal_id: Some("t-1".to_string()),
            }),
        );
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
        let action = machine.next(0, Observation::agent(Some(working)));
        let Action::AskConfirm(text) = &action else {
            panic!("expected a confirmation, got {action:?}");
        };
        assert!(
            text.contains("WARNING: this agent is working"),
            "an interrupt must be spelled out in the confirmation: {text}"
        );
        assert_eq!(
            machine.next(1, Observation::Confirmed(true)),
            Action::SendKeys(vec!["esc".to_string()])
        );
        assert_eq!(
            machine.next(2, Observation::Sent),
            Action::Wait(ESCAPE_SETTLE_MS)
        );
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
            machine.next(0, Observation::agent(Some(blocked))),
            Action::AskConfirm(_)
        ));
        assert_eq!(
            machine.next(1, Observation::Confirmed(true)),
            Action::SendKeys(vec!["esc".to_string()])
        );
    }

    /// The server rejects a prompt while the agent is still blocked, which can
    /// race with the Escape meant to unblock it. Bounded retries, then a clear
    /// failure — never an unbounded poke loop.
    #[test]
    fn a_blocked_exit_prompt_is_retried_a_bounded_number_of_times() {
        let mut blocked = agent();
        blocked.status = AgentStatus::Blocked;
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(blocked)));
        machine.next(1, Observation::Confirmed(true));

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
            to: profile("work"),
            to_inspection: ProfileInspection {
                dir_exists: false,
                ..healthy()
            },
            options: SwitchOptions::default(),
        });
        let action = machine.next(0, Observation::agent(Some(agent())));
        assert!(matches!(
            error_of(&action),
            SwitchError::ProfileDirectoryMissing { .. }
        ));

        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            to: profile("work"),
            to_inspection: ProfileInspection {
                logged_in: false,
                ..healthy()
            },
            options: SwitchOptions::default(),
        });
        let action = machine.next(0, Observation::agent(Some(agent())));
        assert!(matches!(
            error_of(&action),
            SwitchError::ProfileLoggedOut { .. }
        ));

        // …but --force takes it, with the warning spelled out.
        let mut machine = SwitchMachine::new(SwitchInput {
            target: "a1".to_string(),
            to: profile("work"),
            to_inspection: ProfileInspection {
                logged_in: false,
                ..healthy()
            },
            options: SwitchOptions {
                force: true,
                ..SwitchOptions::default()
            },
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
        machine.next(0, Observation::agent(Some(agent())));
        machine.next(0, Observation::Confirmed(true));
        machine.next(0, Observation::Sent);

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
        machine.next(0, Observation::agent(Some(agent())));
        machine.next(0, Observation::Confirmed(true));
        machine.next(0, Observation::Sent);
        let action = machine.next(
            1,
            Observation::Pane(PaneReading::Released {
                terminal_id: Some("t-9".to_string()),
            }),
        );
        assert!(matches!(
            error_of(&action),
            SwitchError::PaneReplaced { .. }
        ));
    }

    #[test]
    fn a_pane_that_cannot_be_read_until_the_deadline_fails_with_the_reason() {
        let mut machine = switching(SwitchOptions {
            timeout_ms: 500,
            ..SwitchOptions::default()
        });
        machine.next(0, Observation::agent(Some(agent())));
        machine.next(0, Observation::Confirmed(true));
        machine.next(0, Observation::Sent);
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
        machine.next(0, Observation::agent(Some(agent())));
        machine.next(0, Observation::Confirmed(true));
        machine.next(0, Observation::Sent);
        machine.next(
            1,
            Observation::Pane(PaneReading::Released {
                terminal_id: Some("t-1".to_string()),
            }),
        );
        let action = machine.next(
            2,
            Observation::launched(Err(SwitchError::Launch {
                detail: "pane busy".to_string(),
            })),
        );
        assert!(matches!(error_of(&action), SwitchError::Launch { .. }));
    }

    /// The conversation was lost: Claude came back under a different id.
    #[test]
    fn a_resume_that_started_a_new_conversation_is_a_loud_failure() {
        let mut machine = switching(SwitchOptions::default());
        machine.next(0, Observation::agent(Some(agent())));
        machine.next(0, Observation::Confirmed(true));
        machine.next(0, Observation::Sent);
        machine.next(
            1,
            Observation::Pane(PaneReading::Released {
                terminal_id: Some("t-1".to_string()),
            }),
        );
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

    #[test]
    fn a_resume_that_never_reports_fails_naming_what_to_do() {
        let mut machine = switching(SwitchOptions {
            timeout_ms: 1_000,
            ..SwitchOptions::default()
        });
        machine.next(0, Observation::agent(Some(agent())));
        machine.next(0, Observation::Confirmed(true));
        machine.next(0, Observation::Sent);
        machine.next(
            1,
            Observation::Pane(PaneReading::Released {
                terminal_id: Some("t-1".to_string()),
            }),
        );
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
        machine.next(0, Observation::agent(Some(agent())));
        machine.next(0, Observation::Confirmed(true));
        machine.next(0, Observation::Sent);
        machine.next(
            1,
            Observation::Pane(PaneReading::Released {
                terminal_id: Some("t-1".to_string()),
            }),
        );
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
