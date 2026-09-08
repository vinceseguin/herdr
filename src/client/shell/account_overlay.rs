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

use crate::accounts::layout::{inspect, InspectOptions};
use crate::accounts::profile::{AccountProfile, Profiles};
use crate::accounts::tokens::{AccountState, AGENT_LABEL};

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

    /// What the row shows on the right: which profile is the default, and
    /// what is wrong with it. Both, because they are independent facts and a
    /// default profile with no hook is exactly the case worth seeing.
    pub(super) fn status_text(&self) -> String {
        match (self.is_default, self.health_label()) {
            (true, "") => "default".to_owned(),
            (true, health) => format!("default · {health}"),
            (false, health) => health.to_owned(),
        }
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
/// PR 8 adds `Switch { agent_name, current }`; the overlay, its key routing
/// and [`AccountJob`] are shared, and only the submitted job differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PickerMode {
    /// Start a new Claude agent in a pane that has none, under the picked
    /// profile, with this name.
    Start { agent_name: String },
}

impl PickerMode {
    fn title(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start claude as account",
        }
    }
}

/// A step of the launch, as the worker thread reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AccountJobEvent {
    /// A phase name for the modal; the worker sends one before each blocking
    /// call so a slow launch never looks hung.
    Progress(&'static str),
    Finished(AccountJobResult),
}

/// How a launch ended.
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
    /// Nothing is running under the picked account. `note` is present when the
    /// environment line had already reached the pane's shell, which outlives
    /// the failure and has to be said out loud.
    Failed {
        message: String,
        note: Option<String>,
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
                            message: "the account launch stopped without a result".to_owned(),
                            note: None,
                        }));
                    }
                    break true;
                }
            }
        };
        (events, finished)
    }

    /// True when the worker has been silent for longer than the stall limit.
    fn stalled(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_event) > ACCOUNT_JOB_STALL_LIMIT
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
    fn detach(&mut self) {
        drop(self.handle.take());
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
            error: None,
            warnings: Vec::new(),
            settled: false,
            job: None,
        }
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
            AccountJobEvent::Finished(AccountJobResult::Launched {
                account,
                state,
                detail,
                warnings,
            }) => {
                self.progress = None;
                self.settled = true;
                self.warnings = warnings;
                self.error = match state {
                    AccountState::Ok => None,
                    AccountState::Mismatch => Some(format!(
                        "claude is running, but not under account {account:?}: {}",
                        detail.unwrap_or_else(|| "its environment disagrees".to_owned())
                    )),
                    other => Some(format!(
                        "claude is running under account {account:?}, but herdr could not \
                         confirm it ({})",
                        other.as_str()
                    )),
                };
                self.error.is_none() && self.warnings.is_empty()
            }
            AccountJobEvent::Finished(AccountJobResult::Failed { message, note }) => {
                self.progress = None;
                self.settled = true;
                self.error = Some(message);
                self.warnings = note.into_iter().collect();
                false
            }
        }
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
    snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
        .map(|agent| agent.agent.clone().unwrap_or_else(|| "agent".to_owned()))
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

/// The rows for a set of resolved profiles, worst-to-best order preserved.
///
/// Split from [`ClientShellState::open_account_picker`] so the mapping is
/// testable without a shell; the `inspect` calls are the only I/O.
fn entries_from_profiles(profiles: &Profiles) -> Vec<AccountEntry> {
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
            note: None,
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
            note: Some(note),
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
        let entries = entries_from_profiles(&self.config.accounts);
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
        let account = &entry.name;
        if !self.active_endpoint_id.is_local() {
            return Err("herdr is no longer attached to the local server".to_owned());
        }
        let Some(snapshot) = self.snapshot.as_deref() else {
            return Err("herdr has no snapshot of this server yet".to_owned());
        };
        if !snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id) {
            return Err(format!("pane {pane_id} is gone"));
        }
        if pane_agent_kind(snapshot, pane_id).is_some() {
            return Err(format!("pane {pane_id} already runs an agent"));
        }
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
        let PickerMode::Start { agent_name } = &picker.mode;
        let agent_name = agent_name.clone();

        let profile = match self.account_launch_preflight(picker, entry) {
            Ok(profile) => profile,
            Err(message) => {
                if let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() {
                    picker.error = Some(message);
                    picker.warnings.clear();
                }
                outcome.repaint = true;
                return;
            }
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("herdr-account-launch".to_owned())
            .spawn(move || run_start_job(pane_id, agent_name, profile, &tx));
        let Some(ClientShellOverlay::AccountPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        match handle {
            Ok(handle) => {
                picker.error = None;
                picker.warnings.clear();
                picker.progress = Some("exporting profile...");
                picker.job = Some(AccountJob {
                    handle: Some(handle),
                    events: rx,
                    last_event: Instant::now(),
                });
            }
            Err(err) => picker.error = Some(format!("could not start the account launch: {err}")),
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
                    "no answer from the server for {}s; the launch may still finish on its \
                     own — check `herdr agent list` before starting another",
                    ACCOUNT_JOB_STALL_LIMIT.as_secs()
                ),
                note: None,
            }));
            finished = true;
        }
        if events.is_empty() && !finished {
            return false;
        }
        let mut close = false;
        for event in events {
            close |= picker.fold(event);
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
                note: Some("CLAUDE_CONFIG_DIR was already exported in pane p1".to_owned()),
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
            note: None,
        }))
        .expect("send");
        tx.send(AccountJobEvent::Progress("two")).expect("send");
        let mut job = AccountJob {
            handle: None,
            events: rx,
            last_event: Instant::now(),
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
            note: None,
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
        let entries = entries_from_profiles(&profiles);
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
                note: Some("the line is still in the pane".to_owned()),
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
