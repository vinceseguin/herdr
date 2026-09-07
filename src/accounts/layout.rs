//! What a Claude profile directory is made of, and how healthy one is (fork).
//!
//! Decision (a) of the E9 plan: an account is a `CLAUDE_CONFIG_DIR` of its
//! own. Transcripts and other machine-wide state are *shared* between
//! profiles by symlink, identity and settings are *copied*, and credentials
//! are *private* — never read, copied, printed or logged. The three lists live
//! here so `herdr account add` (PR 2), `herdr account status` (PR 3) and the
//! guide (PR 11) cannot drift apart.
//!
//! Read-only half in PR 1: [`inspect`] answers "does this directory exist, is
//! it logged in, is the herdr hook installed" without ever opening
//! `.credentials.json`.

use std::path::{Path, PathBuf};

use crate::accounts::profile::AccountProfile;

/// Entries shared with the source profile by symlink when a profile is seeded.
///
/// Transcripts first: `--resume` has to find a conversation started under
/// another account, which is the whole point of switching mid-session.
pub const SHARED_ENTRIES: &[&str] = &[
    "projects",
    "todos",
    "skills",
    "plugins",
    "commands",
    "agents",
    "CLAUDE.md",
    "history.jsonl",
];

/// Entries copied, not shared: Claude Code rewrites them atomically, which
/// would replace a symlink with a regular file and silently rejoin the two
/// profiles.
// The seed lists are the contract `herdr account add` (PR 2) applies and
// the guide (PR 11) documents; PR 1 ships and tests them so the two cannot
// drift apart.
#[allow(dead_code)]
pub const COPIED_ENTRIES: &[&str] = &["settings.json", CLAUDE_JSON_FILE];

/// Entries a new profile never inherits. `.credentials.json` heads the list:
/// a seeded profile is logged out until `herdr account login`.
#[allow(dead_code)] // Applied by `herdr account add` (PR 2).
pub const PRIVATE_ENTRIES: &[&str] = &[
    CREDENTIALS_FILE,
    "statsig",
    "shell-snapshots",
    "debug",
    "cache",
    "ide",
];

/// Top-level `.claude.json` keys removed when the file is copied.
#[allow(dead_code)] // Applied by `scrub_claude_json` (PR 2).
pub const SCRUBBED_IDENTITY_KEYS: &[&str] = &["oauthAccount"];

/// Any top-level `.claude.json` key containing one of these (case-insensitive)
/// is removed too, so a key this build has never heard of cannot carry a
/// secret into a new profile.
#[allow(dead_code)] // Applied by `scrub_claude_json` (PR 2).
pub const SCRUBBED_KEY_SUBSTRINGS: &[&str] = &["apikey", "token", "credential", "secret"];

/// The OAuth credentials file. Its existence and mode are read; its contents
/// never are.
pub const CREDENTIALS_FILE: &str = ".credentials.json";

/// Claude Code's per-user state file, holding `oauthAccount`.
pub const CLAUDE_JSON_FILE: &str = ".claude.json";

/// Where `herdr integration install claude` puts its hook, relative to the
/// profile directory. Mirrors `CLAUDE_HOOK_INSTALL_NAME`
/// (`src/integration/mod.rs`), which is private to that module.
pub const HOOK_DIR: &str = "hooks";
#[cfg(windows)]
pub const HOOK_FILE: &str = "herdr-agent-state.ps1";
#[cfg(not(windows))]
pub const HOOK_FILE: &str = "herdr-agent-state.sh";

/// Mode a credentials file should have on unix.
#[cfg(unix)]
#[allow(dead_code)] // Written by `herdr account login` (PR 3); asserted in tests.
pub const CREDENTIALS_MODE: u32 = 0o600;

/// Largest `.claude.json` this build parses for identity. Real files grow with
/// per-project history, so the read is bounded rather than unbounded.
const MAX_CLAUDE_JSON_BYTES: u64 = 32 * 1024 * 1024;

/// Who a profile is logged in as, for display only.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct AccountIdentity {
    pub email: Option<String>,
    pub organization: Option<String>,
    pub plan: Option<String>,
}

impl AccountIdentity {
    fn is_empty(&self) -> bool {
        self.email.is_none() && self.organization.is_none() && self.plan.is_none()
    }
}

/// The health of one profile directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProfileInspection {
    pub dir_exists: bool,
    /// A credentials file is present. Its contents are never read.
    pub logged_in: bool,
    /// `Some(false)` when the credentials file is readable by more than its
    /// owner; `None` on platforms without unix modes.
    pub credentials_mode_ok: Option<bool>,
    /// Filled only when [`InspectOptions::identity`] asked for it.
    pub identity: Option<AccountIdentity>,
    /// The herdr `SessionStart` hook script is installed in this profile.
    /// Without it Claude never reports its session id, so a switch that keeps
    /// the conversation is impossible.
    pub hook_installed: bool,
    /// Shared entries whose symlink target is gone.
    pub broken_links: Vec<PathBuf>,
}

/// What [`inspect`] should spend time on.
///
/// Identity means parsing `.claude.json`, which is megabytes on a real
/// installation — `herdr account list` does not need it, `status` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InspectOptions {
    pub identity: bool,
}

impl InspectOptions {
    /// Cheap checks only: existence, credentials presence, hook, links.
    pub fn health() -> Self {
        Self { identity: false }
    }

    /// Also read `oauthAccount` for display.
    #[allow(dead_code)] // `herdr account status` (PR 3) is the first caller.
    pub fn with_identity() -> Self {
        Self { identity: true }
    }
}

/// Inspect a profile directory without reading a single secret.
pub fn inspect(profile: &AccountProfile, options: InspectOptions) -> ProfileInspection {
    inspect_dir(&profile.config_dir, options)
}

/// [`inspect`], addressed by directory so tests need no `AccountProfile`.
pub fn inspect_dir(dir: &Path, options: InspectOptions) -> ProfileInspection {
    let dir_exists = dir.is_dir();
    let credentials = dir.join(CREDENTIALS_FILE);
    let credentials_meta = std::fs::symlink_metadata(&credentials).ok();
    let logged_in = credentials_meta.is_some();

    #[cfg(unix)]
    let credentials_mode_ok = credentials_meta.as_ref().map(|metadata| {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o077 == 0
    });
    #[cfg(not(unix))]
    let credentials_mode_ok = {
        let _ = &credentials_meta;
        None
    };

    let hook_installed = dir.join(HOOK_DIR).join(HOOK_FILE).is_file();

    let mut broken_links = Vec::new();
    if dir_exists {
        for entry in SHARED_ENTRIES {
            let path = dir.join(entry);
            let Ok(link_meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if link_meta.file_type().is_symlink() && !path.exists() {
                broken_links.push(path);
            }
        }
    }

    let identity = if options.identity && dir_exists {
        read_identity(&dir.join(CLAUDE_JSON_FILE))
    } else {
        None
    };

    ProfileInspection {
        dir_exists,
        logged_in,
        credentials_mode_ok,
        identity,
        hook_installed,
        broken_links,
    }
}

fn read_identity(path: &Path) -> Option<AccountIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_CLAUDE_JSON_BYTES {
        tracing::debug!(
            path = %path.display(),
            bytes = metadata.len(),
            "skipping oversized .claude.json while reading account identity"
        );
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    identity_from_claude_json(&text)
}

/// Read `oauthAccount` out of a `.claude.json` body.
///
/// Lenient on purpose: the key names are Claude Code's, not herdr's, and a
/// missing or renamed key must degrade to "unknown", never to an error. Only
/// display fields are read; nothing that looks like a secret is returned.
pub fn identity_from_claude_json(text: &str) -> Option<AccountIdentity> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let account = value.get("oauthAccount")?.as_object()?;
    let field = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            account
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
    };
    let identity = AccountIdentity {
        email: field(&["emailAddress", "email"]),
        organization: field(&["organizationName", "organization"]),
        plan: field(&["subscriptionType", "plan"]),
    };
    if identity.is_empty() {
        return None;
    }
    Some(identity)
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
            "herdr-accounts-layout-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn the_three_lists_never_overlap() {
        for shared in SHARED_ENTRIES {
            assert!(
                !COPIED_ENTRIES.contains(shared),
                "{shared} is shared and copied"
            );
            assert!(
                !PRIVATE_ENTRIES.contains(shared),
                "{shared} is shared and private"
            );
        }
        for copied in COPIED_ENTRIES {
            assert!(
                !PRIVATE_ENTRIES.contains(copied),
                "{copied} is copied and private"
            );
        }
    }

    #[test]
    fn credentials_are_private_and_never_shared_or_copied() {
        assert!(PRIVATE_ENTRIES.contains(&CREDENTIALS_FILE));
        assert!(!SHARED_ENTRIES.contains(&CREDENTIALS_FILE));
        assert!(!COPIED_ENTRIES.contains(&CREDENTIALS_FILE));
    }

    #[test]
    fn identity_reads_the_display_fields_and_nothing_else() {
        let text = r#"{
            "hasCompletedOnboarding": true,
            "oauthAccount": {
                "accountUuid": "0000",
                "emailAddress": "person@example.test",
                "organizationName": "Example Org",
                "subscriptionType": "max",
                "accessToken": "secret-value"
            }
        }"#;
        let identity = identity_from_claude_json(text).expect("identity");
        assert_eq!(identity.email.as_deref(), Some("person@example.test"));
        assert_eq!(identity.organization.as_deref(), Some("Example Org"));
        assert_eq!(identity.plan.as_deref(), Some("max"));

        let encoded = serde_json::to_string(&identity).expect("encode");
        assert!(!encoded.contains("secret-value"), "{encoded}");
        assert!(!encoded.contains("accessToken"), "{encoded}");
    }

    #[test]
    fn identity_accepts_the_alternative_key_names() {
        let identity =
            identity_from_claude_json(r#"{"oauthAccount": {"email": "a@b.test", "plan": "pro"}}"#)
                .expect("identity");
        assert_eq!(identity.email.as_deref(), Some("a@b.test"));
        assert_eq!(identity.plan.as_deref(), Some("pro"));
        assert_eq!(identity.organization, None);
    }

    #[test]
    fn identity_degrades_to_none_on_anything_unexpected() {
        for text in [
            "",
            "not json",
            "{}",
            r#"{"oauthAccount": null}"#,
            r#"{"oauthAccount": "person@example.test"}"#,
            r#"{"oauthAccount": {}}"#,
            r#"{"oauthAccount": {"emailAddress": "   "}}"#,
            r#"{"oauthAccount": {"emailAddress": 42}}"#,
        ] {
            assert_eq!(identity_from_claude_json(text), None, "{text:?}");
        }
    }

    #[test]
    fn inspecting_a_missing_directory_reports_everything_absent() {
        let dir = temp_dir("missing").join("gone");
        let inspection = inspect_dir(&dir, InspectOptions::health());
        assert!(!inspection.dir_exists);
        assert!(!inspection.logged_in);
        assert!(!inspection.hook_installed);
        assert_eq!(inspection.credentials_mode_ok, None);
        assert!(inspection.broken_links.is_empty());
        assert_eq!(inspection.identity, None);
    }

    #[test]
    fn inspecting_a_seeded_directory_reports_its_health() {
        let root = temp_dir("seeded");
        let dir = root.join("profile");
        std::fs::create_dir_all(dir.join(HOOK_DIR)).expect("hooks dir");
        std::fs::write(dir.join(HOOK_DIR).join(HOOK_FILE), "#!/bin/sh\n").expect("hook");
        std::fs::write(dir.join(CREDENTIALS_FILE), "{}").expect("credentials");
        std::fs::write(
            dir.join(CLAUDE_JSON_FILE),
            r#"{"oauthAccount": {"emailAddress": "person@example.test"}}"#,
        )
        .expect("claude json");

        let health = inspect_dir(&dir, InspectOptions::health());
        assert!(health.dir_exists);
        assert!(health.logged_in);
        assert!(health.hook_installed);
        assert_eq!(
            health.identity, None,
            "the cheap inspection must not parse .claude.json"
        );

        let full = inspect_dir(&dir, InspectOptions::with_identity());
        assert_eq!(
            full.identity.expect("identity").email.as_deref(),
            Some("person@example.test")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_credentials_file_is_reported() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_dir("modes");
        let dir = root.join("profile");
        std::fs::create_dir_all(&dir).expect("profile dir");
        let credentials = dir.join(CREDENTIALS_FILE);
        std::fs::write(&credentials, "{}").expect("credentials");

        std::fs::set_permissions(
            &credentials,
            std::fs::Permissions::from_mode(CREDENTIALS_MODE),
        )
        .expect("chmod 600");
        assert_eq!(
            inspect_dir(&dir, InspectOptions::health()).credentials_mode_ok,
            Some(true)
        );

        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");
        assert_eq!(
            inspect_dir(&dir, InspectOptions::health()).credentials_mode_ok,
            Some(false)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_shared_link_is_reported() {
        let root = temp_dir("links");
        let dir = root.join("profile");
        std::fs::create_dir_all(&dir).expect("profile dir");
        std::os::unix::fs::symlink(root.join("nowhere"), dir.join("projects")).expect("symlink");

        let inspection = inspect_dir(&dir, InspectOptions::health());
        assert_eq!(inspection.broken_links, vec![dir.join("projects")]);

        let _ = std::fs::remove_dir_all(&root);
    }
}
