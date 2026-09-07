//! Where the two profile sources meet (fork).
//!
//! `[[accounts]]` in `config.toml` is declarative and owned by the user;
//! `<config>/accounts/profiles.toml` is written by `herdr account add`. This
//! module merges them into one ordered, validated [`Profiles`] view and
//! answers the only question every caller actually asks: *which profile does
//! this launch use?* ([`Profiles::choose`]).
//!
//! Pure: it takes the two sources and a home directory and returns data.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::accounts::config::{dir_key, AccountProfileConfig, DEFAULT_AGENT};
use crate::accounts::store::AccountsStore;

pub use crate::accounts::config::validate_name;

/// The value of `--account` that opts out of profiles entirely.
#[allow(dead_code)] // Read by the `--account` flag parsers (PRs 4, 5, 7).
pub const NO_ACCOUNT: &str = "none";

/// Which agent a profile configures. Append-only: `Codex` (`CODEX_HOME`) and
/// friends can follow without changing anything a user wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountAgent {
    Claude,
}

impl AccountAgent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => DEFAULT_AGENT,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            DEFAULT_AGENT => Some(Self::Claude),
            _ => None,
        }
    }

    /// The environment variable that points this agent at a config directory.
    ///
    /// The same name `crate::integration::env::CLAUDE_CONFIG_DIR_ENV_VAR`
    /// holds; that module is private and `src/integration/**` is off limits to
    /// fork work, so the string is repeated here rather than the module opened
    /// up. A test pins it.
    #[allow(dead_code)] // The launch driver (PR 4) is the first caller.
    pub fn config_dir_env_var(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
        }
    }
}

impl std::fmt::Display for AccountAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which source an entry came from. Reported by `herdr account list` because
/// it decides whether `herdr account remove` may touch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileOrigin {
    /// `[[accounts]]` in `config.toml`. herdr never rewrites it.
    Config,
    /// `<config>/accounts/profiles.toml`, written by `herdr account`.
    Store,
}

impl ProfileOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Store => "store",
        }
    }
}

impl std::fmt::Display for ProfileOrigin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One resolved, validated profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountProfile {
    pub name: String,
    pub agent: AccountAgent,
    /// Absolute, tilde-expanded config directory.
    pub config_dir: PathBuf,
    /// Whether this entry asked to be the default (see
    /// [`Profiles::default_profile`] for how ties are broken).
    pub default: bool,
    pub origin: ProfileOrigin,
}

/// The merged view of every configured profile, in resolution order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profiles {
    profiles: Vec<AccountProfile>,
    /// The name `profiles.toml` chose, when it names a profile that exists.
    store_default: Option<String>,
}

/// What a launch should do about accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Consumed by the launch driver (PR 4) and the TUI (PR 7).
pub enum Choice<'a> {
    /// Launch under this profile.
    Profile(&'a AccountProfile),
    /// Launch exactly as a stock herdr would: no environment, no tokens.
    None,
}

/// Why an explicit `--account` could not be honoured.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Reported by the launch driver (PR 4) and the TUI (PR 7).
pub enum ChoiceError {
    /// The name is not a profile. Never falls back to the default: launching
    /// under the wrong account silently is the failure this epic exists to
    /// prevent.
    Unknown {
        name: String,
        available: Vec<String>,
    },
    /// The name could never be a profile.
    InvalidName { name: String, reason: String },
}

impl std::fmt::Display for ChoiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown { name, available } if available.is_empty() => write!(
                formatter,
                "unknown account profile {name:?}; no profiles are configured (see `herdr account list`)"
            ),
            Self::Unknown { name, available } => write!(
                formatter,
                "unknown account profile {name:?}; configured profiles: {}",
                available.join(", ")
            ),
            Self::InvalidName { name, reason } => {
                write!(formatter, "invalid account profile {name:?}: {reason}")
            }
        }
    }
}

// `choose`, `names`, `len` and `is_empty` are the resolution API PRs 4, 5 and 7
// call; PR 1 ships and tests the decision so every consumer resolves a profile
// the same way instead of re-deriving it.
#[allow(dead_code)]
impl Profiles {
    /// Build a view directly from resolved profiles. Test-only.
    #[cfg(test)]
    pub fn test_new(profiles: Vec<AccountProfile>) -> Self {
        Self {
            profiles,
            store_default: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, AccountProfile> {
        self.profiles.iter()
    }

    pub fn get(&self, name: &str) -> Option<&AccountProfile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    pub fn names(&self) -> Vec<String> {
        self.profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect()
    }

    /// The profile used when no `--account` is given.
    ///
    /// `profiles.toml`'s `default` key beats a `default = true` flag, and the
    /// first flagged entry beats a later one (the extra flags are reported as
    /// a diagnostic by [`resolve`]).
    pub fn default_profile(&self) -> Option<&AccountProfile> {
        if let Some(name) = self.store_default.as_deref() {
            if let Some(profile) = self.get(name) {
                return Some(profile);
            }
        }
        self.profiles.iter().find(|profile| profile.default)
    }

    /// Decision (f) of the E9 plan: explicit name → default → the only
    /// profile → no profile.
    pub fn choose(&self, explicit: Option<&str>) -> Result<Choice<'_>, ChoiceError> {
        match explicit {
            Some(NO_ACCOUNT) => Ok(Choice::None),
            Some(name) => {
                if let Err(reason) = validate_name(name) {
                    // `none` is refused by the grammar but handled above, so a
                    // caller reaching here really did type an impossible name.
                    return Err(ChoiceError::InvalidName {
                        name: name.to_string(),
                        reason,
                    });
                }
                match self.get(name) {
                    Some(profile) => Ok(Choice::Profile(profile)),
                    None => Err(ChoiceError::Unknown {
                        name: name.to_string(),
                        available: self.names(),
                    }),
                }
            }
            None => {
                if let Some(profile) = self.default_profile() {
                    return Ok(Choice::Profile(profile));
                }
                match self.profiles.as_slice() {
                    [only] => Ok(Choice::Profile(only)),
                    _ => Ok(Choice::None),
                }
            }
        }
    }
}

/// Merge the two sources into one view, reporting everything it drops.
///
/// Config entries are resolved first, so a name defined in both sources keeps
/// the `config.toml` definition (and reports a diagnostic): the file the user
/// edits by hand wins over the one herdr writes.
pub fn resolve(
    config: &[AccountProfileConfig],
    store: &AccountsStore,
    home: &Path,
) -> (Profiles, Vec<String>) {
    let mut diagnostics = Vec::new();
    let mut profiles: Vec<AccountProfile> = Vec::new();
    let mut seen_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut defaults = 0usize;

    for (origin, entries, section) in [
        (ProfileOrigin::Config, config, "accounts"),
        (ProfileOrigin::Store, store.profiles.as_slice(), "profiles"),
    ] {
        for (index, entry) in entries.iter().enumerate() {
            let field = |key: &str| format!("{section}[{index}].{key}");

            if let Err(reason) = validate_name(&entry.name) {
                diagnostics.push(format!(
                    "invalid account profile name: {} = {:?}; {reason}; ignoring the entry",
                    field("name"),
                    entry.name
                ));
                continue;
            }
            let Some(agent) = AccountAgent::parse(&entry.agent) else {
                diagnostics.push(format!(
                    "unsupported account agent: {} = {:?}; only {DEFAULT_AGENT:?} is supported; ignoring the entry",
                    field("agent"),
                    entry.agent
                ));
                continue;
            };
            if profiles.iter().any(|profile| profile.name == entry.name) {
                diagnostics.push(format!(
                    "duplicate account profile name: {} = {:?}; the earlier definition wins; ignoring the entry",
                    field("name"),
                    entry.name
                ));
                continue;
            }
            let raw = entry.config_dir.trim();
            if raw.is_empty() {
                diagnostics.push(format!(
                    "missing account config_dir: {} is required; ignoring the entry",
                    field("config_dir")
                ));
                continue;
            }
            // Folded to the same key the duplicate check uses, so two
            // entries cannot point at one credentials directory by spelling it
            // `/p/work` and `/p/work/`.
            let config_dir = dir_key(&expand(raw, home));
            if !config_dir.is_absolute() {
                diagnostics.push(format!(
                    "invalid account config_dir: {} = {:?}; must be an absolute path after ~ expansion; ignoring the entry",
                    field("config_dir"),
                    entry.config_dir
                ));
                continue;
            }
            if !seen_dirs.insert(config_dir.clone()) {
                diagnostics.push(format!(
                    "duplicate account config_dir: {} = {:?}; two profiles must not share a directory; ignoring the entry",
                    field("config_dir"),
                    entry.config_dir
                ));
                continue;
            }
            if entry.default {
                defaults += 1;
            }
            profiles.push(AccountProfile {
                name: entry.name.clone(),
                agent,
                config_dir,
                default: entry.default,
                origin,
            });
        }
    }

    if defaults > 1 {
        diagnostics.push(format!(
            "multiple default account profiles: {defaults} entries set default = true; the first one wins"
        ));
    }

    let store_default = match store.default.as_deref() {
        None => None,
        Some(name) if profiles.iter().any(|profile| profile.name == name) => Some(name.to_string()),
        Some(name) => {
            diagnostics.push(format!(
                "unknown default account profile: the account store names {name:?}, which is not a configured profile; ignoring it"
            ));
            None
        }
    };

    (
        Profiles {
            profiles,
            store_default,
        },
        diagnostics,
    )
}

/// The user's home directory.
///
/// Mirrors `crate::integration::env::home_dir`, which is private to a module
/// fork work must not edit. Resolution order is the same: `$HOME`, then the
/// Windows profile variables.
fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }

    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (
            std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty()),
            std::env::var_os("HOMEPATH").filter(|value| !value.is_empty()),
        ) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Some(home);
        }
    }

    None
}

/// Expand a leading `~` against `home`, mirroring
/// `crate::integration::env::expand_tilde_path`, but against an explicit home
/// so the merge stays pure.
fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or_else(|| raw.strip_prefix('~'))
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// Load and merge both sources for the running herdr.
///
/// The only entry point production code should use: the CLI, the launch
/// driver, the TUI and `status` must all see the same merge.
pub fn load_profiles(config: &crate::config::Config) -> (Profiles, Vec<String>) {
    let (store, mut diagnostics) = crate::accounts::store::load();
    let home = match home_dir() {
        Some(home) => home,
        None => {
            diagnostics.push(
                "account profiles: no home directory; ~ in config_dir cannot be expanded"
                    .to_string(),
            );
            PathBuf::new()
        }
    };
    // The section-level problem first: a caller that reads no profiles has to
    // be told the section was thrown away, not left to conclude none were
    // configured. The per-entry problems come from `resolve`, which sees the
    // merged view, so they are never reported twice.
    diagnostics.extend(crate::accounts::config::section_diagnostic(
        &config.accounts,
    ));
    let (profiles, resolution) = resolve(config.accounts.as_slice(), &store, &home);
    diagnostics.extend(resolution);
    (profiles, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    fn entry(name: &str, dir: &str) -> AccountProfileConfig {
        AccountProfileConfig {
            name: name.to_string(),
            config_dir: dir.to_string(),
            ..AccountProfileConfig::default()
        }
    }

    fn store(default: Option<&str>, profiles: Vec<AccountProfileConfig>) -> AccountsStore {
        AccountsStore {
            default: default.map(str::to_string),
            profiles,
            ..AccountsStore::default()
        }
    }

    #[test]
    fn config_entries_resolve_before_store_entries() {
        let (profiles, diagnostics) = resolve(
            &[entry("perso", "~/.claude")],
            &store(None, vec![entry("work", "/opt/work")]),
            &home(),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(profiles.names(), vec!["perso", "work"]);
        assert_eq!(
            profiles.get("perso").expect("perso").config_dir,
            home().join(".claude")
        );
        assert_eq!(
            profiles.get("perso").expect("perso").origin,
            ProfileOrigin::Config
        );
        assert_eq!(
            profiles.get("work").expect("work").origin,
            ProfileOrigin::Store
        );
    }

    #[test]
    fn a_name_defined_in_both_sources_keeps_the_config_entry() {
        let (profiles, diagnostics) = resolve(
            &[entry("perso", "/from/config")],
            &store(None, vec![entry("perso", "/from/store")]),
            &home(),
        );
        assert_eq!(profiles.len(), 1);
        let perso = profiles.get("perso").expect("perso");
        assert_eq!(perso.config_dir, PathBuf::from("/from/config"));
        assert_eq!(perso.origin, ProfileOrigin::Config);
        assert!(
            diagnostics
                .iter()
                .any(|line| line.contains("duplicate account profile name: profiles[0].name")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn two_profiles_may_not_share_a_directory() {
        let (profiles, diagnostics) = resolve(
            &[entry("perso", "~/.claude"), entry("twin", "~/.claude")],
            &store(None, Vec::new()),
            &home(),
        );
        assert_eq!(profiles.names(), vec!["perso"]);
        assert!(
            diagnostics
                .iter()
                .any(|line| line.contains("duplicate account config_dir")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn a_relative_config_dir_is_refused() {
        let (profiles, diagnostics) = resolve(
            &[entry("perso", "claude")],
            &store(None, Vec::new()),
            &home(),
        );
        assert!(profiles.is_empty());
        assert!(
            diagnostics[0].contains("must be an absolute path"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn the_store_default_beats_a_default_flag() {
        let flagged = AccountProfileConfig {
            default: true,
            ..entry("perso", "~/.claude")
        };
        let (profiles, diagnostics) = resolve(
            &[flagged],
            &store(Some("work"), vec![entry("work", "/opt/work")]),
            &home(),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(profiles.default_profile().expect("default").name, "work");
    }

    #[test]
    fn a_store_default_naming_nothing_is_a_diagnostic() {
        let (profiles, diagnostics) = resolve(
            &[entry("perso", "~/.claude")],
            &store(Some("gone"), Vec::new()),
            &home(),
        );
        assert!(profiles.default_profile().is_none());
        assert!(
            diagnostics
                .iter()
                .any(|line| line.contains("unknown default account profile")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn the_first_default_flag_wins_and_the_rest_are_reported() {
        let first = AccountProfileConfig {
            default: true,
            ..entry("perso", "~/.claude")
        };
        let second = AccountProfileConfig {
            default: true,
            ..entry("work", "~/.claude-work")
        };
        let (profiles, diagnostics) = resolve(&[first, second], &store(None, Vec::new()), &home());
        assert_eq!(profiles.default_profile().expect("default").name, "perso");
        assert!(
            diagnostics
                .iter()
                .any(|line| line.contains("multiple default account profiles")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn an_unsupported_agent_drops_only_that_entry() {
        let codex = AccountProfileConfig {
            agent: "codex".to_string(),
            ..entry("codex", "~/.codex")
        };
        let (profiles, diagnostics) = resolve(
            &[codex, entry("perso", "~/.claude")],
            &store(None, Vec::new()),
            &home(),
        );
        assert_eq!(profiles.names(), vec!["perso"]);
        assert!(
            diagnostics[0].contains("unsupported account agent"),
            "{diagnostics:?}"
        );
    }

    fn resolved(name: &str, default: bool) -> AccountProfile {
        AccountProfile {
            name: name.to_string(),
            agent: AccountAgent::Claude,
            config_dir: PathBuf::from(format!("/home/tester/.claude-{name}")),
            default,
            origin: ProfileOrigin::Config,
        }
    }

    #[test]
    fn choose_prefers_an_explicit_name() {
        let profiles = Profiles::test_new(vec![resolved("perso", true), resolved("work", false)]);
        assert_eq!(
            profiles.choose(Some("work")),
            Ok(Choice::Profile(profiles.get("work").expect("work")))
        );
    }

    #[test]
    fn choose_falls_back_to_the_default_then_to_the_only_profile() {
        let with_default =
            Profiles::test_new(vec![resolved("perso", true), resolved("work", false)]);
        assert_eq!(
            with_default.choose(None),
            Ok(Choice::Profile(with_default.get("perso").expect("perso")))
        );

        let single = Profiles::test_new(vec![resolved("only", false)]);
        assert_eq!(
            single.choose(None),
            Ok(Choice::Profile(single.get("only").expect("only")))
        );

        let ambiguous = Profiles::test_new(vec![resolved("perso", false), resolved("work", false)]);
        assert_eq!(ambiguous.choose(None), Ok(Choice::None));

        assert_eq!(Profiles::default().choose(None), Ok(Choice::None));
    }

    #[test]
    fn choose_none_opts_out_even_when_a_default_exists() {
        let profiles = Profiles::test_new(vec![resolved("perso", true)]);
        assert_eq!(profiles.choose(Some(NO_ACCOUNT)), Ok(Choice::None));
    }

    #[test]
    fn choose_never_falls_back_when_a_name_is_wrong() {
        let profiles = Profiles::test_new(vec![resolved("perso", true)]);
        let error = profiles.choose(Some("wrok")).expect_err("unknown name");
        assert_eq!(
            error,
            ChoiceError::Unknown {
                name: "wrok".to_string(),
                available: vec!["perso".to_string()],
            }
        );
        assert!(error.to_string().contains("perso"), "{error}");

        let invalid = profiles.choose(Some("a b")).expect_err("invalid name");
        assert!(matches!(invalid, ChoiceError::InvalidName { .. }));
        assert!(invalid.to_string().contains("invalid account profile"));
    }

    #[test]
    fn a_directory_spelled_two_ways_is_still_one_directory() {
        let (profiles, diagnostics) = resolve(
            &[entry("perso", "~/.claude")],
            &store(None, vec![entry("twin", "~/.claude/")]),
            &home(),
        );
        assert_eq!(profiles.names(), vec!["perso"]);
        assert!(
            diagnostics
                .iter()
                .any(|line| line.contains("duplicate account config_dir")),
            "{diagnostics:?}"
        );
        assert_eq!(
            profiles.get("perso").expect("perso").config_dir,
            home().join(".claude"),
            "the stored directory is normalized, not the raw string"
        );
    }

    #[test]
    fn a_trailing_separator_does_not_change_the_exported_directory() {
        let (profiles, diagnostics) = resolve(
            &[entry("perso", "/p/work/"), entry("other", "/p/./other//")],
            &store(None, Vec::new()),
            &home(),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            profiles.get("perso").expect("perso").config_dir,
            PathBuf::from("/p/work")
        );
        assert_eq!(
            profiles.get("other").expect("other").config_dir,
            PathBuf::from("/p/other")
        );
    }

    #[test]
    fn a_store_entry_is_validated_the_same_way_and_names_its_own_section() {
        let (profiles, diagnostics) = resolve(
            &[],
            &store(
                None,
                vec![
                    entry("bad name", "/p/a"),
                    entry("", "/p/b"),
                    entry("-flag", "/p/c"),
                    entry("ok", "  "),
                    entry("good", "/p/good"),
                ],
            ),
            &home(),
        );
        assert_eq!(profiles.names(), vec!["good"]);
        for expected in [
            "profiles[0].name",
            "profiles[1].name",
            "profiles[2].name",
            "profiles[3].config_dir",
        ] {
            assert!(
                diagnostics.iter().any(|line| line.contains(expected)),
                "missing {expected:?} in {diagnostics:?}"
            );
        }
    }

    #[test]
    fn an_impossible_explicit_name_never_resolves_to_a_profile() {
        let profiles = Profiles::test_new(vec![resolved("perso", true)]);
        for name in ["", "with space", "..", "-perso", "PERSO/../perso"] {
            match profiles.choose(Some(name)) {
                Err(ChoiceError::InvalidName { .. }) | Err(ChoiceError::Unknown { .. }) => {}
                other => panic!("{name:?} resolved to {other:?}"),
            }
        }
    }

    #[test]
    fn tilde_expansion_matches_the_integration_helper() {
        assert_eq!(expand("~", &home()), home());
        assert_eq!(expand("~/.claude", &home()), home().join(".claude"));
        assert_eq!(expand("~.claude", &home()), home().join(".claude"));
        assert_eq!(expand("/opt/claude", &home()), PathBuf::from("/opt/claude"));
    }

    #[test]
    fn the_agent_enum_names_the_claude_config_dir_variable() {
        assert_eq!(AccountAgent::parse("claude"), Some(AccountAgent::Claude));
        assert_eq!(AccountAgent::parse("codex"), None);
        assert_eq!(
            AccountAgent::Claude.config_dir_env_var(),
            "CLAUDE_CONFIG_DIR"
        );
        assert_eq!(AccountAgent::Claude.to_string(), "claude");
        assert_eq!(ProfileOrigin::Store.to_string(), "store");
    }
}
