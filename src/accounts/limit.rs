//! Reading a Claude usage limit off a pane, as pure data (fork).
//!
//! Usage limits are per account, so "this agent is blocked because its account
//! is out of usage" is the one blocked state herdr can act on: the way out is
//! another profile. Nothing here decides to switch — it only reads.
//!
//! The evidence is herdr's own detector. `src/detect/manifests/claude.toml`
//! carries a `usage_limit` rule, and `agent.explain` reports which rule
//! matched, so the *classification* is the manifest's job and this module only
//! interprets its verdict. A manifest state is exactly one of
//! `idle|working|blocked|unknown` with nowhere to record a reason, which is
//! why the reason lives out here rather than in the manifest.
//!
//! The reset time is scraped from the same screen the rule matched, and is
//! **display text only**: it is never parsed into an instant, never compared
//! against the clock, and nothing is scheduled from it. A wrong or missing
//! reset time costs a sentence in a report, not a decision.

use regex::Regex;
use std::sync::OnceLock;

/// The rule id in `src/detect/manifests/claude.toml`. The manifest and this
/// constant are one contract; renaming the rule without renaming this hides
/// every limit herdr can see.
pub const USAGE_LIMIT_RULE_ID: &str = "usage_limit";

/// How far up the screen a reset time is looked for.
///
/// The rule's own region is the status area below the live prompt box; this
/// scan is deliberately wider, because the sentence form of the notice sits in
/// the transcript above it. It is still bounded: the screen could be thousands
/// of lines.
const RESET_SCAN_LINES: usize = 24;

/// The longest reset text that will be reported.
///
/// This text is read from a terminal herdr does not own and printed into
/// another one, so it is bounded like every other borrowed string in the
/// account tooling.
const MAX_RESET_TEXT_CHARS: usize = 48;

/// An agent whose account is out of usage.
///
/// Deliberately thin: the only fact worth carrying is when the caller was told
/// the limit lifts, and even that is text.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageLimit {
    /// The reset time exactly as the screen wrote it (`3pm`, `14:00`), or
    /// `None` when the notice did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_text: Option<String>,
}

/// Whether an `agent.explain` result says the usage-limit rule matched.
///
/// Split out so a caller can decide *before* paying for a screen read.
pub fn matched_usage_limit(explain: &serde_json::Value) -> bool {
    explain
        .get("matched_rule")
        .and_then(|rule| rule.get("id"))
        .and_then(serde_json::Value::as_str)
        == Some(USAGE_LIMIT_RULE_ID)
}

/// A usage limit, when `explain` says the rule matched.
///
/// `screen` is the detection read of the same pane. It only ever contributes
/// the reset text: an empty or stale screen degrades to `Some(UsageLimit {
/// reset_text: None })`, never to "not limited", because the verdict belongs
/// to the manifest and not to this scrape.
pub fn classify(explain: &serde_json::Value, screen: &str) -> Option<UsageLimit> {
    if !matched_usage_limit(explain) {
        return None;
    }
    Some(UsageLimit {
        reset_text: reset_text(screen),
    })
}

/// The reset time a limit notice quotes, if it quotes one.
///
/// Read from the bottom up, because the live notice is the last one on screen
/// and an older one scrolled above it is history.
pub fn reset_text(screen: &str) -> Option<String> {
    static PATTERN: OnceLock<Option<Regex>> = OnceLock::new();
    let pattern = PATTERN
        .get_or_init(|| {
            // Same two spellings the manifest gates on, plus the sentence form.
            Regex::new(
                r"(?i)\b(?:resets?|reset)(?:\s+at)?\s+(\d{1,2}(?::\d{2})?\s*(?:[ap]\.?m\.?)?)",
            )
            .ok()
        })
        .as_ref()?;

    screen
        .lines()
        .rev()
        .take(RESET_SCAN_LINES)
        .find_map(|line| {
            pattern
                .captures(line)
                .and_then(|captures| captures.get(1))
                .map(|matched| matched.as_str().trim().to_string())
        })
        .filter(|text| !text.is_empty() && text.chars().count() <= MAX_RESET_TEXT_CHARS)
}

/// One line a human can act on.
///
/// `account` is the profile the limited agent runs under (`None` when herdr
/// never recorded one), `pane_id` the pane it holds, and `alternatives` the
/// other profiles configured for the same agent — the first is offered because
/// suggesting the account that is already limited would be worse than
/// suggesting nothing.
pub fn hint(
    limit: &UsageLimit,
    account: Option<&str>,
    pane_id: &str,
    alternatives: &[&str],
) -> String {
    let mut text = match account {
        Some(account) => format!("usage limit on account {account:?}"),
        None => "usage limit reached".to_string(),
    };
    if let Some(reset) = limit.reset_text.as_deref() {
        text.push_str(&format!(" (resets {reset})"));
    }
    match alternatives.first() {
        Some(other) => text.push_str(&format!(
            "; switch with `herdr agent switch-account {pane_id} {other}`"
        )),
        // No other profile is configured, so naming a command that cannot be
        // completed would be worse than naming the one that fixes that.
        None => text
            .push_str("; no other account profile is configured (see `herdr account add <name>`)"),
    }
    text
}

/// The other profiles a limited agent could move to.
///
/// Pure list arithmetic, kept here so the CLI and the switch preflight cannot
/// disagree about what "another account" means.
pub fn alternatives<'a>(all: &[&'a str], current: Option<&str>) -> Vec<&'a str> {
    all.iter()
        .copied()
        .filter(|name| Some(*name) != current)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::manifest::explain_for_label;
    use crate::detect::AgentState;

    const LIMIT: &str = include_str!("../../tests/fixtures/fork/claude-usage-limit.txt");
    const LIMIT_PROSE: &str =
        include_str!("../../tests/fixtures/fork/claude-usage-limit-negative.txt");
    const IDLE: &str = include_str!("../../tests/fixtures/fork/claude-idle.txt");
    const WORKING: &str = include_str!("../../tests/fixtures/fork/claude-working.txt");
    const PERMISSION: &str = include_str!("../../tests/fixtures/fork/claude-permission-prompt.txt");
    const LIMIT_WITH_DIALOG: &str =
        include_str!("../../tests/fixtures/fork/claude-usage-limit-with-dialog.txt");

    fn matched(screen: &str) -> Option<String> {
        explain_for_label("claude", screen)
            .matched_rule
            .map(|rule| rule.id)
    }

    fn state(screen: &str) -> AgentState {
        explain_for_label("claude", screen).state
    }

    /// The whole point: the captured limit screen is blocked, by *this* rule.
    ///
    /// The fixture is reconstructed rather than live-captured (see
    /// `tests/fixtures/fork/README.md`), so this test proves the rule reads the
    /// screen the fork believes Claude draws — not that Claude draws it.
    #[test]
    fn the_bundled_manifest_reads_the_limit_screen_as_blocked() {
        assert_eq!(matched(LIMIT).as_deref(), Some(USAGE_LIMIT_RULE_ID));
        assert_eq!(state(LIMIT), AgentState::Blocked);
        let explain = explain_for_label("claude", LIMIT);
        assert!(
            explain.visible_blocker,
            "a limited agent is waiting on a human"
        );
    }

    /// The false positive that would matter: a healthy agent marked blocked and
    /// its human told to switch accounts. Prose about limits is not a limit.
    #[test]
    fn a_transcript_that_talks_about_limits_is_not_a_limit() {
        assert_ne!(matched(LIMIT_PROSE).as_deref(), Some(USAGE_LIMIT_RULE_ID));
        assert_eq!(
            state(LIMIT_PROSE),
            AgentState::Idle,
            "the prompt box is the live state: {:#?}",
            explain_for_label("claude", LIMIT_PROSE).matched_rule
        );
        assert_eq!(classify(&explain_json(LIMIT_PROSE), LIMIT_PROSE), None);
    }

    /// The region is the live status area, so a limit notice that has scrolled
    /// above the current prompt box is history rather than state. This is what
    /// keeps a *relaunched* agent from inheriting the previous session's
    /// limit: `herdr agent switch-account` restarts Claude in the same pane,
    /// and the old screen is still there.
    #[test]
    fn a_limit_notice_above_the_live_prompt_box_is_history() {
        let stale = format!("{LIMIT}\n{IDLE}");
        assert_ne!(matched(&stale).as_deref(), Some(USAGE_LIMIT_RULE_ID));
        assert_eq!(state(&stale), AgentState::Idle);
    }

    #[test]
    fn an_idle_prompt_is_not_a_limit() {
        assert_eq!(matched(IDLE).as_deref(), Some("live_prompt_box"));
        assert_eq!(state(IDLE), AgentState::Idle);
    }

    #[test]
    fn a_working_turn_is_not_a_limit() {
        assert_ne!(matched(WORKING).as_deref(), Some(USAGE_LIMIT_RULE_ID));
        assert_eq!(state(WORKING), AgentState::Working);
    }

    /// An ordinary blocked prompt is blocked for its own reason: the limit rule
    /// must not claim it, or `account status` would offer an account switch to
    /// somebody who just has to answer a question.
    #[test]
    fn an_ordinary_permission_prompt_is_blocked_by_its_own_rule() {
        assert_eq!(state(PERMISSION), AgentState::Blocked);
        assert_ne!(matched(PERMISSION).as_deref(), Some(USAGE_LIMIT_RULE_ID));
        assert_eq!(classify(&explain_json(PERMISSION), PERMISSION), None);
    }

    /// A dialog drawn on top of a really limited account. The limit footer is
    /// in the rule's own region and both its positive gates match there, so
    /// only the `not` gates keep the dialog — the thing a human can actually
    /// act on — as the reported blocker. Without them `account status` would
    /// answer "switch accounts" to someone who has to answer a question.
    ///
    /// Both shapes are pinned because they exercise the priority ladder from
    /// opposite sides: `live_blocked_form` sits at 980, one step *below*
    /// `usage_limit`, and `bash_permission_prompt` at 850, well below it.
    #[test]
    fn a_dialog_on_top_of_a_limit_is_reported_as_the_dialog() {
        assert_eq!(
            matched(LIMIT_WITH_DIALOG).as_deref(),
            Some("live_blocked_form"),
            "the form outranks the prompt box and the limit defers to it"
        );
        assert_eq!(state(LIMIT_WITH_DIALOG), AgentState::Blocked);
        assert_eq!(
            classify(&explain_json(LIMIT_WITH_DIALOG), LIMIT_WITH_DIALOG),
            None
        );

        // The Bash approval shape from the live captures, with the same footer
        // under it.
        let approval = concat!(
            "────────────────────────────────────────────────────────────────\n",
            " Bash command\n\n",
            "   rm -rf /tmp/probe\n\n",
            " Do you want to proceed?\n",
            " ❯ 1. Yes\n",
            "   2. Yes, and don't ask again for: rm *\n",
            "   3. No\n\n",
            " Esc to cancel · Tab to amend · ctrl+e to explain\n",
            "  ⏵⏵ auto mode on · 5-hour limit reached ∙ resets 3pm · /upgrade\n",
        );
        assert_eq!(matched(approval).as_deref(), Some("bash_permission_prompt"));
        assert_eq!(state(approval), AgentState::Blocked);
        assert_eq!(classify(&explain_json(approval), approval), None);
    }

    /// The degenerate region, which is the one way transcript text can reach
    /// this rule at all: `after_last_horizontal_rule` falls back to the whole
    /// screen when no horizontal rule is on it, so a Claude that is not
    /// drawing its prompt box puts its transcript in the rule's region. A
    /// paragraph that wraps the limit sentence onto an indented line of its
    /// own satisfies both positive gates; the transcript-marker `not` gate is
    /// what refuses it.
    ///
    /// This is the headline false positive: a healthy agent reported blocked
    /// and its human told to switch accounts.
    #[test]
    fn a_transcript_without_a_prompt_box_is_not_a_limit() {
        let screen = concat!(
            "\u{23fa} Read(docs/limits.md)\n",
            "  \u{23bf}  Read 40 lines\n",
            "\n",
            "\u{23fa} When the account runs out of usage Claude prints\n",
            "  Claude usage limit reached. Your limit will reset at 3pm.\n",
        );
        assert!(
            !screen.contains('\u{2500}'),
            "the point of the case is that no horizontal rule is on screen"
        );
        assert_ne!(matched(screen).as_deref(), Some(USAGE_LIMIT_RULE_ID));
        assert_ne!(state(screen), AgentState::Blocked);
        assert_eq!(classify(&explain_json(screen), screen), None);
    }

    fn explain_json(screen: &str) -> serde_json::Value {
        crate::detect::manifest::explain_to_json_value(&explain_for_label("claude", screen))
    }

    #[test]
    fn classify_reads_the_verdict_from_the_rule_and_the_time_from_the_screen() {
        assert_eq!(
            classify(&explain_json(LIMIT), LIMIT),
            Some(UsageLimit {
                reset_text: Some("3pm".to_string())
            })
        );
    }

    /// The verdict is the manifest's, not the scrape's: a limit whose screen
    /// says nothing about a reset is still a limit.
    #[test]
    fn a_matched_rule_without_a_reset_time_is_still_a_limit() {
        let explain = serde_json::json!({ "matched_rule": { "id": USAGE_LIMIT_RULE_ID } });
        assert_eq!(
            classify(&explain, ""),
            Some(UsageLimit { reset_text: None })
        );
    }

    #[test]
    fn another_rule_is_never_a_limit() {
        let explain = serde_json::json!({ "matched_rule": { "id": "live_blocked_form" } });
        assert!(!matched_usage_limit(&explain));
        assert_eq!(
            classify(&explain, "5-hour limit reached ∙ resets 3pm"),
            None
        );
        assert_eq!(classify(&serde_json::json!({}), ""), None);
        assert_eq!(
            classify(&serde_json::json!({"matched_rule": null}), ""),
            None
        );
    }

    #[test]
    fn reset_text_reads_the_spellings_the_rule_gates_on() {
        assert_eq!(reset_text("  · resets 3pm"), Some("3pm".to_string()));
        assert_eq!(reset_text("resets at 14:00"), Some("14:00".to_string()));
        assert_eq!(
            reset_text("Your limit will reset at 3:30 pm (America/Toronto)."),
            Some("3:30 pm".to_string())
        );
        assert_eq!(reset_text("Weekly limit reached"), None);
        assert_eq!(reset_text(""), None);
    }

    /// The live notice, not a scrolled-past one.
    #[test]
    fn reset_text_prefers_the_lowest_notice() {
        let screen = "5-hour limit reached ∙ resets 9am\n\n5-hour limit reached ∙ resets 3pm\n";
        assert_eq!(reset_text(screen), Some("3pm".to_string()));
    }

    /// A screen herdr does not own cannot make herdr print a wall of text, and
    /// cannot make it scan one either.
    #[test]
    fn reset_text_is_bounded() {
        // The capture is structurally short — two digits, optional minutes,
        // optional meridiem — so a long run of digits yields a short answer
        // rather than the whole line.
        let long = format!("resets {}", "9".repeat(MAX_RESET_TEXT_CHARS + 10));
        let captured = reset_text(&long).expect("a time-shaped prefix is still a time");
        assert!(
            captured.chars().count() <= MAX_RESET_TEXT_CHARS,
            "{captured:?}"
        );
        // And the scan stops: a notice further up than the window is history.
        let far_up = format!("resets 3pm\n{}", "\n".repeat(RESET_SCAN_LINES + 5));
        assert_eq!(reset_text(&far_up), None);
    }

    #[test]
    fn the_hint_names_the_account_the_pane_and_a_way_out() {
        let limit = UsageLimit {
            reset_text: Some("3pm".to_string()),
        };
        assert_eq!(
            hint(&limit, Some("perso"), "p1", &["work"]),
            "usage limit on account \"perso\" (resets 3pm); switch with \
             `herdr agent switch-account p1 work`"
        );
    }

    #[test]
    fn the_hint_offers_no_command_it_cannot_complete() {
        let limit = UsageLimit::default();
        let text = hint(&limit, Some("perso"), "p1", &[]);
        assert!(!text.contains("switch-account"), "{text}");
        assert!(text.contains("herdr account add"), "{text}");
    }

    #[test]
    fn the_hint_survives_an_agent_with_no_recorded_account() {
        let limit = UsageLimit {
            reset_text: Some("14:00".to_string()),
        };
        let text = hint(&limit, None, "p2", &["work", "perso"]);
        assert_eq!(
            text,
            "usage limit reached (resets 14:00); switch with \
             `herdr agent switch-account p2 work`"
        );
    }

    #[test]
    fn alternatives_never_offer_the_account_that_is_already_limited() {
        assert_eq!(
            alternatives(&["perso", "work"], Some("perso")),
            vec!["work"]
        );
        assert_eq!(
            alternatives(&["perso", "work"], None),
            vec!["perso", "work"]
        );
        assert!(alternatives(&["perso"], Some("perso")).is_empty());
    }
}
