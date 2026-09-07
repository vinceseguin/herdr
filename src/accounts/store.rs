//! `<config>/accounts/profiles.toml` — the CLI-managed half of the sources.
//!
//! herdr never rewrites the user's `config.toml`, so `herdr account add`,
//! `remove` and `default` (PR 2) own this small file instead. It holds names,
//! paths and one default; never a secret.
//!
//! The file schema is `deny_unknown_fields` and carries a `version`, so a file
//! written by a newer herdr is ignored with a diagnostic rather than
//! misinterpreted.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::accounts::config::AccountProfileConfig;

/// Schema version written by this build.
pub const STORE_VERSION: u32 = 1;

/// Directory mode of `<config>/accounts` on unix: the store lists profile
/// directories, so it is kept as private as they are.
#[cfg(unix)]
const STORE_DIR_MODE: u32 = 0o700;
/// File mode of `profiles.toml` on unix.
#[cfg(unix)]
const STORE_FILE_MODE: u32 = 0o600;

/// Largest store file that is read. A profile list is tiny; anything larger is
/// not a store this build wrote.
const MAX_STORE_BYTES: u64 = 1024 * 1024;

/// The parsed `profiles.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountsStore {
    /// Schema version. Always [`STORE_VERSION`] when written by this build.
    #[serde(default = "default_store_version")]
    pub version: u32,
    /// The profile name `herdr account default` chose, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// `[[profiles]]` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<AccountProfileConfig>,
}

fn default_store_version() -> u32 {
    STORE_VERSION
}

impl Default for AccountsStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            default: None,
            profiles: Vec::new(),
        }
    }
}

impl AccountsStore {
    /// Record a new profile.
    ///
    /// Refuses a name or a directory the store already uses; the caller has
    /// already checked the merged view, and this is the last guard before two
    /// entries could point at one credentials directory.
    pub fn add(&mut self, entry: AccountProfileConfig) -> Result<(), String> {
        crate::accounts::config::validate_name(&entry.name)?;
        if self.profiles.iter().any(|stored| stored.name == entry.name) {
            return Err(format!(
                "account profile {:?} is already in the account store",
                entry.name
            ));
        }
        let key = crate::accounts::config::dir_key(std::path::Path::new(&entry.config_dir));
        if let Some(clash) = self.profiles.iter().find(|stored| {
            crate::accounts::config::dir_key(std::path::Path::new(&stored.config_dir)) == key
        }) {
            return Err(format!(
                "account profile {:?} already uses {}; two profiles must not share a directory",
                clash.name,
                key.display()
            ));
        }
        self.profiles.push(entry);
        Ok(())
    }

    /// Drop a profile, clearing the stored default if it named that profile.
    pub fn remove(&mut self, name: &str) -> Result<AccountProfileConfig, String> {
        let Some(index) = self.profiles.iter().position(|stored| stored.name == name) else {
            return Err(format!(
                "account profile {name:?} is not in the account store"
            ));
        };
        let removed = self.profiles.remove(index);
        if self.default.as_deref() == Some(name) {
            self.default = None;
        }
        Ok(removed)
    }

    /// Name the default profile.
    ///
    /// The name may belong to a `[[accounts]]` entry: the store's `default`
    /// key is how `herdr account default` chooses between *any* two configured
    /// profiles without rewriting `config.toml`. The caller checks that the
    /// name resolves; `resolve` reports it as a diagnostic if it later stops.
    pub fn set_default(&mut self, name: &str) -> Result<(), String> {
        crate::accounts::config::validate_name(name)?;
        self.default = Some(name.to_string());
        Ok(())
    }
}

/// Where the store lives for the current config directory.
pub fn store_path() -> PathBuf {
    crate::config::config_dir()
        .join("accounts")
        .join("profiles.toml")
}

/// Parse a store file's text.
///
/// Pure, so the version and schema rules are testable without a filesystem.
pub fn parse(text: &str) -> Result<AccountsStore, String> {
    let store: AccountsStore = toml::from_str(text).map_err(|err| err.to_string())?;
    if store.version != STORE_VERSION {
        return Err(format!(
            "unsupported version {} (this build reads version {STORE_VERSION})",
            store.version
        ));
    }
    Ok(store)
}

/// Render a store back to TOML.
pub fn render(store: &AccountsStore) -> Result<String, String> {
    toml::to_string_pretty(store).map_err(|err| err.to_string())
}

/// Load the store, reporting problems instead of failing.
///
/// A missing file is an empty store with no diagnostic: not having run
/// `herdr account add` is the normal case.
pub fn load() -> (AccountsStore, Vec<String>) {
    load_from(&store_path())
}

fn load_from(path: &std::path::Path) -> (AccountsStore, Vec<String>) {
    let text = match std::fs::metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (AccountsStore::default(), Vec::new())
        }
        Err(err) => {
            return (
                AccountsStore::default(),
                vec![format!(
                    "account store read error: {}: {err}; ignoring saved profiles",
                    path.display()
                )],
            );
        }
        Ok(metadata) if metadata.len() > MAX_STORE_BYTES => {
            return (
                AccountsStore::default(),
                vec![format!(
                    "account store too large: {} is {} bytes; ignoring saved profiles",
                    path.display(),
                    metadata.len()
                )],
            );
        }
        Ok(_) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                return (
                    AccountsStore::default(),
                    vec![format!(
                        "account store read error: {}: {err}; ignoring saved profiles",
                        path.display()
                    )],
                );
            }
        },
    };

    match parse(&text) {
        Ok(store) => (store, Vec::new()),
        Err(reason) => (
            AccountsStore::default(),
            vec![format!(
                "invalid account store: {}: {reason}; ignoring saved profiles",
                path.display()
            )],
        ),
    }
}

/// Write the store atomically, creating `<config>/accounts` if needed.
///
/// Callers must have loaded the store without diagnostics first: `load`
/// degrades an unreadable file to an empty store, and saving that would delete
/// profiles herdr merely failed to parse.
pub fn save(store: &AccountsStore) -> io::Result<()> {
    save_to(&store_path(), store)
}

fn save_to(path: &std::path::Path, store: &AccountsStore) -> io::Result<()> {
    let rendered = render(store).map_err(io::Error::other)?;
    let Some(parent) = path.parent() else {
        return Err(io::Error::other(format!(
            "account store path has no parent: {}",
            path.display()
        )));
    };
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(STORE_DIR_MODE))?;
    }

    // Same directory, so the rename is atomic on every filesystem herdr runs on.
    let temporary = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    {
        use std::io::Write as _;

        // Opened at the final mode rather than written-then-narrowed: the
        // store lists every account's credentials directory.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(STORE_FILE_MODE);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(rendered.as_bytes())?;
        // The rename below publishes this file; a crash must not leave the
        // store renamed but empty.
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        // `mode` applies at creation only, so a leftover temporary from a
        // crashed run is narrowed here.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(STORE_FILE_MODE))?;
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temporary);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "herdr-accounts-store-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn a_missing_store_is_empty_and_silent() {
        let dir = temp_dir("missing");
        let (store, diagnostics) = load_from(&dir.join("profiles.toml"));
        assert_eq!(store, AccountsStore::default());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_round_trips_through_render_and_parse() {
        let store = AccountsStore {
            version: STORE_VERSION,
            default: Some("work".to_string()),
            profiles: vec![AccountProfileConfig {
                name: "work".to_string(),
                agent: "claude".to_string(),
                config_dir: "/home/u/.claude-work".to_string(),
                default: false,
            }],
        };
        let rendered = render(&store).expect("render");
        assert_eq!(parse(&rendered).expect("parse"), store);
    }

    #[test]
    fn a_newer_schema_version_is_ignored_rather_than_guessed() {
        let error = parse("version = 2\n").expect_err("version 2 must be refused");
        assert!(error.contains("unsupported version 2"), "{error}");
    }

    #[test]
    fn unknown_top_level_keys_are_refused() {
        let error = parse("version = 1\ndefualt = \"work\"\n").expect_err("typo must be refused");
        assert!(error.contains("defualt"), "{error}");
    }

    #[test]
    fn an_invalid_store_is_a_diagnostic_not_a_failure() {
        let dir = temp_dir("invalid");
        let path = dir.join("profiles.toml");
        std::fs::write(&path, "version = 1\nprofiles = 3\n").expect("write");
        let (store, diagnostics) = load_from(&path);
        assert_eq!(store, AccountsStore::default());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            diagnostics[0].contains("invalid account store"),
            "{diagnostics:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_refuses_a_duplicate_name_or_a_shared_directory() {
        let mut store = AccountsStore::default();
        store
            .add(AccountProfileConfig {
                name: "work".to_string(),
                agent: "claude".to_string(),
                config_dir: "/home/u/.claude-work".to_string(),
                default: false,
            })
            .expect("first add");

        let duplicate_name = store
            .add(AccountProfileConfig {
                name: "work".to_string(),
                config_dir: "/home/u/.claude-other".to_string(),
                ..AccountProfileConfig::default()
            })
            .expect_err("duplicate name");
        assert!(duplicate_name.contains("already in the account store"));

        // Spelled differently, same credentials directory.
        let duplicate_dir = store
            .add(AccountProfileConfig {
                name: "other".to_string(),
                config_dir: "/home/u/./.claude-work/".to_string(),
                ..AccountProfileConfig::default()
            })
            .expect_err("duplicate directory");
        assert!(
            duplicate_dir.contains("must not share a directory"),
            "{duplicate_dir}"
        );

        let bad_name = store
            .add(AccountProfileConfig {
                name: "../escape".to_string(),
                config_dir: "/home/u/.claude-escape".to_string(),
                ..AccountProfileConfig::default()
            })
            .expect_err("bad name");
        assert!(!bad_name.is_empty());

        assert_eq!(store.profiles.len(), 1);
    }

    #[test]
    fn remove_drops_the_entry_and_clears_a_default_that_named_it() {
        let mut store = AccountsStore::default();
        store
            .add(AccountProfileConfig {
                name: "work".to_string(),
                config_dir: "/home/u/.claude-work".to_string(),
                ..AccountProfileConfig::default()
            })
            .expect("add");
        store.set_default("work").expect("set default");

        assert!(store.remove("nope").is_err());
        let removed = store.remove("work").expect("remove");
        assert_eq!(removed.name, "work");
        assert!(store.profiles.is_empty());
        assert_eq!(
            store.default, None,
            "a default naming a removed profile must not survive"
        );
    }

    #[test]
    fn set_default_accepts_a_config_profile_but_never_an_impossible_name() {
        let mut store = AccountsStore::default();
        // The name may belong to a [[accounts]] entry: that is how `herdr
        // account default` chooses one without rewriting config.toml.
        store.set_default("perso").expect("config-origin default");
        assert_eq!(store.default.as_deref(), Some("perso"));
        assert!(store.set_default("..").is_err());
        assert!(store.set_default("a b").is_err());
        assert_eq!(store.default.as_deref(), Some("perso"));
    }

    #[test]
    fn save_writes_a_store_load_reads_back() {
        let dir = temp_dir("save");
        let path = dir.join("accounts").join("profiles.toml");
        let store = AccountsStore {
            version: STORE_VERSION,
            default: Some("work".to_string()),
            profiles: vec![AccountProfileConfig {
                name: "work".to_string(),
                agent: "claude".to_string(),
                config_dir: "/home/u/.claude-work".to_string(),
                default: false,
            }],
        };
        save_to(&path, &store).expect("save");
        let (loaded, diagnostics) = load_from(&path);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(loaded, store);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir_mode = std::fs::metadata(path.parent().expect("parent"))
                .expect("dir metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, STORE_DIR_MODE);
            let file_mode = std::fs::metadata(&path)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, STORE_FILE_MODE);
        }

        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("tmp"),
            "the temporary file must not survive a save"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
