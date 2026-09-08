//! Shared driver for `scripts/fork/accounts-lab.sh`.
//!
//! The accounts lab is the fixture every E9 PR validates against: one isolated
//! herdr server, two seeded Claude profile directories and a fake `claude`
//! first on `PATH`. Keeping the driver here means every `tests/fork_*.rs` that
//! needs an account agrees on one root, one binary and one teardown path.
//!
//! Nothing this module runs can reach `~/.claude`, `~/.claude.json` or
//! `~/.config/herdr`: the script pins `XDG_CONFIG_HOME` under its own root and
//! every profile directory lives there.

use std::path::PathBuf;
use std::process::{Command, Output};

/// Generous: the script boots a real server and CI runners are slow.
pub const STEP_TIMEOUT_MS: &str = "20000";

/// The session the lab runs under; mirrors `SESSION_NAME` in the script.
pub const SESSION: &str = "accounts-lab";

/// The profiles the lab seeds, in the order `account list` reports them.
pub const DEFAULT_PROFILE: &str = "perso";
pub const SECOND_PROFILE: &str = "work";

/// Where the lab points `CLAUDE_CONFIG_DIR` when no profile was applied, so a
/// launch that chose none still lands inside the lab root.
pub const AMBIENT_PROFILE: &str = "ambient";

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn script_path() -> PathBuf {
    repo_root().join("scripts/fork/accounts-lab.sh")
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
            root: super::fleet_lab::unique_root(&format!("acct-{label}")),
            registered: false,
        }
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    pub fn profile_dir(&self, name: &str) -> PathBuf {
        self.root.join("profiles").join(name)
    }

    pub fn claude_stub(&self) -> PathBuf {
        self.root.join("bin").join("claude")
    }

    /// The directory a lab command falls back to when it applies no profile.
    pub fn ambient_dir(&self) -> PathBuf {
        self.profile_dir(AMBIENT_PROFILE)
    }

    pub fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new("bash");
        command
            .arg(script_path())
            .args(args)
            .env("HERDR_BIN", env!("CARGO_BIN_EXE_herdr"))
            .env("HERDR_ACCOUNTS_LAB_ROOT", &self.root)
            .env("HERDR_ACCOUNTS_LAB_TIMEOUT_MS", STEP_TIMEOUT_MS)
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION");
        command.output().expect("run accounts-lab.sh")
    }

    /// Bring the lab up, then hand its runtime dir to the shared test watchdog
    /// so a leaked server is reaped even if this test panics.
    ///
    /// Registration has to happen *after* `up`: it creates the runtime dir with
    /// an owner marker, and `up` refuses (and later deletes) a root that does
    /// not carry the lab's own marker file.
    pub fn up(&mut self) -> Output {
        let output = self.run(&["up"]);
        if output.status.success() {
            super::register_runtime_dir(&self.runtime_dir());
            self.registered = true;
        }
        output
    }

    /// Run herdr against the lab, with the stub first on `PATH`.
    pub fn herdr(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_herdr"))
            .arg("--session")
            .arg(SESSION)
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("PATH", path)
            // Pinned inside the lab: a herdr subcommand that reads the ambient
            // Claude directory (`integration install claude`, later PRs) must
            // never reach the developer's own ~/.claude.
            .env("CLAUDE_CONFIG_DIR", self.ambient_dir())
            .env("HERDR_BIN", env!("CARGO_BIN_EXE_herdr"))
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .output()
            .expect("run herdr against the accounts lab")
    }

    /// Start a herdr command against the lab in the background.
    ///
    /// Same environment as [`Lab::herdr`], but the caller keeps the child: the
    /// account watcher is a long-running process, and the only honest way to
    /// test what it does while it runs — and what it leaves behind when it is
    /// interrupted — is to run one.
    pub fn herdr_spawn(&self, args: &[&str]) -> std::process::Child {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_herdr"))
            .arg("--session")
            .arg(SESSION)
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("PATH", path)
            .env("CLAUDE_CONFIG_DIR", self.ambient_dir())
            .env("HERDR_BIN", env!("CARGO_BIN_EXE_herdr"))
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn herdr against the accounts lab")
    }

    /// The pane the lab left at a shell prompt.
    pub fn pane_id(&self) -> String {
        let output = self.run(&["status", "--json"]);
        let status: serde_json::Value =
            serde_json::from_str(&stdout_of(&output)).expect("accounts-lab status --json");
        status["pane_id"]
            .as_str()
            .expect("the lab reports a pane")
            .to_string()
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
