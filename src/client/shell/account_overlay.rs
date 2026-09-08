//! Fork (E9): the TUI half of `herdr agent start --account`.
//!
//! Everything E9 adds to the client shell lives here, so an upstream sync can
//! take upstream's side on every file under `src/client/` and re-apply this
//! module's wiring from the list in
//! `docs/fork/decisions/0002-adopt-upstream-multi-machine-client.md`. The
//! upstream files carry one delegating arm each and no account logic.
//!
//! The safety argument is the same one `crate::accounts::client` makes for the
//! CLI, plus one the TUI has and the CLI does not: **the action must reach the
//! pane the user right-clicked, and no other**. So the pane id is pinned when
//! the menu opens, re-checked against the live snapshot when the user presses
//! Enter (the snapshot moves under a modal — a pane can close, or grow an
//! agent, while the picker is up), and then addressed by that id in every API
//! call the worker makes. The worker never asks the shell which pane is
//! focused.
//!
//! The launch itself is not reimplemented: the worker runs exactly the
//! sequence `herdr agent start --account` runs — `accounts::client::prepare`
//! (which refuses anything but a shell at its prompt), `apply_env`,
//! `cli::agent::start_managed_agent` (the stock `agent.start` retry), and
//! `AppliedLine::finish` (which grades the launched process's *own*
//! environment before it claims an account). It runs on its own thread over
//! the local API socket, because that sequence blocks for seconds and the
//! client's event loop must keep drawing; the overlay only folds the events
//! the worker sends back.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::*;

use crate::accounts::client::Confirmation;
use crate::accounts::layout::{inspect, InspectOptions};
use crate::accounts::profile::{AccountProfile, Profiles};
use crate::accounts::switch::{LaunchRequest, SwitchError, SwitchInput, SwitchOptions};
use crate::accounts::tokens::{AccountState, ACCOUNT_TOKEN, AGENT_LABEL};

/// The agent name a first Claude launch gets, and the stem the next ones are
/// numbered from.
const DEFAULT_AGENT_NAME: &str = AGENT_LABEL;

/// How long the worker may go without reporting anything before the modal
/// stops waiting for it.
///
/// The worker's socket calls carry no timeout, so a server that stops
/// answering would otherwise leave a modal that cannot be dismissed — Esc is
/// refused while a launch runs. Every phase is bounded well inside this when
/// the server answers at all: the shell settle wait is 2 s, `agent.start`'s
/// busy retry 2 s, and its readiness wait 30 s. When the budget runs out the
/// worker is detached, not killed: it still grades and records the agent if
/// it ever gets that far, so nothing about the account claim changes — only
/// where the outcome is shown.
const ACCOUNT_JOB_STALL_LIMIT: Duration = Duration::from_secs(90);

/// How long the switch's question must have been on screen before Enter, `y`
/// or a click on `confirm` counts as the answer.
///
/// The question appears in the place the picker was, moments after the Enter
/// or the click that submitted the picker. A double-tapped Enter, a held key's
/// first repeat, or the second half of a double-click on a row would land on
/// it before anyone could have read it — and the `confirm` hit rectangle can
/// still be the previous frame's until the next paint. Nobody reads three
/// lines about stopping their agent in half a second, so an answer that
/// early is treated as the tail of the gesture that opened the question and
/// ignored. Declining is never delayed: a spurious "no" costs nothing.
const CONFIRM_ARM_DELAY: Duration = Duration::from_millis(500);

/// One profile, as the picker shows it.
///
/// Health is the cheap half of [`crate::accounts::layout::inspect`] — stat
/// calls only. The identity half parses `.claude.json`, which is megabytes on
/// a real installation, and this is built while the user is holding a mouse
/// button down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AccountEntry {
    pub(super) name: String,
    pub(super) config_dir: String,
    pub(super) dir_exists: bool,
    pub(super) logged_in: bool,
    pub(super) hook_installed: bool,
    pub(super) is_default: bool,
    /// The account this pane's agent already claims. Only ever true in
    /// [`PickerMode::Switch`]; a switch to it is refused by the machine, and
    /// the row says so before the user tries.
    pub(super) is_current: bool,
}

impl AccountEntry {
    /// The one thing wrong with this profile, worst first, or `""`.
    ///
    /// A profile herdr cannot launch under at all, then one that will land on
    /// Claude's login screen, then one whose session id will never be reported
    /// — so it can never be switched, which is the whole point of the epic.
    pub(super) fn health_label(&self) -> &'static str {
        if !self.dir_exists {
            "missing"
        } else if !self.logged_in {
            "logged out"
        } else if !self.hook_installed {
            "no hook"
        } else {
            ""
        }
    }

    /// True when the row should be drawn as a problem rather than a note.
    pub(super) fn unusable(&self) -> bool {
        !self.dir_exists || !self.logged_in
    }

    /// What the row shows on the right: where the agent is now, which profile
    /// is the default, and what is wrong with it. All three, because they are
    /// independent facts and a default profile with no hook is exactly the
    /// case worth seeing.
    pub(super) fn status_text(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.is_current {
            parts.push("current");
        }
        if self.is_default {
            parts.push("default");
        }
        let health = self.health_label();
        if !health.is_empty() {
            parts.push(health);
        }
        parts.join(" · ")
    }

    pub(super) fn matches_query(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || format!("{} {} {}", self.name, self.config_dir, self.status_text())
                .to_lowercase()
                .contains(&query)
    }
}

/// What the picker is for.
///
/// The overlay, its key routing and [`AccountJob`] are shared between the two;
/// only the submitted job, the confirmation step and the wording differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PickerMode {
    /// Start a new Claude agent in a pane that has none, under the picked
    /// profile, with this name.
    Start { agent_name: String },
    /// Move the Claude agent already running in this pane to another profile,
    /// keeping its conversation.
    Switch {
        /// The managed name the agent had when the menu was opened. The
        /// submit-time preflight refuses if the pane no longer holds an agent
        /// by that name, so the action can only ever reach the agent the user
        /// right-clicked.
        agent_name: String,
        /// What that agent claims today, when it claims anything.
        current: Option<String>,
        /// Send `Escape` before `/exit` when the agent turns out to be
        /// working. Off by default: interrupting a tool call is a decision,
        /// not a default.
        interrupt: bool,
    },
}

impl PickerMode {
    fn title(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start claude as account",
            Self::Switch { .. } => "switch claude account",
        }
    }

    fn is_switch(&self) -> bool {
        matches!(self, Self::Switch { .. })
    }
}

/// A step of the launch or switch, as the worker thread reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AccountJobEvent {
    /// A phase name for the modal; the worker sends one before each blocking
    /// call so a slow launch never looks hung.
    Progress(&'static str),
    /// The switch protocol is asking the question it always asks. The worker
    /// is blocked on the answer and nothing has been sent to the pane;
    /// [`ClientShellState::answer_account_confirm`] is the only thing that
    /// unblocks it.
    Confirm(String),
    Finished(AccountJobResult),
}

/// How a launch or a switch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AccountJobResult {
    /// The agent is running. `state` is the *graded* account state, so a
    /// `Mismatch` lands here too — the token was written, and the user has to
    /// see that the agent is not on the account they picked.
    Launched {
        account: String,
        state: AccountState,
        detail: Option<String>,
        warnings: Vec<String>,
    },
    /// The agent was moved to another profile and kept its conversation.
    /// Graded exactly like a launch, and a `Mismatch` lands here too.
    Switched {
        account: String,
        state: AccountState,
        detail: Option<String>,
        /// The conversation the agent came back with — the same one it had.
        session_id: String,
        warnings: Vec<String>,
    },
    /// Nothing is running under the picked account. `notes` carries everything
    /// the user still has to read: the environment line the pane's shell is
    /// left holding, the switch's own warnings (a relaunched agent *was*
    /// recorded under the new account even though the protocol did not
    /// finish), and the `claude --resume <id>` recovery hint.
    Failed {
        message: String,
        notes: Vec<String>,
        /// True when the worker proved nothing reached the pane, so the agent
        /// is exactly as it was and the picker stays usable. Only the switch
        /// protocol can prove this (`SwitchFailure::touched_pane`); a start
        /// that failed after `prepare` cannot, so it settles the modal.
        refused: bool,
    },
}

/// The worker thread behind a submitted pick.
///
/// Kept alive by the overlay while it runs. The receiver is the only channel;
/// the handle exists so the thread is joined rather than detached once it has
/// sent its terminal event.
#[derive(Debug)]
pub(super) struct AccountJob {
    handle: Option<JoinHandle<()>>,
    events: Receiver<AccountJobEvent>,
    /// When the worker last said anything, for the stall limit.
    last_event: Instant,
    /// The other half of the switch's confirmation. Taken when the user
    /// answers, and dropped with the job otherwise — a worker whose answer
    /// channel closes reads that as "there is nobody to ask" and refuses
    /// without sending anything.
    answers: Option<std::sync::mpsc::Sender<Confirmation>>,
    /// True between the worker's question and the user's answer. The stall
    /// limit is suspended then: the worker is waiting on a person, not on a
    /// server, and a modal that closed itself under a question would answer
    /// it by accident.
    awaiting_confirm: bool,
}

impl AccountJob {
    /// Drain what the worker has said since the last tick.
    ///
    /// A disconnected channel with no terminal event means the worker panicked
    /// or was dropped; report that rather than showing a spinner for ever.
    fn drain(&mut self, now: Instant) -> (Vec<AccountJobEvent>, bool) {
        let mut events = Vec::new();
        let finished = loop {
            match self.events.try_recv() {
                Ok(event) => {
                    self.last_event = now;
                    let terminal = matches!(event, AccountJobEvent::Finished(_));
                    events.push(event);
                    if terminal {
                        break true;
                    }
                }
                Err(TryRecvError::Empty) => break false,
                Err(TryRecvError::Disconnected) => {
                    if !events
                        .iter()
                        .any(|event| matches!(event, AccountJobEvent::Finished(_)))
                    {
                        events.push(AccountJobEvent::Finished(AccountJobResult::Failed {
                            message: "the account job stopped without a result".to_owned(),
                            notes: Vec::new(),
                            refused: false,
                        }));
                    }
                    break true;
                }
            }
        };
        (events, finished)
    }

    /// True when the worker has been silent for longer than the stall limit.
    ///
    /// A worker blocked on the confirmation is not silent in the sense that
    /// matters: it is waiting for the person in front of the modal, has sent
    /// nothing to the pane, and will keep waiting for as long as they need.
    fn stalled(&self, now: Instant) -> bool {
        !self.awaiting_confirm
            && now.saturating_duration_since(self.last_event) > ACCOUNT_JOB_STALL_LIMIT
    }

    /// Answer the question the worker is blocked on, exactly once.
    ///
    /// `false` means the worker is already gone; the drain reports that on the
    /// next tick rather than this call inventing an outcome.
    ///
    /// The stall clock restarts here. It was suspended while the question was
    /// up, but `last_event` still dates from when the question *arrived* — a
    /// person may have taken minutes over it — and the worker's next word
    /// comes only after it has been scheduled and read the agent again. Left
    /// alone, the very next tick could declare a worker that has just been
    /// released "silent for 90 s", detach it, and let the switch run on with
    /// nobody watching.
    fn answer(&mut self, answer: Confirmation, now: Instant) -> bool {
        self.awaiting_confirm = false;
        self.last_event = now;
        self.answers
            .take()
            .is_some_and(|answers| answers.send(answer).is_ok())
    }

    /// Wait for the thread. Only called once its terminal event has been
    /// drained, so this never blocks the event loop for longer than the
    /// worker's own return.
    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Let the thread go without waiting for it. The worker keeps its own
    /// sender and finishes the launch on its own; only its report is lost.
    ///
    /// The answer channel goes with it: a detached switch worker that later
    /// reaches its confirmation finds nobody to ask and refuses, rather than
    /// blocking for ever on a modal that is no longer listening.
    fn detach(&mut self) {
        drop(self.handle.take());
        drop(self.answers.take());
        self.awaiting_confirm = false;
    }
}

/// The picker modal.
///
/// `pane_id` is the pane the context menu was opened on and is never
/// recomputed; see the module docs.
#[derive(Debug)]
pub(super) struct ClientAccountPickerOverlay {
    pub(super) pane_id: String,
    pub(super) mode: PickerMode,
    pub(super) entries: Vec<AccountEntry>,
    pub(super) selected: usize,
    pub(super) query: String,
    pub(super) search_focused: bool,
    /// The phase the worker last reported. `Some` means a job is running and
    /// the modal cannot be dismissed.
    pub(super) progress: Option<&'static str>,
    /// The switch protocol's own question, while it is on screen. The text is
    /// the machine's, verbatim: it names the pane, the agent, both accounts
    /// and the conversation that will be resumed, and it is the only thing
    /// standing between a right-click and Claude being asked to exit.
    pub(super) confirm: Option<String>,
    /// When the question was put on screen, for [`CONFIRM_ARM_DELAY`]. Set by
    /// the tick that folds the worker's `Confirm` event, since the fold itself
    /// is pure and has no clock.
    confirm_shown_at: Option<Instant>,
    pub(super) error: Option<String>,
    pub(super) warnings: Vec<String>,
    /// A launch has ended and its outcome is what the modal shows. From here
    /// Enter and the primary button close the modal; nothing launches again
    /// from a picker that has already launched once. A refusal that happened
    /// *before* any byte reached the pane does not set this — it is shown in
    /// `error`, and the user can pick another profile and try again.
    pub(super) settled: bool,
    job: Option<AccountJob>,
}

impl ClientAccountPickerOverlay {
    pub(super) fn title(&self) -> &'static str {
        self.mode.title()
    }

    pub(super) fn running(&self) -> bool {
        self.job.is_some()
    }

    /// True while the switch's question is on screen and nothing has been sent
    /// to the pane. The list is not navigable then — the only two answers are
    /// yes and no.
    pub(super) fn awaiting_confirm(&self) -> bool {
        self.confirm.is_some()
    }

    /// Whether this picker offers the interrupt toggle, and its state.
    pub(super) fn interrupt(&self) -> Option<bool> {
        match &self.mode {
            PickerMode::Start { .. } => None,
            PickerMode::Switch { interrupt, .. } => Some(*interrupt),
        }
    }

    /// Fresh, before any job: nothing running, nothing settled.
    pub(super) fn idle(
        pane_id: String,
        mode: PickerMode,
        entries: Vec<AccountEntry>,
        selected: usize,
    ) -> Self {
        Self {
            pane_id,
            mode,
            entries,
            selected,
            query: String::new(),
            search_focused: false,
            progress: None,
            confirm: None,
            confirm_shown_at: None,
            error: None,
            warnings: Vec::new(),
            settled: false,
            job: None,
        }
    }

    /// True once the question has been on screen long enough for an answer
    /// to be one — see [`CONFIRM_ARM_DELAY`].
    fn confirm_armed(&self, now: Instant) -> bool {
        self.confirm_shown_at
            .is_some_and(|shown| now.saturating_duration_since(shown) >= CONFIRM_ARM_DELAY)
    }

    pub(super) fn filtered_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.matches_query(&self.query).then_some(index))
            .collect()
    }

    pub(super) fn selected_entry_index(&self) -> Option<usize> {
        let filtered = self.filtered_indices();
        filtered
            .contains(&self.selected)
            .then_some(self.selected)
            .or_else(|| filtered.first().copied())
    }

    fn selected_entry(&self) -> Option<&AccountEntry> {
        self.selected_entry_index()
            .and_then(|index| self.entries.get(index))
    }

    /// Fold one worker event into the modal. Pure: no I/O, no sends.
    ///
    /// Returns `true` when the launch succeeded outright and the modal should
    /// close. Anything the user has to read — a mismatch, a warning, a
    /// failure — keeps it open instead.
    fn fold(&mut self, event: AccountJobEvent) -> bool {
        match event {
            AccountJobEvent::Progress(phase) => {
                self.progress = Some(phase);
                false
            }
            AccountJobEvent::Confirm(question) => {
                self.progress = None;
                self.confirm = Some(question);
                if let Some(job) = self.job.as_mut() {
                    job.awaiting_confirm = true;
                }
                false
            }
            AccountJobEvent::Finished(AccountJobResult::Launched {
                account,
                state,
                detail,
                warnings,
            }) => {
                self.settle(warnings);
                self.error = grade_message("claude is running", &account, state, detail);
                self.error.is_none() && self.warnings.is_empty()
            }
            AccountJobEvent::Finished(AccountJobResult::Switched {
                account,
                state,
                detail,
                session_id,
                warnings,
            }) => {
                self.settle(warnings);
                self.error = grade_message(
                    &format!("claude resumed conversation {session_id}"),
                    &account,
                    state,
                    detail,
                );
                self.error.is_none() && self.warnings.is_empty()
            }
            AccountJobEvent::Finished(AccountJobResult::Failed {
                message,
                notes,
                refused,
            }) => {
                self.settle(notes);
                // A refusal the worker proved reached nothing is not an
                // outcome: the agent is exactly as it was, so the picker stays
                // usable and the user can change their mind about which
                // profile — or about interrupting — and try again.
                self.settled = !refused;
                self.error = Some(message);
                false
            }
        }
    }

    /// Common tail of every terminal event: the job is over, the question is
    /// gone, and whatever it left to read is on screen.
    fn settle(&mut self, notes: Vec<String>) {
        self.progress = None;
        self.confirm = None;
        self.confirm_shown_at = None;
        self.settled = true;
        self.warnings = notes;
    }
}

/// How a graded account state reads in the modal.
///
/// `Ok` is the only state with nothing to say. A `Mismatch` is a running agent
/// on the wrong account and must be read; anything else means herdr could not
/// check, which is never shown as success.
fn grade_message(
    lead: &str,
    account: &str,
    state: AccountState,
    detail: Option<String>,
) -> Option<String> {
    match state {
        AccountState::Ok => None,
        AccountState::Mismatch => Some(format!(
            "{lead}, but not under account {account:?}: {}",
            detail.unwrap_or_else(|| "its environment disagrees".to_owned())
        )),
        other => Some(format!(
            "{lead} under account {account:?}, but herdr could not confirm it ({})",
            other.as_str()
        )),
    }
}

/// The kind of agent occupying `pane_id`, if any, as the pane menu needs it.
///
/// Any entry in `snapshot.agents` means the pane's terminal is an agent
/// terminal — a managed launch (named, possibly still pending detection) or a
/// `claude` someone started by hand. Either way `agent.start` has nothing to
/// do there, so the item is withheld and the picker refuses. A managed entry
/// with no detected kind yet still counts; the placeholder keeps the answer
/// `Some`.
pub(super) fn pane_agent_kind(snapshot: &ClientShellSnapshot, pane_id: &str) -> Option<String> {
    pane_agent(snapshot, pane_id)
        .map(|agent| agent.agent.clone().unwrap_or_else(|| "agent".to_owned()))
}

/// The `snapshot.agents` entry for `pane_id`, if any.
fn pane_agent<'a>(
    snapshot: &'a ClientShellSnapshot,
    pane_id: &str,
) -> Option<&'a crate::protocol::ClientShellAgent> {
    snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
}

/// The managed Claude agent on `pane_id`, as the switch needs it: its name and
/// the account it currently claims.
///
/// `None` for a pane running something else, or a `claude` nobody named —
/// `agent.start` needs a name to register the relaunch under, and inventing
/// one could collide with an agent elsewhere in the session. The switch
/// protocol refuses both cases too; withholding the menu item is only the
/// first of the two answers.
fn pane_claude_agent(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
) -> Option<(String, Option<String>)> {
    let agent = pane_agent(snapshot, pane_id)?;
    if agent.agent.as_deref() != Some(AGENT_LABEL) {
        return None;
    }
    let name = agent.name.clone()?;
    let account = agent
        .tokens
        .iter()
        .find(|(key, _)| key == ACCOUNT_TOKEN)
        .map(|(_, value)| value.clone());
    Some((name, account))
}

#[cfg(test)]
impl ClientAccountPickerOverlay {
    /// The picker with a launch in flight whose worker never speaks, for
    /// tests of the running state. The returned sender keeps the channel
    /// open; drop it to simulate a worker that died.
    pub(super) fn test_running(
        mut self,
        since: Instant,
    ) -> (Self, std::sync::mpsc::Sender<AccountJobEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.progress = Some("starting claude...");
        self.job = Some(AccountJob {
            handle: None,
            events: rx,
            last_event: since,
            answers: None,
            awaiting_confirm: false,
        });
        (self, tx)
    }
}

/// The context-menu item, when this pane can have one.
///
/// `agent_kind` is `None` when the pane runs no managed agent;
/// `accounts_available` is the number of profiles the client can offer, which
/// the caller has already zeroed for a non-local endpoint. Starting an agent
/// under a profile means typing into a local pane and reading a local
/// `/proc`, so v1 offers it on the local server only.
pub(super) fn start_claude_context_item(
    agent_kind: Option<&str>,
    accounts_available: usize,
) -> Option<ClientContextMenuItem> {
    (agent_kind.is_none() && accounts_available >= 1).then_some(ClientContextMenuItem {
        label: "Start Claude as account...",
        action: ClientContextMenuAction::StartClaudeAs,
    })
}

/// The switch item, when this pane can have one.
///
/// Offered on a pane whose agent herdr recognises as Claude, when the client
/// has at least two profiles to move between — one to leave, one to arrive at.
/// `accounts_available` is already zero on a remote endpoint, so the same
/// field carries the local-only rule the launch has.
///
/// Two facts the switch also needs cannot be read from the shell snapshot,
/// because `ClientShellAgent` carries neither: whether the agent has a managed
/// name, and whether it has reported a Claude session id. The name is checked
/// at activation, and the session id is the switch protocol's own first
/// refusal — which happens before a byte reaches the pane and is shown in the
/// modal. Widening the wire to gate the menu on them would be a protocol
/// change for a menu item, which the fork's endpoint rules forbid.
pub(super) fn switch_account_context_item(
    agent_kind: Option<&str>,
    accounts_available: usize,
) -> Option<ClientContextMenuItem> {
    (agent_kind == Some(AGENT_LABEL) && accounts_available >= 2).then_some(ClientContextMenuItem {
        label: "Switch Claude account...",
        action: ClientContextMenuAction::SwitchClaudeAccount,
    })
}

/// The rows for a set of resolved profiles, worst-to-best order preserved.
///
/// Split from [`ClientShellState::open_account_picker`] so the mapping is
/// testable without a shell; the `inspect` calls are the only I/O.
///
/// `current` is the account the pane's agent already claims, so the switch
/// picker can say where the agent is now.
fn entries_from_profiles(profiles: &Profiles, current: Option<&str>) -> Vec<AccountEntry> {
    profiles
        .iter()
        .map(|profile| {
            let inspection = inspect(profile, InspectOptions::health());
            AccountEntry {
                name: profile.name.clone(),
                config_dir: profile.config_dir.display().to_string(),
                dir_exists: inspection.dir_exists,
                logged_in: inspection.logged_in,
                hook_installed: inspection.hook_installed,
                is_default: profiles
                    .default_profile()
                    .is_some_and(|default| default.name == profile.name),
                is_current: current == Some(profile.name.as_str()),
            }
        })
        .collect()
}

/// A managed agent name no agent in `taken` is using.
///
/// `agent.start` refuses a duplicate name, and the TUI has nowhere to ask for
/// one, so it picks: `claude`, then `claude-2`, `claude-3`… The bound is the
/// number of names already taken plus one, so it always terminates.
fn unique_agent_name(taken: &[&str]) -> String {
    if !taken.contains(&DEFAULT_AGENT_NAME) {
        return DEFAULT_AGENT_NAME.to_owned();
    }
    for suffix in 2..=taken.len().saturating_add(2) {
        let candidate = format!("{DEFAULT_AGENT_NAME}-{suffix}");
        if !taken.iter().any(|name| *name == candidate) {
            return candidate;
        }
    }
    // Unreachable: the loop tries more names than there are taken ones.
    format!("{DEFAULT_AGENT_NAME}-{}", taken.len().saturating_add(2))
}

/// What a submitted pick asks the worker thread to do.
///
/// Built on the shell's thread while every fact is still checked against the
/// live client state, then moved into the worker whole: the thread reads no
/// shell state and does no config I/O, so what runs is exactly what was on
/// screen when Enter was pressed.
enum AccountJobSpawn {
    Start {
        pane_id: String,
        agent_name: String,
        profile: AccountProfile,
    },
    Switch {
        pane_id: String,
        /// The managed name the picker was opened for. The pane is addressed
        /// by id, and this is what the protocol's first read must find there.
        agent_name: String,
        profile: AccountProfile,
        interrupt: bool,
    },
}

/// Move the Claude agent in `pane_id` to `profile`, keeping its conversation.
///
/// The whole protocol is `crate::accounts::switch`'s, driven by
/// `crate::accounts::client`, which is what `herdr agent switch-account`
/// runs — the same refusals, the same `/exit`, the same `--resume`, the same
/// grading. This function contributes exactly two things the CLI does
/// differently: the confirmation is a modal rather than a TTY prompt, and the
/// note a stranded environment line leaves behind goes into the modal instead
/// of stderr, where a TUI would paint over it.
///
/// The pane is addressed by id from the first read onwards, so the switch can
/// only ever reach the pane the user right-clicked; and the protocol's own
/// first read must find `agent_name` there, so it can only ever reach the
/// agent that was in it when they did. The shell checked the same name
/// against its snapshot before spawning this thread, but a snapshot is a
/// moment old, and the read that pins the pane is the one that has to agree.
fn run_switch_job(
    pane_id: String,
    agent_name: String,
    profile: AccountProfile,
    interrupt: bool,
    events: &std::sync::mpsc::Sender<AccountJobEvent>,
    answers: &Receiver<Confirmation>,
) {
    // Read the agent's screen once, before anything is sent, so the
    // confirmation can say *why* the switch is being asked for — the same
    // best-effort read `herdr agent switch-account` makes. An unknown target,
    // a server that will not answer, or a screen that matches nothing all mean
    // "herdr saw no limit"; the preflight below is what actually refuses.
    let limit = crate::cli::account::agent_explain(&pane_id).and_then(|explain| {
        if !crate::accounts::limit::matched_usage_limit(&explain) {
            return None;
        }
        let screen = crate::cli::account::detection_screen(&pane_id).unwrap_or_default();
        crate::accounts::limit::classify(&explain, &screen)
    });

    let input = SwitchInput {
        target: pane_id,
        expected_name: Some(agent_name),
        to: profile.clone(),
        to_inspection: inspect(&profile, InspectOptions::health()),
        limit,
        options: SwitchOptions {
            interrupt,
            // The TUI offers no force: overriding a logged-out target or a
            // no-op switch is a deliberate command-line act.
            force: false,
            timeout_ms: crate::accounts::switch::DEFAULT_TIMEOUT_MS,
        },
    };

    let mut confirm = |question: &str| {
        if events
            .send(AccountJobEvent::Confirm(question.to_owned()))
            .is_err()
        {
            // Nobody is listening, so nobody can answer. The machine turns
            // this into `ConfirmationRequired` and sends nothing.
            return Confirmation::Unavailable;
        }
        // Blocks until the modal answers or is dropped. Nothing has been sent
        // to the pane at this point, so waiting here is free.
        answers.recv().unwrap_or(Confirmation::Unavailable)
    };
    // Notes the relaunch could not print: the shell that now exports the new
    // profile's directory outlives a failed `agent.start`.
    let mut notes: Vec<String> = Vec::new();
    let mut launch = |request: &LaunchRequest| switch_relaunch(&profile, request, &mut notes);
    let mut on_phase = |phase: crate::accounts::client::SwitchPhase| {
        let _ = events.send(AccountJobEvent::Progress(phase.label()));
    };

    let outcome = crate::accounts::client::switch_account_with_progress(
        input,
        &mut confirm,
        &mut launch,
        &mut on_phase,
    );

    let finished = match outcome {
        Ok(outcome) => {
            let result = outcome.result;
            let mut warnings = outcome.warnings;
            warnings.dedup();
            AccountJobResult::Switched {
                account: result.to,
                state: result.account_state,
                detail: result.mismatch_detail,
                session_id: result.session_id,
                warnings,
            }
        }
        Err(failure) => {
            let mut notes = notes;
            notes.extend(failure.warnings.iter().cloned());
            if let Some(hint) = failure.recovery_hint() {
                notes.push(hint);
            }
            // The one refusal with a TUI answer the CLI's message cannot
            // name: `--interrupt` is a flag here, not a switch on the modal.
            if matches!(failure.error, SwitchError::AgentWorking { .. }) {
                notes.push(
                    "press i to allow interrupting it, then pick the account again".to_owned(),
                );
            }
            notes.dedup();
            AccountJobResult::Failed {
                message: failure.to_string(),
                notes,
                // Proven by the protocol: nothing was delivered to the pane,
                // so the agent is exactly as it was.
                refused: !failure.touched_pane,
            }
        }
    };
    let _ = events.send(AccountJobEvent::Finished(finished));
}

/// The relaunch half of a switch, for the TUI.
///
/// The same three steps `herdr agent switch-account` runs — `prepare`,
/// `apply_env`, the stock `agent.start` retry — with one difference: when the
/// start fails, the note about the environment line the pane's shell is left
/// holding is *taken* rather than printed, because an `eprintln!` from under
/// a rendered screen is lost. It is carried back to the modal instead.
fn switch_relaunch(
    profile: &AccountProfile,
    request: &LaunchRequest,
    notes: &mut Vec<String>,
) -> Result<crate::accounts::client::AppliedLine, SwitchError> {
    let launch_error = |detail: String| SwitchError::Launch {
        pane_id: request.pane_id.clone(),
        session_id: request.session_id.clone(),
        detail,
    };
    let plan =
        crate::accounts::client::prepare(profile, &request.pane_id, &request.name, &request.args)
            .map_err(|error| launch_error(error.to_string()))?;
    let applied = crate::accounts::client::apply_env(plan)
        .map_err(|error| launch_error(error.to_string()))?;

    let started = crate::cli::agent::start_managed_agent(
        &request.name,
        AGENT_LABEL,
        AGENT_LABEL,
        &request.pane_id,
        &request.args,
        None,
    );
    let detail = match started {
        Ok(Ok(_response)) => return Ok(applied),
        Ok(Err(crate::cli::agent::AgentStartRefusal::Response(response))) => response["error"]
            ["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| response["error"].to_string()),
        Ok(Err(crate::cli::agent::AgentStartRefusal::Transport(err))) => err.to_string(),
        Err(err) => err.to_string(),
    };
    notes.push(applied.take_note());
    Err(launch_error(detail))
}

/// Run the launch, reporting each phase, and never panic on a closed channel.
///
/// This is the worker body. It is a free function so the thread owns no shell
/// state: the pane id, the agent name and the profile are the whole input,
/// and the events are the whole output. The profile is the one the picker
/// showed — resolved by the client from the same config the CLI reads, and
/// checked against the row the user chose before the thread starts — so what
/// launches is exactly what was on screen, and this thread does no config
/// I/O of its own.
fn run_start_job(
    pane_id: String,
    agent_name: String,
    profile: AccountProfile,
    events: &std::sync::mpsc::Sender<AccountJobEvent>,
) {
    let send = |event: AccountJobEvent| {
        // A closed receiver means the modal is gone or the client is shutting
        // down. Keep going — stopping between `apply_env` and `finish` would
        // leave an agent running with no account recorded.
        let _ = events.send(event);
    };
    let failed = |message: String| {
        AccountJobEvent::Finished(AccountJobResult::Failed {
            message,
            notes: Vec::new(),
            // A start cannot prove the pane was untouched the way the switch
            // protocol can, so its failures always settle the modal.
            refused: false,
        })
    };

    send(AccountJobEvent::Progress("exporting profile..."));
    let plan = match crate::accounts::client::prepare(&profile, &pane_id, &agent_name, &[]) {
        Ok(plan) => plan,
        Err(error) => {
            send(failed(error.to_string()));
            return;
        }
    };
    let warnings = plan.warnings.clone();
    let applied = match crate::accounts::client::apply_env(plan) {
        Ok(applied) => applied,
        Err(error) => {
            send(failed(error.to_string()));
            return;
        }
    };

    send(AccountJobEvent::Progress("starting claude..."));
    let started = crate::cli::agent::start_managed_agent(
        &agent_name,
        AGENT_LABEL,
        AGENT_LABEL,
        &pane_id,
        &[],
        None,
    );
    let refusal = match started {
        Ok(Ok(_response)) => None,
        Ok(Err(crate::cli::agent::AgentStartRefusal::Response(response))) => Some(
            response["error"]["message"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| response["error"].to_string()),
        ),
        Ok(Err(crate::cli::agent::AgentStartRefusal::Transport(err))) => Some(err.to_string()),
        Err(err) => Some(err.to_string()),
    };
    if let Some(message) = refusal {
        // The pane's shell already exports the directory. `take_note` is what
        // disarms the guard's `eprintln!`, which in a TUI would be painted
        // over and lost; the note goes in the modal instead.
        let note = applied.take_note();
        send(AccountJobEvent::Finished(AccountJobResult::Failed {
            message: format!("claude did not start: {message}"),
            notes: vec![note],
            refused: false,
        }));
        return;
    }

    send(AccountJobEvent::Progress("verifying account..."));
    let outcome = applied.finish();
    let detail = (outcome.account_state == AccountState::Mismatch)
        .then(|| outcome.mismatch_detail())
        .or_else(|| outcome.actual_config_dir.clone());
    let mut all_warnings = warnings;
    all_warnings.extend(outcome.warnings.iter().cloned());
    all_warnings.dedup();
    send(AccountJobEvent::Finished(AccountJobResult::Launched {
        account: outcome.account,
        state: outcome.account_state,
        detail,
        warnings: all_warnings,
    }));
}

impl ClientShellState {
    /// How many account profiles this client can offer for `endpoint`.
    ///
    /// Zero on a remote endpoint: the launch types into a pane and then reads
    /// the launched process's environment, and both are local operations.
    pub(super) fn accounts_available_for(&self, endpoint_id: &ClientEndpointId) -> usize {
        if !endpoint_id.is_local() {
            return 0;
        }
        self.config.accounts.len()
    }

    /// Open the picker on the pane the context menu was opened on.
    pub(super) fn open_account_picker(&mut self, pane_id: String, outcome: &mut ClientShellInput) {
        if !self.active_endpoint_id.is_local() {
            return;
        }
        let entries = entries_from_profiles(&self.config.accounts, None);
        if entries.is_empty() {
            return;
        }
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        if !snapshot.panes.iter().any(|pane| pane.pane_id == pane_id) {
            return;
        }
        if pane_agent_kind(snapshot, &pane_id).is_some() {
            return;
        }
        let taken = snapshot
            .agents
            .iter()
            .filter_map(|agent| agent.name.as_deref())
            .collect::<Vec<_>>();
        let agent_name = unique_agent_name(&taken);
        let selected = entries
            .iter()
            .position(|entry| entry.is_default)
            .unwrap_or(0);
        self.overlay = Some(ClientShellOverlay::AccountPicker(
            ClientAccountPickerOverlay::idle(
                pane_id,
                PickerMode::Start { agent_name },
                entries,
                selected,
            ),
        ));
        outcome.repaint = true;
    }

    /// Open the picker to move the Claude agent in this pane to another
    /// profile.
    ///
    /// The pane is the one the context menu was opened on, and the agent is
    /// whichever managed Claude holds it *now* — read here rather than carried
    /// on the menu target, so the action cannot be aimed at an agent that has
    /// since gone. Two profiles are required: one to leave, one to arrive at.
    pub(super) fn open_account_switch_picker(
        &mut self,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        if !self.active_endpoint_id.is_local() {
            return;
        }
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        if !snapshot.panes.iter().any(|pane| pane.pane_id == pane_id) {
            return;
        }
        let Some((agent_name, current)) = pane_claude_agent(snapshot, &pane_id) else {
            return;
        };
        let entries = entries_from_profiles(&self.config.accounts, current.as_deref());
        if entries.len() < 2 {
            return;
        }
        // Land on somewhere worth going: not where the agent already is, and
        // not a profile herdr already knows it cannot launch under.
        let selected = entries
            .iter()
            .position(|entry| !entry.is_current && !entry.unusable())
            .or_else(|| entries.iter().position(|entry| !entry.is_current))
            .unwrap_or(0);
        self.overlay = Some(ClientShellOverlay::AccountPicker(
            ClientAccountPickerOverlay::idle(
                pane_id,
                PickerMode::Switch {
                    agent_name,
                    current,
                    interrupt: false,
                },
                entries,
                selected,
            ),
        ));
        outcome.repaint = true;
    }

    /// Flip whether a switch may interrupt a working agent.
    ///
    /// Only meaningful in [`PickerMode::Switch`], and only before the job
    /// starts: the machine's preflight reads it once, and an agent that is
    /// working refuses the switch outright unless it is set.
    fn toggle_account_interrupt(&mut self) -> bool {
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() else {
            return false;
        };
        if picker.running() || picker.settled {
            return false;
        }
        let PickerMode::Switch { interrupt, .. } = &mut picker.mode else {
            return false;
        };
        *interrupt = !*interrupt;
        true
    }

    fn move_account_picker_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let filtered = picker.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let current = filtered
            .iter()
            .position(|index| *index == picker.selected)
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, filtered.len().saturating_sub(1) as isize) as usize;
        picker.selected = filtered[next];
        // A refusal was about the row that was highlighted; moving off it is
        // the user's answer to it.
        if !picker.settled {
            picker.error = None;
        }
    }

    /// Why the highlighted row cannot be launched right now, or the profile
    /// to launch it under.
    ///
    /// Every check here happens before a byte reaches any pane. The pinned
    /// pane is re-checked against the live snapshot because a modal can be up
    /// for minutes and the pane may have closed or grown an agent; the
    /// endpoint is re-checked because a pending activation can complete under
    /// the modal, after which `self.snapshot` describes *another server's*
    /// panes — whose ids collide with local ones — while the worker would
    /// still type into the local pane of that id. The profile is re-read from
    /// the client's config, which reloads live, and must still describe the
    /// row the user chose: a name that now points somewhere else is not the
    /// account they picked.
    fn account_launch_preflight(
        &self,
        picker: &ClientAccountPickerOverlay,
        entry: &AccountEntry,
    ) -> Result<AccountProfile, String> {
        let pane_id = &picker.pane_id;
        let snapshot = self.account_pane_still_there(pane_id)?;
        if pane_agent_kind(snapshot, pane_id).is_some() {
            return Err(format!("pane {pane_id} already runs an agent"));
        }
        self.account_profile_unchanged(entry)
    }

    /// The same checks for a switch: the pane must still be the local pane the
    /// menu was opened on, and it must still hold the very agent the picker
    /// was opened for — same managed name, still recognised as Claude.
    ///
    /// An agent that exited, was renamed, or was replaced by another one is
    /// refused here rather than switched: the whole action is "move *that*
    /// agent", and the pane id alone stops meaning that the moment the agent
    /// behind it changes. The protocol pins the pane again from its own first
    /// read and re-checks it after the confirmation; this is the check that
    /// happens before a thread is even spawned.
    fn account_switch_preflight(
        &self,
        picker: &ClientAccountPickerOverlay,
        entry: &AccountEntry,
        agent_name: &str,
    ) -> Result<AccountProfile, String> {
        let pane_id = &picker.pane_id;
        let snapshot = self.account_pane_still_there(pane_id)?;
        match pane_claude_agent(snapshot, pane_id) {
            Some((name, _)) if name == agent_name => {}
            Some((name, _)) => {
                return Err(format!(
                    "pane {pane_id} now runs agent {name:?}, not {agent_name:?}; close this and \
                     reopen the menu"
                ))
            }
            None => {
                return Err(format!(
                    "agent {agent_name:?} is no longer running in pane {pane_id}"
                ))
            }
        }
        if entry.is_current {
            return Err(format!(
                "agent {agent_name:?} already runs under account {:?}",
                entry.name
            ));
        }
        self.account_profile_unchanged(entry)
    }

    /// The endpoint and the pane, re-read from the live client state.
    ///
    /// The endpoint is re-checked because a pending activation can complete
    /// under the modal, after which `self.snapshot` describes *another
    /// server's* panes — whose ids collide with local ones — while the worker
    /// would still address the local pane of that id.
    fn account_pane_still_there(&self, pane_id: &str) -> Result<&ClientShellSnapshot, String> {
        if !self.active_endpoint_id.is_local() {
            return Err("herdr is no longer attached to the local server".to_owned());
        }
        let Some(snapshot) = self.snapshot.as_deref() else {
            return Err("herdr has no snapshot of this server yet".to_owned());
        };
        if !snapshot.panes.iter().any(|pane| pane.pane_id == pane_id) {
            return Err(format!("pane {pane_id} is gone"));
        }
        Ok(snapshot)
    }

    /// The profile is re-read from the client's config, which reloads live,
    /// and must still describe the row the user chose: a name that now points
    /// somewhere else is not the account they picked.
    fn account_profile_unchanged(&self, entry: &AccountEntry) -> Result<AccountProfile, String> {
        let account = &entry.name;
        let Some(profile) = self.config.accounts.get(account) else {
            return Err(format!(
                "account {account:?} is no longer configured; close this and reopen the menu"
            ));
        };
        if profile.config_dir.display().to_string() != entry.config_dir {
            return Err(format!(
                "account {account:?} changed since this opened; close this and reopen the menu"
            ));
        }
        if !entry.dir_exists {
            return Err(format!(
                "account {account:?} has no config directory; run `herdr account add {account}`"
            ));
        }
        Ok(profile.clone())
    }

    /// Start the launch for the highlighted profile.
    ///
    /// Nothing happens unless [`Self::account_launch_preflight`] agrees; a
    /// refusal is shown in the modal and leaves it usable, since nothing was
    /// typed anywhere.
    fn submit_account_picker(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_ref() else {
            return;
        };
        if picker.running() || picker.settled {
            return;
        }
        let Some(entry) = picker.selected_entry() else {
            return;
        };
        let pane_id = picker.pane_id.clone();

        // Every refusal below happens before a thread exists, so nothing can
        // have reached any pane.
        let (spawn, first_phase) = match picker.mode.clone() {
            PickerMode::Start { agent_name } => {
                let profile = match self.account_launch_preflight(picker, entry) {
                    Ok(profile) => profile,
                    Err(message) => return self.refuse_account_picker(message, outcome),
                };
                (
                    AccountJobSpawn::Start {
                        pane_id,
                        agent_name,
                        profile,
                    },
                    "exporting profile...",
                )
            }
            PickerMode::Switch {
                agent_name,
                interrupt,
                ..
            } => {
                let profile = match self.account_switch_preflight(picker, entry, &agent_name) {
                    Ok(profile) => profile,
                    Err(message) => return self.refuse_account_picker(message, outcome),
                };
                (
                    AccountJobSpawn::Switch {
                        pane_id,
                        agent_name,
                        profile,
                        interrupt,
                    },
                    crate::accounts::client::SwitchPhase::Preflight.label(),
                )
            }
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("herdr-account-job".to_owned())
            .spawn(move || match spawn {
                AccountJobSpawn::Start {
                    pane_id,
                    agent_name,
                    profile,
                } => run_start_job(pane_id, agent_name, profile, &tx),
                AccountJobSpawn::Switch {
                    pane_id,
                    agent_name,
                    profile,
                    interrupt,
                } => run_switch_job(pane_id, agent_name, profile, interrupt, &tx, &answer_rx),
            });
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        match handle {
            Ok(handle) => {
                picker.error = None;
                picker.warnings.clear();
                picker.progress = Some(first_phase);
                picker.job = Some(AccountJob {
                    handle: Some(handle),
                    events: rx,
                    last_event: Instant::now(),
                    answers: Some(answer_tx),
                    awaiting_confirm: false,
                });
            }
            Err(err) => picker.error = Some(format!("could not start the account job: {err}")),
        }
        outcome.repaint = true;
    }

    /// Show a refusal that reached nothing, and leave the picker usable.
    fn refuse_account_picker(&mut self, message: String, outcome: &mut ClientShellInput) {
        if let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() {
            picker.error = Some(message);
            picker.warnings.clear();
        }
        outcome.repaint = true;
    }

    /// Answer the switch's confirmation.
    ///
    /// The only place `Observation::Confirmed` can come from in the TUI. `yes`
    /// releases the worker into the protocol; `no` makes it fail with
    /// `Declined`, having sent nothing.
    fn answer_account_confirm(&mut self, yes: bool, outcome: &mut ClientShellInput) {
        self.answer_account_confirm_at(yes, Instant::now(), outcome);
    }

    /// [`Self::answer_account_confirm`] at a given instant, so the arming
    /// delay is testable without waiting it out.
    fn answer_account_confirm_at(
        &mut self,
        yes: bool,
        now: Instant,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        if picker.confirm.is_none() {
            return;
        }
        if yes && !picker.confirm_armed(now) {
            // The tail of the keystroke or click that opened the question,
            // not an answer to it. The question stays up.
            return;
        }
        let delivered = picker.job.as_mut().is_some_and(|job| {
            job.answer(
                if yes {
                    Confirmation::Yes
                } else {
                    Confirmation::No
                },
                now,
            )
        });
        picker.confirm = None;
        picker.confirm_shown_at = None;
        // The worker's own next phase overwrites this as soon as it moves; the
        // line is here so the modal never shows an empty body between the
        // answer and the next event.
        picker.progress = Some(if yes {
            crate::accounts::client::SwitchPhase::Recheck.label()
        } else {
            "cancelling..."
        });
        if !delivered {
            // The worker is gone. Its channel is closed too, so the next tick
            // reports that rather than this call inventing an outcome.
            tracing::debug!(
                "the account switch worker was gone when the confirmation was answered"
            );
        }
        outcome.repaint = true;
    }

    /// Fold whatever the worker has reported since the last tick.
    ///
    /// Called once per client timer tick (≤ 100 ms). Costs one `try_recv` when
    /// no picker is open, and nothing at all per pane or per render.
    pub(crate) fn tick_account_picker(&mut self) -> bool {
        self.tick_account_picker_at(Instant::now())
    }

    /// [`Self::tick_account_picker`] at a given instant, so the stall limit is
    /// testable without waiting it out.
    fn tick_account_picker_at(&mut self, now: Instant) -> bool {
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() else {
            return false;
        };
        let Some(job) = picker.job.as_mut() else {
            return false;
        };
        let (mut events, mut finished) = job.drain(now);
        if !finished && events.is_empty() && job.stalled(now) {
            // Detach rather than join: a thread that is stuck in a socket
            // read would take the event loop down with it.
            job.detach();
            events.push(AccountJobEvent::Finished(AccountJobResult::Failed {
                message: format!(
                    "no answer from the server for {}s; the job may still finish on its \
                     own — check `herdr agent list` before starting another",
                    ACCOUNT_JOB_STALL_LIMIT.as_secs()
                ),
                notes: Vec::new(),
                refused: false,
            }));
            finished = true;
        }
        if events.is_empty() && !finished {
            return false;
        }
        let mut close = false;
        for event in events {
            let asks = matches!(event, AccountJobEvent::Confirm(_));
            close |= picker.fold(event);
            if asks {
                picker.confirm_shown_at = Some(now);
            }
        }
        if finished {
            if let Some(mut job) = picker.job.take() {
                job.join();
            }
        }
        if close && finished {
            self.overlay = None;
        }
        true
    }

    /// Keys for the picker. `true` means the key was ours.
    pub(super) fn route_account_picker_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_ref() else {
            return false;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        // A launch in flight owns the pane; dismissing the modal would hide
        // the only place its outcome is reported.
        let running = picker.running();
        let search_focused = picker.search_focused;
        let settled = picker.settled;
        let switch = picker.mode.is_switch();

        // The confirmation owns every key while it is up: there are exactly
        // two answers, and a keystroke that fell through to the list under a
        // question about stopping somebody's agent would be a surprise.
        if picker.awaiting_confirm() {
            match code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.answer_account_confirm(true, outcome)
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.answer_account_confirm(false, outcome)
                }
                _ => {}
            }
            return true;
        }

        match code {
            KeyCode::Esc if !running => {
                self.overlay = None;
                outcome.repaint = true;
            }
            // Once a launch has ended, Enter acknowledges its outcome rather
            // than launching a second agent into the same pane.
            KeyCode::Enter if !running && settled => {
                self.overlay = None;
                outcome.repaint = true;
            }
            KeyCode::Enter if !running => self.submit_account_picker(outcome),
            KeyCode::Up if !running => {
                self.move_account_picker_selection(-1);
                outcome.repaint = true;
            }
            KeyCode::Down if !running => {
                self.move_account_picker_selection(1);
                outcome.repaint = true;
            }
            // Only the switch has it, and only before the job starts.
            KeyCode::Char('i') | KeyCode::Char('I')
                if switch && !running && !settled && !search_focused =>
            {
                if self.toggle_account_interrupt() {
                    outcome.repaint = true;
                }
            }
            KeyCode::Char('/') if !running && !search_focused => {
                if let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() {
                    picker.search_focused = true;
                }
                outcome.repaint = true;
            }
            KeyCode::Backspace if !running && search_focused => {
                if let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() {
                    picker.query.pop();
                    if let Some(first) = picker.filtered_indices().first().copied() {
                        picker.selected = first;
                    }
                }
                outcome.repaint = true;
            }
            KeyCode::Char(character)
                if !running
                    && search_focused
                    && modifiers
                        .difference(crossterm::event::KeyModifiers::SHIFT)
                        .is_empty() =>
            {
                let text = key
                    .generated_text
                    .clone()
                    .unwrap_or_else(|| character.to_string());
                if let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() {
                    picker.query.push_str(&text);
                    if let Some(first) = picker.filtered_indices().first().copied() {
                        picker.selected = first;
                    }
                }
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }

    /// Mouse for the picker. `true` means the event was ours.
    ///
    /// Herdr is mouse-first and this modal is reached by right-clicking a
    /// pane, so the hand is already on the mouse when it opens.
    pub(super) fn route_account_picker_mouse(
        &mut self,
        kind: crossterm::event::MouseEventKind,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};

        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_ref() else {
            return false;
        };
        let running = picker.running();
        let settled = picker.settled;

        // Same rule as the keyboard: while the question is up, the two
        // buttons are the only things on the modal that do anything.
        if picker.awaiting_confirm() {
            if let MouseEventKind::Down(MouseButton::Left) = kind {
                if super::contains(self.hits.overlay_primary, point) {
                    self.answer_account_confirm(true, outcome);
                } else if super::contains(self.hits.overlay_cancel, point) {
                    self.answer_account_confirm(false, outcome);
                }
            }
            return true;
        }

        match kind {
            MouseEventKind::ScrollUp if !running => {
                self.move_account_picker_selection(-1);
                outcome.repaint = true;
            }
            MouseEventKind::ScrollDown if !running => {
                self.move_account_picker_selection(1);
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) if !running => {
                if super::contains(self.hits.overlay_cancel, point) {
                    self.overlay = None;
                    outcome.repaint = true;
                } else if super::contains(self.hits.worktree_search, point) {
                    if let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() {
                        picker.search_focused = true;
                    }
                    outcome.repaint = true;
                } else if let Some((_, index)) = self
                    .hits
                    .worktree_rows
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .copied()
                {
                    if settled {
                        // A result is on screen; a click on a row is not a
                        // second launch.
                        return true;
                    }
                    if let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() {
                        picker.selected = index;
                    }
                    self.submit_account_picker(outcome);
                } else if super::contains(self.hits.overlay_primary, point) {
                    if settled {
                        self.overlay = None;
                    } else {
                        self.submit_account_picker(outcome);
                    }
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str, is_default: bool) -> AccountEntry {
        AccountEntry {
            name: name.to_owned(),
            config_dir: format!("/tmp/profiles/{name}"),
            dir_exists: true,
            logged_in: true,
            hook_installed: true,
            is_default,
            is_current: false,
        }
    }

    fn picker(entries: Vec<AccountEntry>) -> ClientAccountPickerOverlay {
        ClientAccountPickerOverlay::idle(
            "p1".to_owned(),
            PickerMode::Start {
                agent_name: "claude".to_owned(),
            },
            entries,
            0,
        )
    }

    fn agent_on(
        pane_id: &str,
        name: Option<&str>,
        kind: Option<&str>,
    ) -> crate::protocol::ClientShellAgent {
        crate::protocol::ClientShellAgent {
            pane_id: pane_id.into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: name.map(Into::into),
            display_agent: kind.map(Into::into),
            agent: kind.map(Into::into),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        }
    }

    #[test]
    fn the_menu_item_needs_a_free_pane_a_profile_and_a_local_endpoint() {
        // A pane with no agent and one profile is the only case that offers it.
        assert!(start_claude_context_item(None, 1).is_some());
        assert!(start_claude_context_item(None, 4).is_some());
        // An agent already runs here.
        assert!(start_claude_context_item(Some("claude"), 2).is_none());
        assert!(start_claude_context_item(Some("codex"), 2).is_none());
        // No profiles configured — or a remote endpoint, which the caller
        // reports as zero.
        assert!(start_claude_context_item(None, 0).is_none());
    }

    #[test]
    fn the_item_action_is_the_one_the_activation_arm_matches() {
        let item = start_claude_context_item(None, 1).expect("item");
        assert_eq!(item.action, ClientContextMenuAction::StartClaudeAs);
        assert_eq!(item.label, "Start Claude as account...");
    }

    #[test]
    fn agent_names_avoid_the_names_already_running() {
        assert_eq!(unique_agent_name(&[]), "claude");
        assert_eq!(unique_agent_name(&["codex"]), "claude");
        assert_eq!(unique_agent_name(&["claude"]), "claude-2");
        assert_eq!(unique_agent_name(&["claude", "claude-2"]), "claude-3");
        // Gaps are filled, not skipped.
        assert_eq!(unique_agent_name(&["claude", "claude-3"]), "claude-2");
    }

    #[test]
    fn agent_name_generation_terminates_on_a_dense_run() {
        let taken = (1..=64)
            .map(|index| {
                if index == 1 {
                    "claude".to_owned()
                } else {
                    format!("claude-{index}")
                }
            })
            .collect::<Vec<_>>();
        let borrowed = taken.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(unique_agent_name(&borrowed), "claude-65");
    }

    #[test]
    fn filtering_keeps_the_selection_addressable() {
        let mut overlay = picker(vec![entry("perso", true), entry("work", false)]);
        assert_eq!(overlay.filtered_indices(), vec![0, 1]);
        overlay.query = "wor".to_owned();
        assert_eq!(overlay.filtered_indices(), vec![1]);
        // The selection is out of the filter, so the first match answers.
        assert_eq!(overlay.selected_entry_index(), Some(1));
        overlay.query = "nothing".to_owned();
        assert_eq!(overlay.selected_entry_index(), None);
    }

    #[test]
    fn a_query_matches_the_directory_and_the_status_word() {
        let mut missing = entry("work", false);
        missing.dir_exists = false;
        assert_eq!(missing.status_text(), "missing");
        assert!(missing.matches_query("MISS"));
        assert!(missing.matches_query("/tmp/profiles"));
        assert!(!missing.matches_query("perso"));
    }

    #[test]
    fn a_row_shows_the_default_and_the_worst_problem_together() {
        let mut candidate = entry("work", true);
        assert_eq!(candidate.status_text(), "default");
        assert!(!candidate.unusable());
        candidate.hook_installed = false;
        assert_eq!(candidate.status_text(), "default · no hook");
        assert!(
            !candidate.unusable(),
            "a missing hook costs the switch, not the launch"
        );
        candidate.logged_in = false;
        assert_eq!(candidate.status_text(), "default · logged out");
        assert!(candidate.unusable());
        candidate.dir_exists = false;
        assert_eq!(candidate.status_text(), "default · missing");

        let mut other = entry("perso", false);
        assert_eq!(other.status_text(), "");
        other.hook_installed = false;
        assert_eq!(other.status_text(), "no hook");
    }

    #[test]
    fn a_clean_launch_closes_the_modal_and_a_mismatch_does_not() {
        let mut overlay = picker(vec![entry("work", false)]);
        assert!(!overlay.fold(AccountJobEvent::Progress("starting claude...")));
        assert_eq!(overlay.progress, Some("starting claude..."));
        assert!(
            overlay.fold(AccountJobEvent::Finished(AccountJobResult::Launched {
                account: "work".to_owned(),
                state: AccountState::Ok,
                detail: None,
                warnings: Vec::new(),
            }))
        );
        assert_eq!(overlay.progress, None);
        assert_eq!(overlay.error, None);

        let mut overlay = picker(vec![entry("work", false)]);
        assert!(
            !overlay.fold(AccountJobEvent::Finished(AccountJobResult::Launched {
                account: "work".to_owned(),
                state: AccountState::Mismatch,
                detail: Some("it names /home/u/.claude".to_owned()),
                warnings: Vec::new(),
            }))
        );
        let error = overlay.error.expect("a mismatch must be shown");
        assert!(error.contains("not under account"), "{error}");
        assert!(error.contains("/home/u/.claude"), "{error}");
    }

    #[test]
    fn an_unverified_launch_stays_on_screen_rather_than_claiming_the_account() {
        let mut overlay = picker(vec![entry("work", false)]);
        assert!(
            !overlay.fold(AccountJobEvent::Finished(AccountJobResult::Launched {
                account: "work".to_owned(),
                state: AccountState::Unverified,
                detail: None,
                warnings: Vec::new(),
            }))
        );
        let error = overlay.error.expect("unverified must be shown");
        assert!(error.contains("could not confirm"), "{error}");
    }

    #[test]
    fn a_successful_launch_with_a_warning_keeps_the_warning_on_screen() {
        let mut overlay = picker(vec![entry("work", false)]);
        assert!(
            !overlay.fold(AccountJobEvent::Finished(AccountJobResult::Launched {
                account: "work".to_owned(),
                state: AccountState::Ok,
                detail: None,
                warnings: vec!["the profile has no herdr hook".to_owned()],
            }))
        );
        assert_eq!(overlay.error, None);
        assert_eq!(overlay.warnings.len(), 1);
    }

    #[test]
    fn a_failure_that_left_the_line_in_the_pane_reports_it() {
        let mut overlay = picker(vec![entry("work", false)]);
        assert!(
            !overlay.fold(AccountJobEvent::Finished(AccountJobResult::Failed {
                message: "claude did not start: agent_pane_busy".to_owned(),
                notes: vec!["CLAUDE_CONFIG_DIR was already exported in pane p1".to_owned()],
                refused: false,
            }))
        );
        assert!(overlay.error.is_some());
        assert_eq!(overlay.warnings.len(), 1);
        assert!(overlay.warnings[0].contains("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn a_worker_that_dies_without_a_result_is_reported_not_awaited() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(AccountJobEvent::Progress("exporting profile..."))
            .expect("send");
        drop(tx);
        let mut job = AccountJob {
            handle: None,
            events: rx,
            last_event: Instant::now(),
            answers: None,
            awaiting_confirm: false,
        };
        let (events, finished) = job.drain(Instant::now());
        assert!(finished);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1],
            AccountJobEvent::Finished(AccountJobResult::Failed { .. })
        ));
    }

    #[test]
    fn draining_stops_at_the_first_terminal_event() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(AccountJobEvent::Progress("one")).expect("send");
        tx.send(AccountJobEvent::Finished(AccountJobResult::Failed {
            message: "no".to_owned(),
            notes: Vec::new(),
            refused: false,
        }))
        .expect("send");
        tx.send(AccountJobEvent::Progress("two")).expect("send");
        let mut job = AccountJob {
            handle: None,
            events: rx,
            last_event: Instant::now(),
            answers: None,
            awaiting_confirm: false,
        };
        let (events, finished) = job.drain(Instant::now());
        assert!(finished);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_launch_outcome_settles_the_picker_but_a_refusal_does_not() {
        let mut overlay = picker(vec![entry("work", false)]);
        assert!(!overlay.settled);
        overlay.fold(AccountJobEvent::Progress("starting claude..."));
        assert!(!overlay.settled, "progress is not an outcome");
        overlay.fold(AccountJobEvent::Finished(AccountJobResult::Failed {
            message: "no".to_owned(),
            notes: Vec::new(),
            refused: false,
        }));
        assert!(overlay.settled);

        let mut overlay = picker(vec![entry("work", false)]);
        overlay.fold(AccountJobEvent::Finished(AccountJobResult::Launched {
            account: "work".to_owned(),
            state: AccountState::Ok,
            detail: None,
            warnings: Vec::new(),
        }));
        assert!(overlay.settled, "a clean launch is an outcome too");
    }

    // ---- the shell wiring: the item, and the pane the action reaches ----

    fn profiles(names: &[(&str, bool)], root: &std::path::Path) -> Profiles {
        Profiles::test_new(
            names
                .iter()
                .map(|(name, default)| AccountProfile {
                    name: (*name).to_owned(),
                    agent: crate::accounts::profile::AccountAgent::Claude,
                    config_dir: root.join(name),
                    default: *default,
                    origin: crate::accounts::profile::ProfileOrigin::Config,
                })
                .collect(),
        )
    }

    /// A snapshot with a second pane, so "the pane the menu was opened on" is
    /// a claim a test can actually falsify.
    fn two_pane_snapshot() -> ClientShellSnapshot {
        let mut snapshot = super::super::tests::snapshot();
        let first = snapshot.panes[0].clone();
        snapshot.panes.push(crate::protocol::ClientShellPane {
            pane_id: "pane_2".into(),
            focused: false,
            ..first
        });
        snapshot
    }

    fn shell_with(profiles: Profiles, snapshot: ClientShellSnapshot) -> ClientShellState {
        let mut config = ClientShellConfig::from_config(&crate::config::Config::default());
        config.accounts = profiles;
        let mut state = ClientShellState::new(config);
        state.set_snapshot(Box::new(snapshot));
        state
    }

    fn menu_has_account_item(state: &ClientShellState) -> bool {
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::ContextMenu(menu)) => menu
                .items()
                .iter()
                .any(|item| item.action == ClientContextMenuAction::StartClaudeAs),
            _ => panic!("expected a context menu"),
        }
    }

    #[test]
    fn the_pane_menu_offers_the_account_item_only_when_a_profile_exists() {
        let root = std::env::temp_dir();
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        assert!(
            !menu_has_account_item(&state),
            "no profiles configured, so nothing to pick"
        );

        state.overlay = None;
        state.config.accounts = profiles(&[("work", true)], &root);
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        assert!(menu_has_account_item(&state));
    }

    #[test]
    fn a_pane_that_already_runs_an_agent_gets_no_account_item() {
        let root = std::env::temp_dir();
        let mut snapshot = two_pane_snapshot();
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            pane_id: "pane_2".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some("claude".into()),
            display_agent: Some("claude".into()),
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        });
        let mut state = shell_with(profiles(&[("work", true)], &root), snapshot);
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        assert!(!menu_has_account_item(&state));
        // The pane next door is still free, so it still offers the item.
        state.overlay = None;
        state.open_pane_context_menu("pane_1".to_owned(), 0, 0);
        assert!(menu_has_account_item(&state));
    }

    #[test]
    fn a_remote_endpoint_offers_no_accounts() {
        let root = std::env::temp_dir();
        let state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        assert_eq!(state.accounts_available_for(&ClientEndpointId::Local), 1);
        assert_eq!(
            state.accounts_available_for(&ClientEndpointId::Ssh(
                crate::client::endpoint::ProfileId::generate()
            )),
            0,
            "v1 types into a local pane and reads a local /proc"
        );
    }

    #[test]
    fn activating_the_item_opens_the_picker_on_the_pane_it_was_opened_on() {
        let root = std::env::temp_dir();
        let mut state = shell_with(
            profiles(&[("perso", false), ("work", true)], &root),
            two_pane_snapshot(),
        );
        // The focused pane is `pane_1`; the menu is opened on the other one.
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        let index = match state.overlay.as_ref() {
            Some(ClientShellOverlay::ContextMenu(menu)) => menu
                .items()
                .iter()
                .position(|item| item.action == ClientContextMenuAction::StartClaudeAs)
                .expect("account item"),
            _ => panic!("expected a context menu"),
        };
        let mut outcome = ClientShellInput::default();
        state.activate_context_menu_item(index, &mut outcome);
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert_eq!(picker.pane_id, "pane_2");
                assert_eq!(picker.entries.len(), 2);
                // The default profile is preselected, and it is not the first.
                assert_eq!(
                    picker.selected_entry().map(|entry| entry.name.as_str()),
                    Some("work")
                );
                assert!(matches!(
                    &picker.mode,
                    PickerMode::Start { agent_name } if agent_name == "claude"
                ));
            }
            other => panic!("expected the account picker, got {other:?}"),
        }
    }

    #[test]
    fn the_picker_refuses_a_pane_that_vanished_while_it_was_open() {
        let root = std::env::temp_dir();
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        assert!(matches!(
            state.overlay,
            Some(ClientShellOverlay::AccountPicker(_))
        ));

        let mut snapshot = two_pane_snapshot();
        snapshot.panes.retain(|pane| pane.pane_id != "pane_2");
        state.set_snapshot(Box::new(snapshot));
        state.submit_account_picker(&mut ClientShellInput::default());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(
                    !picker.running(),
                    "nothing may be launched into a dead pane"
                );
                assert_eq!(picker.error.as_deref(), Some("pane pane_2 is gone"));
            }
            other => panic!("expected the picker to stay open, got {other:?}"),
        }
    }

    #[test]
    fn the_picker_refuses_a_pane_that_grew_an_agent_while_it_was_open() {
        let root = std::env::temp_dir();
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());

        let mut snapshot = two_pane_snapshot();
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            pane_id: "pane_2".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some("claude".into()),
            display_agent: Some("claude".into()),
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
        });
        state.set_snapshot(Box::new(snapshot));
        state.submit_account_picker(&mut ClientShellInput::default());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(!picker.running());
                assert_eq!(
                    picker.error.as_deref(),
                    Some("pane pane_2 already runs an agent")
                );
            }
            other => panic!("expected the picker to stay open, got {other:?}"),
        }
    }

    #[test]
    fn the_picker_refuses_a_profile_whose_directory_is_missing() {
        let root = std::env::temp_dir().join("herdr-account-overlay-absent");
        let _ = std::fs::remove_dir_all(&root);
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        state.submit_account_picker(&mut ClientShellInput::default());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(!picker.running());
                let error = picker.error.as_deref().expect("a refusal");
                assert!(error.contains("no config directory"), "{error}");
            }
            other => panic!("expected the picker to stay open, got {other:?}"),
        }
    }

    #[test]
    fn a_picker_with_no_profiles_never_opens() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        assert!(state.overlay.is_none());
    }

    #[test]
    fn ticking_without_a_picker_is_free_and_reports_no_repaint() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        assert!(!state.tick_account_picker());
    }

    #[test]
    fn entries_carry_the_default_and_the_health_of_each_profile() {
        let root = std::env::temp_dir().join(format!(
            "herdr-account-overlay-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        let present = root.join("perso");
        std::fs::create_dir_all(&present).expect("profile dir");
        std::fs::write(present.join(".credentials.json"), "{}").expect("credentials");

        let profile = |name: &str, dir: PathBuf, default: bool| AccountProfile {
            name: name.to_owned(),
            agent: crate::accounts::profile::AccountAgent::Claude,
            config_dir: dir,
            default,
            origin: crate::accounts::profile::ProfileOrigin::Config,
        };
        let profiles = Profiles::test_new(vec![
            profile("perso", present.clone(), true),
            profile("work", root.join("work"), false),
        ]);
        let entries = entries_from_profiles(&profiles, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "perso");
        assert!(entries[0].is_default);
        assert!(entries[0].dir_exists);
        assert!(entries[0].logged_in);
        assert!(!entries[0].hook_installed);
        assert_eq!(entries[1].status_text(), "missing");
        assert!(!entries[1].dir_exists);
        assert_eq!(
            entries[1].config_dir,
            root.join("work").display().to_string()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- the refusals that keep a launch off the wrong pane or server ----

    /// A profile directory that exists, so the preflight gets past the
    /// `dir_exists` gate and the test reaches the check it is about.
    fn present_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-account-overlay-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(root.join("work")).expect("profile dir");
        root
    }

    fn picker_error(state: &ClientShellState) -> Option<String> {
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(!picker.running(), "nothing may have been launched");
                picker.error.clone()
            }
            other => panic!("expected the picker to stay open, got {other:?}"),
        }
    }

    #[test]
    fn a_pane_occupied_by_an_unmanaged_agent_gets_no_item_and_is_refused() {
        let root = present_root("unmanaged");
        let mut snapshot = two_pane_snapshot();
        // A `claude` someone typed by hand: detected, never named.
        snapshot
            .agents
            .push(agent_on("pane_2", None, Some("claude")));
        assert_eq!(
            pane_agent_kind(&snapshot, "pane_2").as_deref(),
            Some("claude")
        );
        assert_eq!(pane_agent_kind(&snapshot, "pane_1"), None);

        let mut state = shell_with(profiles(&[("work", true)], &root), snapshot.clone());
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        assert!(!menu_has_account_item(&state));

        // Opened on a free pane, then the agent appears under the modal.
        state.overlay = None;
        state.set_snapshot(Box::new(two_pane_snapshot()));
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        state.set_snapshot(Box::new(snapshot));
        state.submit_account_picker(&mut ClientShellInput::default());
        assert_eq!(
            picker_error(&state).as_deref(),
            Some("pane pane_2 already runs an agent")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_managed_agent_without_a_detected_kind_still_occupies_its_pane() {
        let mut snapshot = two_pane_snapshot();
        // Named by `agent.start`, not yet recognised on screen.
        snapshot
            .agents
            .push(agent_on("pane_2", Some("claude"), None));
        assert_eq!(
            pane_agent_kind(&snapshot, "pane_2").as_deref(),
            Some("agent"),
            "the item must not be offered on a pane a launch is pending in"
        );
        assert!(
            start_claude_context_item(pane_agent_kind(&snapshot, "pane_2").as_deref(), 1).is_none()
        );
    }

    #[test]
    fn the_picker_refuses_once_the_client_is_attached_elsewhere() {
        let root = present_root("endpoint");
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        // A pending activation completes under the modal: the snapshot is now
        // another server's, whose `pane_2` is not the pane the menu was
        // opened on even though the id matches.
        state.active_endpoint_id =
            ClientEndpointId::Ssh(crate::client::endpoint::ProfileId::generate());
        state.submit_account_picker(&mut ClientShellInput::default());
        assert_eq!(
            picker_error(&state).as_deref(),
            Some("herdr is no longer attached to the local server")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_picker_refuses_a_profile_the_config_no_longer_holds() {
        let root = present_root("vanished");
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        // A live config reload dropped the section.
        state.config.accounts = Profiles::default();
        state.submit_account_picker(&mut ClientShellInput::default());
        let error = picker_error(&state).expect("a refusal");
        assert!(error.contains("no longer configured"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_picker_refuses_a_profile_that_now_points_elsewhere() {
        let root = present_root("moved");
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        // Same name, different directory: not the row the user chose.
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("work")).expect("moved profile dir");
        state.config.accounts = profiles(&[("work", true)], &elsewhere);
        state.submit_account_picker(&mut ClientShellInput::default());
        let error = picker_error(&state).expect("a refusal");
        assert!(error.contains("changed since this opened"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refusal_leaves_the_picker_usable_and_moving_off_the_row_clears_it() {
        let root = std::env::temp_dir().join("herdr-account-overlay-refusal-usable");
        let _ = std::fs::remove_dir_all(&root);
        let mut state = shell_with(
            profiles(&[("perso", false), ("work", true)], &root),
            two_pane_snapshot(),
        );
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        state.submit_account_picker(&mut ClientShellInput::default());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(picker.error.is_some());
                assert!(
                    !picker.settled,
                    "nothing was typed, so the picker is not spent"
                );
            }
            other => panic!("expected the picker, got {other:?}"),
        }
        state.move_account_picker_selection(-1);
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert_eq!(picker.error, None);
                assert_eq!(
                    picker.selected_entry().map(|entry| entry.name.as_str()),
                    Some("perso")
                );
            }
            other => panic!("expected the picker, got {other:?}"),
        }
    }

    #[test]
    fn a_settled_picker_never_submits_again() {
        let root = present_root("settled");
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        if let Some(ClientShellOverlay::AccountPicker(picker)) = state.overlay.as_mut() {
            picker.fold(AccountJobEvent::Finished(AccountJobResult::Failed {
                message: "claude did not start: agent_pane_busy".to_owned(),
                notes: vec!["the line is still in the pane".to_owned()],
                refused: false,
            }));
        }
        state.submit_account_picker(&mut ClientShellInput::default());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(!picker.running(), "a second launch must not start");
                assert_eq!(
                    picker.error.as_deref(),
                    Some("claude did not start: agent_pane_busy"),
                    "the outcome stays on screen"
                );
            }
            other => panic!("expected the picker, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- the worker's lifetime as the tick sees it ----

    #[test]
    fn a_silent_worker_is_reported_and_detached_after_the_stall_limit() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let started = Instant::now();
        let (overlay, keep_alive) = picker(vec![entry("work", false)]).test_running(started);
        state.overlay = Some(ClientShellOverlay::AccountPicker(overlay));

        // Inside the budget: still waiting, nothing to repaint.
        let within = started + ACCOUNT_JOB_STALL_LIMIT - Duration::from_secs(1);
        assert!(!state.tick_account_picker_at(within));
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => assert!(picker.running()),
            other => panic!("expected the picker, got {other:?}"),
        }

        // Past it, with the channel still open: give up on the report, not
        // on the launch.
        let beyond = started + ACCOUNT_JOB_STALL_LIMIT + Duration::from_secs(1);
        assert!(state.tick_account_picker_at(beyond));
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(!picker.running(), "the modal must be dismissable again");
                assert!(picker.settled);
                let error = picker.error.as_deref().expect("the stall is reported");
                assert!(error.contains("no answer from the server"), "{error}");
                assert!(error.contains("herdr agent list"), "{error}");
            }
            other => panic!("expected the picker to stay open, got {other:?}"),
        }
        drop(keep_alive);
    }

    #[test]
    fn a_worker_that_speaks_resets_the_stall_clock() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let started = Instant::now();
        let (overlay, tx) = picker(vec![entry("work", false)]).test_running(started);
        state.overlay = Some(ClientShellOverlay::AccountPicker(overlay));

        let later = started + ACCOUNT_JOB_STALL_LIMIT - Duration::from_secs(1);
        tx.send(AccountJobEvent::Progress("verifying account..."))
            .expect("send");
        assert!(state.tick_account_picker_at(later));
        // The limit is measured from the last word, not from the start.
        let beyond_start = started + ACCOUNT_JOB_STALL_LIMIT + Duration::from_secs(1);
        assert!(!state.tick_account_picker_at(beyond_start));
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(picker.running());
                assert_eq!(picker.progress, Some("verifying account..."));
            }
            other => panic!("expected the picker, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_launch_closes_the_modal_from_the_tick() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let (overlay, tx) = picker(vec![entry("work", false)]).test_running(Instant::now());
        state.overlay = Some(ClientShellOverlay::AccountPicker(overlay));
        tx.send(AccountJobEvent::Finished(AccountJobResult::Launched {
            account: "work".to_owned(),
            state: AccountState::Ok,
            detail: None,
            warnings: Vec::new(),
        }))
        .expect("send");
        assert!(state.tick_account_picker_at(Instant::now()));
        assert!(state.overlay.is_none(), "a clean launch needs no reading");
    }

    // ---- the switch: the item, the pane and agent it reaches, its answer ----

    /// A claude agent on `pane_2`, named, claiming `account`.
    fn claude_agent_on(pane_id: &str, name: &str, account: Option<&str>) -> ClientShellSnapshot {
        let mut snapshot = two_pane_snapshot();
        let mut agent = agent_on(pane_id, Some(name), Some(AGENT_LABEL));
        if let Some(account) = account {
            agent.tokens = vec![(ACCOUNT_TOKEN.to_owned(), account.to_owned())];
        }
        snapshot.agents.push(agent);
        snapshot
    }

    fn menu_has_switch_item(state: &ClientShellState) -> bool {
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::ContextMenu(menu)) => menu
                .items()
                .iter()
                .any(|item| item.action == ClientContextMenuAction::SwitchClaudeAccount),
            _ => panic!("expected a context menu"),
        }
    }

    #[test]
    fn the_switch_item_needs_a_claude_agent_and_somewhere_to_move_it() {
        // One profile is where it already is; there has to be a second.
        assert!(switch_account_context_item(Some("claude"), 2).is_some());
        assert!(switch_account_context_item(Some("claude"), 5).is_some());
        assert!(switch_account_context_item(Some("claude"), 1).is_none());
        // Not Claude, or nothing running at all.
        assert!(switch_account_context_item(Some("codex"), 3).is_none());
        assert!(switch_account_context_item(Some("agent"), 3).is_none());
        assert!(switch_account_context_item(None, 3).is_none());
        // A remote endpoint reaches the item as zero profiles.
        assert!(switch_account_context_item(Some("claude"), 0).is_none());
    }

    #[test]
    fn the_two_account_items_are_never_offered_together() {
        let root = std::env::temp_dir();
        // A free pane offers only the start item.
        let mut state = shell_with(
            profiles(&[("perso", true), ("work", false)], &root),
            two_pane_snapshot(),
        );
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        assert!(menu_has_account_item(&state));
        assert!(!menu_has_switch_item(&state));

        // A pane running Claude offers only the switch.
        let mut state = shell_with(
            profiles(&[("perso", true), ("work", false)], &root),
            claude_agent_on("pane_2", "a1", Some("perso")),
        );
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        assert!(!menu_has_account_item(&state));
        assert!(menu_has_switch_item(&state));
    }

    #[test]
    fn activating_the_switch_item_opens_the_picker_on_that_pane_and_agent() {
        let root = std::env::temp_dir();
        let mut state = shell_with(
            profiles(&[("perso", true), ("work", false)], &root),
            claude_agent_on("pane_2", "a1", Some("perso")),
        );
        state.open_pane_context_menu("pane_2".to_owned(), 0, 0);
        let index = match state.overlay.as_ref() {
            Some(ClientShellOverlay::ContextMenu(menu)) => menu
                .items()
                .iter()
                .position(|item| item.action == ClientContextMenuAction::SwitchClaudeAccount)
                .expect("switch item"),
            _ => panic!("expected a context menu"),
        };
        let mut outcome = ClientShellInput::default();
        state.activate_context_menu_item(index, &mut outcome);
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert_eq!(picker.pane_id, "pane_2");
                assert!(matches!(
                    &picker.mode,
                    PickerMode::Switch { agent_name, current, interrupt }
                        if agent_name == "a1"
                            && current.as_deref() == Some("perso")
                            && !*interrupt
                ));
                // The account it is on is marked, and the selection is not it.
                assert!(picker.entries[0].is_current);
                assert!(
                    picker.entries[0]
                        .status_text()
                        .starts_with("current · default"),
                    "{}",
                    picker.entries[0].status_text()
                );
                assert_eq!(
                    picker.selected_entry().map(|entry| entry.name.as_str()),
                    Some("work"),
                    "the picker opens on somewhere worth going"
                );
            }
            other => panic!("expected the account picker, got {other:?}"),
        }
    }

    #[test]
    fn the_switch_picker_never_opens_without_a_named_claude_and_two_profiles() {
        let root = std::env::temp_dir();
        let two = profiles(&[("perso", true), ("work", false)], &root);

        // One profile: nowhere to move to.
        let mut state = shell_with(
            profiles(&[("perso", true)], &root),
            claude_agent_on("pane_2", "a1", Some("perso")),
        );
        state.open_account_switch_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        assert!(state.overlay.is_none());

        // A `claude` nobody named: `agent.start` has no name to resume under.
        let mut snapshot = two_pane_snapshot();
        snapshot
            .agents
            .push(agent_on("pane_2", None, Some(AGENT_LABEL)));
        let mut state = shell_with(two.clone(), snapshot);
        state.open_account_switch_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        assert!(state.overlay.is_none());

        // Another agent entirely.
        let mut snapshot = two_pane_snapshot();
        snapshot
            .agents
            .push(agent_on("pane_2", Some("c1"), Some("codex")));
        let mut state = shell_with(two.clone(), snapshot);
        state.open_account_switch_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        assert!(state.overlay.is_none());

        // A pane that is not there.
        let mut state = shell_with(two, claude_agent_on("pane_2", "a1", None));
        state.open_account_switch_picker("pane_9".to_owned(), &mut ClientShellInput::default());
        assert!(state.overlay.is_none());
    }

    #[test]
    fn the_switch_refuses_once_the_pane_holds_another_agent() {
        let root = present_root("switch-moved");
        let mut state = shell_with(
            profiles(&[("perso", true), ("work", false)], &root),
            claude_agent_on("pane_2", "a1", Some("perso")),
        );
        state.open_account_switch_picker("pane_2".to_owned(), &mut ClientShellInput::default());

        // The agent exited and another one took the pane while the modal was up.
        state.set_snapshot(Box::new(claude_agent_on("pane_2", "a2", Some("perso"))));
        state.submit_account_picker(&mut ClientShellInput::default());
        let error = picker_error(&state).expect("a refusal");
        assert!(error.contains("now runs agent \"a2\""), "{error}");

        // And when it left altogether.
        state.set_snapshot(Box::new(two_pane_snapshot()));
        state.submit_account_picker(&mut ClientShellInput::default());
        let error = picker_error(&state).expect("a refusal");
        assert!(error.contains("no longer running"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_switch_refuses_the_account_the_agent_is_already_on() {
        let root = present_root("switch-same");
        let mut state = shell_with(
            profiles(&[("perso", true), ("work", false)], &root),
            claude_agent_on("pane_2", "a1", Some("work")),
        );
        state.open_account_switch_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        // Aim it back at where it already is.
        if let Some(ClientShellOverlay::AccountPicker(picker)) = state.overlay.as_mut() {
            picker.selected = picker
                .entries
                .iter()
                .position(|entry| entry.is_current)
                .expect("the current row");
        }
        state.submit_account_picker(&mut ClientShellInput::default());
        let error = picker_error(&state).expect("a refusal");
        assert!(error.contains("already runs under account"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_interrupt_toggle_belongs_to_the_switch_only() {
        let root = std::env::temp_dir();
        let mut state = shell_with(
            profiles(&[("perso", true), ("work", false)], &root),
            claude_agent_on("pane_2", "a1", Some("perso")),
        );
        state.open_account_switch_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert_eq!(picker.interrupt(), Some(false))
            }
            other => panic!("expected the picker, got {other:?}"),
        }
        assert!(state.toggle_account_interrupt());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert_eq!(picker.interrupt(), Some(true))
            }
            other => panic!("expected the picker, got {other:?}"),
        }

        // A start picker has no such thing to toggle.
        let mut state = shell_with(profiles(&[("work", true)], &root), two_pane_snapshot());
        state.open_account_picker("pane_2".to_owned(), &mut ClientShellInput::default());
        assert!(!state.toggle_account_interrupt());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => assert_eq!(picker.interrupt(), None),
            other => panic!("expected the picker, got {other:?}"),
        }
    }

    // ---- the confirmation: the only door between a right-click and /exit ----

    fn key(code: KeyCode) -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty())
    }

    /// A switch picker with a worker blocked on its confirmation, the question
    /// having been up long enough that an answer counts.
    fn confirming(state: &mut ClientShellState) -> Receiver<Confirmation> {
        let armed = Instant::now()
            .checked_sub(CONFIRM_ARM_DELAY)
            .expect("an instant before now");
        confirming_since(state, armed)
    }

    /// [`confirming`], with the question put on screen at `shown`.
    fn confirming_since(state: &mut ClientShellState, shown: Instant) -> Receiver<Confirmation> {
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let (overlay, event_tx) = ClientAccountPickerOverlay::idle(
            "pane_2".to_owned(),
            PickerMode::Switch {
                agent_name: "a1".to_owned(),
                current: Some("perso".to_owned()),
                interrupt: false,
            },
            vec![entry("perso", true), entry("work", false)],
            1,
        )
        .test_running(Instant::now());
        state.overlay = Some(ClientShellOverlay::AccountPicker(overlay));
        if let Some(ClientShellOverlay::AccountPicker(picker)) = state.overlay.as_mut() {
            if let Some(job) = picker.job.as_mut() {
                job.answers = Some(answer_tx);
            }
        }
        event_tx
            .send(AccountJobEvent::Confirm("Switch agent \"a1\"?".to_owned()))
            .expect("send");
        assert!(state.tick_account_picker_at(shown));
        std::mem::forget(event_tx);
        answer_rx
    }

    fn picker_of(state: &ClientShellState) -> &ClientAccountPickerOverlay {
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => picker,
            other => panic!("expected the picker, got {other:?}"),
        }
    }

    /// The question appears where the picker was, right after the Enter or
    /// click that submitted it. An answer that early is that gesture's tail.
    #[test]
    fn a_yes_within_the_arming_delay_is_not_an_answer_but_a_no_is() {
        let shown = Instant::now();
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let answers = confirming_since(&mut state, shown);
        let too_soon = shown + CONFIRM_ARM_DELAY / 2;
        let mut outcome = ClientShellInput::default();

        state.answer_account_confirm_at(true, too_soon, &mut outcome);
        assert!(
            answers.try_recv().is_err(),
            "a yes before anyone could have read the question is ignored"
        );
        assert!(
            picker_of(&state).awaiting_confirm(),
            "the question stays up"
        );
        assert!(picker_of(&state).running());

        // Once the delay has passed, the same key is the answer.
        state.answer_account_confirm_at(true, shown + CONFIRM_ARM_DELAY, &mut outcome);
        assert_eq!(answers.try_recv(), Ok(Confirmation::Yes));
        assert!(!picker_of(&state).awaiting_confirm());

        // Declining is never delayed.
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let answers = confirming_since(&mut state, shown);
        state.answer_account_confirm_at(false, too_soon, &mut outcome);
        assert_eq!(answers.try_recv(), Ok(Confirmation::No));
    }

    /// A person may take minutes over the question. Answering it must not
    /// read as "the worker has been silent for minutes".
    #[test]
    fn answering_the_question_restarts_the_stall_clock() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let answers = confirming(&mut state);
        let much_later = Instant::now() + ACCOUNT_JOB_STALL_LIMIT * 10;
        let mut outcome = ClientShellInput::default();
        state.answer_account_confirm_at(true, much_later, &mut outcome);
        assert_eq!(answers.try_recv(), Ok(Confirmation::Yes));

        // The worker has been released and has not spoken yet; the modal
        // waits for it rather than declaring a stall on the next tick.
        let next_tick = much_later + Duration::from_millis(100);
        assert!(!state.tick_account_picker_at(next_tick));
        let picker = picker_of(&state);
        assert!(picker.running(), "the worker is still being waited for");
        assert!(!picker.settled);
        assert_eq!(picker.error, None);

        // The limit counts from the answer, not from the question.
        assert!(state.tick_account_picker_at(much_later + ACCOUNT_JOB_STALL_LIMIT * 2));
        let picker = picker_of(&state);
        assert!(picker.settled);
        assert!(picker
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no answer from the server")));
    }

    #[test]
    fn the_question_owns_the_keyboard_and_enter_is_the_only_yes() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let answers = confirming(&mut state);
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(picker.awaiting_confirm());
                assert_eq!(picker.confirm.as_deref(), Some("Switch agent \"a1\"?"));
                assert_eq!(picker.progress, None);
            }
            other => panic!("expected the picker, got {other:?}"),
        }

        // Everything that is not an answer is swallowed: no navigation, no
        // filtering, no dismissal under a question about somebody's agent.
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char('/'),
            KeyCode::Char('i'),
            KeyCode::Char('x'),
        ] {
            let mut outcome = ClientShellInput::default();
            assert!(state.route_account_picker_key(&key(code), &mut outcome));
            assert!(
                answers.try_recv().is_err(),
                "{code:?} must not answer the question"
            );
        }
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(picker.awaiting_confirm(), "still waiting");
                assert_eq!(picker.selected, 1, "the selection never moved");
            }
            other => panic!("expected the picker, got {other:?}"),
        }

        let mut outcome = ClientShellInput::default();
        assert!(state.route_account_picker_key(&key(KeyCode::Enter), &mut outcome));
        assert_eq!(answers.try_recv(), Ok(Confirmation::Yes));
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(!picker.awaiting_confirm());
                assert!(picker.running(), "the worker carries on");
            }
            other => panic!("expected the picker, got {other:?}"),
        }
    }

    #[test]
    fn esc_and_n_decline_the_question_and_nothing_else_can() {
        for code in [KeyCode::Esc, KeyCode::Char('n')] {
            let mut state = shell_with(Profiles::default(), two_pane_snapshot());
            let answers = confirming(&mut state);
            let mut outcome = ClientShellInput::default();
            assert!(state.route_account_picker_key(&key(code), &mut outcome));
            assert_eq!(answers.try_recv(), Ok(Confirmation::No), "{code:?}");
            // Esc under the question answers it; it does not close the modal,
            // which is where the worker's "nothing was sent" lands.
            assert!(matches!(
                state.overlay,
                Some(ClientShellOverlay::AccountPicker(_))
            ));
        }
    }

    #[test]
    fn a_question_is_answered_exactly_once() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let answers = confirming(&mut state);
        let mut outcome = ClientShellInput::default();
        state.route_account_picker_key(&key(KeyCode::Enter), &mut outcome);
        assert_eq!(answers.try_recv(), Ok(Confirmation::Yes));
        // A second Enter is the settled-picker path, not another answer.
        state.route_account_picker_key(&key(KeyCode::Enter), &mut outcome);
        assert!(answers.try_recv().is_err(), "the machine asks once");
    }

    #[test]
    fn a_worker_waiting_on_a_person_never_stalls() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let _answers = confirming(&mut state);
        let much_later = Instant::now() + ACCOUNT_JOB_STALL_LIMIT * 10;
        assert!(!state.tick_account_picker_at(much_later));
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(
                    picker.awaiting_confirm(),
                    "a question waits as long as it takes"
                );
            }
            other => panic!("expected the picker, got {other:?}"),
        }
    }

    #[test]
    fn a_switch_that_reached_nothing_leaves_the_picker_usable() {
        let mut overlay = ClientAccountPickerOverlay::idle(
            "pane_2".to_owned(),
            PickerMode::Switch {
                agent_name: "a1".to_owned(),
                current: Some("perso".to_owned()),
                interrupt: false,
            },
            vec![entry("perso", true), entry("work", false)],
            1,
        );
        assert!(
            !overlay.fold(AccountJobEvent::Finished(AccountJobResult::Failed {
                message: "agent \"a1\" is working".to_owned(),
                notes: vec!["press i to allow interrupting it".to_owned()],
                refused: true,
            }))
        );
        assert!(
            !overlay.settled,
            "nothing was sent, so the user may try again"
        );
        assert!(overlay.error.is_some());
        assert_eq!(overlay.warnings.len(), 1);
        assert_eq!(overlay.confirm, None);

        // One that did reach the pane settles: the pane is not as it was, and
        // a second attempt from the same modal would be a guess.
        let mut overlay = ClientAccountPickerOverlay::idle(
            "pane_2".to_owned(),
            PickerMode::Switch {
                agent_name: "a1".to_owned(),
                current: None,
                interrupt: false,
            },
            vec![entry("work", false)],
            0,
        );
        assert!(
            !overlay.fold(AccountJobEvent::Finished(AccountJobResult::Failed {
                message: "claude did not come back".to_owned(),
                notes: vec!["run `claude --resume abc` in pane pane_2".to_owned()],
                refused: false,
            }))
        );
        assert!(overlay.settled);
        assert!(overlay.warnings[0].contains("--resume"));
    }

    #[test]
    fn a_clean_switch_reports_the_conversation_it_kept_and_closes() {
        let mut overlay = ClientAccountPickerOverlay::idle(
            "pane_2".to_owned(),
            PickerMode::Switch {
                agent_name: "a1".to_owned(),
                current: Some("perso".to_owned()),
                interrupt: false,
            },
            vec![entry("perso", true), entry("work", false)],
            1,
        );
        assert!(
            overlay.fold(AccountJobEvent::Finished(AccountJobResult::Switched {
                account: "work".to_owned(),
                state: AccountState::Ok,
                detail: None,
                session_id: "abc-123".to_owned(),
                warnings: Vec::new(),
            }))
        );
        assert_eq!(overlay.error, None);
        assert!(overlay.settled);

        // A graded mismatch is a running agent on the wrong account: it stays
        // on screen, naming both the conversation and what the environment says.
        let mut overlay = ClientAccountPickerOverlay::idle(
            "pane_2".to_owned(),
            PickerMode::Switch {
                agent_name: "a1".to_owned(),
                current: None,
                interrupt: false,
            },
            vec![entry("work", false)],
            0,
        );
        assert!(
            !overlay.fold(AccountJobEvent::Finished(AccountJobResult::Switched {
                account: "work".to_owned(),
                state: AccountState::Mismatch,
                detail: Some("it names /home/u/.claude".to_owned()),
                session_id: "abc-123".to_owned(),
                warnings: Vec::new(),
            }))
        );
        let error = overlay.error.expect("a mismatch must be shown");
        assert!(error.contains("abc-123"), "{error}");
        assert!(error.contains("/home/u/.claude"), "{error}");
    }

    #[test]
    fn a_detached_worker_stops_waiting_for_an_answer_nobody_will_give() {
        let (answer_tx, answer_rx) = std::sync::mpsc::channel::<Confirmation>();
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        let mut job = AccountJob {
            handle: None,
            events: event_rx,
            last_event: Instant::now(),
            answers: Some(answer_tx),
            awaiting_confirm: true,
        };
        assert!(answer_rx.try_recv().is_err());
        job.detach();
        // The worker's `recv` returns `Err` rather than blocking for ever, and
        // the machine reads that as "there is nobody to ask".
        assert!(matches!(answer_rx.recv(), Err(std::sync::mpsc::RecvError)));
        assert!(!job.awaiting_confirm);
    }

    #[test]
    fn a_worker_that_died_is_reported_by_the_tick_and_the_modal_stays() {
        let mut state = shell_with(Profiles::default(), two_pane_snapshot());
        let (overlay, tx) = picker(vec![entry("work", false)]).test_running(Instant::now());
        state.overlay = Some(ClientShellOverlay::AccountPicker(overlay));
        drop(tx);
        assert!(state.tick_account_picker_at(Instant::now()));
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::AccountPicker(picker)) => {
                assert!(!picker.running());
                assert!(picker.settled);
                let error = picker.error.as_deref().expect("reported");
                assert!(error.contains("without a result"), "{error}");
            }
            other => panic!("expected the picker to stay open, got {other:?}"),
        }
    }
}
