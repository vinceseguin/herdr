//! Turning a Claude usage limit into a label every herdr surface shows (fork).
//!
//! Usage limits are per account, so "this agent is stuck because its account
//! ran out" is the one blocked state herdr can offer a way out of. `herdr
//! account watch` is the opt-in helper that keeps that fact visible: it walks
//! the running agents, asks herdr's own detector about the blocked Claude ones,
//! and reports [`crate::accounts::tokens`]'s vocabulary back onto the pane so
//! the sidebar, `herdr agent get`, `herdr account status` and
//! `herdr fleet status --json` all say the same thing.
//!
//! This module is the decision half, and it is pure: it folds observations into
//! [`WatchAction`]s and holds no socket, clock source or I/O of its own. The
//! runtime half is `crate::accounts::client::watch`.
//!
//! Three properties are load-bearing, and the tests below pin all three.
//!
//! 1. **Only an agent herdr can attribute is ever labelled.** A pane with no
//!    `account` token was not launched through the account tooling — herdr does
//!    not know whose licence it is spending — so the watcher leaves it exactly
//!    as it found it. The same goes for anything that is not Claude.
//! 2. **Nothing is ever typed.** Every action this module can emit is a
//!    metadata report; there is no variant that sends input, and no variant
//!    that switches an account. Decision (d) of the epic is "detect and
//!    suggest, never auto-switch", and the way to keep that true is to give the
//!    watcher no vocabulary for it.
//! 3. **A label is a lease, not a fact on disk.** Every label carries a TTL, so
//!    a watcher that is killed, crashes or loses its machine cannot leave a
//!    `limited` badge on an agent forever: the server drops it when the lease
//!    runs out. A watcher that exits normally clears its labels first, which is
//!    faster than waiting for the lease.
//!
//! The observation loop is a poll rather than an event subscription, which is a
//! correction to the plan: the stock API's `pane.agent_status_changed`
//! subscription is *per pane* (`Subscription::PaneAgentStatusChanged` requires
//! a `pane_id`, `src/api/schema/events.rs`), so a watcher that wants every
//! Claude agent on the machine would have to open one subscription per pane and
//! reconnect whenever a pane appears. Widening that subscription is a server
//! change, which E9 does not make. Polling `agent.list` is one request per
//! interval whatever the pane count, and it has a property the event stream
//! does not: it re-reads the labels herdr actually holds, so a server restart —
//! after which no token survives (`src/persist/snapshot.rs` carries none) — is
//! repaired by the next poll instead of being missed forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::accounts::limit::UsageLimit;
use crate::accounts::tokens::{AccountState, AGENT_LABEL};
use crate::api::schema::{AgentInfo, AgentStatus};

/// The `state_labels.blocked` text a limited agent shows.
///
/// Replaces the word `blocked` in the sidebar row and in every client that
/// renders `state_labels` (`src/client/shell/agent_sidebar.rs`). Short on
/// purpose: it shares a narrow column with the agent name.
pub const LIMIT_LABEL: &str = "usage limit";

/// The agent status whose label [`LIMIT_LABEL`] replaces.
///
/// The server only accepts `idle|working|blocked|done|unknown` here
/// (`normalize_state_labels`, `src/app/api/panes.rs`), and a usage limit is a
/// blocker a human has to act on.
pub const LIMIT_LABEL_STATE: &str = "blocked";

/// The smallest gap between two `agent.explain` calls for one pane.
///
/// `agent.explain` is a round trip that re-runs detection, so a blocked agent
/// must not turn into one explain per poll however short the interval is.
pub const EXPLAIN_DEBOUNCE: Duration = Duration::from_secs(5);

/// How long a blocked agent that is *not* limited rests between re-reads.
///
/// The first reads of a blocked episode can genuinely disagree with the final
/// one — the status flips as soon as the screen settles, and the detector may
/// still be looking at the frame before the notice — so the watcher looks
/// again a few times. After that the agent is blocked on something else (a
/// permission prompt, a question) and re-reading it every few seconds would be
/// pure cost, so it drops to one read a minute in case a limit arrives while
/// the human is away.
pub const BLOCKED_RECHECK: Duration = Duration::from_secs(60);

/// How many times one blocked episode is explained before it backs off to
/// [`BLOCKED_RECHECK`].
pub const EPISODE_EXPLAIN_BUDGET: u32 = 3;

/// The most panes one watcher tracks at once.
///
/// A bound rather than a guess: the map only ever holds panes that ran a Claude
/// agent under a herdr-managed account, but the watcher is a long-running
/// process reading a list it does not control, and an unbounded map fed by a
/// remote list is how a long-running process leaks.
pub const MAX_TRACKED_PANES: usize = 512;

/// What the driver should do next about one pane.
///
/// There is deliberately no variant that types, prompts, switches or kills:
/// the watcher's entire vocabulary is "look" and "label".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchAction {
    /// Ask herdr's detector why this pane is blocked.
    Explain { pane_id: String },
    /// Report the limit: `state_labels.blocked` and `account_state = limited`.
    Label {
        pane_id: String,
        /// The agent's managed name, for the line the CLI prints.
        name: Option<String>,
        /// The profile the agent runs under.
        account: String,
        limit: UsageLimit,
        /// How long the server should hold the label without another report.
        lease: Duration,
        /// True the first time this pane is labelled (or the first time after
        /// the label went missing), false for a lease refresh. Only a change
        /// is worth a line on stdout.
        announce: bool,
    },
    /// Take the label back off, restoring the `account_state` it replaced.
    Clear {
        pane_id: String,
        name: Option<String>,
        account: String,
        /// The `account_state` value the label overwrote, restored verbatim.
        /// `None` removes the token, which is the honest answer when the
        /// watcher never saw one: absent means "herdr does not know", where a
        /// guessed `ok` would mean "herdr checked".
        restore_state: Option<String>,
    },
    /// Nothing to do for this observation.
    Nothing,
}

impl WatchAction {
    /// The pane an action addresses, if it addresses one.
    pub fn pane_id(&self) -> Option<&str> {
        match self {
            Self::Explain { pane_id }
            | Self::Label { pane_id, .. }
            | Self::Clear { pane_id, .. } => Some(pane_id),
            Self::Nothing => None,
        }
    }
}

/// One agent as the watcher reads it, reduced from `agent.list`.
///
/// A reduction rather than the schema type so the fold and its tests need no
/// server, and so a change to `AgentInfo` cannot quietly reshape the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchAgent {
    pub pane_id: String,
    pub name: Option<String>,
    /// The agent kind herdr detected (`claude`, `codex`, …).
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    /// `tokens.account`: the profile the launcher recorded.
    pub account: Option<String>,
    /// `tokens.account_state`, verbatim — including a value this build has
    /// never heard of.
    pub account_state: Option<String>,
    /// `state_labels.blocked` as the server currently holds it, so the watcher
    /// can tell its own label from someone else's and from none at all.
    pub blocked_label: Option<String>,
}

impl WatchAgent {
    pub fn from_agent_info(agent: &AgentInfo) -> Self {
        Self {
            pane_id: agent.pane_id.clone(),
            name: agent.name.clone(),
            agent: agent.agent.clone(),
            agent_status: agent.agent_status,
            account: agent
                .tokens
                .get(crate::accounts::tokens::ACCOUNT_TOKEN)
                .cloned(),
            account_state: agent
                .tokens
                .get(crate::accounts::tokens::ACCOUNT_STATE_TOKEN)
                .cloned(),
            blocked_label: agent.state_labels.get(LIMIT_LABEL_STATE).cloned(),
        }
    }

    /// Whether this agent is one the watcher may ever write to.
    ///
    /// Both halves matter. A non-Claude agent has no Claude account, and an
    /// agent with no `account` token was not launched through the account
    /// tooling: labelling it would be herdr inventing a fact about a process
    /// it did not start.
    fn attributable(&self) -> Option<&str> {
        if self.agent.as_deref() != Some(AGENT_LABEL) {
            return None;
        }
        self.account.as_deref().filter(|name| !name.is_empty())
    }
}

/// What the watcher believes about one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    /// The account the label was written against, so a pane that moved to
    /// another profile is re-evaluated rather than cleared under the new name.
    account: String,
    /// The agent's managed name as last seen, so the clears a shutdown drains
    /// can name the agent rather than only its pane.
    name: Option<String>,
    /// The limit currently labelled, and when the lease was last refreshed.
    labelled: Option<Labelled>,
    /// The `account_state` seen before the first label, restored on clear.
    restore_state: Option<String>,
    /// When this pane was last explained, and how many times in this episode.
    last_explain: Option<Instant>,
    episode_explains: u32,
    /// Whether the last observation had the agent blocked. A blocked episode
    /// ends the moment it is anything else, which is also when the label goes.
    blocked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Labelled {
    limit: UsageLimit,
    refreshed_at: Instant,
}

/// The watcher's memory: what it has labelled and what it has asked about.
#[derive(Debug, Default)]
pub struct WatchState {
    panes: HashMap<String, Tracked>,
    /// Set once when [`MAX_TRACKED_PANES`] is first hit, so the driver can warn
    /// exactly once rather than every poll.
    overflowed: bool,
}

impl WatchState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the tracked-pane bound has ever been hit.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// How many panes are tracked right now.
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.panes.len()
    }

    /// Whether this pane currently carries the watcher's label.
    #[cfg(test)]
    pub fn is_labelled(&self, pane_id: &str) -> bool {
        self.panes
            .get(pane_id)
            .is_some_and(|tracked| tracked.labelled.is_some())
    }

    /// The clears a graceful exit owes: one per pane still labelled.
    ///
    /// Draining, so a second call after a partial failure does not repeat a
    /// clear the server already accepted.
    pub fn drain_clears(&mut self) -> Vec<WatchAction> {
        let mut clears = Vec::new();
        for (pane_id, tracked) in self.panes.iter_mut() {
            if tracked.labelled.take().is_none() {
                continue;
            }
            clears.push(WatchAction::Clear {
                pane_id: pane_id.clone(),
                name: tracked.name.clone(),
                account: tracked.account.clone(),
                restore_state: tracked.restore_state.clone(),
            });
        }
        clears.sort_by(|left, right| left.pane_id().cmp(&right.pane_id()));
        clears
    }

    /// Fold one poll of the agent list into the actions it calls for.
    ///
    /// `lease` is the TTL the driver will put on every label it writes; the
    /// fold needs it to know when a lease is halfway through and wants
    /// refreshing.
    pub fn observe(
        &mut self,
        agents: &[WatchAgent],
        lease: Duration,
        now: Instant,
    ) -> Vec<WatchAction> {
        let mut actions = Vec::new();
        let mut seen = std::collections::HashSet::with_capacity(agents.len());
        for agent in agents {
            seen.insert(agent.pane_id.as_str());
            match self.observe_agent(agent, lease, now) {
                WatchAction::Nothing => {}
                action => actions.push(action),
            }
        }
        // An agent that is gone takes its labels with it: the server drops
        // metadata scoped to the Claude integration when the process exits, and
        // a pane herdr no longer lists cannot be reported against anyway.
        self.panes
            .retain(|pane_id, _| seen.contains(pane_id.as_str()));
        actions
    }

    /// The single-agent fold, exposed so the semantics can be tested one
    /// observation at a time.
    pub fn observe_agent(
        &mut self,
        agent: &WatchAgent,
        lease: Duration,
        now: Instant,
    ) -> WatchAction {
        let Some(account) = agent.attributable() else {
            // Not ours. Forget it rather than hold a label decision about a
            // pane the watcher is not allowed to write to.
            self.panes.remove(&agent.pane_id);
            return WatchAction::Nothing;
        };

        if agent.agent_status != AgentStatus::Blocked {
            return self.leave_blocked(agent);
        }

        let account = account.to_string();
        if !self.panes.contains_key(&agent.pane_id) {
            if self.panes.len() >= MAX_TRACKED_PANES {
                self.overflowed = true;
                return WatchAction::Nothing;
            }
            self.panes.insert(
                agent.pane_id.clone(),
                Tracked {
                    account: account.clone(),
                    name: agent.name.clone(),
                    labelled: None,
                    restore_state: agent.account_state.clone(),
                    last_explain: None,
                    episode_explains: 0,
                    blocked: false,
                },
            );
        }
        let Some(tracked) = self.panes.get_mut(&agent.pane_id) else {
            return WatchAction::Nothing;
        };
        tracked.name = agent.name.clone();

        // The pane moved to another profile under the watcher's feet (a
        // `switch-account`, say). The old episode is over: start again against
        // the new account rather than clearing a label in its name.
        if tracked.account != account {
            tracked.account = account.clone();
            tracked.labelled = None;
            tracked.restore_state = agent.account_state.clone();
            tracked.last_explain = None;
            tracked.episode_explains = 0;
        }

        if !tracked.blocked {
            // A new blocked episode: forget the previous one's budget, and
            // remember the state this label will have to restore.
            tracked.blocked = true;
            tracked.episode_explains = 0;
            if tracked.labelled.is_none()
                && agent.account_state.as_deref() != Some(AccountState::Limited.as_str())
            {
                tracked.restore_state = agent.account_state.clone();
            }
        }

        if let Some(labelled) = tracked.labelled.clone() {
            let label_intact = agent.blocked_label.as_deref() == Some(LIMIT_LABEL)
                && agent.account_state.as_deref() == Some(AccountState::Limited.as_str());
            // The label is gone from a pane the watcher still believes is
            // limited: the lease expired, the server restarted and lost its
            // in-memory tokens, or something else cleared it. Put it back, and
            // say so, because a reader saw an unlabelled limited agent.
            if !label_intact {
                tracked.labelled = Some(Labelled {
                    limit: labelled.limit.clone(),
                    refreshed_at: now,
                });
                return WatchAction::Label {
                    pane_id: agent.pane_id.clone(),
                    name: agent.name.clone(),
                    account,
                    limit: labelled.limit,
                    lease,
                    announce: true,
                };
            }
            // Halfway through the lease: renew it before the server drops it.
            // Renewing on every poll instead would be a write per poll per
            // limited agent for no extra safety.
            if now.saturating_duration_since(labelled.refreshed_at) >= lease / 2 {
                tracked.labelled = Some(Labelled {
                    limit: labelled.limit.clone(),
                    refreshed_at: now,
                });
                return WatchAction::Label {
                    pane_id: agent.pane_id.clone(),
                    name: agent.name.clone(),
                    account,
                    limit: labelled.limit,
                    lease,
                    announce: false,
                };
            }
            return WatchAction::Nothing;
        }

        let quiet_for = tracked
            .last_explain
            .map(|last| now.saturating_duration_since(last));
        let wait = if tracked.episode_explains >= EPISODE_EXPLAIN_BUDGET {
            BLOCKED_RECHECK
        } else {
            EXPLAIN_DEBOUNCE
        };
        if quiet_for.is_some_and(|elapsed| elapsed < wait) {
            return WatchAction::Nothing;
        }
        tracked.last_explain = Some(now);
        tracked.episode_explains = tracked.episode_explains.saturating_add(1);
        WatchAction::Explain {
            pane_id: agent.pane_id.clone(),
        }
    }

    /// What the driver learnt from the explain it was asked for.
    ///
    /// `None` is "the detector did not report a usage limit" — which covers
    /// both "blocked by something else" and "the call failed", because neither
    /// is evidence of a limit and the watcher will look again.
    pub fn explained(
        &mut self,
        agent: &WatchAgent,
        limit: Option<UsageLimit>,
        lease: Duration,
        now: Instant,
    ) -> WatchAction {
        let Some(account) = agent.attributable().map(str::to_string) else {
            return WatchAction::Nothing;
        };
        let Some(limit) = limit else {
            return WatchAction::Nothing;
        };
        // The agent stopped being blocked between the poll and the answer.
        // Labelling it now would put a limit badge on an agent that is working.
        if agent.agent_status != AgentStatus::Blocked {
            return WatchAction::Nothing;
        }
        let Some(tracked) = self.panes.get_mut(&agent.pane_id) else {
            return WatchAction::Nothing;
        };
        if tracked.account != account {
            return WatchAction::Nothing;
        }
        let announce = tracked
            .labelled
            .as_ref()
            .is_none_or(|labelled| labelled.limit != limit);
        tracked.labelled = Some(Labelled {
            limit: limit.clone(),
            refreshed_at: now,
        });
        WatchAction::Label {
            pane_id: agent.pane_id.clone(),
            name: agent.name.clone(),
            account,
            limit,
            lease,
            announce,
        }
    }

    /// A label the server refused. Forget it, so the next poll tries again
    /// rather than believing a label that was never written.
    pub fn label_failed(&mut self, pane_id: &str) {
        if let Some(tracked) = self.panes.get_mut(pane_id) {
            tracked.labelled = None;
        }
    }

    /// A clear the server refused. Remember the label again, so the exit path
    /// and the next poll both keep trying to take it off.
    pub fn clear_failed(&mut self, pane_id: &str, limit: UsageLimit, now: Instant) {
        if let Some(tracked) = self.panes.get_mut(pane_id) {
            tracked.labelled = Some(Labelled {
                limit,
                refreshed_at: now,
            });
        }
    }

    /// The pane left `blocked`: the episode is over whatever ended it.
    ///
    /// Deliberately *any* non-blocked status, not a return to idle. The
    /// `usage_limit` rule reads the status area below the live prompt box
    /// (`after_last_horizontal_rule`), so a notice that scrolls above the box
    /// stops matching on its own and the agent can land on `working` or
    /// `unknown` rather than `idle`. Clearing only on idle would strand the
    /// label on every other way out.
    fn leave_blocked(&mut self, agent: &WatchAgent) -> WatchAction {
        let Some(tracked) = self.panes.get_mut(&agent.pane_id) else {
            return WatchAction::Nothing;
        };
        tracked.blocked = false;
        tracked.episode_explains = 0;
        tracked.last_explain = None;
        let Some(_) = tracked.labelled.take() else {
            // Keep the pane tracked but idle: nothing to undo.
            tracked.restore_state = agent.account_state.clone();
            return WatchAction::Nothing;
        };
        WatchAction::Clear {
            pane_id: agent.pane_id.clone(),
            name: agent.name.clone(),
            account: tracked.account.clone(),
            restore_state: tracked.restore_state.clone(),
        }
    }
}

/// The lease a label carries, derived from the poll interval.
///
/// Four intervals: long enough that a slow poll, a busy server or one dropped
/// request does not make the badge flicker, short enough that a watcher which
/// was killed leaves a stale `limited` for seconds rather than for the rest of
/// the session. Floored so a very short interval does not produce a lease the
/// watcher cannot renew in time, and capped at the server's own limit
/// (`METADATA_TTL_MAX_MS`, `src/app/api_helpers.rs`).
pub fn lease_for(interval: Duration) -> Duration {
    const FLOOR: Duration = Duration::from_secs(15);
    const CEILING: Duration = Duration::from_millis(86_400_000);
    interval.saturating_mul(4).max(FLOOR).min(CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE: Duration = Duration::from_secs(20);

    fn agent(pane: &str, status: AgentStatus) -> WatchAgent {
        WatchAgent {
            pane_id: pane.to_string(),
            name: Some("a1".to_string()),
            agent: Some(AGENT_LABEL.to_string()),
            agent_status: status,
            account: Some("work".to_string()),
            account_state: Some(AccountState::Ok.as_str().to_string()),
            blocked_label: None,
        }
    }

    /// The agent as the server reports it once the watcher's label has landed.
    fn labelled(pane: &str) -> WatchAgent {
        let mut agent = agent(pane, AgentStatus::Blocked);
        agent.account_state = Some(AccountState::Limited.as_str().to_string());
        agent.blocked_label = Some(LIMIT_LABEL.to_string());
        agent
    }

    fn limit() -> UsageLimit {
        UsageLimit {
            reset_text: Some("3pm".to_string()),
        }
    }

    /// The happy path, one observation at a time: blocked → explain → label.
    #[test]
    fn a_blocked_claude_agent_is_explained_then_labelled() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);

        assert_eq!(
            state.observe_agent(&blocked, LEASE, now),
            WatchAction::Explain {
                pane_id: "pane_1".into()
            }
        );
        let action = state.explained(&blocked, Some(limit()), LEASE, now);
        assert_eq!(
            action,
            WatchAction::Label {
                pane_id: "pane_1".into(),
                name: Some("a1".into()),
                account: "work".into(),
                limit: limit(),
                lease: LEASE,
                announce: true,
            }
        );
        assert!(state.is_labelled("pane_1"));
    }

    /// The debounce the plan asks for: a second poll inside the window costs
    /// nothing, because `agent.explain` re-runs detection on the server.
    #[test]
    fn a_second_look_inside_the_debounce_window_asks_nothing() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);

        assert!(matches!(
            state.observe_agent(&blocked, LEASE, now),
            WatchAction::Explain { .. }
        ));
        // No limit found: the agent is blocked on something else.
        assert_eq!(
            state.explained(&blocked, None, LEASE, now),
            WatchAction::Nothing
        );
        assert_eq!(
            state.observe_agent(&blocked, LEASE, now + Duration::from_secs(1)),
            WatchAction::Nothing
        );
        assert!(matches!(
            state.observe_agent(&blocked, LEASE, now + EXPLAIN_DEBOUNCE),
            WatchAction::Explain { .. }
        ));
    }

    /// An agent parked on a permission prompt must not cost one explain every
    /// few seconds for as long as the human is away.
    #[test]
    fn a_blocked_agent_that_is_not_limited_backs_off() {
        let mut state = WatchState::new();
        let mut now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);

        for _ in 0..EPISODE_EXPLAIN_BUDGET {
            assert!(matches!(
                state.observe_agent(&blocked, LEASE, now),
                WatchAction::Explain { .. }
            ));
            assert_eq!(
                state.explained(&blocked, None, LEASE, now),
                WatchAction::Nothing
            );
            now += EXPLAIN_DEBOUNCE;
        }
        assert_eq!(
            state.observe_agent(&blocked, LEASE, now),
            WatchAction::Nothing,
            "the budget is spent; the pane rests"
        );
        assert!(
            matches!(
                state.observe_agent(&blocked, LEASE, now + BLOCKED_RECHECK),
                WatchAction::Explain { .. }
            ),
            "but a limit that arrives later is still found"
        );
    }

    /// PR 9's region is `after_last_horizontal_rule`, so a limit that scrolls
    /// above the live prompt box stops matching by itself and the agent can
    /// land on any status at all. The clear must not wait for `idle`.
    #[test]
    fn any_non_blocked_status_clears_the_label() {
        for status in [
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Done,
            AgentStatus::Unknown,
        ] {
            let mut state = WatchState::new();
            let now = Instant::now();
            let blocked = agent("pane_1", AgentStatus::Blocked);
            let _ = state.observe_agent(&blocked, LEASE, now);
            let _ = state.explained(&blocked, Some(limit()), LEASE, now);
            assert!(state.is_labelled("pane_1"));

            let action = state.observe_agent(&agent("pane_1", status), LEASE, now);
            assert_eq!(
                action,
                WatchAction::Clear {
                    pane_id: "pane_1".into(),
                    name: Some("a1".into()),
                    account: "work".into(),
                    restore_state: Some("ok".into()),
                },
                "{status:?} must clear the label"
            );
            assert!(!state.is_labelled("pane_1"));
        }
    }

    /// The one rule that keeps the watcher honest: it labels only agents whose
    /// account it can name, and only Claude ones.
    #[test]
    fn an_agent_the_watcher_cannot_attribute_is_left_alone() {
        let mut state = WatchState::new();
        let now = Instant::now();

        let mut no_account = agent("pane_1", AgentStatus::Blocked);
        no_account.account = None;
        assert_eq!(
            state.observe_agent(&no_account, LEASE, now),
            WatchAction::Nothing
        );
        assert_eq!(
            state.explained(&no_account, Some(limit()), LEASE, now),
            WatchAction::Nothing,
            "not even an explicit limit may label an unattributed agent"
        );

        let mut empty_account = agent("pane_2", AgentStatus::Blocked);
        empty_account.account = Some(String::new());
        assert_eq!(
            state.observe_agent(&empty_account, LEASE, now),
            WatchAction::Nothing
        );

        let mut other_agent = agent("pane_3", AgentStatus::Blocked);
        other_agent.agent = Some("codex".to_string());
        assert_eq!(
            state.observe_agent(&other_agent, LEASE, now),
            WatchAction::Nothing
        );

        let mut unknown_agent = agent("pane_4", AgentStatus::Blocked);
        unknown_agent.agent = None;
        assert_eq!(
            state.observe_agent(&unknown_agent, LEASE, now),
            WatchAction::Nothing
        );

        assert_eq!(state.tracked(), 0, "nothing unattributable is remembered");
    }

    /// A pane that acquires an account token later is picked up, and one that
    /// loses it is dropped without the watcher writing anything.
    #[test]
    fn an_account_token_that_appears_or_vanishes_is_followed() {
        let mut state = WatchState::new();
        let now = Instant::now();

        let mut unattributed = agent("pane_1", AgentStatus::Blocked);
        unattributed.account = None;
        assert_eq!(
            state.observe_agent(&unattributed, LEASE, now),
            WatchAction::Nothing
        );

        let blocked = agent("pane_1", AgentStatus::Blocked);
        assert!(matches!(
            state.observe_agent(&blocked, LEASE, now),
            WatchAction::Explain { .. }
        ));
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);
        assert!(state.is_labelled("pane_1"));

        assert_eq!(
            state.observe_agent(&unattributed, LEASE, now),
            WatchAction::Nothing,
            "an agent that lost its account is forgotten, not cleared under a \
             name the watcher no longer knows"
        );
        assert!(!state.is_labelled("pane_1"));
    }

    /// The server restart case: tokens are in-memory only, so after one the
    /// label is simply gone. The watcher must notice and put it back rather
    /// than believing its own memory.
    #[test]
    fn a_label_that_went_missing_is_reported_again() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let _ = state.observe_agent(&blocked, LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);

        // The server still shows the label: nothing to do until the lease is
        // halfway through.
        assert_eq!(
            state.observe_agent(&labelled("pane_1"), LEASE, now),
            WatchAction::Nothing
        );

        // The label is gone (a restart, an expired lease, another writer).
        let action = state.observe_agent(&blocked, LEASE, now + Duration::from_secs(1));
        assert_eq!(
            action,
            WatchAction::Label {
                pane_id: "pane_1".into(),
                name: Some("a1".into()),
                account: "work".into(),
                limit: limit(),
                lease: LEASE,
                announce: true,
            }
        );
    }

    /// The lease is renewed before the server drops it, and a renewal is not
    /// announced: nothing changed for the reader.
    #[test]
    fn a_lease_is_renewed_halfway_through_and_says_nothing() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let _ = state.observe_agent(&blocked, LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);

        assert_eq!(
            state.observe_agent(
                &labelled("pane_1"),
                LEASE,
                now + LEASE / 2 - Duration::from_millis(1)
            ),
            WatchAction::Nothing
        );
        let action = state.observe_agent(&labelled("pane_1"), LEASE, now + LEASE / 2);
        assert_eq!(
            action,
            WatchAction::Label {
                pane_id: "pane_1".into(),
                name: Some("a1".into()),
                account: "work".into(),
                limit: limit(),
                lease: LEASE,
                announce: false,
            }
        );
    }

    /// The clear restores what the label replaced. `unverified` is a real
    /// answer the launcher gives on a platform that cannot read a process
    /// environment, and overwriting it with `ok` on the way out would upgrade
    /// an unverified agent to a verified one for free.
    #[test]
    fn the_clear_restores_the_state_the_label_replaced() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let mut blocked = agent("pane_1", AgentStatus::Blocked);
        blocked.account_state = Some(AccountState::Unverified.as_str().to_string());

        let _ = state.observe_agent(&blocked, LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);

        let mut recovered = agent("pane_1", AgentStatus::Idle);
        recovered.account_state = Some(AccountState::Limited.as_str().to_string());
        assert_eq!(
            state.observe_agent(&recovered, LEASE, now),
            WatchAction::Clear {
                pane_id: "pane_1".into(),
                name: Some("a1".into()),
                account: "work".into(),
                restore_state: Some("unverified".into()),
            }
        );
    }

    /// An agent that never carried an `account_state` gets the token removed,
    /// not invented: absent means "herdr does not know".
    #[test]
    fn a_clear_without_a_previous_state_removes_the_token() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let mut blocked = agent("pane_1", AgentStatus::Blocked);
        blocked.account_state = None;

        let _ = state.observe_agent(&blocked, LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);
        assert_eq!(
            state.observe_agent(
                &agent_without_state("pane_1", AgentStatus::Idle),
                LEASE,
                now
            ),
            WatchAction::Clear {
                pane_id: "pane_1".into(),
                name: Some("a1".into()),
                account: "work".into(),
                restore_state: None,
            }
        );
    }

    fn agent_without_state(pane: &str, status: AgentStatus) -> WatchAgent {
        let mut agent = agent(pane, status);
        agent.account_state = None;
        agent
    }

    /// A `switch-account` under the watcher's feet: the label belonged to the
    /// old profile, so the episode restarts against the new one rather than a
    /// stale limit following the agent to a fresh account.
    #[test]
    fn a_pane_that_changed_account_starts_a_new_episode() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let _ = state.observe_agent(&blocked, LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);
        assert!(state.is_labelled("pane_1"));

        let mut moved = agent("pane_1", AgentStatus::Blocked);
        moved.account = Some("perso".to_string());
        assert_eq!(
            state.observe_agent(&moved, LEASE, now),
            WatchAction::Explain {
                pane_id: "pane_1".into()
            }
        );
        assert!(!state.is_labelled("pane_1"));
        assert_eq!(
            state.explained(&blocked, Some(limit()), LEASE, now),
            WatchAction::Nothing,
            "an answer about the old account cannot label the new one"
        );
    }

    /// An agent that stopped being blocked between the poll and the explain's
    /// answer must not be labelled by that answer.
    #[test]
    fn an_agent_that_recovered_before_the_answer_is_not_labelled() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let _ = state.observe_agent(&blocked, LEASE, now);

        assert_eq!(
            state.explained(
                &agent("pane_1", AgentStatus::Idle),
                Some(limit()),
                LEASE,
                now
            ),
            WatchAction::Nothing
        );
        assert!(!state.is_labelled("pane_1"));
    }

    /// A pane that leaves the agent list is forgotten: its metadata went with
    /// the process (`applies_to_source`), and reporting against a pane the
    /// server no longer lists would only produce errors.
    #[test]
    fn an_agent_that_disappeared_is_forgotten() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let _ = state.observe(std::slice::from_ref(&blocked), LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);
        assert_eq!(state.tracked(), 1);

        let actions = state.observe(&[], LEASE, now);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(state.tracked(), 0);
    }

    /// The exit path: every label the watcher still holds is handed back once.
    #[test]
    fn a_graceful_exit_drains_every_label_exactly_once() {
        let mut state = WatchState::new();
        let now = Instant::now();
        for pane in ["pane_1", "pane_2"] {
            let blocked = agent(pane, AgentStatus::Blocked);
            let _ = state.observe_agent(&blocked, LEASE, now);
            let _ = state.explained(&blocked, Some(limit()), LEASE, now);
        }

        let clears = state.drain_clears();
        assert_eq!(clears.len(), 2, "{clears:?}");
        assert_eq!(clears[0].pane_id(), Some("pane_1"));
        assert_eq!(clears[1].pane_id(), Some("pane_2"));
        assert!(
            matches!(&clears[0], WatchAction::Clear { name, .. } if name.as_deref() == Some("a1")),
            "an exit clear names the agent it labelled: {:?}",
            clears[0]
        );
        assert!(
            state.drain_clears().is_empty(),
            "a drained label is not cleared twice"
        );
    }

    /// A refused report is not a written one.
    #[test]
    fn a_label_the_server_refused_is_retried() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let _ = state.observe_agent(&blocked, LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);
        state.label_failed("pane_1");
        assert!(!state.is_labelled("pane_1"));

        // The debounce still holds, so the retry is the next explain window.
        assert!(matches!(
            state.observe_agent(&blocked, LEASE, now + EXPLAIN_DEBOUNCE),
            WatchAction::Explain { .. }
        ));
    }

    /// A refused clear is remembered, so the exit path tries again.
    #[test]
    fn a_clear_the_server_refused_is_remembered() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let _ = state.observe_agent(&blocked, LEASE, now);
        let _ = state.explained(&blocked, Some(limit()), LEASE, now);
        let _ = state.observe_agent(&agent("pane_1", AgentStatus::Idle), LEASE, now);
        assert!(!state.is_labelled("pane_1"));

        state.clear_failed("pane_1", limit(), now);
        assert!(state.is_labelled("pane_1"));
        assert_eq!(state.drain_clears().len(), 1);
    }

    /// A long-running process reading a list it does not control keeps a bound
    /// on what it remembers.
    #[test]
    fn tracked_panes_are_bounded() {
        let mut state = WatchState::new();
        let now = Instant::now();
        let agents: Vec<_> = (0..MAX_TRACKED_PANES + 8)
            .map(|index| agent(&format!("pane_{index}"), AgentStatus::Blocked))
            .collect();

        let actions = state.observe(&agents, LEASE, now);
        assert_eq!(state.tracked(), MAX_TRACKED_PANES);
        assert_eq!(actions.len(), MAX_TRACKED_PANES);
        assert!(state.overflowed());
    }

    /// The lease is always renewable and always within the server's limits.
    #[test]
    fn the_lease_stays_inside_the_servers_ttl_range() {
        for interval in [
            Duration::from_millis(1),
            Duration::from_millis(500),
            Duration::from_secs(5),
            Duration::from_secs(300),
            Duration::from_secs(86_400),
        ] {
            let lease = lease_for(interval);
            assert!(lease >= Duration::from_millis(1), "{interval:?}");
            assert!(lease <= Duration::from_millis(86_400_000), "{interval:?}");
            assert!(
                lease / 2 >= interval || lease == Duration::from_millis(86_400_000),
                "a lease must be renewable within one poll: {interval:?} -> {lease:?}"
            );
        }
    }

    /// The watcher's whole vocabulary, pinned. A variant that could type into a
    /// pane, prompt an agent or switch an account would break decision (d) of
    /// the epic, and this is the test that would have to be changed to add one.
    #[test]
    fn the_watcher_can_only_look_and_label() {
        let now = Instant::now();
        let mut state = WatchState::new();
        let blocked = agent("pane_1", AgentStatus::Blocked);
        let mut seen = Vec::new();
        seen.push(state.observe_agent(&blocked, LEASE, now));
        seen.push(state.explained(&blocked, Some(limit()), LEASE, now));
        seen.push(state.observe_agent(&agent("pane_1", AgentStatus::Idle), LEASE, now));
        seen.push(state.observe_agent(&agent("pane_2", AgentStatus::Idle), LEASE, now));

        for action in seen {
            assert!(
                matches!(
                    action,
                    WatchAction::Explain { .. }
                        | WatchAction::Label { .. }
                        | WatchAction::Clear { .. }
                        | WatchAction::Nothing
                ),
                "unexpected action: {action:?}"
            );
        }
    }
}
