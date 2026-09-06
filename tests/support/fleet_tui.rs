//! A Fleet console under a real PTY, for the fork's TUI integration tests.
//!
//! The Rust twin of `scripts/fork/tui-drive.py`: `portable-pty` gives the
//! client a terminal with a real window size (a client that sees a 0x0 grid
//! refuses to start), the output is stripped of ANSI escapes so an assertion
//! reads what a person would read, and `Drop` detaches the console and reaps
//! it so a failing test never leaks a herdr.
//!
//! Every later fork PR drives the console through this type. Add helpers here;
//! do not rename what is already used.

#![allow(dead_code)] // Each test binary uses a different subset of the helpers.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use super::fleet_lab::Lab;

/// `prefix + q`: herdr's detach, which ends a console.
pub const DETACH_KEYS: &[u8] = b"\x02q";

/// The config directory name this build reads inside `XDG_CONFIG_HOME`.
pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

/// Append a `[fleet]` section to the lab's throwaway config.
///
/// The lab writes its own `config.toml`; the console needs the fleet block on
/// top of it, and only ever inside the lab's own `XDG_CONFIG_HOME`.
pub fn append_lab_config(lab: &Lab, extra: &str) {
    let dir = lab.root.join("xdg").join(app_dir_name());
    fs::create_dir_all(&dir).expect("lab config dir");
    let path = dir.join("config.toml");
    let mut existing = fs::read_to_string(&path).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(extra);
    fs::write(&path, existing).expect("write the lab's config.toml");
}

/// Strip ANSI escapes so assertions read the screen, not the styling.
pub fn strip_ansi(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            match ch {
                '\r' => out.push('\n'),
                '\n' | '\t' => out.push(ch),
                c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                c => out.push(c),
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // String sequences, terminated by BEL or ST.
            Some(']' | 'P' | 'X' | '^' | '_') => {
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if next == '\u{7}' || (previous == '\u{1b}' && next == '\\') {
                        break;
                    }
                    previous = next;
                }
            }
            // Charset designators take one more byte.
            Some('(' | ')' | '*' | '+') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

/// A running Fleet console, its PTY, and everything it has printed.
pub struct FleetConsole {
    master: Option<Box<dyn MasterPty + Send>>,
    /// Taken once at spawn: `MasterPty::take_writer` panics on a second call.
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
    pid: Option<u32>,
    cols: u16,
    rows: u16,
    detached: bool,
}

impl FleetConsole {
    /// Spawn `herdr fleet` against `lab`, on a `cols` x `rows` terminal.
    pub fn spawn(lab: &Lab, cols: u16, rows: u16) -> Self {
        Self::spawn_with_args(lab, &["fleet"], cols, rows)
    }

    pub fn spawn_with_args(lab: &Lab, args: &[&str], cols: u16, rows: u16) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pty for the console");

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
        command.args(args);
        command.env("XDG_CONFIG_HOME", lab.root.join("xdg"));
        command.env("XDG_RUNTIME_DIR", lab.runtime_dir());
        command.env("HERDR_DISABLE_SOUND", "1");
        command.env("TERM", "xterm-256color");
        command.env("SHELL", "/bin/sh");
        command.env_remove("HERDR_SOCKET_PATH");
        command.env_remove("HERDR_CLIENT_SOCKET_PATH");
        command.env_remove("HERDR_ENV");
        command.env_remove("HERDR_SESSION");
        command.env_remove("HERDR_REMOTE_KEYBINDINGS");

        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn the console");
        let pid = child.process_id();
        super::register_spawned_herdr_pid(pid);
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .expect("clone the console's pty reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let drain = Arc::clone(&output);
        thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => drain
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .extend_from_slice(&buffer[..read]),
                }
            }
        });

        let writer = pair.master.take_writer().expect("console pty writer");

        Self {
            master: Some(pair.master),
            writer: Some(writer),
            child,
            output,
            pid,
            cols,
            rows,
            detached: false,
        }
    }

    /// Force a full repaint and forget everything drawn before it.
    ///
    /// A client draws frame *diffs*: a screen that changed one character only
    /// ever wrote that character, so accumulated output is a history, not a
    /// screen. Resizing the window and back makes the client redraw
    /// everything; clearing the buffer in between makes what follows readable
    /// as a screen — the Rust twin of `tui-drive.py --redraw`, and the only
    /// way to assert that something is *no longer* shown.
    pub fn redraw(&mut self) {
        let Some(master) = self.master.as_ref() else {
            return;
        };
        let size = |rows: u16| PtySize {
            rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let _ = master.resize(size(self.rows.saturating_sub(1).max(1)));
        thread::sleep(Duration::from_millis(250));
        self.output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let _ = master.resize(size(self.rows));
        thread::sleep(Duration::from_millis(250));
    }

    /// Everything the console has drawn, with the escapes removed.
    pub fn screen_text(&self) -> String {
        let bytes = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        strip_ansi(&bytes)
    }

    /// Wait until `needle` appears on the screen.
    pub fn wait_for_text(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.screen_text().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Type into the console.
    pub fn send(&mut self, bytes: &[u8]) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        writer.write_all(bytes).expect("write to the console");
        writer.flush().expect("flush the console's input");
    }

    /// `prefix + q`, then wait for the process to go.
    pub fn detach(&mut self, timeout: Duration) -> bool {
        if self.detached {
            return true;
        }
        self.detached = true;
        self.send(DETACH_KEYS);
        self.wait_for_exit(timeout)
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for FleetConsole {
    fn drop(&mut self) {
        if !self.detached {
            self.detached = true;
            self.send(DETACH_KEYS);
            self.wait_for_exit(Duration::from_secs(5));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        drop(self.writer.take());
        drop(self.master.take());
        super::unregister_spawned_herdr_pid(self.pid);
    }
}

/// Read a lab pane's recent output through the lab's own herdr binary.
pub fn pane_text(lab: &Lab, session: &str, pane: &str) -> String {
    let output = lab.herdr(session, &["pane", "read", pane, "--source", "recent"]);
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// The pane id the lab's `env` output exports for one session.
pub fn lab_pane_id(lab: &Lab, index: usize) -> String {
    let env = String::from_utf8_lossy(&lab.run(&["env"]).stdout).to_string();
    let key = format!("export HERDR_FLEET_LAB_PANE_{index}=");
    for line in env.lines() {
        if let Some(value) = line.strip_prefix(&key) {
            return value.trim().trim_matches('"').to_string();
        }
    }
    panic!("the lab did not export a pane id for lab-{index}: {env}");
}

/// A `[fleet]` block naming `count` lab sessions as local hosts.
pub fn lab_fleet_config(count: usize) -> String {
    let mut config = String::from("\n[fleet]\ninclude_local = false\n");
    for index in 1..=count {
        config.push_str(&format!(
            "\n[[fleet.hosts]]\nname = \"lab-{index}\"\nkind = \"local\"\nsession = \"lab-{index}\"\n"
        ));
    }
    config
}

/// Fail with the screen text attached: a TUI assertion is unreadable without it.
pub fn assert_screen(console: &FleetConsole, needle: &str, timeout: Duration, what: &str) {
    assert!(
        console.wait_for_text(needle, timeout),
        "{what}: {needle:?} never appeared.\n--- screen ---\n{}\n--- end ---",
        console.screen_text()
    );
}

/// Where the lab keeps a session's client socket, so a test can wait for it.
pub fn lab_client_socket(lab: &Lab, session: &str) -> std::path::PathBuf {
    lab.root
        .join("xdg")
        .join(app_dir_name())
        .join("sessions")
        .join(session)
        .join("herdr-client.sock")
}

/// `true` when `path` names an existing directory entry.
pub fn exists(path: &Path) -> bool {
    path.exists()
}
