//! A real `herdr gateway` process, in front of a real fleet lab or on its own.
//!
//! Every later E3 integration test (and E4's browser smoke) drives the gateway
//! through this fixture, so it owns the fiddly parts once: writing a
//! `[fleet]`/`[gateway]` config into a throwaway `XDG_CONFIG_HOME`, learning
//! the ephemeral port from the process's own `listening on …` line, and
//! stopping the process with `SIGTERM` so the clean-shutdown path is what the
//! tests exercise.
//!
//! Add helpers here; never rename one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::fleet_lab::{stdout_of, Lab};

/// How long to wait for the process to print its listen address.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for it to exit after `SIGTERM`.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// The config directory name the binary under test uses. The test binary and
/// `herdr` are built with the same profile, so this cannot disagree.
pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

/// Where the gateway a test starts keeps its config and its tokens.
#[derive(Debug, Clone)]
pub struct GatewayEnv {
    /// Value of `XDG_CONFIG_HOME`.
    pub config_home: PathBuf,
    /// Value of `XDG_RUNTIME_DIR`, when the target servers need one.
    pub runtime_dir: Option<PathBuf>,
}

impl GatewayEnv {
    pub fn for_lab(lab: &Lab) -> Self {
        Self {
            config_home: lab.root.join("xdg"),
            runtime_dir: Some(lab.runtime_dir()),
        }
    }

    /// `<config_home>/<app>` — herdr's own config directory.
    pub fn config_dir(&self) -> PathBuf {
        self.config_home.join(app_dir_name())
    }

    /// `<config_home>/<app>/gateway` — the token store.
    pub fn gateway_dir(&self) -> PathBuf {
        self.config_dir().join("gateway")
    }

    /// A `herdr` invocation with this environment and none of the caller's.
    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_CONFIG_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION");
        if let Some(runtime_dir) = &self.runtime_dir {
            command.env("XDG_RUNTIME_DIR", runtime_dir);
        }
        command
    }

    /// Append `toml` to every spelling of the config file that exists, or
    /// create the one this build reads.
    pub fn append_config(&self, toml: &str) {
        let mut written = false;
        for app in ["herdr", "herdr-dev"] {
            let dir = self.config_home.join(app);
            if !dir.is_dir() {
                continue;
            }
            append_config_file(&dir.join("config.toml"), toml);
            written = true;
        }
        if !written {
            let dir = self.config_dir();
            std::fs::create_dir_all(&dir).expect("create the config dir");
            append_config_file(&dir.join("config.toml"), toml);
        }
    }

    /// Run `herdr gateway <args>` to completion — for the refusal paths, which
    /// never reach a listener.
    pub fn run_gateway(&self, args: &[&str]) -> Output {
        self.command()
            .arg("gateway")
            .args(args)
            .output()
            .expect("run `herdr gateway`")
    }
}

fn append_config_file(path: &Path, toml: &str) {
    let mut existing = std::fs::read_to_string(path).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push('\n');
    existing.push_str(toml);
    existing.push('\n');
    std::fs::write(path, existing).expect("write the config file");
}

/// `[fleet]` naming `sessions` as local hosts, and never the caller's own
/// default session.
pub fn fleet_config(sessions: &[String]) -> String {
    let mut block = String::from("[fleet]\ninclude_local = false\n");
    for session in sessions {
        block.push_str(&format!(
            "\n[[fleet.hosts]]\nname = \"{session}\"\nkind = \"local\"\nsession = \"{session}\"\n"
        ));
    }
    block
}

/// One HTTP response, parsed far enough to assert on.
pub struct HttpResponse {
    pub status: u16,
    /// Lowercased header names with their values, in the order received.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|err| panic!("body is not JSON ({err}): {}", self.body))
    }
}

/// A running `herdr gateway`.
pub struct Gateway {
    pub addr: SocketAddr,
    pub env: GatewayEnv,
    child: Option<Child>,
    read_token: String,
    control_token: String,
}

impl Gateway {
    /// Point a gateway at the lab's sessions and start it on an ephemeral
    /// loopback port. `extra_config` is appended verbatim (a `[gateway]`
    /// block, usually).
    pub fn spawn(lab: &Lab, extra_config: &str) -> Self {
        let sessions = lab_sessions(lab);
        assert!(!sessions.is_empty(), "the lab has no sessions");
        let env = GatewayEnv::for_lab(lab);
        let mut config = fleet_config(&sessions);
        if !extra_config.is_empty() {
            config.push('\n');
            config.push_str(extra_config);
        }
        env.append_config(&config);
        Self::spawn_in(env)
    }

    /// Start a gateway on an ephemeral loopback port in an already-prepared
    /// environment.
    pub fn spawn_in(env: GatewayEnv) -> Self {
        let mut child = env
            .command()
            .args(["gateway", "--bind", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `herdr gateway`");
        super::register_spawned_herdr_pid(Some(child.id()));

        let addr = match read_listen_address(&mut child) {
            Ok(addr) => addr,
            Err(error) => {
                let _ = child.kill();
                let stderr = child
                    .wait_with_output()
                    .ok()
                    .map(|output| String::from_utf8_lossy(&output.stderr).to_string())
                    .unwrap_or_default();
                panic!("gateway did not start ({error}); stderr: {stderr}");
            }
        };

        let gateway_dir = env.gateway_dir();
        let read_token = read_token_file(&gateway_dir, "read.token");
        let control_token = read_token_file(&gateway_dir, "control.token");

        Self {
            addr,
            env,
            child: Some(child),
            read_token,
            control_token,
        }
    }

    pub fn gateway_dir(&self) -> PathBuf {
        self.env.gateway_dir()
    }

    /// The `read`-scope bearer token. Never print this in test output.
    pub fn read_token(&self) -> &str {
        &self.read_token
    }

    /// The `control`-scope bearer token. Never print this in test output.
    pub fn control_token(&self) -> &str {
        &self.control_token
    }

    pub fn authorization(&self) -> String {
        format!("Bearer {}", self.read_token)
    }

    pub fn control_authorization(&self) -> String {
        format!("Bearer {}", self.control_token)
    }

    /// A plain HTTP/1.1 `GET`, with `Connection: close` so the whole response
    /// is one read.
    pub fn http_get(&self, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.addr
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");

        let mut stream = TcpStream::connect(self.addr).expect("connect to the gateway");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("read timeout");
        stream
            .write_all(request.as_bytes())
            .expect("write the request");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read the response");
        parse_response(&raw)
    }

    /// Run `scripts/fork/ws-client.py` against this gateway with the `read`
    /// bearer token, to completion.
    ///
    /// The python client is the fork's only WebSocket implementation (no
    /// dependency for something the tests and the epic validation drive by
    /// hand), and it prints one line per message: `text <payload>`,
    /// `binary <hex|length>` or `close <code>`.
    pub fn ws(&self, path: &str, args: &[&str]) -> Output {
        self.ws_with(path, &[("Authorization", &self.authorization())], args)
    }

    /// [`Self::ws`] with exactly the headers given — no token is added, so a
    /// test can drive the unauthenticated and foreign-origin paths, and a
    /// control-scope test can present its own bearer.
    pub fn ws_with(&self, path: &str, headers: &[(&str, &str)], args: &[&str]) -> Output {
        self.ws_spawn(path, headers, args)
            .wait_with_output()
            .expect("run ws-client.py")
    }

    /// [`Self::ws_with`], left running — for a test that has to change the
    /// world (stop a host, type into a pane) while the socket is open.
    pub fn ws_spawn(&self, path: &str, headers: &[(&str, &str)], args: &[&str]) -> Child {
        let mut command = Command::new("python3");
        command
            .arg(ws_client_script())
            .arg(format!("ws://{}{path}", self.addr));
        for (name, value) in headers {
            command.arg("--header").arg(format!("{name}: {value}"));
        }
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ws-client.py")
    }

    /// Ask the gateway to stop, and return its exit code.
    pub fn stop(&mut self) -> Option<i32> {
        let mut child = self.child.take()?;
        let pid = child.id();
        send_sigterm(pid);
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    super::unregister_spawned_herdr_pid(Some(pid));
                    return status.code();
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    super::unregister_spawned_herdr_pid(Some(pid));
                    panic!("the gateway did not exit within {STOP_TIMEOUT:?} of SIGTERM");
                }
                Err(error) => panic!("waiting for the gateway failed: {error}"),
            }
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            send_sigterm(pid);
            let _ = child.wait();
            super::unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

/// The repository's WebSocket stand-in.
pub fn ws_client_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("fork")
        .join("ws-client.py")
}

/// The stdout lines of a `ws-client.py` run, one per message.
pub fn ws_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// The JSON of a `text <payload>` line, or a panic naming what was there.
pub fn ws_text_json(line: &str) -> serde_json::Value {
    let payload = line
        .strip_prefix("text ")
        .unwrap_or_else(|| panic!("not a text message: {line}"));
    serde_json::from_str(payload).unwrap_or_else(|err| panic!("not JSON ({err}): {payload}"))
}

fn send_sigterm(pid: u32) {
    // SAFETY: `kill` on a pid this process spawned and has not yet reaped, so
    // the number cannot have been recycled.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

/// The lab's session names, from the script's own machine-readable status.
pub fn lab_sessions(lab: &Lab) -> Vec<String> {
    let output = lab.run(&["status", "--json"]);
    assert!(output.status.success(), "fleet-lab.sh status --json failed");
    let status: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("status --json is one JSON object");
    status["sessions"]
        .as_array()
        .map(|sessions| {
            sessions
                .iter()
                .filter_map(|session| session["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn read_listen_address(child: &mut Child) -> Result<SocketAddr, String> {
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if sender.send(line).is_err() {
                return;
            }
        }
    });

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for the listen address".to_string());
        }
        match receiver.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(rest) = line.trim().strip_prefix("listening on http://") {
                    return rest
                        .parse()
                        .map_err(|_| format!("unparseable listen line: {line}"));
                }
            }
            Err(_) => return Err("the gateway stopped before it listened".to_string()),
        }
    }
}

fn read_token_file(gateway_dir: &Path, name: &str) -> String {
    let path = gateway_dir.join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
        .trim()
        .to_string()
}

fn parse_response(raw: &[u8]) -> HttpResponse {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    HttpResponse {
        status,
        headers,
        body: body.to_string(),
    }
}
