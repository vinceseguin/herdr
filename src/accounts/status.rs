//! What `herdr account status` reports, assembled as pure data (fork).
//!
//! Two facts about one profile, joined here so the CLI and (later) the TUI
//! cannot disagree about either:
//!
//! * **health** — does the directory exist, is a credentials file present and
//!   private, is the herdr session hook installed, are the shared symlinks
//!   still pointing somewhere. All of that comes from
//!   [`crate::accounts::layout::inspect`], which never opens
//!   `.credentials.json`;
//! * **placement** — which running agents claim this profile, from the
//!   `account` / `account_state` metadata tokens the launcher reports.
//!
//! Nothing here reads a secret. The identity shown is `oauthAccount`'s
//! display fields (email, organization, plan) out of `.claude.json`, which is
//! the same thing Claude Code prints in its own status line; the credentials
//! file contributes its existence and its unix mode and nothing else.
//!
//! Placement is *reported*, not verified: `account_state` is whatever the
//! launcher or the switch driver last graded, and an agent that carries no
//! account token at all is simply not on any profile as far as herdr knows.
//! A token value this build does not recognise is printed verbatim and marked
//! unknown rather than guessed — a newer herdr may report a state this one has
//! never heard of.

use crate::accounts::layout::{AccountIdentity, ProfileInspection};
use crate::accounts::limit::UsageLimit;
use crate::accounts::profile::{AccountProfile, Profiles};
use crate::accounts::tokens::{AccountState, ACCOUNT_STATE_TOKEN, ACCOUNT_TOKEN};
use crate::api::schema::{AgentInfo, AgentStatus};

/// One running agent, reduced to the facts a profile report needs.
///
/// Split from [`AgentInfo`] so the assembly below is testable without a
/// server and so a schema change cannot silently reshape the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFact {
    pub pane_id: String,
    pub name: Option<String>,
    pub agent_status: AgentStatus,
    /// `tokens.account`: the profile this agent was launched under.
    pub account: Option<String>,
    /// `tokens.account_state`, verbatim — including a value this build does
    /// not know.
    pub account_state: Option<String>,
    /// Set only when herdr's own detector said so, which costs an
    /// `agent.explain` and is therefore the caller's decision rather than a
    /// field [`AgentFact::from_agent_info`] can fill in.
    pub limit: Option<UsageLimit>,
}

impl AgentFact {
    pub fn from_agent_info(agent: &AgentInfo) -> Self {
        Self {
            pane_id: agent.pane_id.clone(),
            name: agent.name.clone(),
            agent_status: agent.agent_status,
            account: agent.tokens.get(ACCOUNT_TOKEN).cloned(),
            account_state: agent.tokens.get(ACCOUNT_STATE_TOKEN).cloned(),
            limit: None,
        }
    }

    /// Record what the detector said about this agent's account usage.
    pub fn with_limit(mut self, limit: Option<UsageLimit>) -> Self {
        self.limit = limit;
        self
    }
}

/// An agent as it appears under one profile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentOnAccount {
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub agent_status: AgentStatus,
    /// The reported state, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_state: Option<String>,
    /// False when `account_state` carries a value this build cannot interpret.
    /// Reported rather than dropped: an older herdr reading a newer one's
    /// token must show what it saw, not pretend the agent has no state.
    pub account_state_known: bool,
    /// Present only when herdr's detector matched the usage-limit rule on this
    /// agent's screen. Absent means "not seen", never "not limited": a status
    /// run without a server, or against an agent that was not blocked when it
    /// looked, never asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<UsageLimit>,
}

impl AgentOnAccount {
    fn from_fact(fact: &AgentFact) -> Self {
        let account_state = fact.account_state.clone();
        let account_state_known = account_state
            .as_deref()
            .is_none_or(|value| AccountState::parse(value).is_some());
        Self {
            pane_id: fact.pane_id.clone(),
            name: fact.name.clone(),
            agent_status: fact.agent_status,
            account_state,
            account_state_known,
            limit: fact.limit.clone(),
        }
    }
}

/// One profile's row in `herdr account status`.
///
/// Deliberately a superset of `herdr account list`'s row — same field names,
/// same meanings — so a reader who learned one already knows the other.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AccountStatus {
    pub name: String,
    pub agent: &'static str,
    pub config_dir: String,
    pub default: bool,
    pub origin: &'static str,
    pub dir_exists: bool,
    /// A credentials file is present. Its contents were never read.
    pub logged_in: bool,
    /// `Some(false)` when that file is readable by more than its owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials_mode_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<AccountIdentity>,
    pub hook_installed: bool,
    pub broken_links: Vec<String>,
    /// Agents claiming this profile, or `None` when no server could be asked.
    ///
    /// `null` and `[]` say different things — "herdr does not know" versus
    /// "herdr knows there are none" — so the distinction survives into JSON
    /// rather than collapsing into an empty list.
    pub agents: Option<Vec<AgentOnAccount>>,
}

/// Join profiles, their health and the running agents into one report.
///
/// `inspect` is a closure so this stays free of I/O: the CLI passes
/// `|profile| layout::inspect(profile, InspectOptions::with_identity())`, a
/// test passes a canned map.
///
/// `only` narrows the report to one profile *before* `inspect` runs, so
/// `herdr account status <name>` never parses another profile's `.claude.json`
/// — megabytes on a real installation. Which profile is the default is still
/// decided against the whole merged view, so a one-profile report says the
/// same thing the full one would.
pub fn assemble(
    profiles: &Profiles,
    only: Option<&str>,
    inspect: impl Fn(&AccountProfile) -> ProfileInspection,
    agents: Option<&[AgentFact]>,
) -> Vec<AccountStatus> {
    let default = profiles
        .default_profile()
        .map(|profile| profile.name.clone());
    profiles
        .iter()
        .filter(|profile| only.is_none_or(|name| profile.name == name))
        .map(|profile| {
            let inspection = inspect(profile);
            let on_account = agents.map(|agents| {
                agents
                    .iter()
                    .filter(|fact| fact.account.as_deref() == Some(profile.name.as_str()))
                    .map(AgentOnAccount::from_fact)
                    .collect()
            });
            AccountStatus {
                name: profile.name.clone(),
                agent: profile.agent.as_str(),
                config_dir: profile.config_dir.display().to_string(),
                default: default.as_deref() == Some(profile.name.as_str()),
                origin: profile.origin.as_str(),
                dir_exists: inspection.dir_exists,
                logged_in: inspection.logged_in,
                credentials_mode_ok: inspection.credentials_mode_ok,
                identity: inspection.identity,
                hook_installed: inspection.hook_installed,
                broken_links: inspection
                    .broken_links
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect(),
                agents: on_account,
            }
        })
        .collect()
}

/// Agents whose `account` token names a profile that is not configured.
///
/// They belong in no row above, and dropping them silently would hide exactly
/// the situation a reader needs to see: an agent still running under a profile
/// that has since been removed or renamed.
pub fn agents_on_unknown_profiles<'a>(
    profiles: &Profiles,
    agents: &'a [AgentFact],
) -> Vec<&'a AgentFact> {
    agents
        .iter()
        .filter(|fact| match fact.account.as_deref() {
            Some(name) => profiles.get(name).is_none(),
            None => false,
        })
        .collect()
}

/// The human-readable report.
pub fn render_text(statuses: &[AccountStatus]) -> String {
    if statuses.is_empty() {
        return "No account profiles configured.\n\
                Add [[accounts]] to config.toml (see `herdr --default-config`), \
                or run `herdr account add <name>`.\n"
            .to_string();
    }

    let mut out = String::new();
    for (index, status) in statuses.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut tags = vec![status.origin.to_string()];
        if status.default {
            tags.push("default".to_string());
        }
        out.push_str(&format!("{} ({})\n", status.name, tags.join(", ")));
        out.push_str(&field(
            "dir",
            &if status.dir_exists {
                status.config_dir.clone()
            } else {
                format!("{} (missing)", status.config_dir)
            },
        ));
        out.push_str(&field("identity", &render_identity(status)));
        out.push_str(&field("logged in", &render_login(status)));
        out.push_str(&field(
            "hook",
            if status.hook_installed {
                "installed"
            } else {
                "missing (session ids are not reported; switching accounts will not work)"
            },
        ));
        for link in &status.broken_links {
            out.push_str(&field("broken link", link));
        }
        out.push_str(&field("agents", &render_agents(status)));
    }
    out
}

fn field(label: &str, value: &str) -> String {
    format!("  {label:<12} {value}\n")
}

fn render_identity(status: &AccountStatus) -> String {
    let Some(identity) = status.identity.as_ref() else {
        return if status.logged_in {
            "unknown (no oauthAccount in .claude.json)".to_string()
        } else {
            "unknown".to_string()
        };
    };
    let parts: Vec<&str> = [
        identity.email.as_deref(),
        identity.organization.as_deref(),
        identity.plan.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "unknown".to_string()
    } else {
        parts.join(" · ")
    }
}

fn render_login(status: &AccountStatus) -> String {
    if !status.logged_in {
        return format!("no (run `herdr account login {}`)", status.name);
    }
    match status.credentials_mode_ok {
        // Never the mode itself, and never the file: only the verdict.
        Some(false) => "yes (credentials file is readable by others; chmod 600 it)".to_string(),
        _ => "yes".to_string(),
    }
}

fn render_agents(status: &AccountStatus) -> String {
    let Some(agents) = status.agents.as_ref() else {
        return "- (no herdr server answered)".to_string();
    };
    if agents.is_empty() {
        return "none".to_string();
    }
    agents
        .iter()
        .map(|agent| {
            let name = agent.name.as_deref().unwrap_or("(unnamed)");
            let state = match (agent.account_state.as_deref(), agent.account_state_known) {
                (Some(state), true) => format!(" {state}"),
                (Some(state), false) => format!(" {state} (unknown to this herdr)"),
                (None, _) => String::new(),
            };
            // The one blocked reason this report can act on, so it is spelled
            // out next to the agent rather than left to `agent explain`.
            let limit = match agent.limit.as_ref() {
                Some(limit) => match limit.reset_text.as_deref() {
                    Some(reset) => format!(" — usage limit, resets {reset}"),
                    None => " — usage limit".to_string(),
                },
                None => String::new(),
            };
            format!(
                "{name} on {} {}{state}{limit}",
                agent.pane_id,
                agent_status_str(agent.agent_status)
            )
        })
        .collect::<Vec<_>>()
        // One continuation line per extra agent, indented under the column
        // `field` opens: two spaces, a twelve-wide label, one space.
        .join("\n               ")
}

/// The wire spelling of an agent status, matched exhaustively so a new upstream
/// variant is a compile error rather than a silently wrong word.
fn agent_status_str(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::accounts::profile::{AccountAgent, ProfileOrigin};

    fn profile(name: &str, default: bool, origin: ProfileOrigin) -> AccountProfile {
        AccountProfile {
            name: name.to_string(),
            agent: AccountAgent::Claude,
            config_dir: PathBuf::from(format!("/p/{name}")),
            default,
            origin,
        }
    }

    fn profiles() -> Profiles {
        Profiles::test_new(vec![
            profile("perso", true, ProfileOrigin::Config),
            profile("work", false, ProfileOrigin::Store),
        ])
    }

    fn healthy() -> ProfileInspection {
        ProfileInspection {
            dir_exists: true,
            logged_in: true,
            credentials_mode_ok: Some(true),
            identity: Some(AccountIdentity {
                email: Some("someone@example.test".to_string()),
                organization: Some("Example Org".to_string()),
                plan: Some("max".to_string()),
            }),
            hook_installed: true,
            broken_links: Vec::new(),
        }
    }

    fn fact(pane: &str, name: &str, account: Option<&str>, state: Option<&str>) -> AgentFact {
        AgentFact {
            pane_id: pane.to_string(),
            name: Some(name.to_string()),
            agent_status: AgentStatus::Working,
            account: account.map(str::to_string),
            account_state: state.map(str::to_string),
            limit: None,
        }
    }

    #[test]
    fn agents_are_matched_to_the_profile_their_token_names() {
        let agents = [
            fact("%1.1", "a1", Some("work"), Some("ok")),
            fact("%1.2", "a2", Some("perso"), Some("unverified")),
            fact("%1.3", "a3", Some("work"), Some("mismatch")),
        ];
        let statuses = assemble(&profiles(), None, |_| healthy(), Some(&agents));

        let perso = &statuses[0];
        assert_eq!(perso.name, "perso");
        assert!(perso.default);
        assert_eq!(perso.origin, "config");
        let on_perso = perso.agents.as_ref().expect("agents were listed");
        assert_eq!(on_perso.len(), 1);
        assert_eq!(on_perso[0].name.as_deref(), Some("a2"));

        let work = &statuses[1];
        assert!(!work.default);
        assert_eq!(work.origin, "store");
        let on_work = work.agents.as_ref().expect("agents were listed");
        assert_eq!(on_work.len(), 2);
        assert_eq!(on_work[0].pane_id, "%1.1");
        assert_eq!(on_work[1].account_state.as_deref(), Some("mismatch"));
    }

    /// An agent with no `account` token was not launched through herdr's
    /// account tooling. It belongs to no profile, and claiming it for the
    /// default one would be an invention.
    #[test]
    fn an_agent_without_an_account_token_belongs_to_no_profile() {
        let agents = [fact("%1.1", "a1", None, None)];
        let statuses = assemble(&profiles(), None, |_| healthy(), Some(&agents));
        for status in &statuses {
            assert_eq!(status.agents.as_deref(), Some(&[][..]), "{status:#?}");
        }
        assert!(agents_on_unknown_profiles(&profiles(), &agents).is_empty());
    }

    #[test]
    fn an_agent_naming_a_profile_that_is_gone_is_surfaced_not_dropped() {
        let agents = [
            fact("%1.1", "a1", Some("retired"), Some("ok")),
            fact("%1.2", "a2", Some("work"), Some("ok")),
        ];
        let orphans = agents_on_unknown_profiles(&profiles(), &agents);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].pane_id, "%1.1");
    }

    /// A newer herdr may report a state this build has never heard of. Showing
    /// it verbatim and marking it unknown is honest; dropping it would hide an
    /// agent's state entirely.
    #[test]
    fn an_unknown_account_state_is_kept_verbatim_and_marked() {
        let agents = [
            fact("%1.1", "a1", Some("work"), Some("quota_paused")),
            fact("%1.2", "a2", Some("work"), Some("ok")),
            fact("%1.3", "a3", Some("work"), None),
        ];
        let statuses = assemble(&profiles(), None, |_| healthy(), Some(&agents));
        let on_work = statuses[1].agents.as_ref().expect("agents");
        assert_eq!(on_work[0].account_state.as_deref(), Some("quota_paused"));
        assert!(!on_work[0].account_state_known);
        assert!(on_work[1].account_state_known);
        assert!(
            on_work[2].account_state_known,
            "no state at all is not an unknown state"
        );

        let text = render_text(&statuses);
        assert!(
            text.contains("quota_paused (unknown to this herdr)"),
            "{text}"
        );
    }

    /// `null` agents and `[]` agents are different answers: one is "no server
    /// was reachable", the other "the server has none".
    #[test]
    fn an_unreachable_server_leaves_the_agents_column_unknown_not_empty() {
        let statuses = assemble(&profiles(), None, |_| healthy(), None);
        assert!(statuses[0].agents.is_none());
        let text = render_text(&statuses);
        assert!(text.contains("- (no herdr server answered)"), "{text}");

        let statuses = assemble(&profiles(), None, |_| healthy(), Some(&[]));
        assert_eq!(statuses[0].agents.as_deref(), Some(&[][..]));
        assert!(render_text(&statuses).contains("none"));
    }

    #[test]
    fn render_text_is_the_golden_report() {
        let agents = [
            fact("%1.1", "a1", Some("work"), Some("ok")),
            fact("%1.4", "a4", Some("work"), None),
        ];
        let mut statuses = assemble(&profiles(), None, |_| healthy(), Some(&agents));
        statuses[0].logged_in = false;
        statuses[0].credentials_mode_ok = None;
        statuses[0].identity = None;
        statuses[0].hook_installed = false;
        statuses[0].dir_exists = false;
        statuses[1].broken_links = vec!["/p/work/projects".to_string()];

        assert_eq!(
            render_text(&statuses),
            "\
perso (config, default)
  dir          /p/perso (missing)
  identity     unknown
  logged in    no (run `herdr account login perso`)
  hook         missing (session ids are not reported; switching accounts will not work)
  agents       none

work (store)
  dir          /p/work
  identity     someone@example.test · Example Org · max
  logged in    yes
  hook         installed
  broken link  /p/work/projects
  agents       a1 on %1.1 working ok
               a4 on %1.4 working
"
        );

        // A second agent lines up under the first rather than one column left.
        let report = render_text(&statuses);
        let column = |needle: &str| {
            let line = report
                .lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is in the report: {report}"));
            line.find(needle).unwrap_or_default()
        };
        assert_eq!(column("a1 on"), column("a4 on"), "{report}");
    }

    #[test]
    fn a_world_readable_credentials_file_is_called_out_without_being_read() {
        let mut statuses = assemble(&profiles(), None, |_| healthy(), Some(&[]));
        statuses[0].credentials_mode_ok = Some(false);
        let text = render_text(&statuses);
        assert!(text.contains("readable by others"), "{text}");
        assert!(!text.contains(".credentials.json"), "{text}");
    }

    /// `status <name>` must not pay for the profiles it does not print: the
    /// identity read it does is a whole `.claude.json` parse.
    #[test]
    fn a_named_report_inspects_only_that_profile_and_keeps_the_default_flag() {
        let inspected = std::cell::RefCell::new(Vec::new());
        let statuses = assemble(
            &profiles(),
            Some("perso"),
            |profile| {
                inspected.borrow_mut().push(profile.name.clone());
                healthy()
            },
            Some(&[]),
        );
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].name, "perso");
        assert!(statuses[0].default);
        assert_eq!(inspected.into_inner(), vec!["perso".to_string()]);

        // And a non-default profile still reports as non-default on its own.
        let statuses = assemble(&profiles(), Some("work"), |_| healthy(), Some(&[]));
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].default);

        assert!(assemble(&profiles(), Some("nope"), |_| healthy(), Some(&[])).is_empty());
    }

    #[test]
    fn an_empty_report_says_how_to_make_one() {
        let text = render_text(&[]);
        assert!(text.contains("No account profiles configured."), "{text}");
        assert!(text.contains("herdr account add"), "{text}");
    }

    /// The report is display-only. Nothing that could carry a secret — the
    /// credentials file's name, its mode, an OAuth field — reaches the output.
    #[test]
    fn a_status_row_carries_no_credential_material() {
        let statuses = assemble(&profiles(), None, |_| healthy(), Some(&[]));
        let encoded = serde_json::to_string(&statuses)
            .expect("encode")
            .to_ascii_lowercase();
        for forbidden in ["credentials.json", "oauth", "token", "password", "secret"] {
            assert!(
                !encoded.contains(forbidden),
                "{forbidden} leaked into an account status row: {encoded}"
            );
        }
        assert!(
            encoded.contains("credentials_mode_ok"),
            "the mode verdict is still reported: {encoded}"
        );
    }

    #[test]
    fn agent_facts_come_straight_off_the_tokens() {
        let mut agent = AgentInfo {
            terminal_id: "t1".to_string(),
            name: Some("a1".to_string()),
            agent: Some("claude".to_string()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Blocked,
            screen_detection_skipped: false,
            state_labels: Default::default(),
            tokens: Default::default(),
            agent_session: None,
            workspace_id: "w1".to_string(),
            tab_id: "t1".to_string(),
            pane_id: "%1.1".to_string(),
            focused: false,
            launch_pending: false,
            interactive_ready: true,
            state_change_seq: 0,
            cwd: None,
            foreground_cwd: None,
            revision: 1,
        };
        agent
            .tokens
            .insert(ACCOUNT_TOKEN.to_string(), "work".to_string());
        agent
            .tokens
            .insert(ACCOUNT_STATE_TOKEN.to_string(), "ok".to_string());

        let fact = AgentFact::from_agent_info(&agent);
        assert_eq!(fact.pane_id, "%1.1");
        assert_eq!(fact.name.as_deref(), Some("a1"));
        assert_eq!(fact.agent_status, AgentStatus::Blocked);
        assert_eq!(fact.account.as_deref(), Some("work"));
        assert_eq!(fact.account_state.as_deref(), Some("ok"));

        agent.tokens.clear();
        let fact = AgentFact::from_agent_info(&agent);
        assert_eq!(fact.account, None);
        assert_eq!(fact.account_state, None);
    }

    /// A limit herdr saw is a limit the report says out loud, in both shapes.
    #[test]
    fn a_usage_limit_reaches_the_row_and_the_text_report() {
        let profiles = Profiles::test_new(vec![profile("perso", true, ProfileOrigin::Config)]);
        let limited = fact("%1.1", "a1", Some("perso"), Some("ok")).with_limit(Some(
            crate::accounts::limit::UsageLimit {
                reset_text: Some("3pm".to_string()),
            },
        ));
        let statuses = assemble(&profiles, None, |_| healthy(), Some(&[limited]));

        let agents = statuses[0].agents.as_ref().expect("agents");
        assert_eq!(
            agents[0]
                .limit
                .as_ref()
                .and_then(|limit| limit.reset_text.as_deref()),
            Some("3pm")
        );
        let encoded = serde_json::to_string(&statuses).expect("encode");
        assert!(
            encoded.contains(r#""limit":{"reset_text":"3pm"}"#),
            "{encoded}"
        );
        let text = render_text(&statuses);
        assert!(text.contains("usage limit, resets 3pm"), "{text}");
    }

    /// Not asking is not an answer: an agent nobody explained carries no
    /// `limit` key at all, rather than one that reads as "healthy".
    #[test]
    fn an_agent_that_was_never_explained_carries_no_limit_key() {
        let profiles = Profiles::test_new(vec![profile("perso", true, ProfileOrigin::Config)]);
        let statuses = assemble(
            &profiles,
            None,
            |_| healthy(),
            Some(&[fact("%1.1", "a1", Some("perso"), Some("ok"))]),
        );
        let encoded = serde_json::to_string(&statuses).expect("encode");
        assert!(!encoded.contains("limit"), "{encoded}");
        assert!(!render_text(&statuses).contains("usage limit"));
    }
}
