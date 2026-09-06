//! Shared driver for `scripts/fork/fleet-lab.sh`.
//!
//! The fleet lab is the fixture every fork epic validates against, so more than
//! one `tests/fork_*.rs` drives it. Keeping the driver here means those tests
//! agree on one isolated root, one binary and one teardown path.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Generous: the script boots real servers and CI runners are slow.
pub const STEP_TIMEOUT_MS: &str = "20000";

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn script_path() -> PathBuf {
    repo_root().join("scripts/fork/fleet-lab.sh")
}

/// A lab root that no other test (or a concurrent `just ci`) can collide with.
pub fn unique_root(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    // Kept short on purpose: session sockets live under this root and unix
    // socket paths are capped at ~108 bytes (104 on macOS, whose temp_dir()
    // is already ~50 bytes long), so prefer a plain /tmp like tests/cli does.
    let base = if Path::new("/tmp").is_dir() {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    base.join(format!(
        "herdr-lab-{label}-{}-{}",
        std::process::id(),
        nanos % 1_000_000
    ))
}

pub fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

pub struct Lab {
    pub root: PathBuf,
    registered: bool,
}

impl Lab {
    pub fn new(label: &str) -> Self {
        Self {
            root: unique_root(label),
            registered: false,
        }
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_with_bin(env!("CARGO_BIN_EXE_herdr"), args)
    }

    pub fn run_with_bin(&self, bin: &str, args: &[&str]) -> Output {
        let mut command = Command::new("bash");
        command
            .arg(script_path())
            .args(args)
            .env("HERDR_BIN", bin)
            .env("HERDR_FLEET_LAB_ROOT", &self.root)
            .env("HERDR_FLEET_LAB_TIMEOUT_MS", STEP_TIMEOUT_MS)
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION");
        command.output().expect("run fleet-lab.sh")
    }

    /// Bring the lab up, then hand its runtime dir to the shared test watchdog
    /// so a leaked server is reaped even if this test panics.
    ///
    /// Registration has to happen *after* `up`: it creates the runtime dir with
    /// an owner marker, and `up` refuses (and later deletes) a root that does
    /// not carry the lab's own marker file.
    pub fn up(&mut self, count: &str) -> Output {
        let output = self.run(&["up", count]);
        if output.status.success() {
            super::register_runtime_dir(&self.runtime_dir());
            self.registered = true;
        }
        output
    }

    pub fn herdr(&self, session: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr"))
            .arg("--session")
            .arg(session)
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .output()
            .expect("run herdr against the lab")
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = self.run(&["down"]);
        if self.registered {
            super::unregister_runtime_dir(&self.runtime_dir());
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
