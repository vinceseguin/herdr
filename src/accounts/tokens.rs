//! The account metadata vocabulary reported on stock servers (fork).
//!
//! An agent's account is a runtime fact about a pane, so it lives where every
//! other per-agent fact lives: `pane.report_metadata` tokens on the server,
//! surfaced as `AgentInfo.tokens` and `ClientShellAgent.tokens`. This module
//! owns the exact strings, because a launcher, a switch driver, the sidebar
//! snippet in the docs and the fleet report all have to agree on them.
//!
//! Names are the user-facing contract: add, never rename.

// PR 1 ships the vocabulary and its tests before the first producer exists —
// the launcher (PR 4), the switch driver (PR 5), the TUI (PR 7) and
// `account watch` (PR 10) are what report and read these. Without this the
// dead-code lint would force the constants to land in the PR that happens to
// use them first, which is exactly the drift this module exists to prevent.
#![allow(dead_code)]

/// `source` of every metadata report the fork's account tooling makes.
///
/// Fits the server's source grammar (`[A-Za-z0-9:._-]{1,80}`,
/// `src/app/api_helpers.rs`) and is namespaced so it can never collide with an
/// agent integration's own `herdr:*` source.
pub const METADATA_SOURCE: &str = "fork:accounts";

/// `applies_to_source` of those reports: the Claude integration's source.
///
/// Metadata scoped to it is cleared by the server when the Claude process
/// exits (`src/terminal/state.rs` retain logic), so a stale account never
/// outlives the agent that was launched under it.
pub const APPLIES_TO_SOURCE: &str = "herdr:claude";

/// Agent label reported alongside the tokens.
pub const AGENT_LABEL: &str = "claude";

/// Token holding the profile name (`account = "work"`).
pub const ACCOUNT_TOKEN: &str = "account";

/// Token holding how well that name is known to be true.
pub const ACCOUNT_STATE_TOKEN: &str = "account_state";

/// How much evidence there is that an agent really runs under its account.
///
/// Nothing is reported as [`AccountState::Ok`] without evidence: a platform
/// that cannot read a process environment reports [`AccountState::Unverified`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountState {
    /// The process environment was read and names the profile's directory.
    Ok,
    /// The launch applied the profile, but the environment could not be read.
    Unverified,
    /// The environment was read and names a *different* directory.
    Mismatch,
    /// The agent is blocked on this account's usage limit.
    Limited,
    /// The profile has no credentials file; the agent will ask for a login.
    LoggedOut,
}

impl AccountState {
    /// The token value. Kept inside the server's 80-byte value limit.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unverified => "unverified",
            Self::Mismatch => "mismatch",
            Self::Limited => "limited",
            Self::LoggedOut => "logged_out",
        }
    }

    /// Parse a token value written by any version of herdr.
    ///
    /// Unknown values are `None` on purpose: a newer herdr may report a state
    /// this build does not know, and an older reader must ignore it rather
    /// than guess.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ok" => Some(Self::Ok),
            "unverified" => Some(Self::Unverified),
            "mismatch" => Some(Self::Mismatch),
            "limited" => Some(Self::Limited),
            "logged_out" => Some(Self::LoggedOut),
            _ => None,
        }
    }

    /// Every state, so callers (and tests) can enumerate the vocabulary.
    pub const ALL: [Self; 5] = [
        Self::Ok,
        Self::Unverified,
        Self::Mismatch,
        Self::Limited,
        Self::LoggedOut,
    ];
}

impl std::fmt::Display for AccountState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's metadata source grammar, `src/app/api_helpers.rs`.
    fn is_valid_source(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 80
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'_' | b'-')
            })
    }

    #[test]
    fn sources_match_the_server_grammar() {
        assert!(is_valid_source(METADATA_SOURCE));
        assert!(is_valid_source(APPLIES_TO_SOURCE));
    }

    #[test]
    fn token_keys_fit_the_server_limits() {
        for key in [ACCOUNT_TOKEN, ACCOUNT_STATE_TOKEN] {
            assert!(!key.is_empty(), "token key must not be empty");
            assert!(key.len() <= 32, "{key} exceeds the 32-byte key limit");
        }
    }

    #[test]
    fn state_values_fit_the_server_value_limit_and_round_trip() {
        for state in AccountState::ALL {
            let value = state.as_str();
            assert!(value.len() <= 80, "{value} exceeds the 80-byte value limit");
            assert_eq!(AccountState::parse(value), Some(state));
            assert_eq!(state.to_string(), value);
        }
    }

    #[test]
    fn unknown_state_values_are_ignored_rather_than_guessed() {
        for value in ["", "OK", "ok ", "rate_limited", "unknown"] {
            assert_eq!(AccountState::parse(value), None, "{value:?}");
        }
    }

    #[test]
    fn state_values_are_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for state in AccountState::ALL {
            assert!(seen.insert(state.as_str()), "duplicate state {state}");
        }
    }
}
