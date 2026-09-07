//! `[[accounts]]` — the declarative half of the account profile sources (fork).
//!
//! One entry describes one Claude config directory. Every field has a default
//! so a malformed entry is reported as a diagnostic instead of failing the
//! whole config parse, matching `[fleet]` and `[gateway]`.
//!
//! Field names are the user-facing contract: add fields, never rename them.

use std::collections::BTreeSet;

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// Longest accepted profile name. Chosen so `tokens.account = <name>` always
/// fits the server's 80-byte metadata value limit and a sidebar cell.
pub const MAX_NAME_LEN: usize = 32;

/// The only agent accepted in v1 (decision (c) of the E9 plan).
pub const DEFAULT_AGENT: &str = "claude";

/// One `[[accounts]]` entry, and one `[[profiles]]` entry of the CLI-managed
/// store — the two sources share this shape on purpose.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AccountProfileConfig {
    /// Display name and id (`[A-Za-z0-9._-]`, at most 32 bytes). Required.
    pub name: String,
    /// Which agent the profile configures. Only `"claude"` in v1.
    pub agent: String,
    /// This profile's `CLAUDE_CONFIG_DIR`. Tilde-expanded; must be absolute
    /// after expansion. Required.
    pub config_dir: String,
    /// Use this profile when an agent is started without an explicit choice.
    pub default: bool,
}

impl Default for AccountProfileConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            agent: DEFAULT_AGENT.to_string(),
            config_dir: String::new(),
            default: false,
        }
    }
}

/// The `accounts` key of `config.toml`, as written.
///
/// `[[accounts]]` is an array of tables. A plain `[accounts]` table — the
/// shape `[accounts.defaults]` creates — is *not* a parse failure: reserved
/// keys must be reported as a diagnostic, not cost the user their whole
/// config, which is what a hard type error would do on the startup path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AccountsSection {
    profiles: Vec<AccountProfileConfig>,
    /// The key was a table rather than an array of tables.
    reserved_table: bool,
}

impl AccountsSection {
    pub fn as_slice(&self) -> &[AccountProfileConfig] {
        &self.profiles
    }

    #[allow(dead_code)] // Read by `herdr account add` (PR 2) and the TUI (PR 7).
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    #[allow(dead_code)] // Read by `herdr account add` (PR 2) and the TUI (PR 7).
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Build a section directly. Test-only.
    #[cfg(test)]
    pub fn test_new(profiles: Vec<AccountProfileConfig>) -> Self {
        Self {
            profiles,
            reserved_table: false,
        }
    }
}

impl<'de> Deserialize<'de> for AccountsSection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = AccountsSection;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an array of [[accounts]] tables")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut profiles = Vec::new();
                while let Some(entry) = sequence.next_element::<AccountProfileConfig>()? {
                    profiles.push(entry);
                }
                Ok(AccountsSection {
                    profiles,
                    reserved_table: false,
                })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(AccountsSection {
                    profiles: Vec::new(),
                    reserved_table: true,
                })
            }
        }

        deserializer.deserialize_any(SectionVisitor)
    }
}

/// Why a profile name is not usable, or `Ok(())`.
///
/// The grammar is deliberately narrow: the name travels through a metadata
/// token value, a CLI argument and a TOML key, so anything that would need
/// quoting anywhere is refused up front.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("profile names must not be empty".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "profile names must be at most {MAX_NAME_LEN} characters"
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("profile names accept letters, digits, '.', '_' and '-' only".to_string());
    }
    if name == "none" {
        return Err("\"none\" is reserved: `--account none` opts out of profiles".to_string());
    }
    Ok(())
}

/// The diagnostic reported when `accounts` is a table rather than an array of
/// tables — the shape `[accounts.defaults]` would have.
pub fn reserved_table_diagnostic() -> String {
    "invalid accounts config: [accounts] must be an array of tables ([[accounts]]); \
     [accounts.defaults] is reserved for per-workspace defaults and is not read yet; \
     ignoring section"
        .to_string()
}

/// Validate `[[accounts]]` without touching the filesystem.
///
/// Every problem is a diagnostic; nothing here fails a config load. The
/// paths are reported as raw strings because expansion happens later, in
/// [`crate::accounts::profile::resolve`], which reports its own diagnostics
/// for the merged view.
pub fn diagnostics(section: &AccountsSection) -> Vec<String> {
    if section.reserved_table {
        return vec![reserved_table_diagnostic()];
    }
    diagnostics_for("accounts", &section.profiles)
}

/// [`diagnostics`], with the TOML path prefix the entries were read from.
pub fn diagnostics_for(section: &str, profiles: &[AccountProfileConfig]) -> Vec<String> {
    let mut diagnostics = Vec::new();
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    let mut seen_dirs: BTreeSet<&str> = BTreeSet::new();
    let mut defaults = 0usize;

    for (index, profile) in profiles.iter().enumerate() {
        let field = |key: &str| format!("{section}[{index}].{key}");

        match validate_name(&profile.name) {
            Err(reason) => diagnostics.push(format!(
                "invalid account profile name: {} = {:?}; {reason}; ignoring the entry",
                field("name"),
                profile.name
            )),
            Ok(()) => {
                if !seen_names.insert(profile.name.as_str()) {
                    diagnostics.push(format!(
                        "duplicate account profile name: {} = {:?}; profile names must be unique; ignoring the entry",
                        field("name"),
                        profile.name
                    ));
                }
            }
        }

        if profile.agent != DEFAULT_AGENT {
            diagnostics.push(format!(
                "unsupported account agent: {} = {:?}; only {DEFAULT_AGENT:?} is supported; ignoring the entry",
                field("agent"),
                profile.agent
            ));
        }

        if profile.config_dir.trim().is_empty() {
            diagnostics.push(format!(
                "missing account config_dir: {} is required; ignoring the entry",
                field("config_dir")
            ));
        } else if !seen_dirs.insert(profile.config_dir.as_str()) {
            diagnostics.push(format!(
                "duplicate account config_dir: {} = {:?}; two profiles must not share a directory; ignoring the entry",
                field("config_dir"),
                profile.config_dir
            ));
        }

        if profile.default {
            defaults += 1;
        }
    }

    if defaults > 1 {
        diagnostics.push(format!(
            "multiple default account profiles: {defaults} entries set default = true; the first one wins"
        ));
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, dir: &str) -> AccountProfileConfig {
        AccountProfileConfig {
            name: name.to_string(),
            config_dir: dir.to_string(),
            ..AccountProfileConfig::default()
        }
    }

    fn section(profiles: Vec<AccountProfileConfig>) -> AccountsSection {
        AccountsSection::test_new(profiles)
    }

    #[test]
    fn an_entry_defaults_to_the_claude_agent() {
        let parsed: AccountProfileConfig =
            toml::from_str("name = \"perso\"\nconfig_dir = \"~/.claude\"\n").expect("valid entry");
        assert_eq!(parsed.agent, "claude");
        assert!(!parsed.default);
        assert!(diagnostics(&section(vec![parsed])).is_empty());
    }

    #[test]
    fn a_valid_pair_reports_nothing() {
        let profiles = vec![
            AccountProfileConfig {
                default: true,
                ..profile("perso", "~/.claude")
            },
            profile("work", "~/.claude-work"),
        ];
        let section = section(profiles);
        assert!(
            diagnostics(&section).is_empty(),
            "{:?}",
            diagnostics(&section)
        );
    }

    #[test]
    fn every_invalid_shape_is_reported_as_a_diagnostic() {
        let profiles = vec![
            profile("", "~/.claude"),
            profile("has space", "~/.a"),
            profile("none", "~/.b"),
            profile(&"x".repeat(MAX_NAME_LEN + 1), "~/.c"),
            profile("empty-dir", "   "),
            AccountProfileConfig {
                agent: "codex".to_string(),
                ..profile("codex-profile", "~/.codex")
            },
            AccountProfileConfig {
                default: true,
                ..profile("one", "~/.one")
            },
            AccountProfileConfig {
                default: true,
                ..profile("two", "~/.two")
            },
            profile("one", "~/.three"),
            profile("four", "~/.one"),
        ];
        let reported = diagnostics(&section(profiles));

        for expected in [
            "invalid account profile name: accounts[0].name",
            "invalid account profile name: accounts[1].name",
            "invalid account profile name: accounts[2].name",
            "invalid account profile name: accounts[3].name",
            "missing account config_dir: accounts[4].config_dir",
            "unsupported account agent: accounts[5].agent",
            "duplicate account profile name: accounts[8].name",
            "duplicate account config_dir: accounts[9].config_dir",
            "multiple default account profiles",
        ] {
            assert!(
                reported.iter().any(|line| line.contains(expected)),
                "missing {expected:?} in {reported:#?}"
            );
        }
    }

    #[test]
    fn the_store_section_names_itself_in_its_diagnostics() {
        let reported = diagnostics_for("profiles", &[profile("bad name", "~/.claude")]);
        assert_eq!(reported.len(), 1);
        assert!(reported[0].contains("profiles[0].name"), "{reported:?}");
    }

    #[test]
    fn name_grammar_accepts_what_the_docs_promise() {
        for name in ["perso", "work", "a", "A.b_c-1", &"n".repeat(MAX_NAME_LEN)] {
            assert!(validate_name(name).is_ok(), "{name} should be accepted");
        }
        for name in ["", "with space", "sl/ash", "quote\"", "acc🙂", "none"] {
            assert!(validate_name(name).is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn the_reserved_table_diagnostic_names_the_array_syntax() {
        let diagnostic = reserved_table_diagnostic();
        assert!(diagnostic.contains("[[accounts]]"), "{diagnostic}");
        assert!(diagnostic.contains("[accounts.defaults]"), "{diagnostic}");
    }

    #[test]
    fn an_array_of_tables_deserializes_into_profiles() {
        let parsed: AccountsSection =
            toml::from_str("[[accounts]]\nname = \"perso\"\nconfig_dir = \"~/.claude\"\n")
                .map(|wrapper: Wrapper| wrapper.accounts)
                .expect("array of tables");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.as_slice()[0].name, "perso");
        assert!(diagnostics(&parsed).is_empty());
    }

    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        accounts: AccountsSection,
    }

    #[test]
    fn a_reserved_table_is_a_diagnostic_not_a_parse_failure() {
        let wrapper: Wrapper = toml::from_str("[accounts.defaults]\nworkspace = \"x\"\n")
            .expect("a table must still parse");
        assert!(wrapper.accounts.is_empty());
        assert_eq!(
            diagnostics(&wrapper.accounts),
            vec![reserved_table_diagnostic()]
        );
    }

    #[test]
    fn a_missing_section_is_empty_and_silent() {
        let wrapper: Wrapper = toml::from_str("").expect("empty config");
        assert!(wrapper.accounts.is_empty());
        assert!(diagnostics(&wrapper.accounts).is_empty());
    }
}
