//! Credentials: the token store, paired devices, one-time pairing codes and
//! the per-peer failure limiter.
//!
//! Everything here is pure or plain blocking file I/O — no tokio, no axum, no
//! socket — so the whole security model is testable without a runtime. PR 4's
//! middleware and PR 8's pairing commands call it; nothing in this module
//! knows what an HTTP request is.
//!
//! Two rules shape the code:
//!
//! * **Nothing compares a secret with `==`.** Presented secrets are hashed
//!   with SHA-256 and compared with [`subtle`]'s constant-time equality, so
//!   neither the value nor its length leaks through timing. Only digests are
//!   ever stored for devices and pairing codes.
//! * **A wrong guess never teaches the guesser anything.** A device cookie
//!   with a real id and a wrong secret, a pairing code for an id that does not
//!   exist, and a pairing code with a wrong secret all produce the same
//!   outcome, and none of them deletes a file.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::{Choice, ConstantTimeEq};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use super::paths::{self, CONTROL_TOKEN_FILE, DEVICES_FILE, READ_TOKEN_FILE};

/// Bytes of randomness behind every token, device secret and pairing secret.
const SECRET_BYTES: usize = 32;
/// Hex characters a secret is presented as.
pub const SECRET_HEX_LEN: usize = SECRET_BYTES * 2;
/// Peers the failure limiter tracks before it starts evicting.
pub const MAX_TRACKED_PEERS: usize = 4096;

/// What a credential is allowed to do.
///
/// `control` implies `read`: a control credential may do everything a read
/// credential may. Nothing widens the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenScope {
    Read,
    Control,
}

impl TokenScope {
    /// Whether a credential with this scope satisfies a route needing `needed`.
    pub fn allows(self, needed: TokenScope) -> bool {
        matches!(
            (self, needed),
            (TokenScope::Control, _) | (TokenScope::Read, TokenScope::Read)
        )
    }

    /// Wire and file-name spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenScope::Read => "read",
            TokenScope::Control => "control",
        }
    }

    /// The token file this scope is stored in.
    pub fn token_file_name(self) -> &'static str {
        match self {
            TokenScope::Read => READ_TOKEN_FILE,
            TokenScope::Control => CONTROL_TOKEN_FILE,
        }
    }

    /// Both scopes, control first so a presented secret is checked against the
    /// stronger one before the weaker one. The commands that report per-scope
    /// counts name the two scopes explicitly, so this is the tests' way to
    /// cover both without repeating them.
    #[cfg(test)]
    pub fn all() -> [TokenScope; 2] {
        [TokenScope::Control, TokenScope::Read]
    }
}

/// SHA-256 of a secret. The only representation of a credential the gateway
/// keeps in memory or on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    /// Digest the presented bytes. Hashing first means the comparison below is
    /// over two fixed-length values whatever the caller sent, so a presented
    /// secret's length never changes the work done.
    pub fn of(secret: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(secret);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&hasher.finalize());
        Self(digest)
    }

    /// Constant-time equality as a [`Choice`], for callers that must not
    /// branch yet.
    pub fn ct_eq_choice(&self, other: &Self) -> Choice {
        self.0.ct_eq(&other.0)
    }

    /// Constant-time equality.
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.ct_eq_choice(other).into()
    }

    /// Lowercase hex, for the records that persist a digest.
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
        out
    }

    /// Parse the hex form written by [`TokenDigest::to_hex`].
    pub fn from_hex(value: &str) -> Option<Self> {
        if value.len() != 64 {
            return None;
        }
        let bytes = value.as_bytes();
        let mut digest = [0u8; 32];
        for (index, slot) in digest.iter_mut().enumerate() {
            let high = hex_value(bytes[index * 2])?;
            let low = hex_value(bytes[index * 2 + 1])?;
            *slot = (high << 4) | low;
        }
        Some(Self(digest))
    }
}

fn hex_digit(value: u8) -> char {
    char::from(match value {
        0..=9 => b'0' + value,
        _ => b'a' + (value - 10),
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 32 random bytes from the OS CSPRNG as 64 lowercase hex characters.
pub fn random_secret_hex() -> io::Result<String> {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|err| io::Error::other(format!("failed to read from the system CSPRNG: {err}")))?;
    let mut out = String::with_capacity(SECRET_HEX_LEN);
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    Ok(out)
}

/// Seconds since the unix epoch, for the records that carry a timestamp.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// How a request proved its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// The `herdr_gateway_device` cookie minted by a pairing exchange.
    Device { id: String },
}

/// An authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub scope: TokenScope,
    pub via: Credential,
}

impl Principal {
    pub fn bearer(scope: TokenScope) -> Self {
        Self {
            scope,
            via: Credential::Bearer,
        }
    }

    pub fn device(scope: TokenScope, id: String) -> Self {
        Self {
            scope,
            via: Credential::Device { id },
        }
    }

    /// Whether this caller satisfies a route that needs `needed`.
    pub fn allows(&self, needed: TokenScope) -> bool {
        self.scope.allows(needed)
    }
}

/// What a private file looks like from the outside, so a holder can tell that
/// it was replaced without reading it again.
///
/// [`paths::write_private_file`] renames a freshly created file over its
/// target, so on unix the inode alone is already decisive; the length and the
/// modification time cover the platforms that have no inode. Comparing a stamp
/// is two `stat`s, which is what makes a per-request freshness check cheap
/// enough to be unconditional.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: u64,
}

/// The stamp of `path`, or `None` when there is no regular file there.
fn stamp_of(path: &Path) -> Option<FileStamp> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

/// The two bearer tokens, as digests.
///
/// The secrets themselves are read once at load time to digest them and are
/// not kept; only `herdr gateway pair` ever needs the plain value again, and
/// it reads the file.
///
/// A store remembers where it was loaded from and what the two files looked
/// like, so [`TokenStore::refresh`] can pick up a `herdr gateway rotate-token`
/// that happened in another process. Rotation is a revocation, and a
/// revocation that only takes effect after a restart is not one.
#[derive(Debug, Clone)]
pub struct TokenStore {
    dir: PathBuf,
    read: TokenDigest,
    control: TokenDigest,
    /// Stamps of the `read` and `control` files, in that order, taken
    /// **before** the digests were read: a file replaced during the read is
    /// then reloaded on the next refresh rather than missed forever.
    stamps: [Option<FileStamp>; 2],
}

impl TokenStore {
    /// Load both token files from `dir`, creating the directory (`0700`) and
    /// any missing token (`0600`, 64 hex characters) on the way.
    ///
    /// An existing token that is group- or world-readable, owned by someone
    /// else, or not 64 hex characters is refused with a message naming the
    /// file, rather than silently regenerated: a token the user has already
    /// put on a phone must not change without them asking.
    pub fn load_or_create(dir: &Path) -> io::Result<Self> {
        paths::create_private_dir(dir)?;
        Self::from_digests(
            dir,
            load_or_create_token(dir, TokenScope::Read)?,
            load_or_create_token(dir, TokenScope::Control)?,
        )
    }

    /// Load both token files without creating anything.
    pub fn load(dir: &Path) -> io::Result<Self> {
        paths::verify_private_dir(dir)?;
        Self::from_digests(
            dir,
            read_token_stamped(&dir.join(READ_TOKEN_FILE))?,
            read_token_stamped(&dir.join(CONTROL_TOKEN_FILE))?,
        )
    }

    /// Reload both files when either has been replaced since they were read.
    ///
    /// Called before every bearer comparison, so a token rotated by
    /// `herdr gateway rotate-token` in another process stops working on the
    /// next request rather than at the next restart. The common case is two
    /// `stat` calls and no allocation.
    ///
    /// A store that cannot be reloaded keeps the digests it already has: a
    /// token file that momentarily cannot be read must not lock every client
    /// out, and the process that wrote it reports the same error to the
    /// operator. The stamp is still recorded, so one broken file is logged
    /// once rather than on every request, and any *further* change is retried.
    pub fn refresh(&mut self) {
        let stamps = [
            stamp_of(&self.dir.join(READ_TOKEN_FILE)),
            stamp_of(&self.dir.join(CONTROL_TOKEN_FILE)),
        ];
        if stamps == self.stamps {
            return;
        }
        match Self::load(&self.dir) {
            Ok(reloaded) => {
                tracing::info!(target: "gateway", "reloaded the gateway token store after it changed on disk");
                *self = reloaded;
            }
            Err(error) => {
                tracing::warn!(
                    target: "gateway",
                    error = %error,
                    "could not reload the gateway token store; the tokens already in memory stay in force"
                );
                self.stamps = stamps;
            }
        }
    }

    /// Two identical token files would make the read token a control token
    /// (`verify_bearer` checks control first); refuse them rather than let a
    /// copy-paste silently widen a scope.
    fn from_digests(
        dir: &Path,
        (read, read_stamp): (TokenDigest, Option<FileStamp>),
        (control, control_stamp): (TokenDigest, Option<FileStamp>),
    ) -> io::Result<Self> {
        if read.ct_eq(&control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} and {} hold the same token, so the read token would grant control; \
                     delete one of them to regenerate it",
                    dir.join(READ_TOKEN_FILE).display(),
                    dir.join(CONTROL_TOKEN_FILE).display()
                ),
            ));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            read,
            control,
            stamps: [read_stamp, control_stamp],
        })
    }

    /// The scope a presented bearer token proves, or `None`.
    ///
    /// Both comparisons always run, so a token matching neither costs the same
    /// as one matching `read`, and the presented value is never parsed as hex
    /// first — a malformed token simply fails to match.
    pub fn verify_bearer(&self, presented: &str) -> Option<TokenScope> {
        let digest = TokenDigest::of(presented.as_bytes());
        let is_control = digest.ct_eq_choice(&self.control);
        let is_read = digest.ct_eq_choice(&self.read);
        // An empty presented value must never match, even if a token file were
        // somehow empty; `load` refuses those, and this is the second line.
        if presented.is_empty() {
            return None;
        }
        if bool::from(is_control) {
            Some(TokenScope::Control)
        } else if bool::from(is_read) {
            Some(TokenScope::Read)
        } else {
            None
        }
    }

    /// Replace one scope's token file with a fresh secret.
    ///
    /// The caller is responsible for revoking that scope's devices
    /// ([`DeviceStore::revoke_scope`]) and its outstanding pairing codes
    /// ([`PairingStore::revoke_scope`]), and for reloading the store.
    pub fn rotate(dir: &Path, scope: TokenScope) -> io::Result<()> {
        paths::create_private_dir(dir)?;
        let secret = random_secret_hex()?;
        paths::write_private_file(&dir.join(scope.token_file_name()), secret.as_bytes())
    }

    /// The digest of one scope's token, so `rotate-token` can check that the
    /// file really changed.
    pub fn digest(&self, scope: TokenScope) -> TokenDigest {
        match scope {
            TokenScope::Read => self.read,
            TokenScope::Control => self.control,
        }
    }
}

fn load_or_create_token(
    dir: &Path,
    scope: TokenScope,
) -> io::Result<(TokenDigest, Option<FileStamp>)> {
    let path = dir.join(scope.token_file_name());
    match read_token_stamped(&path) {
        Ok(loaded) => Ok(loaded),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let secret = random_secret_hex()?;
            paths::write_private_file(&path, secret.as_bytes())?;
            Ok((TokenDigest::of(secret.as_bytes()), stamp_of(&path)))
        }
        Err(err) => Err(err),
    }
}

/// [`read_token`] with the stamp taken **before** the read, so a file replaced
/// while it is being read leaves a stale stamp and is reloaded next time.
fn read_token_stamped(path: &Path) -> io::Result<(TokenDigest, Option<FileStamp>)> {
    let stamp = stamp_of(path);
    Ok((read_token(path)?, stamp))
}

fn read_token(path: &Path) -> io::Result<TokenDigest> {
    let bytes = paths::read_private_file(path)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| invalid_token(path, "is not valid UTF-8"))?
        .trim()
        .to_string();
    if text.len() != SECRET_HEX_LEN || !text.bytes().all(|byte| hex_value(byte).is_some()) {
        return Err(invalid_token(
            path,
            "is not 64 hexadecimal characters; delete it to regenerate",
        ));
    }
    Ok(TokenDigest::of(text.as_bytes()))
}

fn invalid_token(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} {reason}", path.display()),
    )
}

/// One device paired through a `herdr gateway pair` URL.
///
/// `secret_sha256` is the digest of the cookie secret; the secret itself is
/// shown once, in the pairing exchange, and never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub scope: TokenScope,
    pub secret_sha256: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default)]
    pub last_seen_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceFile {
    #[serde(default)]
    devices: Vec<DeviceRecord>,
}

/// `devices.json`: the devices that exchanged a pairing code for a cookie.
///
/// Like [`TokenStore`], a store remembers what the file looked like when it
/// was read so [`DeviceStore::refresh`] can pick up a revocation performed by
/// another process — `herdr gateway rotate-token` writes this file too, and a
/// gateway that kept a stale copy would both keep honouring a revoked cookie
/// and write the revoked records back the next time it recorded activity.
#[derive(Debug)]
pub struct DeviceStore {
    path: PathBuf,
    devices: Vec<DeviceRecord>,
    stamp: Option<FileStamp>,
}

impl DeviceStore {
    /// Load `<dir>/devices.json`, treating a missing file as an empty store.
    ///
    /// A corrupt file is an error, not an empty store: silently forgetting
    /// every paired device would look like a bug to the user and would let a
    /// truncated write become a revocation.
    pub fn load(dir: &Path) -> io::Result<Self> {
        Self::load_file(dir.join(DEVICES_FILE))
    }

    fn load_file(path: PathBuf) -> io::Result<Self> {
        // Before the read, for the same reason as the token stamps.
        let stamp = stamp_of(&path);
        let devices = match paths::read_private_file(&path) {
            Ok(bytes) => {
                let file: DeviceFile = serde_json::from_slice(&bytes).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} is not valid device JSON: {err}", path.display()),
                    )
                })?;
                file.devices
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        Ok(Self {
            path,
            devices,
            stamp,
        })
    }

    /// Reload the file when it has been replaced since it was read.
    ///
    /// Called before a cookie is verified and before activity is recorded, so
    /// a `rotate-token` in another process revokes cookies here at once — and
    /// so this process never persists records that a revocation removed.
    pub fn refresh(&mut self) {
        let stamp = stamp_of(&self.path);
        if stamp == self.stamp {
            return;
        }
        match Self::load_file(self.path.clone()) {
            Ok(reloaded) => {
                tracing::info!(target: "gateway", "reloaded the paired devices after the file changed on disk");
                *self = reloaded;
            }
            Err(error) => {
                tracing::warn!(
                    target: "gateway",
                    error = %error,
                    "could not reload the paired devices; the records already in memory stay in force"
                );
                self.stamp = stamp;
            }
        }
    }

    pub fn devices(&self) -> &[DeviceRecord] {
        &self.devices
    }

    /// Verify a `herdr_gateway_device=<id>.<secret>` cookie value.
    ///
    /// The loop never breaks early and never returns from inside: a cookie
    /// naming a real device with the wrong secret does exactly the same work
    /// as one naming a device that does not exist.
    pub fn verify_cookie(&self, cookie: &str) -> Option<(TokenScope, String)> {
        let (id, secret) = cookie.split_once('.')?;
        if id.is_empty() || secret.is_empty() {
            return None;
        }
        let digest = TokenDigest::of(secret.as_bytes());
        let mut matched: Option<(TokenScope, String)> = None;
        for record in &self.devices {
            let secret_matches = record
                .digest()
                .map(|stored| digest.ct_eq_choice(&stored))
                .unwrap_or_else(|| Choice::from(0u8));
            let id_matches = record.id.as_bytes().ct_eq(id.as_bytes());
            if bool::from(secret_matches & id_matches) {
                matched = Some((record.scope, record.id.clone()));
            }
        }
        matched
    }

    /// Mint a device: returns the cookie value `<id>.<secret>`, which the
    /// caller hands to the browser once and cannot recover afterwards.
    pub fn insert(&mut self, scope: TokenScope, label: &str, now_unix: u64) -> io::Result<String> {
        let id = random_secret_hex()?;
        let secret = random_secret_hex()?;
        self.devices.push(DeviceRecord {
            id: id.clone(),
            scope,
            secret_sha256: TokenDigest::of(secret.as_bytes()).to_hex(),
            label: sanitize_label(label),
            created_unix: now_unix,
            last_seen_unix: now_unix,
        });
        self.persist()?;
        Ok(format!("{id}.{secret}"))
    }

    /// Forget every device of `scope`, returning how many were removed. Called
    /// by `rotate-token`: rotating a token must not leave cookies behind that
    /// still carry the scope it granted.
    pub fn revoke_scope(&mut self, scope: TokenScope) -> io::Result<usize> {
        let before = self.devices.len();
        self.devices.retain(|record| record.scope != scope);
        let removed = before - self.devices.len();
        if removed > 0 {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Forget one device by id, returning whether it existed.
    // Reached by this module's tests today; the operator-facing revoke-device
    // command that consumes it belongs to the fleet UI, not to E3.
    #[allow(dead_code)]
    pub fn revoke_id(&mut self, id: &str) -> io::Result<bool> {
        let before = self.devices.len();
        self.devices.retain(|record| record.id != id);
        let removed = before != self.devices.len();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Record that a device was seen. Persisted lazily by the caller — the
    /// gateway does not write a file per request.
    pub fn touch(&mut self, id: &str, now_unix: u64) -> bool {
        for record in &mut self.devices {
            if record.id == id {
                record.last_seen_unix = now_unix;
                return true;
            }
        }
        false
    }

    pub fn persist(&mut self) -> io::Result<()> {
        let file = DeviceFile {
            devices: self.devices.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        paths::write_private_file(&self.path, &json)?;
        // Our own write must not look like someone else's: stamp it now, or
        // the next refresh would reload the file we just wrote.
        self.stamp = stamp_of(&self.path);
        Ok(())
    }
}

impl DeviceRecord {
    fn digest(&self) -> Option<TokenDigest> {
        TokenDigest::from_hex(&self.secret_sha256)
    }
}

/// Keep a device label printable and bounded; it is user input that ends up in
/// a JSON file and in `herdr gateway status`.
fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .filter(|ch| !ch.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_string()
}

/// One outstanding pairing code, as stored in `pairings/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    pub id: String,
    pub secret_sha256: String,
    pub scope: TokenScope,
    pub expires_unix: u64,
    /// What the operator called the device they are about to pair. It travels
    /// with the code because the device record is created when the code is
    /// redeemed, on a machine the operator is not typing at.
    #[serde(default)]
    pub label: String,
}

/// Why a pairing code was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    /// No such code — including a real id presented with the wrong secret, so
    /// guessing an id teaches nothing.
    NotFound,
    /// The code existed and its window has passed. The file is gone.
    Expired,
    /// The presented text is not `<id>.<secret>` of the right shape. Nothing
    /// was looked up, and nothing was deleted.
    Invalid,
    /// The store could not be read or written.
    Io(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::NotFound => f.write_str("pairing code not found"),
            PairingError::Expired => f.write_str("pairing code expired"),
            PairingError::Invalid => f.write_str("malformed pairing code"),
            PairingError::Io(err) => write!(f, "pairing store error: {err}"),
        }
    }
}

/// `pairings/`: one `0600` file per outstanding `herdr gateway pair` code.
#[derive(Debug)]
pub struct PairingStore {
    dir: PathBuf,
}

impl PairingStore {
    pub fn new(gateway_dir: &Path) -> Self {
        Self {
            dir: paths::pairings_dir(gateway_dir),
        }
    }

    /// Where the codes live. The commands reach the store through this type
    /// rather than the directory, so this is the tests' window onto the layout.
    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Mint a code valid for `ttl_secs`. Returns the text to put in the
    /// pairing URL (`<id>.<secret>`) and the stored record.
    pub fn create(
        &self,
        scope: TokenScope,
        label: &str,
        ttl_secs: u64,
        now_unix: u64,
    ) -> io::Result<(String, PairingCode)> {
        paths::create_private_dir(&self.dir)?;
        let id = random_secret_hex()?;
        let secret = random_secret_hex()?;
        let code = PairingCode {
            id: id.clone(),
            secret_sha256: TokenDigest::of(secret.as_bytes()).to_hex(),
            scope,
            expires_unix: now_unix.saturating_add(ttl_secs),
            label: sanitize_label(label),
        };
        let json = serde_json::to_vec(&code).map_err(io::Error::other)?;
        paths::write_private_file(&self.code_path(&id), &json)?;
        Ok((format!("{id}.{secret}"), code))
    }

    /// Redeem a code exactly once.
    ///
    /// Success and expiry delete the file. A wrong secret does not: an
    /// attacker who can guess ids must not be able to delete a code the user
    /// is about to use, and must not learn that the id existed.
    ///
    /// The file lookup is not constant-time (a missing id fails at `open`, a
    /// present one after a read and a digest), which is acceptable only
    /// because ids are 256 random bits: there is nothing to enumerate.
    pub fn consume(&self, code_text: &str, now_unix: u64) -> Result<PairingCode, PairingError> {
        let (id, secret) = split_code(code_text).ok_or(PairingError::Invalid)?;
        let path = self.code_path(id);
        let bytes = match paths::read_private_file(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(PairingError::NotFound)
            }
            Err(err) => return Err(PairingError::Io(err.to_string())),
        };
        let stored = serde_json::from_slice::<PairingCode>(&bytes)
            .ok()
            .and_then(|code| code.digest().map(|digest| (code, digest)));
        let Some((code, stored)) = stored else {
            // A file we wrote that no longer parses (or carries a digest we
            // cannot read) is unusable; remove it so it cannot accumulate, and
            // say nothing about its contents.
            let _ = std::fs::remove_file(&path);
            return Err(PairingError::NotFound);
        };

        if !TokenDigest::of(secret.as_bytes()).ct_eq(&stored) {
            return Err(PairingError::NotFound);
        }
        if now_unix >= code.expires_unix {
            let _ = std::fs::remove_file(&path);
            return Err(PairingError::Expired);
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(code),
            // Someone redeemed it between our read and our delete: it is theirs.
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(PairingError::NotFound),
            // Refusing here is what makes the code one-time: if we cannot
            // delete it, we must not hand out a device for it.
            Err(err) => Err(PairingError::Io(err.to_string())),
        }
    }

    /// Delete every expired code, returning how many were removed.
    pub fn sweep_expired(&self, now_unix: u64) -> io::Result<usize> {
        // An unparseable leftover is swept too: it can never be redeemed.
        self.remove_matching(|code| code.is_none_or(|code| now_unix >= code.expires_unix))
    }

    /// Delete every outstanding code of one scope, returning how many were
    /// removed.
    ///
    /// `rotate-token` calls it: a pending pairing URL for the scope being
    /// rotated would otherwise still mint a device with the scope the operator
    /// just revoked, which is the one thing rotation exists to prevent.
    pub fn revoke_scope(&self, scope: TokenScope) -> io::Result<usize> {
        self.remove_matching(|code| code.is_some_and(|code| code.scope == scope))
    }

    /// How many codes are outstanding and still redeemable. Reads only; the
    /// caller sweeps first when it wants the expired ones gone.
    pub fn pending(&self, now_unix: u64) -> io::Result<usize> {
        let mut pending = 0;
        for (_, code) in self.entries()? {
            if code.is_some_and(|code| now_unix < code.expires_unix) {
                pending += 1;
            }
        }
        Ok(pending)
    }

    /// Every file in the store with the code it holds, or `None` when it does
    /// not parse as one. A directory that does not exist is an empty store.
    fn entries(&self) -> io::Result<Vec<(PathBuf, Option<PairingCode>)>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut codes = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(bytes) = paths::read_private_file(&path) else {
                continue;
            };
            codes.push((path, serde_json::from_slice::<PairingCode>(&bytes).ok()));
        }
        Ok(codes)
    }

    fn remove_matching(&self, matches: impl Fn(Option<&PairingCode>) -> bool) -> io::Result<usize> {
        let mut removed = 0;
        for (path, code) in self.entries()? {
            if matches(code.as_ref()) && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn code_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

impl PairingCode {
    fn digest(&self) -> Option<TokenDigest> {
        TokenDigest::from_hex(&self.secret_sha256)
    }
}

/// Split `<id>.<secret>`, refusing anything that is not two 64-character
/// hexadecimal halves.
///
/// The id becomes a file name, so this is also the path-traversal guard: `.`,
/// `..`, `/` and `\` cannot survive the hex check.
fn split_code(code_text: &str) -> Option<(&str, &str)> {
    let (id, secret) = code_text.split_once('.')?;
    if !is_secret_hex(id) || !is_secret_hex(secret) {
        return None;
    }
    Some((id, secret))
}

fn is_secret_hex(value: &str) -> bool {
    value.len() == SECRET_HEX_LEN && value.bytes().all(|byte| hex_value(byte).is_some())
}

/// Per-peer failed-authentication limiter.
///
/// Pure apart from the `Instant` the caller passes in, so the window behaviour
/// is tested without sleeping. A successful authentication deliberately does
/// **not** clear the counter: letting one correct credential reset the window
/// would turn a valid read token into an oracle for brute-forcing the control
/// token.
#[derive(Debug)]
pub struct AuthLimiter {
    limit: u32,
    window: Duration,
    peers: HashMap<IpAddr, Vec<Instant>>,
}

impl AuthLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit: limit.max(1),
            window,
            peers: HashMap::new(),
        }
    }

    /// The limiter `[gateway]` configures, with the documented defaults in
    /// place of any value the config diagnostics rejected.
    pub fn from_config(config: &crate::config::GatewayConfig) -> Self {
        Self::new(
            config.effective_auth_failure_limit(),
            config.effective_auth_failure_window(),
        )
    }

    /// `Ok(())` when `peer` may attempt again, `Err(retry_after)` when it is
    /// blocked for that long.
    pub fn check(&mut self, peer: IpAddr, now: Instant) -> Result<(), Duration> {
        let Some(failures) = self.peers.get_mut(&peer) else {
            return Ok(());
        };
        retain_recent(failures, now, self.window);
        if failures.is_empty() {
            self.peers.remove(&peer);
            return Ok(());
        }
        if (failures.len() as u32) < self.limit {
            return Ok(());
        }
        // Blocked until the oldest failure in the window falls out of it.
        let oldest = failures.first().copied().unwrap_or(now);
        let elapsed = now.saturating_duration_since(oldest);
        Err(self.window.saturating_sub(elapsed))
    }

    /// Record one failed authentication from `peer`.
    pub fn record_failure(&mut self, peer: IpAddr, now: Instant) {
        if !self.peers.contains_key(&peer) && self.peers.len() >= MAX_TRACKED_PEERS {
            self.evict_one(now);
        }
        let failures = self.peers.entry(peer).or_default();
        retain_recent(failures, now, self.window);
        // Bounded per peer as well: past the limit the extra stamps decide
        // nothing and would grow without end under a flood.
        if (failures.len() as u32) < self.limit {
            failures.push(now);
        }
    }

    /// Peers currently tracked. Exposed for this module's tests; a running
    /// gateway never reports it (`herdr gateway status` reads files, not the
    /// daemon's memory).
    #[cfg(test)]
    pub fn tracked_peers(&self) -> usize {
        self.peers.len()
    }

    /// Drop expired peers, or failing that one peer: an unblocked one before
    /// any blocked one, and among those the peer whose last failure is oldest
    /// (the one closest to expiring anyway).
    ///
    /// Preferring unblocked peers means a flood of fresh addresses cannot
    /// lift an existing block until it has earned `MAX_TRACKED_PEERS` blocks
    /// of its own; it is a cost, not a guarantee — an attacker with unlimited
    /// addresses never needs the evicted one back.
    fn evict_one(&mut self, now: Instant) {
        let window = self.window;
        self.peers.retain(|_, failures| {
            retain_recent(failures, now, window);
            !failures.is_empty()
        });
        if self.peers.len() < MAX_TRACKED_PEERS {
            return;
        }
        let limit = self.limit;
        let victim = self
            .peers
            .iter()
            .min_by_key(|(_, failures)| {
                let blocked = failures.len() as u32 >= limit;
                (blocked, failures.last().copied())
            })
            .map(|(peer, _)| *peer);
        if let Some(peer) = victim {
            self.peers.remove(&peer);
        }
    }
}

fn retain_recent(failures: &mut Vec<Instant>, now: Instant, window: Duration) {
    failures.retain(|stamp| now.saturating_duration_since(*stamp) < window);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-gateway-auth-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn token_text(dir: &Path, scope: TokenScope) -> String {
        String::from_utf8(fs::read(dir.join(scope.token_file_name())).expect("token"))
            .expect("utf8")
            .trim()
            .to_string()
    }

    #[test]
    fn control_allows_read_but_read_never_allows_control() {
        assert!(TokenScope::Control.allows(TokenScope::Read));
        assert!(TokenScope::Control.allows(TokenScope::Control));
        assert!(TokenScope::Read.allows(TokenScope::Read));
        assert!(!TokenScope::Read.allows(TokenScope::Control));
        assert!(!Principal::bearer(TokenScope::Read).allows(TokenScope::Control));
        assert!(Principal::device(TokenScope::Control, "id".into()).allows(TokenScope::Control));
    }

    #[test]
    fn secrets_are_sixty_four_lowercase_hex_characters_and_differ() {
        let first = random_secret_hex().expect("secret");
        let second = random_secret_hex().expect("secret");
        assert_eq!(first.len(), SECRET_HEX_LEN);
        assert!(first.bytes().all(|byte| hex_value(byte).is_some()));
        assert!(first.chars().all(|ch| !ch.is_ascii_uppercase()));
        assert_ne!(first, second, "the CSPRNG must not repeat");
    }

    #[test]
    fn digests_round_trip_through_hex_and_compare_in_constant_time() {
        let digest = TokenDigest::of(b"secret");
        let parsed = TokenDigest::from_hex(&digest.to_hex()).expect("hex round trip");
        assert!(digest.ct_eq(&parsed));
        assert!(!digest.ct_eq(&TokenDigest::of(b"secrer")));
        assert_eq!(TokenDigest::from_hex("nope"), None);
        assert_eq!(TokenDigest::from_hex(&"z".repeat(64)), None);
    }

    #[test]
    fn load_or_create_writes_both_tokens_privately() {
        let dir = temp_dir("tokens");
        let store = TokenStore::load_or_create(&dir).expect("create");
        for scope in TokenScope::all() {
            let path = dir.join(scope.token_file_name());
            assert!(path.exists(), "{} must exist", path.display());
            paths::verify_private_file(&path).expect("token is private");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::symlink_metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "mode {mode:o}");
            }
            assert_eq!(token_text(&dir, scope).len(), SECRET_HEX_LEN);
        }
        paths::verify_private_dir(&dir).expect("dir is private");

        // Reloading keeps the same tokens.
        let again = TokenStore::load(&dir).expect("load");
        for scope in TokenScope::all() {
            assert!(store.digest(scope).ct_eq(&again.digest(scope)));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_bearer_accepts_only_the_exact_token_for_its_scope() {
        let dir = temp_dir("bearer");
        let store = TokenStore::load_or_create(&dir).expect("create");
        let read = token_text(&dir, TokenScope::Read);
        let control = token_text(&dir, TokenScope::Control);

        assert_eq!(store.verify_bearer(&read), Some(TokenScope::Read));
        assert_eq!(store.verify_bearer(&control), Some(TokenScope::Control));
        // A read token never proves control: the scope comes from which file
        // matched, so no route can be fooled by presenting the weaker token.
        assert_ne!(store.verify_bearer(&read), Some(TokenScope::Control));

        let mut flipped = read.clone();
        flipped.pop();
        flipped.push(if read.ends_with('a') { 'b' } else { 'a' });
        assert_eq!(store.verify_bearer(&flipped), None);
        assert_eq!(store.verify_bearer(&read[..SECRET_HEX_LEN - 1]), None);
        assert_eq!(store.verify_bearer("not-hex"), None);
        assert_eq!(store.verify_bearer(""), None);
        assert_eq!(store.verify_bearer(&read.to_uppercase()), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_token_is_refused_by_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        TokenStore::load_or_create(&dir).expect("create");
        let path = dir.join(READ_TOKEN_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = TokenStore::load(&dir).expect_err("0644 token must be refused");
        assert!(
            err.to_string().contains("read.token"),
            "message must name the file: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_token_file_is_refused_instead_of_regenerated() {
        let dir = temp_dir("malformed");
        TokenStore::load_or_create(&dir).expect("create");
        let path = dir.join(CONTROL_TOKEN_FILE);
        paths::write_private_file(&path, b"short\n").expect("write");
        let err = TokenStore::load_or_create(&dir).expect_err("malformed token must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("control.token"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn identical_token_files_are_refused() {
        let dir = temp_dir("identical");
        TokenStore::load_or_create(&dir).expect("create");
        let read = token_text(&dir, TokenScope::Read);
        paths::write_private_file(&dir.join(CONTROL_TOKEN_FILE), read.as_bytes())
            .expect("copy the read token over the control token");
        let err = TokenStore::load(&dir).expect_err("identical tokens must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("read.token") && err.to_string().contains("control.token"),
            "{err}"
        );
        assert!(
            !err.to_string().contains(&read),
            "the error must not print the token"
        );
        TokenStore::load_or_create(&dir).expect_err("load_or_create refuses them too");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_changes_only_the_named_scope() {
        let dir = temp_dir("rotate");
        TokenStore::load_or_create(&dir).expect("create");
        let read_before = token_text(&dir, TokenScope::Read);
        let control_before = token_text(&dir, TokenScope::Control);

        TokenStore::rotate(&dir, TokenScope::Control).expect("rotate");
        assert_eq!(token_text(&dir, TokenScope::Read), read_before);
        assert_ne!(token_text(&dir, TokenScope::Control), control_before);

        let store = TokenStore::load(&dir).expect("reload");
        assert_eq!(store.verify_bearer(&control_before), None);
        assert_eq!(
            store.verify_bearer(&token_text(&dir, TokenScope::Control)),
            Some(TokenScope::Control)
        );
        paths::verify_private_file(&dir.join(CONTROL_TOKEN_FILE)).expect("still private");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn devices_verify_insert_and_revoke_by_scope() {
        let dir = temp_dir("devices");
        paths::create_private_dir(&dir).expect("dir");
        let mut store = DeviceStore::load(&dir).expect("empty store");
        assert!(store.devices().is_empty());

        let phone = store
            .insert(TokenScope::Control, "phone", 100)
            .expect("insert");
        let tablet = store
            .insert(TokenScope::Read, "tablet\u{7}", 101)
            .expect("insert");
        assert_eq!(
            store.verify_cookie(&phone).map(|(scope, _)| scope),
            Some(TokenScope::Control)
        );
        assert_eq!(
            store.verify_cookie(&tablet).map(|(scope, _)| scope),
            Some(TokenScope::Read)
        );
        assert_eq!(store.devices()[1].label, "tablet", "labels are sanitized");

        // A real id with the wrong secret is refused.
        let (id, _) = phone.split_once('.').expect("split");
        assert_eq!(
            store.verify_cookie(&format!("{id}.{}", "0".repeat(64))),
            None
        );
        assert_eq!(store.verify_cookie("no-separator"), None);
        assert_eq!(store.verify_cookie(&format!("{id}.")), None);

        // Reloading from disk sees the same devices.
        let reloaded = DeviceStore::load(&dir).expect("reload");
        assert_eq!(reloaded.devices().len(), 2);
        assert!(reloaded.verify_cookie(&phone).is_some());
        paths::verify_private_file(&dir.join(DEVICES_FILE)).expect("private");

        let mut store = reloaded;
        assert_eq!(store.revoke_scope(TokenScope::Control).expect("revoke"), 1);
        assert_eq!(store.verify_cookie(&phone), None);
        assert!(
            store.verify_cookie(&tablet).is_some(),
            "revoking one scope must keep the other's devices"
        );
        assert!(store.touch(&store.devices()[0].id.clone(), 200));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pairing_id_can_only_ever_be_sixty_four_hex_characters() {
        let secret = "a".repeat(SECRET_HEX_LEN);
        let hex_63 = "b".repeat(SECRET_HEX_LEN - 1);
        for id in [
            "..",
            ".",
            "",
            "/",
            "\\",
            "../../etc/passwd",
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "..\\..\\x",
            &format!("{hex_63}/"),
            &format!("/{hex_63}"),
            &format!("{hex_63}\\"),
            &format!("{hex_63}\u{0}"),
            &"\u{ff10}".repeat(SECRET_HEX_LEN), // fullwidth digits
            &"\u{0430}".repeat(SECRET_HEX_LEN), // cyrillic a
            &"g".repeat(SECRET_HEX_LEN),
            &"a".repeat(SECRET_HEX_LEN + 1),
        ] {
            assert_eq!(
                split_code(&format!("{id}.{secret}")),
                None,
                "{id:?} must not become a file name"
            );
        }
        // What survives is exactly [0-9a-fA-F]{64}, which cannot leave the
        // pairings directory.
        let store = PairingStore::new(Path::new("/tmp/herdr-gateway-test"));
        let text = format!("{secret}.{secret}");
        let (id, _) = split_code(&text).expect("hex id");
        let path = store.code_path(id);
        assert!(path.starts_with(store.dir()), "{}", path.display());
        assert_eq!(path.parent(), Some(store.dir()));
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == format!("{secret}.json")));
    }

    #[test]
    fn a_pairing_code_is_redeemable_exactly_once() {
        let dir = temp_dir("pairing");
        let store = PairingStore::new(&dir);
        let (code, record) = store
            .create(TokenScope::Control, "", 600, 1_000)
            .expect("create");
        assert_eq!(record.scope, TokenScope::Control);
        assert_eq!(record.expires_unix, 1_600);
        paths::verify_private_dir(store.dir()).expect("pairings dir is private");

        let consumed = store.consume(&code, 1_100).expect("consume");
        assert_eq!(consumed.id, record.id);
        assert_eq!(store.consume(&code, 1_100), Err(PairingError::NotFound));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_failures_are_distinguished_and_leave_the_store_consistent() {
        let dir = temp_dir("pairing-fail");
        let store = PairingStore::new(&dir);

        // Malformed input never touches the filesystem.
        assert_eq!(store.consume("nope", 0), Err(PairingError::Invalid));
        assert_eq!(
            store.consume(&format!("{}.{}", "../etc/passwd", "a".repeat(64)), 0),
            Err(PairingError::Invalid),
            "an id that is not hex can never become a path"
        );
        assert_eq!(
            store.consume(&format!("{}.{}", "a".repeat(64), "zz"), 0),
            Err(PairingError::Invalid)
        );

        // A wrong secret looks exactly like a missing code and keeps the file.
        let (code, record) = store
            .create(TokenScope::Read, "", 600, 1_000)
            .expect("create");
        let (id, _) = code.split_once('.').expect("split");
        assert_eq!(
            store.consume(&format!("{id}.{}", "0".repeat(64)), 1_100),
            Err(PairingError::NotFound)
        );
        assert!(
            store.dir().join(format!("{}.json", record.id)).exists(),
            "a wrong secret must not delete a valid code"
        );
        // Neither does a wrong secret on an already-expired code: expiry is
        // only revealed to someone holding the secret.
        assert_eq!(
            store.consume(&format!("{id}.{}", "0".repeat(64)), 5_000),
            Err(PairingError::NotFound)
        );
        assert!(store.dir().join(format!("{}.json", record.id)).exists());

        // Expiry reports itself and removes the file.
        assert_eq!(store.consume(&code, 1_600), Err(PairingError::Expired));
        assert!(!store.dir().join(format!("{}.json", record.id)).exists());

        // A file that no longer parses is removed and reads as not found.
        let (code, record) = store
            .create(TokenScope::Read, "", 600, 1_000)
            .expect("create");
        let path = store.dir().join(format!("{}.json", record.id));
        paths::write_private_file(&path, b"{ not json").expect("corrupt");
        assert_eq!(store.consume(&code, 1_100), Err(PairingError::NotFound));
        assert!(!path.exists(), "a corrupt code is swept on contact");
        let (code, record) = store
            .create(TokenScope::Read, "", 600, 1_000)
            .expect("create");
        let path = store.dir().join(format!("{}.json", record.id));
        let mut tampered = record.clone();
        tampered.secret_sha256 = "zz".to_string();
        paths::write_private_file(&path, &serde_json::to_vec(&tampered).expect("json"))
            .expect("tamper");
        assert_eq!(store.consume(&code, 1_100), Err(PairingError::NotFound));
        assert!(!path.exists());

        // Sweeping removes stale codes.
        let (_, stale) = store
            .create(TokenScope::Read, "", 30, 1_000)
            .expect("create");
        let (_, fresh) = store
            .create(TokenScope::Read, "", 600, 1_000)
            .expect("create");
        assert_eq!(store.sweep_expired(1_100).expect("sweep"), 1);
        assert!(!store.dir().join(format!("{}.json", stale.id)).exists());
        assert!(store.dir().join(format!("{}.json", fresh.id)).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_limiter_blocks_after_the_limit_and_recovers_after_the_window() {
        let peer: IpAddr = "203.0.113.7".parse().expect("ip");
        let other: IpAddr = "203.0.113.8".parse().expect("ip");
        let window = Duration::from_secs(60);
        let mut limiter = AuthLimiter::new(5, window);
        let start = Instant::now();

        for step in 0..4 {
            limiter.record_failure(peer, start + Duration::from_secs(step));
            assert_eq!(
                limiter.check(peer, start + Duration::from_secs(step)),
                Ok(())
            );
        }
        limiter.record_failure(peer, start + Duration::from_secs(4));
        let retry = limiter
            .check(peer, start + Duration::from_secs(5))
            .expect_err("blocked after five failures");
        assert_eq!(retry, Duration::from_secs(55));

        // Another peer is unaffected.
        assert_eq!(limiter.check(other, start + Duration::from_secs(5)), Ok(()));
        // The window is per stamp, so the peer is free once the oldest expires
        // and the count drops back below the limit.
        assert_eq!(limiter.check(peer, start + Duration::from_secs(61)), Ok(()));
    }

    #[test]
    fn a_successful_attempt_does_not_reset_the_window() {
        // The limiter has no "success" input on purpose; this test pins that
        // the only way out is time.
        let peer: IpAddr = "198.51.100.9".parse().expect("ip");
        let mut limiter = AuthLimiter::new(2, Duration::from_secs(60));
        let start = Instant::now();
        limiter.record_failure(peer, start);
        limiter.record_failure(peer, start);
        assert!(limiter.check(peer, start).is_err());
        assert!(limiter
            .check(peer, start + Duration::from_secs(30))
            .is_err());
        assert!(limiter.check(peer, start + Duration::from_secs(61)).is_ok());
    }

    #[test]
    fn the_limiter_takes_its_numbers_from_the_config_with_defaults_for_bad_values() {
        let config: crate::config::GatewayConfig =
            toml::from_str("auth_failure_limit = 2\nauth_failure_window_secs = 10\n")
                .expect("parses");
        let mut limiter = AuthLimiter::from_config(&config);
        let peer: IpAddr = "192.0.2.1".parse().expect("ip");
        let start = Instant::now();
        limiter.record_failure(peer, start);
        assert!(limiter.check(peer, start).is_ok());
        limiter.record_failure(peer, start);
        assert_eq!(limiter.check(peer, start), Err(Duration::from_secs(10)));
        assert!(limiter.check(peer, start + Duration::from_secs(10)).is_ok());

        let config: crate::config::GatewayConfig =
            toml::from_str("auth_failure_limit = 0\nauth_failure_window_secs = 0\n")
                .expect("parses");
        let mut limiter = AuthLimiter::from_config(&config);
        for _ in 0..5 {
            limiter.record_failure(peer, start);
        }
        assert_eq!(
            limiter.check(peer, start),
            Err(Duration::from_secs(60)),
            "zero values fall back to the documented defaults, never to 'no limit'"
        );
    }

    #[test]
    fn a_flood_of_new_peers_evicts_unblocked_peers_before_blocked_ones() {
        let mut limiter = AuthLimiter::new(2, Duration::from_secs(60));
        let start = Instant::now();
        let blocked: IpAddr = "192.0.2.5".parse().expect("ip");
        limiter.record_failure(blocked, start);
        limiter.record_failure(blocked, start);
        assert!(limiter.check(blocked, start).is_err());

        // Every later peer is newer than the blocked one, so a last-failure
        // policy alone would evict the block first.
        for index in 1..=(MAX_TRACKED_PEERS + 8) {
            let peer = IpAddr::from(std::net::Ipv6Addr::from(index as u128));
            limiter.record_failure(peer, start + Duration::from_millis(index as u64));
        }
        assert!(limiter.tracked_peers() <= MAX_TRACKED_PEERS);
        assert!(
            limiter
                .check(blocked, start + Duration::from_secs(1))
                .is_err(),
            "the blocked peer must survive a flood of one-failure peers"
        );
    }

    #[test]
    fn the_limiter_is_bounded_in_peers_and_in_stamps_per_peer() {
        let mut limiter = AuthLimiter::new(3, Duration::from_secs(60));
        let start = Instant::now();
        for index in 0..(MAX_TRACKED_PEERS + 64) {
            let peer = IpAddr::from(std::net::Ipv6Addr::from(index as u128));
            limiter.record_failure(peer, start + Duration::from_millis(index as u64));
        }
        assert!(
            limiter.tracked_peers() <= MAX_TRACKED_PEERS,
            "tracked {} peers",
            limiter.tracked_peers()
        );

        let peer: IpAddr = "192.0.2.5".parse().expect("ip");
        for _ in 0..100 {
            limiter.record_failure(peer, start);
        }
        assert!(limiter.check(peer, start).is_err());
        // Still blocked for exactly one window, not one hundred.
        assert!(limiter.check(peer, start + Duration::from_secs(61)).is_ok());
    }
}
