//! Filesystem layout of `<config>/gateway/` and the private-file helpers the
//! token, device and pairing stores share.
//!
//! Everything the gateway persists is a secret or a secret's digest, so the
//! directory is `0700` and every file is `0600`, created with those bits
//! rather than relaxed afterwards, and verified before it is read. That is the
//! same contract herdr's sockets already have (`src/server/socket_paths.rs`
//! restricts them to `0600` so another local uid cannot reach agent
//! terminals); an HTTP token stored world-readable would undo it.
//!
//! On Windows the mode checks are no-ops — NTFS ACLs are not POSIX bits, and
//! `restrict_socket_permissions` makes the same choice — so the helpers keep
//! the same signatures and only log at `debug`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

/// Mode of `<config>/gateway/` and `<config>/gateway/pairings/`.
#[cfg(unix)]
pub const DIRECTORY_MODE: u32 = 0o700;
/// Mode of every file the gateway writes under that directory.
#[cfg(unix)]
pub const FILE_MODE: u32 = 0o600;

/// The `read` scope's bearer token.
pub const READ_TOKEN_FILE: &str = "read.token";
/// The `control` scope's bearer token.
pub const CONTROL_TOKEN_FILE: &str = "control.token";
/// Paired devices (ids, scopes and digests — never a secret).
pub const DEVICES_FILE: &str = "devices.json";
/// One file per outstanding `herdr gateway pair` code.
pub const PAIRINGS_DIR: &str = "pairings";
/// `{pid, listen, started_unix}` of the running gateway, removed on clean exit.
pub const RUNTIME_FILE: &str = "gateway.json";

/// `<config>/gateway/`, where `<config>` is herdr's own config directory, so
/// `XDG_CONFIG_HOME` moves the token store with everything else. `--config`
/// (`HERDR_CONFIG_PATH`) relocates only the config *file*, as it does for the
/// rest of herdr; the store stays in the config directory, and `herdr gateway
/// pair`/`status` (PR 8) must resolve it the same way.
pub fn gateway_dir() -> PathBuf {
    crate::config::config_dir().join("gateway")
}

/// `<config>/gateway/pairings/`.
pub fn pairings_dir(gateway_dir: &Path) -> PathBuf {
    gateway_dir.join(PAIRINGS_DIR)
}

/// Create `path` (and its parents) as a private directory, or verify that an
/// existing one is private.
///
/// A new directory is created `0700` in one step, never created open and
/// chmod'ed afterwards. An existing directory we own is tightened to `0700`
/// (a wider mode only exposes file names; the files inside are `0600`). An
/// existing directory owned by another uid, or anything that is not a
/// directory, is refused rather than fixed: a directory someone else can write
/// is not one we can make safe by chmod'ing it.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    match private_dir_builder().create(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    set_private_dir_mode(path)?;
    verify_private_dir(path)
}

/// A builder that creates the directory already `0700` (unix), so it is never
/// open for even a moment before being tightened.
#[cfg(unix)]
fn private_dir_builder() -> fs::DirBuilder {
    let mut builder = fs::DirBuilder::new();
    builder.mode(DIRECTORY_MODE);
    builder
}

#[cfg(not(unix))]
fn private_dir_builder() -> fs::DirBuilder {
    fs::DirBuilder::new()
}

#[cfg(unix)]
fn set_private_dir_mode(path: &Path) -> io::Result<()> {
    // Only tighten a directory we own; `verify_private_dir` refuses the rest.
    // `set_permissions` follows symlinks, so a symlink (which is never a
    // directory here) is left alone and refused by the verification instead.
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir()
        && metadata.uid() == effective_uid()
        && metadata.mode() & 0o777 != DIRECTORY_MODE
    {
        fs::set_permissions(path, fs::Permissions::from_mode(DIRECTORY_MODE))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir_mode(path: &Path) -> io::Result<()> {
    tracing::debug!(
        target: "gateway",
        path = %path.display(),
        "skipping directory mode restriction on this platform"
    );
    Ok(())
}

/// Refuse a gateway directory that is not a same-uid, owner-only directory.
#[cfg(unix)]
pub fn verify_private_dir(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(not_private(path, "is not a directory"));
    }
    if metadata.uid() != effective_uid() {
        return Err(not_private(path, "is owned by another user"));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(not_private(
            path,
            "is readable by group or other; run chmod 700 on it",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn verify_private_dir(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(not_private(path, "is not a directory"));
    }
    tracing::debug!(
        target: "gateway",
        path = %path.display(),
        "skipping directory permission check on this platform"
    );
    Ok(())
}

/// Refuse a gateway file that is not a same-uid, owner-only regular file.
///
/// Called before every read of a secret: a token that became `0644` after it
/// was written is not a token we can keep trusting.
// Reads of secrets go through `read_private_file`, which verifies the open
// descriptor, and every caller that reads one does. This standalone check
// stats a path instead, which is what a test needs to assert a file's mode
// without reading it.
#[cfg(test)]
pub fn verify_private_file(path: &Path) -> io::Result<()> {
    verify_private_metadata(path, &fs::symlink_metadata(path)?)
}

#[cfg(unix)]
fn verify_private_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_file() {
        return Err(not_private(path, "is not a regular file"));
    }
    if metadata.uid() != effective_uid() {
        return Err(not_private(path, "is owned by another user"));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(not_private(
            path,
            "is readable by group or other; run chmod 600 on it or delete it to regenerate",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.file_type().is_file() {
        return Err(not_private(path, "is not a regular file"));
    }
    tracing::debug!(
        target: "gateway",
        path = %path.display(),
        "skipping file permission check on this platform"
    );
    Ok(())
}

/// Per-process counter so two threads writing the same file never share a
/// temporary name (one would otherwise delete the other's in-progress file).
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` as a private file, atomically.
///
/// The temporary file is created `0600` (and `O_EXCL`, so a planted symlink
/// or leftover is never opened) in the destination directory — which
/// [`create_private_dir`] has just verified is ours and `0700` — so the rename
/// never crosses a filesystem and the secret is never briefly readable by
/// anyone else. It is synced, then renamed over the target.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    create_private_dir(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{file_name}.tmp.{}.{sequence}",
        std::process::id()
    ));
    // A leftover temp file from a crashed run with a recycled pid must not be
    // reused: `create_new` below refuses to open it, and it could be a symlink.
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    let write = (|| -> io::Result<()> {
        use std::io::Write;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(FILE_MODE);
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(err) = write {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

/// Read a private file, verifying privacy on the open descriptor.
///
/// The file is opened `O_NOFOLLOW` and the checks run against that
/// descriptor's own metadata, so nothing can be swapped for a symlink between
/// the check and the read.
pub fn read_private_file(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(path)?;
    verify_private_metadata(path, &file.metadata()?)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn not_private(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} {reason}", path.display()),
    )
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments and has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-gateway-paths-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn create_private_dir_is_idempotent_and_private() {
        let dir = temp_dir("dir");
        create_private_dir(&dir).expect("create");
        create_private_dir(&dir).expect("create again");
        verify_private_dir(&dir).expect("private");
        #[cfg(unix)]
        {
            let mode = fs::symlink_metadata(&dir).expect("metadata").mode() & 0o777;
            assert_eq!(mode, DIRECTORY_MODE, "mode {mode:o}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_private_file_creates_owner_only_files() {
        let dir = temp_dir("write");
        let path = dir.join("secret");
        write_private_file(&path, b"value\n").expect("write");
        assert_eq!(read_private_file(&path).expect("read"), b"value\n");
        #[cfg(unix)]
        {
            let mode = fs::symlink_metadata(&path).expect("metadata").mode() & 0o777;
            assert_eq!(mode, FILE_MODE, "mode {mode:o}");
        }
        // Rewriting replaces the contents and leaves no temp file behind.
        write_private_file(&path, b"next\n").expect("rewrite");
        assert_eq!(read_private_file(&path).expect("read"), b"next\n");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_file_is_refused_by_name() {
        let dir = temp_dir("mode");
        let path = dir.join("read.token");
        write_private_file(&path, b"value\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = read_private_file(&path).expect_err("0644 must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("read.token"),
            "message must name the file: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_directory_is_refused() {
        let dir = temp_dir("dirmode");
        create_private_dir(&dir).expect("create");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        let err = verify_private_dir(&dir).expect_err("0755 must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_refused_instead_of_followed() {
        let dir = temp_dir("symlink");
        create_private_dir(&dir).expect("create");
        let target = dir.join("target");
        write_private_file(&target, b"value\n").expect("write");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let err = verify_private_file(&link).expect_err("symlink must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_file_never_follows_a_symlink() {
        let dir = temp_dir("readlink");
        create_private_dir(&dir).expect("create");
        let target = dir.join("target");
        write_private_file(&target, b"value\n").expect("write");
        let link = dir.join("read.token");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let err = read_private_file(&link).expect_err("a symlink must not be read through");
        assert_ne!(
            err.kind(),
            io::ErrorKind::NotFound,
            "a symlink is refused, not treated as absent (which would regenerate over it): {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_wider_directory_we_own_is_tightened_on_create() {
        let dir = temp_dir("tighten");
        fs::create_dir_all(&dir).expect("create");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        create_private_dir(&dir).expect("an owned 0755 directory is tightened");
        let mode = fs::symlink_metadata(&dir).expect("metadata").mode() & 0o777;
        assert_eq!(mode, DIRECTORY_MODE, "mode {mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_where_the_directory_should_be_is_refused() {
        let dir = temp_dir("notdir");
        fs::create_dir_all(&dir).expect("create");
        let path = dir.join("gateway");
        fs::write(&path, b"").expect("file");
        let err = create_private_dir(&path).expect_err("a file is not a private directory");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writes_to_one_file_all_succeed() {
        let dir = temp_dir("concurrent");
        let path = dir.join("devices.json");
        create_private_dir(&dir).expect("create");
        let threads: Vec<_> = (0..8u8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || write_private_file(&path, &[index; 16]))
            })
            .collect();
        for thread in threads {
            thread.join().expect("join").expect("every writer succeeds");
        }
        let bytes = read_private_file(&path).expect("read");
        assert_eq!(bytes.len(), 16);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]), "torn write");
        let leftovers = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0, "temp files left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gateway_paths_hang_off_the_config_directory() {
        let dir = PathBuf::from("/tmp/herdr-test-config");
        assert_eq!(pairings_dir(&dir), dir.join("pairings"));
    }
}
