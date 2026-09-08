//! What a Claude profile directory is made of, and how healthy one is (fork).
//!
//! Decision (a) of the E9 plan: an account is a `CLAUDE_CONFIG_DIR` of its
//! own. Transcripts and other machine-wide state are *shared* between
//! profiles by symlink, identity and settings are *copied*, and credentials
//! are *private* — never read, copied, printed or logged. The three lists live
//! here so `herdr account add` (PR 2), `herdr account status` (PR 3) and the
//! guide (PR 11) cannot drift apart.
//!
//! [`inspect`] answers "does this directory exist, is it logged in, is the
//! herdr hook installed" without ever opening `.credentials.json`, and
//! [`plan_seed`] / [`apply_seed`] build a new profile directory out of an
//! existing one. Both halves obey the same rule, checked twice in
//! [`guard_no_credentials`]: a credentials file is never read, copied,
//! linked, printed or logged — only its existence and mode.

use std::io;
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
pub const COPIED_ENTRIES: &[&str] = &["settings.json", CLAUDE_JSON_FILE];

/// Entries a new profile never inherits. `.credentials.json` heads the list:
/// a seeded profile is logged out until `herdr account login`.
pub const PRIVATE_ENTRIES: &[&str] = &[
    CREDENTIALS_FILE,
    "statsig",
    "shell-snapshots",
    "debug",
    "cache",
    "ide",
];

/// Top-level `.claude.json` keys removed when the file is copied.
pub const SCRUBBED_IDENTITY_KEYS: &[&str] = &["oauthAccount"];

/// Any top-level `.claude.json` key containing one of these (case-insensitive)
/// is removed too, so a key this build has never heard of cannot carry a
/// secret into a new profile.
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

/// Longest identity field `herdr account status` will accept for display.
///
/// An address, an organization name and a plan word all fit with room to
/// spare. The bound exists because `.claude.json` is not herdr's file: without
/// it a megabyte-long `subscriptionType` would be printed straight into
/// someone's terminal.
const MAX_IDENTITY_CHARS: usize = 120;

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

    /// Also read `oauthAccount` for display. Used by `herdr account status`.
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
    // Display-only strings out of a file herdr does not own, printed straight
    // into a terminal by `herdr account status`: a control character in one of
    // them would be an escape sequence on someone's screen.
    let field = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            account
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| is_printable_identity(value))
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

/// Whether one `oauthAccount` display field is safe to print at a terminal.
///
/// Empty is nothing to show. A control character would be an escape sequence on
/// someone's screen, and the bidi and zero-width formatting characters can
/// reorder or hide what surrounds them, so a value that renders as one address
/// and reads as another is refused rather than cleaned up — the caller then
/// falls through to the next key name, and failing that reports the identity as
/// unknown. Length is checked first so the scan itself stays bounded.
fn is_printable_identity(value: &str) -> bool {
    !value.is_empty()
        && value.chars().take(MAX_IDENTITY_CHARS + 1).count() <= MAX_IDENTITY_CHARS
        && !value
            .chars()
            .any(|ch| ch.is_control() || is_text_direction_control(ch))
}

/// The bidi and zero-width formatting characters.
///
/// The same class `sanitize_status_text` (`src/app/tab_bar_status.rs`) refuses
/// for the tab bar; that helper is private to its module and `src/app/` is not
/// a file this fork edits, so the list is repeated rather than shared.
fn is_text_direction_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

// ---------------------------------------------------------------------------
// Write half: building a new profile directory out of an existing one.
// ---------------------------------------------------------------------------

/// Mode of a profile directory on unix. A Claude config directory holds
/// credentials, so it is never group- or world-readable.
#[cfg(unix)]
pub const PROFILE_DIR_MODE: u32 = 0o700;

/// Mode of a file the seed writes on unix.
#[cfg(unix)]
const PROFILE_FILE_MODE: u32 = 0o600;

/// Largest file the seed copies. `settings.json` and `.claude.json` are the
/// only two, and a `.claude.json` is megabytes at worst.
const MAX_COPIED_BYTES: u64 = MAX_CLAUDE_JSON_BYTES;

/// One entry a seed would create in the new profile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SeedEntry {
    /// Name of the entry inside the profile directory.
    pub entry: String,
    /// Where it comes from, already resolved through a symlink so a chain of
    /// profiles cannot form.
    pub source: PathBuf,
    /// Where it lands in the new profile.
    pub target: PathBuf,
}

/// Everything `herdr account add` would do to a new profile directory, decided
/// before a single byte is written so `--dry-run` and the real run cannot
/// disagree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SeedPlan {
    /// The profile directory being copied from (lexically normalized).
    pub source: PathBuf,
    /// The profile directory being created (lexically normalized).
    pub target: PathBuf,
    /// [`SHARED_ENTRIES`] present in the source: shared by symlink.
    pub links: Vec<SeedEntry>,
    /// [`COPIED_ENTRIES`] present in the source.
    pub copies: Vec<SeedEntry>,
    /// The subset of `copies` whose identity keys are removed on the way.
    pub scrub: Vec<PathBuf>,
    /// Everything the seed deliberately leaves behind, with the reason.
    pub skipped: Vec<String>,
}

/// What [`apply_seed`] actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SeedReport {
    /// The target directory did not exist and was created.
    pub created: bool,
    pub linked: Vec<PathBuf>,
    pub copied: Vec<PathBuf>,
    pub scrubbed: Vec<PathBuf>,
    /// Non-fatal problems: an entry that was already there, one the platform
    /// could not share, one that could not be scrubbed.
    pub warnings: Vec<String>,
}

/// A path with `.` and `..` removed, purely lexically.
///
/// Sound for a path whose components do not exist yet (nothing can be a
/// symlink), and the second half of [`resolved_key`] for one that does.
fn collapse(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut collapsed = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match collapsed.components().next_back() {
                // `/..` is `/`, and `..` at the start of a relative path has
                // nothing to pop, so it stays.
                Some(Component::Normal(_)) => {
                    collapsed.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => collapsed.push(component.as_os_str()),
            },
            other => collapsed.push(other.as_os_str()),
        }
    }
    collapsed
}

/// A path as the *filesystem* sees it: the longest existing prefix resolved
/// through symlinks, with whatever does not exist yet appended.
///
/// [`crate::accounts::config::dir_key`] is lexical — it keeps `..` and cannot
/// see a symlink — so `/a/link` and `/a/real` compare unequal even when `link`
/// points at `real`, and `/a/x/../a` compares unequal to `/a/a`. Either
/// spelling would be two account profiles quietly sharing one login, so every
/// check that must not be talked around compares these keys too.
///
/// Falls back to the lexical form when nothing on the path exists; there is
/// then no symlink to hide behind either.
pub fn resolved_key(path: &Path) -> PathBuf {
    let lexical = collapse(path);
    let mut prefix = lexical.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&prefix) {
            let mut resolved = canonical;
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        let Some(name) = prefix.file_name().map(std::ffi::OsStr::to_os_string) else {
            return lexical;
        };
        tail.push(name);
        if !prefix.pop() {
            return lexical;
        }
    }
}

/// Whether `inner` is `outer` or lives inside it, comparing both the lexical
/// and the resolved spelling of each.
fn contains_or_equals(outer: &Path, inner: &Path) -> bool {
    inner.starts_with(outer) || resolved_key(inner).starts_with(resolved_key(outer))
}

/// The invariant this module exists to keep.
///
/// Nothing named [`CREDENTIALS_FILE`] may appear on either end of any planned
/// operation, and nothing the seed would *share* may be a directory that holds
/// credentials — a `projects` symlink pointing at a Claude config directory
/// would put one account's login inside another profile just as surely as
/// copying the file. Checked when a plan is built *and* again before it is
/// applied, so a future edit to the seed lists — or a plan built by some other
/// code path — cannot quietly start moving one account's login into another
/// profile.
fn guard_no_credentials(plan: &SeedPlan) -> io::Result<()> {
    let is_credentials =
        |path: &Path| path.file_name().and_then(|name| name.to_str()) == Some(CREDENTIALS_FILE);
    let offending = plan
        .links
        .iter()
        .chain(plan.copies.iter())
        .flat_map(|entry| [entry.source.as_path(), entry.target.as_path()])
        .chain(plan.scrub.iter().map(PathBuf::as_path))
        .find(|path| is_credentials(path));
    if let Some(path) = offending {
        return Err(io::Error::other(format!(
            "refusing to seed {}: credentials are never shared between account profiles",
            path.display()
        )));
    }

    for entry in &plan.links {
        // Existence only; the file is never opened.
        if std::fs::symlink_metadata(entry.source.join(CREDENTIALS_FILE)).is_ok() {
            return Err(io::Error::other(format!(
                "refusing to share {} as {}: it holds credentials, so it is an account's own \
                 config directory rather than shared state",
                entry.source.display(),
                entry.entry
            )));
        }
        if contains_or_equals(&entry.source, &plan.source)
            || contains_or_equals(&entry.source, &plan.target)
        {
            return Err(io::Error::other(format!(
                "refusing to share {} as {}: it contains an account profile directory",
                entry.source.display(),
                entry.entry
            )));
        }
    }
    Ok(())
}

/// Decide how a new profile directory would be seeded from an existing one.
///
/// Pure over a directory *listing*: it stats the source's entries and reads
/// nothing. The three lists in this module are the only thing it consults, so
/// an entry nobody classified is skipped rather than guessed at.
pub fn plan_seed(source: &Path, target: &Path) -> io::Result<SeedPlan> {
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "source profile directory not found: {} (pass --from <profile> or --config-dir)",
                source.display()
            ),
        ));
    }

    let source_dir = crate::accounts::config::dir_key(source);
    let target_dir = crate::accounts::config::dir_key(target);
    // Lexically *and* as the filesystem resolves them: `dir_key` keeps `..`
    // and cannot see a symlink, so `<source>/../<source>` and a link pointing
    // at the source both spell the source's own directory without matching it.
    // Seeding a profile into its own source is two account profiles sharing
    // one login, which is the failure this epic exists to prevent.
    let source_real = resolved_key(source);
    let target_real = resolved_key(target);
    if source_dir == target_dir || source_real == target_real {
        return Err(io::Error::other(format!(
            "the new profile would use the source's own directory: {}",
            source_dir.display()
        )));
    }
    if target_dir.starts_with(&source_dir)
        || source_dir.starts_with(&target_dir)
        || target_real.starts_with(&source_real)
        || source_real.starts_with(&target_real)
    {
        return Err(io::Error::other(format!(
            "account profile directories must not nest: {} and {}",
            source_dir.display(),
            target_dir.display()
        )));
    }

    let mut plan = SeedPlan {
        source: source_dir,
        target: target_dir,
        links: Vec::new(),
        copies: Vec::new(),
        scrub: Vec::new(),
        skipped: Vec::new(),
    };

    for entry in SHARED_ENTRIES {
        let path = source.join(entry);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            plan.skipped
                .push(format!("{entry}: absent from the source profile"));
            continue;
        };
        // A source entry that is itself a symlink (the source was seeded too)
        // is followed once, so profiles never chain: every profile points
        // straight at the directory that actually holds the transcripts.
        let resolved = if metadata.file_type().is_symlink() {
            match std::fs::canonicalize(&path) {
                Ok(resolved) => resolved,
                Err(err) => {
                    plan.skipped.push(format!(
                        "{entry}: the source profile's link does not resolve ({err})"
                    ));
                    continue;
                }
            }
        } else {
            path
        };
        plan.links.push(SeedEntry {
            entry: (*entry).to_string(),
            source: resolved,
            target: plan.target.join(entry),
        });
    }

    for entry in COPIED_ENTRIES {
        let path = source.join(entry);
        let Ok(link_metadata) = std::fs::symlink_metadata(&path) else {
            plan.skipped
                .push(format!("{entry}: absent from the source profile"));
            continue;
        };
        // Resolved before it is classified, so `guard_no_credentials` below
        // sees the name of the file that would actually be read: a
        // `.claude.json` symlinked at `.credentials.json` must never be copied
        // into a new profile under an innocent name.
        let path = if link_metadata.file_type().is_symlink() {
            match std::fs::canonicalize(&path) {
                Ok(resolved) => resolved,
                Err(err) => {
                    plan.skipped.push(format!(
                        "{entry}: the source profile's link does not resolve ({err})"
                    ));
                    continue;
                }
            }
        } else {
            path
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            plan.skipped
                .push(format!("{entry}: absent from the source profile"));
            continue;
        };
        if !metadata.is_file() {
            plan.skipped
                .push(format!("{entry}: not a regular file in the source profile"));
            continue;
        }
        let destination = plan.target.join(entry);
        plan.copies.push(SeedEntry {
            entry: (*entry).to_string(),
            source: path,
            target: destination.clone(),
        });
        if *entry == CLAUDE_JSON_FILE {
            plan.scrub.push(destination);
        }
    }

    for entry in PRIVATE_ENTRIES {
        if std::fs::symlink_metadata(source.join(entry)).is_ok() {
            plan.skipped
                .push(format!("{entry}: private to each account; never copied"));
        }
    }

    let mut extras: Vec<String> = Vec::new();
    if let Ok(listing) = std::fs::read_dir(source) {
        for entry in listing.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let known = SHARED_ENTRIES.contains(&name.as_str())
                || COPIED_ENTRIES.contains(&name.as_str())
                || PRIVATE_ENTRIES.contains(&name.as_str());
            if !known {
                extras.push(name);
            }
        }
    }
    extras.sort();
    for name in extras {
        plan.skipped
            .push(format!("{name}: not seeded (Claude Code recreates it)"));
    }

    guard_no_credentials(&plan)?;
    Ok(plan)
}

/// Carry out a [`SeedPlan`].
///
/// Never destructive: an entry that already exists in the target is left
/// exactly as it was and reported as a warning. `force` only relaxes the
/// refusal to seed into a directory that already has something in it, and
/// never relaxes the refusal to seed into one that already holds credentials.
pub fn apply_seed(plan: &SeedPlan, force: bool) -> io::Result<SeedReport> {
    guard_no_credentials(plan)?;

    let target = plan.target.as_path();
    let mut report = SeedReport::default();

    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::other(format!(
                "refusing to seed through a symlink: {}",
                target.display()
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::other(format!(
                "the profile directory exists and is not a directory: {}",
                target.display()
            )));
        }
        Ok(_) => {
            if std::fs::read_dir(target)?.next().is_some() {
                if !force {
                    return Err(io::Error::other(format!(
                        "profile directory is not empty: {} (pass --force to seed into it)",
                        target.display()
                    )));
                }
                // Existence only; the file is never opened.
                if std::fs::symlink_metadata(target.join(CREDENTIALS_FILE)).is_ok() {
                    return Err(io::Error::other(format!(
                        "{} is already logged in; refusing to seed another account's identity into it",
                        target.display()
                    )));
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => report.created = true,
        Err(err) => return Err(err),
    }

    // Created 0700 rather than created-then-narrowed: a Claude config
    // directory must never be readable by anyone else, not even for the
    // instant between `create_dir_all` and a `chmod`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(PROFILE_DIR_MODE)
            .create(target)?;
        // `recursive` leaves an existing directory's mode alone.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(PROFILE_DIR_MODE))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(target)?;

    for entry in &plan.links {
        if std::fs::symlink_metadata(&entry.target).is_ok() {
            report.warnings.push(format!(
                "{}: already present in the new profile; left as it was",
                entry.entry
            ));
            continue;
        }
        match link_entry(&entry.source, &entry.target) {
            Ok(()) => report.linked.push(entry.target.clone()),
            Err(err) => report.warnings.push(format!(
                "{}: not shared with the source profile ({err})",
                entry.entry
            )),
        }
    }

    let scrub: std::collections::BTreeSet<&Path> =
        plan.scrub.iter().map(PathBuf::as_path).collect();
    for entry in &plan.copies {
        if std::fs::symlink_metadata(&entry.target).is_ok() {
            report.warnings.push(format!(
                "{}: already present in the new profile; left as it was",
                entry.entry
            ));
            continue;
        }
        let body = match read_bounded(&entry.source) {
            Ok(body) => body,
            Err(reason) => {
                report
                    .warnings
                    .push(format!("{}: not copied ({reason})", entry.entry));
                continue;
            }
        };
        let scrubbed = scrub.contains(entry.target.as_path());
        let body = if scrubbed {
            match scrub_claude_json(&body) {
                Ok(text) => text,
                Err(reason) => {
                    // Never copy an identity file this build could not scrub:
                    // the whole point of the copy is that it arrives without
                    // the source account's identity in it.
                    report
                        .warnings
                        .push(format!("{}: not copied ({reason})", entry.entry));
                    continue;
                }
            }
        } else {
            body
        };
        match write_private(&entry.target, &body) {
            Ok(()) => {}
            // Something created the entry between the check above and this
            // write. Never destructive means never destructive: leave it.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                report.warnings.push(format!(
                    "{}: already present in the new profile; left as it was",
                    entry.entry
                ));
                continue;
            }
            Err(err) => return Err(err),
        }
        if scrubbed {
            report.scrubbed.push(entry.target.clone());
        }
        report.copied.push(entry.target.clone());
    }

    Ok(report)
}

/// Read a file the seed copies, refusing anything implausibly large or not
/// text. Nothing outside [`COPIED_ENTRIES`] ever reaches this.
fn read_bounded(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|err| err.to_string())?;
    if metadata.len() > MAX_COPIED_BYTES {
        return Err(format!(
            "{} bytes is larger than this build copies",
            metadata.len()
        ));
    }
    std::fs::read_to_string(path).map_err(|err| err.to_string())
}

/// Write a seeded file, owner-only on unix, refusing to clobber.
fn write_private(path: &Path, body: &str) -> io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(PROFILE_FILE_MODE);
    }
    let mut file = options.open(path)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()
}

#[cfg(unix)]
fn link_entry(source: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

/// Windows has no unprivileged symlink by default and herdr does not create
/// junctions, so a share that cannot be made is reported as a warning by the
/// caller and the new profile simply starts without that entry.
#[cfg(windows)]
fn link_entry(source: &Path, target: &Path) -> io::Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, target)
    } else {
        std::os::windows::fs::symlink_file(source, target)
    }
}

/// Whether a top-level `.claude.json` key is removed when the file is copied.
///
/// The explicit list plus the substring list: a key this build has never heard
/// of cannot carry a secret into a new profile just because it is new.
pub fn is_scrubbed_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    SCRUBBED_IDENTITY_KEYS
        .iter()
        .any(|scrubbed| scrubbed.eq_ignore_ascii_case(key))
        || SCRUBBED_KEY_SUBSTRINGS
            .iter()
            .any(|needle| lowered.contains(needle))
}

/// Remove the identity keys from a `.claude.json` body, keeping everything
/// else (onboarding, project trust, MCP servers) so the new profile does not
/// start from zero.
///
/// Top level only, by decision (a): a nested MCP server's own credentials are
/// the user's, not the account's, and dropping them would break the copied
/// settings this exists to carry over.
pub fn scrub_claude_json(text: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| format!("{CLAUDE_JSON_FILE} is not valid JSON: {err}"))?;
    let serde_json::Value::Object(mut object) = value else {
        return Err(format!("{CLAUDE_JSON_FILE} is not a JSON object"));
    };
    object.retain(|key, _| !is_scrubbed_key(key));
    serde_json::to_string_pretty(&serde_json::Value::Object(object)).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guide documents the seed layout, and this is what keeps it true.
    ///
    /// `docs/fork/accounts.md` prints the share/copy/private lists as a table
    /// a human reads before trusting a new profile with an account. Adding an
    /// entry here and forgetting the guide would leave that table quietly
    /// wrong, so every constant this module exports must appear in the guide
    /// as an inline code span.
    #[test]
    fn the_accounts_guide_lists_the_seed_layout() {
        let guide_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fork/accounts.md");
        let guide = std::fs::read_to_string(&guide_path)
            .unwrap_or_else(|err| panic!("read {}: {err}", guide_path.display()));

        let mut missing = Vec::new();
        for entry in SHARED_ENTRIES
            .iter()
            .chain(COPIED_ENTRIES)
            .chain(PRIVATE_ENTRIES)
            .chain(SCRUBBED_IDENTITY_KEYS)
            .chain(SCRUBBED_KEY_SUBSTRINGS)
            .chain([&HOOK_DIR, &HOOK_FILE])
        {
            if !guide.contains(&format!("`{entry}`")) {
                missing.push(*entry);
            }
        }
        assert!(
            missing.is_empty(),
            "docs/fork/accounts.md does not document {missing:?}; every \
             src/accounts/layout.rs entry must appear there as `like this`"
        );
    }

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
            // A control character would be an escape sequence once printed.
            r#"{"oauthAccount": {"emailAddress": "a\u001b[2Jb"}}"#,
            r#"{"oauthAccount": {"emailAddress": "a\nb"}}"#,
            // A right-to-left override renders an address as one thing while it
            // reads as another.
            r#"{"oauthAccount": {"emailAddress": "a\u202eb@example.test"}}"#,
            r#"{"oauthAccount": {"emailAddress": "a\u200bb@example.test"}}"#,
        ] {
            assert_eq!(identity_from_claude_json(text), None, "{text:?}");
        }
    }

    /// `.claude.json` is not herdr's file, and `herdr account status` prints
    /// what this returns straight into a terminal.
    #[test]
    fn an_oversized_identity_field_is_refused_rather_than_printed() {
        let huge = "a".repeat(MAX_IDENTITY_CHARS + 1);
        let text = format!(
            r#"{{"oauthAccount": {{"emailAddress": "{huge}", "subscriptionType": "max"}}}}"#
        );
        let identity = identity_from_claude_json(&text).expect("the plan still shows");
        assert_eq!(identity.email, None, "the oversized field is dropped");
        assert_eq!(identity.plan.as_deref(), Some("max"));

        // Exactly at the bound is still shown: the refusal is a cap, not a
        // guess about what an address may look like.
        let at_limit = "b".repeat(MAX_IDENTITY_CHARS);
        let text = format!(r#"{{"oauthAccount": {{"emailAddress": "{at_limit}"}}}}"#);
        assert_eq!(
            identity_from_claude_json(&text)
                .expect("identity")
                .email
                .as_deref(),
            Some(at_limit.as_str())
        );
    }

    /// A deeply nested `.claude.json` must not take the parser down with it.
    #[test]
    fn a_deeply_nested_claude_json_degrades_to_no_identity() {
        let depth = 2_000;
        let text = format!(
            "{{\"oauthAccount\": {{\"emailAddress\": \"a@b.test\"}}, \"deep\": {}{}}}",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        assert_eq!(identity_from_claude_json(&text), None);
    }

    #[test]
    fn inspecting_a_missing_directory_reports_everything_absent() {
        let root = temp_dir("missing");
        let dir = root.join("gone");
        let inspection = inspect_dir(&dir, InspectOptions::health());
        assert!(!inspection.dir_exists);
        assert!(!inspection.logged_in);
        assert!(!inspection.hook_installed);
        assert_eq!(inspection.credentials_mode_ok, None);
        assert!(inspection.broken_links.is_empty());
        assert_eq!(inspection.identity, None);
        let _ = std::fs::remove_dir_all(&root);
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

    /// Proof, not just intent: an unreadable credentials file is still reported
    /// as logged in, which is only possible if nothing ever opens it.
    #[cfg(unix)]
    #[test]
    fn inspect_never_opens_the_credentials_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_dir("unreadable");
        let dir = root.join("profile");
        std::fs::create_dir_all(&dir).expect("profile dir");
        let credentials = dir.join(CREDENTIALS_FILE);
        std::fs::write(&credentials, "{\"claudeAiOauth\":{\"accessToken\":\"x\"}}")
            .expect("credentials");
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");
        if std::fs::read_to_string(&credentials).is_ok() {
            // root, or a filesystem that ignores modes: the check would prove
            // nothing here.
            let _ = std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600));
            let _ = std::fs::remove_dir_all(&root);
            return;
        }

        let inspection = inspect_dir(&dir, InspectOptions::with_identity());
        assert!(inspection.logged_in, "existence is read from metadata only");
        assert_eq!(
            inspection.credentials_mode_ok,
            Some(true),
            "0000 is private"
        );
        assert_eq!(inspection.identity, None, "there is no .claude.json here");

        let _ = std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // seeding
    // -----------------------------------------------------------------------

    /// A source profile that looks like a used Claude installation, including
    /// the one file that must never move.
    fn seed_source_tree(root: &Path) -> PathBuf {
        let source = root.join("source");
        std::fs::create_dir_all(source.join("projects")).expect("projects");
        std::fs::create_dir_all(source.join("todos")).expect("todos");
        std::fs::create_dir_all(source.join("statsig")).expect("statsig");
        std::fs::create_dir_all(source.join("shell-snapshots")).expect("shell-snapshots");
        std::fs::write(source.join("CLAUDE.md"), "# memory\n").expect("CLAUDE.md");
        std::fs::write(source.join("settings.json"), r#"{"hooks":{}}"#).expect("settings.json");
        std::fs::write(
            source.join(CLAUDE_JSON_FILE),
            r#"{"hasCompletedOnboarding":true,"projects":{"/tmp":{"allowedTools":[]}},
                "mcpServers":{"a":{"command":"x"}},
                "oauthAccount":{"emailAddress":"person@example.test"},
                "primaryApiKey":"sk-secret","customApiKeyResponses":{"approved":["sk-1"]}}"#,
        )
        .expect(CLAUDE_JSON_FILE);
        std::fs::write(
            source.join(CREDENTIALS_FILE),
            r#"{"claudeAiOauth":{"a":1}}"#,
        )
        .expect("credentials");
        std::fs::write(source.join("mystery-file"), "?").expect("mystery");
        source
    }

    #[test]
    fn a_seed_plan_shares_transcripts_copies_settings_and_touches_no_credentials() {
        let root = temp_dir("plan");
        let source = seed_source_tree(&root);
        let target = root.join("target");

        let plan = plan_seed(&source, &target).expect("plan");

        let linked: Vec<&str> = plan
            .links
            .iter()
            .map(|entry| entry.entry.as_str())
            .collect();
        assert_eq!(linked, vec!["projects", "todos", "CLAUDE.md"]);
        for entry in &plan.links {
            assert_eq!(entry.target, plan.target.join(&entry.entry));
            assert_eq!(entry.source, source.join(&entry.entry));
        }

        let copied: Vec<&str> = plan
            .copies
            .iter()
            .map(|entry| entry.entry.as_str())
            .collect();
        assert_eq!(copied, vec!["settings.json", CLAUDE_JSON_FILE]);
        assert_eq!(plan.scrub, vec![plan.target.join(CLAUDE_JSON_FILE)]);

        let skipped = plan.skipped.join("\n");
        assert!(
            skipped.contains(&format!("{CREDENTIALS_FILE}: private to each account")),
            "{skipped}"
        );
        assert!(skipped.contains("statsig: private"), "{skipped}");
        assert!(skipped.contains("mystery-file: not seeded"), "{skipped}");
        assert!(skipped.contains("skills: absent"), "{skipped}");

        // The skip list names the credentials file (that is the point of the
        // list); nothing the plan would actually *do* may mention it.
        let operations = serde_json::json!({
            "links": &plan.links,
            "copies": &plan.copies,
            "scrub": &plan.scrub,
        })
        .to_string();
        assert!(
            !operations.contains(CREDENTIALS_FILE),
            "the plan must not operate on a credentials path: {operations}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_seed_plan_refuses_the_source_itself_and_nested_directories() {
        let root = temp_dir("nesting");
        let source = seed_source_tree(&root);

        for target in [source.clone(), source.join("inner"), root.clone()] {
            let error = plan_seed(&source, &target)
                .expect_err(&format!("{} must be refused", target.display()));
            assert!(
                error.to_string().contains("directory") || error.to_string().contains("nest"),
                "{error}"
            );
        }

        let missing = root.join("gone");
        let error = plan_seed(&missing, &root.join("target")).expect_err("missing source");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole point of `guard_no_credentials`: even a plan built by hand
    /// with a credentials path in it is refused, at plan time and at apply
    /// time.
    #[test]
    fn a_plan_naming_a_credentials_file_is_refused() {
        let root = temp_dir("guard");
        let source = seed_source_tree(&root);
        let target = root.join("target");
        let mut plan = plan_seed(&source, &target).expect("plan");
        plan.copies.push(SeedEntry {
            entry: CREDENTIALS_FILE.to_string(),
            source: source.join(CREDENTIALS_FILE),
            target: target.join(CREDENTIALS_FILE),
        });

        let error = apply_seed(&plan, false).expect_err("credentials must be refused");
        assert!(error.to_string().contains("never shared"), "{error}");
        assert!(
            !target.join(CREDENTIALS_FILE).exists(),
            "nothing may be written once the guard fires"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn applying_a_seed_links_copies_scrubs_and_leaves_credentials_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_dir("apply");
        let source = seed_source_tree(&root);
        let target = root.join("target");
        let credentials = source.join(CREDENTIALS_FILE);
        let before = std::fs::metadata(&credentials)
            .expect("credentials metadata")
            .modified()
            .expect("mtime");

        let plan = plan_seed(&source, &target).expect("plan");
        let report = apply_seed(&plan, false).expect("apply");
        assert!(report.created);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        assert_eq!(
            std::fs::metadata(&target)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            PROFILE_DIR_MODE
        );

        assert!(std::fs::symlink_metadata(target.join("projects"))
            .expect("projects")
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(target.join("projects")).expect("read_link"),
            source.join("projects")
        );

        // A shared entry is genuinely one directory, not a copy.
        std::fs::write(source.join("projects").join("a.jsonl"), "{}").expect("transcript");
        assert!(target.join("projects").join("a.jsonl").is_file());

        assert_eq!(
            std::fs::read_to_string(target.join("settings.json")).expect("settings"),
            r#"{"hooks":{}}"#
        );

        let seeded: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(target.join(CLAUDE_JSON_FILE)).expect("claude json"),
        )
        .expect("json");
        assert_eq!(seeded.get("oauthAccount"), None);
        assert_eq!(seeded.get("primaryApiKey"), None);
        assert_eq!(seeded.get("customApiKeyResponses"), None);
        assert!(seeded.get("mcpServers").is_some());
        assert_eq!(seeded["hasCompletedOnboarding"], serde_json::json!(true));
        assert_eq!(report.scrubbed, vec![target.join(CLAUDE_JSON_FILE)]);

        // The one file this epic must never move, on both ends.
        assert!(
            !target.join(CREDENTIALS_FILE).exists(),
            "a seeded profile is logged out"
        );
        let after = std::fs::metadata(&credentials)
            .expect("credentials metadata")
            .modified()
            .expect("mtime");
        assert_eq!(before, after, "the source credentials file was touched");

        for path in [target.join("settings.json"), target.join(CLAUDE_JSON_FILE)] {
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("copied metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                PROFILE_FILE_MODE,
                "{}",
                path.display()
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_source_symlink_is_followed_once_so_profiles_never_chain() {
        let root = temp_dir("chain");
        let first = seed_source_tree(&root);
        let second = root.join("second");
        let third = root.join("third");

        apply_seed(&plan_seed(&first, &second).expect("plan"), false).expect("apply");
        apply_seed(&plan_seed(&second, &third).expect("plan"), false).expect("apply");

        assert_eq!(
            std::fs::read_link(third.join("projects")).expect("read_link"),
            std::fs::canonicalize(first.join("projects")).expect("canonical")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_empty_target_needs_force_and_a_logged_in_one_is_refused_outright() {
        let root = temp_dir("force");
        let source = seed_source_tree(&root);
        let target = root.join("target");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(target.join("something"), "x").expect("something");

        let plan = plan_seed(&source, &target).expect("plan");
        let error = apply_seed(&plan, false).expect_err("non-empty target");
        assert!(error.to_string().contains("--force"), "{error}");

        let report = apply_seed(&plan, true).expect("forced apply");
        assert!(!report.created);
        assert!(target.join("settings.json").is_file());

        // Now the same directory, logged in: force must not overlay another
        // account's identity onto a live login.
        std::fs::write(target.join(CREDENTIALS_FILE), "{}").expect("credentials");
        let error = apply_seed(&plan, true).expect_err("logged-in target");
        assert!(error.to_string().contains("already logged in"), "{error}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_resolved_key_collapses_dot_dot_and_falls_back_lexically() {
        assert_eq!(
            resolved_key(Path::new("/nonexistent-herdr-accounts/a/../b/./c")),
            PathBuf::from("/nonexistent-herdr-accounts/b/c")
        );
        assert_eq!(resolved_key(Path::new("/..")), PathBuf::from("/"));
    }

    #[cfg(unix)]
    #[test]
    fn a_resolved_key_sees_through_a_symlinked_ancestor() {
        let root = temp_dir("resolved");
        let real = root.join("real");
        std::fs::create_dir_all(real.join("inner")).expect("inner");
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert_eq!(
            resolved_key(&link.join("inner")),
            resolved_key(&real.join("inner"))
        );
        // And a directory that does not exist yet still resolves its parents.
        assert_eq!(
            resolved_key(&link.join("later")),
            resolved_key(&real).join("later")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `dir_key` is lexical, so these three spellings of the source directory
    /// all compare unequal to it. Every one of them would be a second account
    /// profile pointing at the first one's login.
    #[test]
    fn a_target_that_only_spells_the_source_differently_is_refused() {
        let root = temp_dir("spelling");
        let source = seed_source_tree(&root);

        let error = plan_seed(&source, &root.join("elsewhere").join("..").join("source"))
            .expect_err("`..` must not walk back into the source");
        assert!(
            error.to_string().contains("source's own directory"),
            "{error}"
        );

        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&source, &link).expect("symlink");
            let error = plan_seed(&source, &link).expect_err("a symlinked target");
            assert!(
                error.to_string().contains("source's own directory"),
                "{error}"
            );
            let error = plan_seed(&link, &source).expect_err("a symlinked source");
            assert!(
                error.to_string().contains("source's own directory"),
                "{error}"
            );

            let inner = root.join("link").join("inner");
            let error = plan_seed(&source, &inner).expect_err("nested through a link");
            assert!(error.to_string().contains("nest"), "{error}");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The crafted-source case: a copied entry that is really the credentials
    /// file wearing another name.
    #[cfg(unix)]
    #[test]
    fn a_copied_entry_symlinked_at_the_credentials_file_is_refused() {
        let root = temp_dir("disguise");
        let source = seed_source_tree(&root);
        std::fs::remove_file(source.join(CLAUDE_JSON_FILE)).expect("remove .claude.json");
        std::os::unix::fs::symlink(source.join(CREDENTIALS_FILE), source.join(CLAUDE_JSON_FILE))
            .expect("symlink");

        let error = plan_seed(&source, &root.join("target")).expect_err("disguised credentials");
        assert!(error.to_string().contains("never shared"), "{error}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other crafted-source case: a *shared* entry that is really a whole
    /// Claude config directory, so the new profile would read the source's
    /// credentials through it.
    #[cfg(unix)]
    #[test]
    fn a_shared_entry_pointing_at_a_config_directory_is_refused() {
        let root = temp_dir("shared-config");
        let source = seed_source_tree(&root);
        std::fs::remove_dir_all(source.join("projects")).expect("remove projects");
        std::os::unix::fs::symlink(&source, source.join("projects")).expect("symlink");

        let error = plan_seed(&source, &root.join("target")).expect_err("credentials directory");
        assert!(error.to_string().contains("holds credentials"), "{error}");

        // Same shape without a credentials file: a link that swallows the
        // profile directories themselves is still refused.
        let bare = root.join("bare");
        std::fs::create_dir_all(&bare).expect("bare source");
        std::os::unix::fs::symlink(&root, bare.join("todos")).expect("symlink");
        let error = plan_seed(&bare, &root.join("target2")).expect_err("ancestor");
        assert!(
            error.to_string().contains("contains an account profile"),
            "{error}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_entries_are_left_alone_rather_than_overwritten() {
        let root = temp_dir("existing");
        let source = seed_source_tree(&root);
        let target = root.join("target");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(target.join("settings.json"), "mine").expect("settings");

        let report = apply_seed(&plan_seed(&source, &target).expect("plan"), true).expect("apply");
        assert_eq!(
            std::fs::read_to_string(target.join("settings.json")).expect("settings"),
            "mine"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("settings.json: already present")),
            "{:?}",
            report.warnings
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unscrubbable_identity_file_is_not_copied_at_all() {
        let root = temp_dir("unscrubbable");
        let source = root.join("source");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(source.join(CLAUDE_JSON_FILE), "not json at all").expect("claude json");
        let target = root.join("target");

        let report = apply_seed(&plan_seed(&source, &target).expect("plan"), false).expect("apply");
        assert!(
            !target.join(CLAUDE_JSON_FILE).exists(),
            "a file this build cannot scrub must not be copied"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("not valid JSON")),
            "{:?}",
            report.warnings
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scrubbing_drops_identity_and_keeps_everything_else() {
        let scrubbed = scrub_claude_json(
            r#"{
                "hasCompletedOnboarding": true,
                "projects": {"/tmp": {"allowedTools": []}},
                "mcpServers": {"a": {"command": "x"}},
                "oauthAccount": {"emailAddress": "person@example.test"},
                "primaryApiKey": "sk-secret",
                "customApiKeyResponses": {"approved": ["sk-1"]},
                "OAUTHACCOUNT": {"emailAddress": "person@example.test"},
                "cachedRefreshToken": "rt",
                "someCredentialBlob": "c",
                "clientSecret": "s"
            }"#,
        )
        .expect("scrub");

        for forbidden in [
            "oauthAccount",
            "OAUTHACCOUNT",
            "primaryApiKey",
            "customApiKeyResponses",
            "cachedRefreshToken",
            "someCredentialBlob",
            "clientSecret",
            "person@example.test",
            "sk-secret",
            "sk-1",
            "rt",
        ] {
            assert!(
                !scrubbed.contains(forbidden),
                "{forbidden} survived: {scrubbed}"
            );
        }
        let value: serde_json::Value = serde_json::from_str(&scrubbed).expect("json");
        assert_eq!(value["hasCompletedOnboarding"], serde_json::json!(true));
        assert!(value.get("projects").is_some());
        assert!(value.get("mcpServers").is_some());
    }

    #[test]
    fn scrubbing_refuses_anything_that_is_not_a_json_object() {
        for text in ["", "not json", "[]", "3", "\"a\"", "null"] {
            assert!(scrub_claude_json(text).is_err(), "{text:?}");
        }
        assert_eq!(
            scrub_claude_json("{}").expect("empty object"),
            "{}".to_string()
        );
    }

    #[test]
    fn every_scrubbed_substring_is_matched_case_insensitively() {
        for needle in SCRUBBED_KEY_SUBSTRINGS {
            assert_eq!(
                *needle,
                needle.to_ascii_lowercase(),
                "the substring list must be lowercase for the case-insensitive match"
            );
            assert!(is_scrubbed_key(&format!("my{}Blob", needle.to_uppercase())));
        }
        for key in SCRUBBED_IDENTITY_KEYS {
            assert!(is_scrubbed_key(key));
            assert!(is_scrubbed_key(&key.to_uppercase()));
        }
        assert!(!is_scrubbed_key("projects"));
        assert!(!is_scrubbed_key("hasCompletedOnboarding"));
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
