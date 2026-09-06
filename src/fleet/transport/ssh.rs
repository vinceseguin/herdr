//! A herdr server on another machine, reached through the shared stdio bridge.
//!
//! `herdr --remote` already knows how to reach a host: discover the herdr
//! binary there, bind a private local socket, and run
//! `ssh -T <target> "exec <herdr> [--session <name>] remote-client-bridge"` per
//! accepted connection, piping both ways. E1 PR 3 widened exactly that much of
//! `src/remote/attach.rs` to `pub(crate)`; this transport is the fleet-side
//! adapter, so an ssh host is the same [`LocalStream`] every other host is.
//!
//! What it deliberately does **not** do (plan decision (f)): it never installs
//! or uploads a binary, never stops or hands off a remote server, never
//! prompts, and never reads `HERDR_REMOTE_BINARY`. A host with no
//! generation-1 herdr is one host reported unavailable, with the one-time
//! `herdr --remote <target>` install command in its reason.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;

use super::HostTransport;
use crate::fleet::handshake::REMOTE_HANDSHAKE_READ_TIMEOUT;
use crate::fleet::hosts::HostId;
use crate::ipc::{connect_local_stream, LocalStream};
use crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION;
use crate::remote::{
    discover_remote_herdr, local_forward_socket_path_scoped, BridgeErrorSink, RemoteHerdr,
    RemoteSsh, SshStdioBridge,
};

/// One ssh host's connection factory.
///
/// Everything it owns is reused across reconnects: the ssh session (so the
/// control master is not rebuilt), the discovery result (so a reconnect does
/// not pay three ssh round-trips) and the bridge listener (it is a listener;
/// only the per-connection ssh child dies). A bridge-level failure invalidates
/// all three, because it means the ssh path itself is in question.
pub struct SshTransport {
    host: HostId,
    target: String,
    session_name: String,
    manage_ssh_config: bool,
    /// Whether the per-connection `ssh` child runs in batch mode with its
    /// stderr discarded. Read on every bridge start, not only the first: a
    /// retired session rebuilds the bridge with the same policy.
    noninteractive: bool,
    /// Declared before `ssh` on purpose, and torn down first in [`Drop`]: the
    /// bridge's ssh children ride the control master that dropping `ssh`
    /// closes.
    bridge: Option<SshStdioBridge>,
    ssh: Option<RemoteSsh>,
    /// The remote herdr discovery found, cached until a failure retires it.
    herdr: Option<RemoteHerdr>,
    /// The forward socket, scoped by host id so two hosts pointing at the same
    /// target and session never share one socket file.
    local_socket: PathBuf,
    /// Set by the bridge's accept thread when an ssh child fails.
    bridge_error: Arc<Mutex<Option<String>>>,
}

impl SshTransport {
    /// A transport for one ssh host.
    ///
    /// `noninteractive` picks the ssh policy for every bridged connection:
    /// `false` is `herdr --remote`'s interactive path (the operator's own auth
    /// flow, ssh's errors on the inherited stderr), `true` is the daemon path
    /// (`BatchMode=yes`, no password prompts, stderr discarded). See
    /// [`crate::fleet::connector::FleetConnectorOptions::ssh_noninteractive`].
    pub fn new(
        host: HostId,
        target: String,
        session: Option<String>,
        manage_ssh_config: bool,
        noninteractive: bool,
    ) -> Self {
        let session_name =
            session.unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
        // Derived once: the path is per (pid, host, target, session) and the
        // bridge binds it, so it must not move under a reconnect.
        let local_socket = local_forward_socket_path_scoped(host.as_str(), &target, &session_name);
        Self {
            host,
            target,
            session_name,
            manage_ssh_config,
            noninteractive,
            bridge: None,
            ssh: None,
            herdr: None,
            local_socket,
            bridge_error: Arc::new(Mutex::new(None)),
        }
    }

    /// The forward socket this host's bridge listens on.
    #[cfg(all(test, unix))]
    pub fn local_socket(&self) -> &std::path::Path {
        &self.local_socket
    }

    /// Take whatever the bridge's accept thread last reported.
    fn take_bridge_error(&self) -> Option<String> {
        lock(&self.bridge_error).take()
    }

    /// Retire the ssh session, the discovery result and the bridge.
    ///
    /// The next `connect` rebuilds all three. Order matters: the bridge goes
    /// first so its children are gone before the control master is closed.
    fn retire_ssh_session(&mut self) {
        self.bridge = None;
        self.herdr = None;
        self.ssh = None;
    }

    /// The ssh session, built on first use.
    ///
    /// Building it may write herdr's managed ssh config, so it is deliberately
    /// not done in `new`: constructing a transport performs no I/O.
    fn ssh_session(&mut self) -> &RemoteSsh {
        self.ssh.get_or_insert_with(|| {
            RemoteSsh::new(
                self.target.clone(),
                self.manage_ssh_config,
                self.session_name.clone(),
            )
        })
    }

    /// The remote herdr, discovered once and cached.
    ///
    /// Nothing is cached after a failure, so the next attempt probes again.
    /// Only a transport-level failure retires the ssh session: a host that
    /// answered "no compatible herdr" has a working ssh path, and keeping its
    /// control master means every retry at the backoff ceiling costs probes,
    /// not a new master and a rewritten managed config.
    fn remote_herdr(&mut self) -> io::Result<RemoteHerdr> {
        if let Some(herdr) = &self.herdr {
            return Ok(herdr.clone());
        }
        let host = self.host.clone();
        let target = self.target.clone();
        let discovered = discover_remote_herdr(self.ssh_session());
        match discovered {
            Ok(Some(herdr)) => {
                tracing::debug!(host = %host, binary = %herdr.shell_path, "discovered a remote herdr");
                self.herdr = Some(herdr.clone());
                Ok(herdr)
            }
            Ok(None) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no herdr with endpoint generation {} on host; run `herdr --remote {target}` once to install it",
                    ENDPOINT_PROTOCOL_GENERATION
                ),
            )),
            Err(error) => {
                tracing::debug!(host = %host, error = %error, "fleet ssh discovery failed");
                self.retire_ssh_session();
                Err(error)
            }
        }
    }

    /// Start the bridge listener if it is not already up.
    fn ensure_bridge(&mut self, herdr: RemoteHerdr) -> io::Result<()> {
        if self.bridge.is_some() {
            return Ok(());
        }
        let errors = Arc::clone(&self.bridge_error);
        // Runs on the bridge's accept thread between connections, so it only
        // records; `connect` is what turns it into a host-local reason.
        let sink = BridgeErrorSink::Report(Arc::new(move |message: String| {
            *lock(&errors) = Some(message);
        }));
        let target = self.target.clone();
        let session_name = self.session_name.clone();
        let local_socket = self.local_socket.clone();
        let options = self.ssh_session().options().cloned();
        let bridge = SshStdioBridge::start_with(
            target,
            herdr,
            local_socket,
            session_name,
            options.as_ref(),
            // Interactive ssh (`false`) is what `herdr --remote` uses: the
            // managed ssh config, the control master and the user's own auth
            // flow, with ssh's errors on the inherited stderr. A daemon flips
            // this to batch mode and sends that stderr to /dev/null.
            self.noninteractive,
            sink,
        )?;
        self.bridge = Some(bridge);
        Ok(())
    }
}

impl HostTransport for SshTransport {
    fn connect(&mut self) -> io::Result<LocalStream> {
        // A bridge failure recorded since the last attempt is *the* reason
        // this host dropped: the local socket is still accepting, so nothing
        // later in this chain would ever mention the ssh child that died.
        // Report it, and retire the session so the next attempt re-probes the
        // host over ssh rather than trusting a cache the failure invalidated.
        if let Some(reason) = self.take_bridge_error() {
            tracing::debug!(host = %self.host, reason = %reason, "fleet ssh bridge reported a failure");
            self.retire_ssh_session();
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, reason));
        }

        let herdr = self.remote_herdr()?;
        self.ensure_bridge(herdr)?;

        let stream = match connect_local_stream(&self.local_socket) {
            Ok(stream) => stream,
            Err(error) => {
                // The listener is ours; if it cannot be reached, the bridge is
                // not usable and must be rebuilt.
                self.bridge = None;
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "could not reach the ssh bridge socket {}: {error}",
                        self.local_socket.display()
                    ),
                ));
            }
        };
        // The supervisor reads with blocking `read_message`.
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    fn read_timeout(&self) -> Duration {
        REMOTE_HANDSHAKE_READ_TIMEOUT
    }

    fn describe(&self) -> String {
        format!("ssh {} (session {})", self.target, self.session_name)
    }

    #[cfg(test)]
    fn ssh_noninteractive_for_test(&self) -> Option<bool> {
        Some(self.noninteractive)
    }
}

impl Drop for SshTransport {
    fn drop(&mut self) {
        // Explicit, so the order survives a field reordering: the bridge
        // unlinks its socket and joins its accept thread, then closing the ssh
        // session exits the control master.
        self.bridge = None;
        self.ssh = None;
    }
}

/// A mutex guard that survives a poisoned lock.
///
/// A panic on the bridge's accept thread must not take the host with it: the
/// slot holds one `Option<String>` whose invariants a panic cannot break.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A fake `ssh` on `PATH`, for tests in this crate.
///
/// Mirrors `remote::attach`'s shim: nextest runs one process per test, so a
/// test may replace `PATH` for its own `Command::new("ssh")` without touching
/// another test — the lock below is belt-and-braces for a plain `cargo test`.
#[cfg(all(test, unix))]
pub(crate) mod fake_ssh {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    /// Serializes the tests that replace `PATH`.
    pub(crate) fn ssh_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// What the shim should do when asked to run the bridge command.
    pub(crate) enum Bridge {
        /// Proxy stdio to this unix socket, the way real ssh proxies to the
        /// remote herdr's stdio.
        ProxyTo(PathBuf),
        /// Fail, the way ssh does when the host is gone.
        Fail(u8),
    }

    pub(crate) struct FakeSsh {
        dir: PathBuf,
        trace: PathBuf,
        prior_path: Option<std::ffi::OsString>,
    }

    impl FakeSsh {
        /// Install a shim answering herdr's discovery probes with
        /// `generation` (`None` = no herdr on the host at all) and handling
        /// the bridge command with `bridge`.
        pub(crate) fn install(name: &str, generation: Option<u32>, bridge: Bridge) -> Self {
            Self::write(name, Some((generation, bridge)))
        }

        /// Install a shim that fails every command, the way ssh does when the
        /// host is unreachable.
        pub(crate) fn install_unreachable(name: &str) -> Self {
            Self::write(name, None)
        }

        fn write(name: &str, answers: Option<(Option<u32>, Bridge)>) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "herdr-fleet-fake-ssh-{name}-{}-{nanos}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create fake ssh dir");
            let trace = dir.join("argv.log");

            let proxy = dir.join("proxy.py");
            fs::write(&proxy, PROXY_PY).expect("write bridge proxy");

            let Some((generation, bridge)) = answers else {
                let script = format!(
                    "#!/bin/sh\nprintf 'ARGV %s\\n' \"$*\" >> '{trace}'\n\
                     printf 'ssh: connect to host unreachable-host port 22: \
                     Connection refused\\n' >&2\nexit 255\n",
                    trace = trace.display()
                );
                return Self::finish(dir, trace, script);
            };

            let bridge_case = match &bridge {
                Bridge::ProxyTo(socket) => {
                    format!("exec python3 '{}' '{}'", proxy.display(), socket.display())
                }
                Bridge::Fail(code) => format!("exit {code}"),
            };
            let client_status = match generation {
                Some(generation) => format!(
                    "printf '%s\\n' '{{\"version\":\"0.0.0-test\",\"protocol\":22,\"endpoint_protocol_generation\":{generation}}}'; exit 0"
                ),
                None => "exit 1".to_string(),
            };
            let script = format!(
                r#"#!/bin/sh
trace='{trace}'
printf 'ARGV %s\n' "$*" >> "$trace"
last=''
for arg in "$@"; do last="$arg"; done
payload="$last"
if [ "$last" = '/bin/sh -s' ]; then
  payload=$(cat)
fi
printf 'PAYLOAD %s\n' "$payload" >> "$trace"
case "$payload" in
  *remote-client-bridge*) {bridge_case} ;;
  *'uname -s'*) printf 'Linux\nx86_64\n'; exit 0 ;;
  *'status client --json'*) {client_status} ;;
  *'command -v herdr'*) exit 1 ;;
  *) exit 0 ;;
esac
"#,
                trace = trace.display()
            );
            Self::finish(dir, trace, script)
        }

        fn finish(dir: PathBuf, trace: PathBuf, script: String) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let ssh = dir.join("ssh");
            fs::write(&ssh, script).expect("write fake ssh");
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).expect("chmod fake ssh");

            let prior_path = std::env::var_os("PATH");
            let joined = match &prior_path {
                Some(value) => {
                    let mut entries = vec![dir.clone()];
                    entries.extend(std::env::split_paths(value));
                    std::env::join_paths(entries).expect("join PATH")
                }
                None => dir.clone().into_os_string(),
            };
            std::env::set_var("PATH", joined);

            Self {
                dir,
                trace,
                prior_path,
            }
        }

        pub(crate) fn trace(&self) -> String {
            fs::read_to_string(&self.trace).unwrap_or_default()
        }
    }

    impl Drop for FakeSsh {
        fn drop(&mut self) {
            match self.prior_path.take() {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Pipes this process's stdio to a unix socket, like ssh pipes to the
    /// remote herdr's stdio.
    const PROXY_PY: &str = r#"import os, socket, sys, threading

path = sys.argv[1]
sock = socket.socket(socket.AF_UNIX)
sock.connect(path)


def upload():
    while True:
        data = os.read(0, 65536)
        if not data:
            break
        sock.sendall(data)
    try:
        sock.shutdown(socket.SHUT_WR)
    except OSError:
        pass


thread = threading.Thread(target=upload, daemon=True)
thread.start()
while True:
    data = sock.recv(65536)
    if not data:
        break
    os.write(1, data)
"#;
}

#[cfg(all(test, unix))]
mod tests {
    use super::fake_ssh::{ssh_env_lock, Bridge, FakeSsh};
    use super::*;

    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use interprocess::local_socket::traits::Listener as _;

    use crate::fleet::handshake::{endpoint_handshake, HandshakeOutcome, HandshakeParams};
    use crate::ipc::bind_local_listener;
    use crate::protocol::endpoint::{EndpointServerWelcome, ENDPOINT_WELCOME_KIND};
    use crate::protocol::{self, ClientMessage, ClientSurfaceSize, ServerMessage, MAX_FRAME_SIZE};
    use crate::remote::{remote_bridge_command, RemotePlatform};

    /// The `ssh` argv `herdr --remote` runs per accepted connection, as the
    /// shim traces it: `-T <target> <remote command>`, built by the very
    /// function the `--remote` bridge calls, so the two cannot drift apart.
    fn expected_bridge_argv(target: &str, session_name: &str) -> String {
        let herdr = RemoteHerdr::for_platform(RemotePlatform::local());
        format!(
            "ARGV -T {target} {}",
            remote_bridge_command(&herdr, session_name)
        )
    }

    /// The trace lines for bridge commands, exactly as run.
    fn bridge_argv_lines(trace: &str) -> Vec<&str> {
        trace
            .lines()
            .filter(|line| line.starts_with("ARGV ") && line.contains("remote-client-bridge"))
            .collect()
    }

    /// Wait for a condition the bridge's accept thread satisfies out of band:
    /// `connect` returns as soon as the kernel queues the connection on the
    /// listener's backlog, and the accept thread picks it up and spawns the
    /// ssh child on its next poll.
    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    fn host(id: &str) -> HostId {
        HostId::new(id).expect("valid host id")
    }

    fn ssh_transport(host_id: &str, target: &str, session: Option<&str>) -> SshTransport {
        ssh_transport_with(host_id, target, session, false)
    }

    fn ssh_transport_with(
        host_id: &str,
        target: &str,
        session: Option<&str>,
        noninteractive: bool,
    ) -> SshTransport {
        SshTransport::new(
            host(host_id),
            target.to_string(),
            session.map(str::to_string),
            // Never true in a test: `RemoteSsh::new(_, true)` reads the
            // running user's `~/.ssh/config` into a managed config and would
            // start a control master against the shim.
            false,
            noninteractive,
        )
    }

    /// A minimal endpoint server on a unix socket: welcome, then silence.
    struct FakeEndpoint {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        connections: Arc<Mutex<usize>>,
    }

    impl FakeEndpoint {
        fn start(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let socket = std::env::temp_dir().join(format!(
                "herdr-fleet-ssh-{name}-{}-{nanos}.sock",
                std::process::id()
            ));
            let listener = bind_local_listener(&socket).expect("bind fake endpoint");
            let stop = Arc::new(AtomicBool::new(false));
            let connections = Arc::new(Mutex::new(0usize));
            let thread_stop = Arc::clone(&stop);
            let thread_connections = Arc::clone(&connections);
            std::thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    let Ok(mut stream) = listener.accept() else {
                        return;
                    };
                    if thread_stop.load(Ordering::Acquire) {
                        return;
                    }
                    *lock(&thread_connections) += 1;
                    std::thread::spawn(move || {
                        let Ok(_hello) =
                            protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE)
                        else {
                            return;
                        };
                        let welcome =
                            EndpointServerWelcome::compatible(vec!["pane.write".to_string()]);
                        let welcome = ServerMessage::EndpointControl {
                            kind: ENDPOINT_WELCOME_KIND.to_string(),
                            data: serde_json::to_string(&welcome).expect("welcome encodes"),
                        };
                        if protocol::write_message(&mut stream, &welcome).is_err() {
                            return;
                        }
                        while protocol::read_message::<_, ClientMessage>(
                            &mut stream,
                            MAX_FRAME_SIZE,
                        )
                        .is_ok()
                        {}
                    });
                }
            });
            Self {
                socket,
                stop,
                connections,
            }
        }

        fn connections(&self) -> usize {
            *lock(&self.connections)
        }
    }

    impl Drop for FakeEndpoint {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = connect_local_stream(&self.socket);
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    #[test]
    fn a_connection_runs_the_remote_bridge_command_and_handshakes() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let endpoint = FakeEndpoint::start("bridge-ok");
        let shim = FakeSsh::install(
            "bridge-ok",
            Some(1),
            Bridge::ProxyTo(endpoint.socket.clone()),
        );

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        let mut stream = transport.connect().expect("connect through the bridge");
        let outcome = endpoint_handshake(
            &mut stream,
            &HandshakeParams::read_only(ClientSurfaceSize { cols: 80, rows: 24 }),
        )
        .expect("handshake completes");
        assert!(
            matches!(outcome, HandshakeOutcome::Connected(_)),
            "unexpected handshake outcome: {outcome:?}"
        );
        assert_eq!(endpoint.connections(), 1);

        let trace = shim.trace();
        // The exact argv `herdr --remote` runs — byte for byte, nothing before
        // `-T`, nothing after `remote-client-bridge` — and exactly once.
        assert_eq!(
            bridge_argv_lines(&trace),
            vec![
                "ARGV -T herdr-ssh-lab exec \"$HOME/.local/bin/herdr\" --session lab-1 remote-client-bridge"
            ],
            "trace: {trace}"
        );
        assert_eq!(
            bridge_argv_lines(&trace),
            vec![expected_bridge_argv("herdr-ssh-lab", "lab-1").as_str()],
            "the fleet bridge command drifted from `herdr --remote`'s; trace: {trace}"
        );
        for forbidden in [
            "status server --json",
            "mkdir -p",
            "chmod 755",
            "remote-client-bridge --",
        ] {
            assert!(
                !trace.contains(forbidden),
                "the fleet transport must never {forbidden}; trace: {trace}"
            );
        }
    }

    /// The daemon switch reaches the real `ssh` argv.
    ///
    /// A gateway cannot answer a password prompt and has no terminal for ssh
    /// to warn on, so `bridge_connection` must run the child with
    /// `BatchMode=yes` (and, out of the shim's reach, `stderr` on /dev/null).
    /// The shim traces argv, so this asserts the flag on the wire rather than
    /// on the field that was set.
    #[test]
    fn a_noninteractive_transport_runs_ssh_in_batch_mode() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let endpoint = FakeEndpoint::start("bridge-batch");
        let shim = FakeSsh::install(
            "bridge-batch",
            Some(1),
            Bridge::ProxyTo(endpoint.socket.clone()),
        );

        let mut transport = ssh_transport_with("lab-ssh", "herdr-ssh-lab", Some("lab-1"), true);
        let _stream = transport.connect().expect("connect through the bridge");
        assert!(
            wait_until(Duration::from_secs(10), || shim
                .trace()
                .contains("remote-client-bridge")),
            "the bridge command never ran; trace: {}",
            shim.trace()
        );

        let trace = shim.trace();
        let bridge = bridge_argv_lines(&trace);
        assert_eq!(bridge.len(), 1, "trace: {trace}");
        for option in [
            "-o BatchMode=yes",
            "-o NumberOfPasswordPrompts=0",
            "-o ConnectTimeout=10",
        ] {
            assert!(
                bridge[0].contains(option),
                "a daemon transport must run ssh with {option}; trace: {trace}"
            );
        }
        // The remote command itself is untouched: only the local ssh policy
        // changes, so a batch-mode host runs exactly what an interactive one
        // runs.
        assert!(
            bridge[0].ends_with(&expected_bridge_argv("herdr-ssh-lab", "lab-1")["ARGV ".len()..]),
            "trace: {trace}"
        );
    }

    /// The default is still `herdr --remote`'s interactive ssh, so a CLI keeps
    /// the operator's auth flow and ssh's own error output.
    #[test]
    fn an_interactive_transport_does_not_add_batch_mode() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let endpoint = FakeEndpoint::start("bridge-interactive");
        let shim = FakeSsh::install(
            "bridge-interactive",
            Some(1),
            Bridge::ProxyTo(endpoint.socket.clone()),
        );

        let mut transport = ssh_transport_with("lab-ssh", "herdr-ssh-lab", Some("lab-1"), false);
        let _stream = transport.connect().expect("connect through the bridge");
        assert!(
            wait_until(Duration::from_secs(10), || shim
                .trace()
                .contains("remote-client-bridge")),
            "the bridge command never ran; trace: {}",
            shim.trace()
        );

        let trace = shim.trace();
        assert!(
            !trace.contains("BatchMode"),
            "an interactive transport must not force batch mode; trace: {trace}"
        );
    }

    #[test]
    fn the_default_session_is_not_named_on_the_remote_command() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let endpoint = FakeEndpoint::start("bridge-default");
        let shim = FakeSsh::install(
            "bridge-default",
            Some(1),
            Bridge::ProxyTo(endpoint.socket.clone()),
        );

        let mut transport = ssh_transport("box", "workbox", None);
        let _stream = transport.connect().expect("connect through the bridge");
        assert!(
            wait_until(Duration::from_secs(10), || shim
                .trace()
                .contains("remote-client-bridge")),
            "the bridge command never ran; trace: {}",
            shim.trace()
        );

        let trace = shim.trace();
        assert_eq!(
            bridge_argv_lines(&trace),
            vec!["ARGV -T workbox exec \"$HOME/.local/bin/herdr\" remote-client-bridge"],
            "trace: {trace}"
        );
        assert_eq!(
            bridge_argv_lines(&trace),
            vec![expected_bridge_argv("workbox", crate::session::DEFAULT_SESSION_NAME).as_str()],
            "the fleet bridge command drifted from `herdr --remote`'s; trace: {trace}"
        );
        assert!(!trace.contains("--session"), "trace: {trace}");
    }

    #[test]
    fn a_host_without_a_generation_one_herdr_names_the_install_command() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let shim = FakeSsh::install("gen2", Some(2), Bridge::Fail(1));

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        let error = transport.connect().expect_err("generation 2 is not usable");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let message = error.to_string();
        assert!(
            message.contains("no herdr with endpoint generation 1 on host"),
            "unexpected reason: {message}"
        );
        assert!(
            message.contains("run `herdr --remote herdr-ssh-lab` once to install it"),
            "unexpected reason: {message}"
        );
        let trace = shim.trace();
        assert!(
            !trace.contains("remote-client-bridge"),
            "an incompatible host must not be bridged; trace: {trace}"
        );
        assert!(
            !trace.contains("status server --json") && !trace.contains("mkdir -p"),
            "the fleet transport must never install or inspect the server; trace: {trace}"
        );
        // The ssh path answered; only the discovery result is discarded.
        assert!(
            transport.ssh.is_some(),
            "a host without a compatible herdr must keep its ssh session"
        );
        assert!(transport.herdr.is_none() && transport.bridge.is_none());
        assert!(
            !transport.local_socket().exists(),
            "an incompatible host must not leave a forward socket behind"
        );
    }

    #[test]
    fn an_unreachable_host_reports_the_ssh_failure() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let shim = FakeSsh::install_unreachable("unreachable");

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        let error = transport.connect().expect_err("the host is unreachable");
        assert!(
            error.to_string().contains("Connection refused"),
            "the ssh failure must reach the reason: {error}"
        );
        assert!(
            !transport.local_socket().exists(),
            "a failed discovery must not leave a forward socket behind"
        );
        // The ssh path itself failed: the session (and its control master,
        // when managed) is retired so the next attempt starts clean.
        assert!(
            transport.ssh.is_none(),
            "an unreachable host must not keep a possibly stale ssh session"
        );
        let trace = shim.trace();
        assert!(
            !trace.contains("remote-client-bridge"),
            "an unreachable host must not be bridged; trace: {trace}"
        );
    }

    #[test]
    fn a_bridge_failure_becomes_the_next_connect_reason() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let _shim = FakeSsh::install("bridge-fails", Some(1), Bridge::Fail(7));

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        // The bridge is a listener: the connection is accepted, and the ssh
        // child that fails behind it shows up as an immediate EOF.
        let mut stream = transport.connect().expect("the bridge socket accepts");
        let mut buffer = [0_u8; 1];
        let read = stream.read(&mut buffer);
        assert!(
            matches!(&read, Ok(0)) || read.is_err(),
            "the failing ssh child must end the stream: {read:?}"
        );
        drop(stream);

        // The sink records on the accept thread, just after the child exits.
        let deadline = Instant::now() + Duration::from_secs(10);
        let error = loop {
            match transport.connect() {
                Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => break error,
                other => {
                    assert!(
                        Instant::now() < deadline,
                        "the bridge failure never reached a connect reason: {other:?}"
                    );
                    drop(other);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        let message = error.to_string();
        assert!(
            message.contains("remote bridge failed"),
            "unexpected reason: {message}"
        );
        assert!(
            message.contains("ssh bridge exited with"),
            "unexpected reason: {message}"
        );
    }

    #[test]
    fn discovery_is_cached_across_reconnects() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let endpoint = FakeEndpoint::start("cache");
        let shim = FakeSsh::install("cache", Some(1), Bridge::ProxyTo(endpoint.socket.clone()));

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        for _ in 0..3 {
            let stream = transport.connect().expect("connect through the bridge");
            drop(stream);
        }
        assert!(
            wait_until(Duration::from_secs(10), || endpoint.connections() == 3),
            "the bridge carried {} of 3 connections",
            endpoint.connections()
        );

        let probes = shim
            .trace()
            .lines()
            .filter(|line| line.contains("status client --json"))
            .count();
        assert_eq!(
            probes,
            1,
            "discovery must run once, not per reconnect; trace: {}",
            shim.trace()
        );
    }

    #[test]
    fn a_discovery_failure_is_retried_on_the_next_attempt() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let shim = FakeSsh::install("retry", None, Bridge::Fail(1));

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        transport.connect().expect_err("no herdr on the host");
        transport.connect().expect_err("no herdr on the host");

        let probes = shim
            .trace()
            .lines()
            .filter(|line| line.contains("uname -s"))
            .count();
        assert_eq!(
            probes,
            2,
            "a failed discovery must not be cached; trace: {}",
            shim.trace()
        );
        assert!(
            transport.ssh.is_some(),
            "retrying discovery must reuse the ssh session, not rebuild it"
        );
    }

    #[test]
    fn a_lost_forward_socket_is_rebuilt_on_the_next_attempt() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let endpoint = FakeEndpoint::start("rebuild");
        let shim = FakeSsh::install("rebuild", Some(1), Bridge::ProxyTo(endpoint.socket.clone()));

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        // Handshake before letting go: `connect` returns as soon as the kernel
        // queues the connection on the bridge's listener, and the accept thread
        // only picks it up on its next poll. A stream dropped right away can
        // still be sitting in that backlog when the bridge is torn down below,
        // in which case it never spawns an ssh child and never reaches the
        // host. The welcome proves this connection did.
        let mut stream = transport.connect().expect("connect through the bridge");
        let outcome = endpoint_handshake(
            &mut stream,
            &HandshakeParams::read_only(ClientSurfaceSize { cols: 80, rows: 24 }),
        )
        .expect("handshake completes through the first bridge");
        assert!(
            matches!(outcome, HandshakeOutcome::Connected(_)),
            "unexpected handshake outcome: {outcome:?}"
        );
        assert_eq!(endpoint.connections(), 1);
        drop(stream);
        let socket = transport.local_socket().to_path_buf();
        assert!(socket.exists(), "the bridge socket was never bound");

        // Something outside herdr (a tmp cleaner) removes the socket file: the
        // listener is still up, but nobody can reach it any more.
        std::fs::remove_file(&socket).expect("remove the forward socket");
        let error = transport
            .connect()
            .expect_err("a missing forward socket cannot be connected to");
        assert!(
            error
                .to_string()
                .contains("could not reach the ssh bridge socket"),
            "unexpected reason: {error}"
        );
        assert!(
            transport.bridge.is_none(),
            "an unreachable bridge must be dropped, not kept"
        );

        // The next attempt binds a fresh listener at the same path and the
        // cached discovery is reused: no new probe.
        let mut stream = transport.connect().expect("the bridge is rebuilt");
        assert!(socket.exists(), "the bridge socket was not rebound");
        let outcome = endpoint_handshake(
            &mut stream,
            &HandshakeParams::read_only(ClientSurfaceSize { cols: 80, rows: 24 }),
        )
        .expect("handshake completes through the rebuilt bridge");
        assert!(
            matches!(outcome, HandshakeOutcome::Connected(_)),
            "unexpected handshake outcome: {outcome:?}"
        );
        assert_eq!(
            endpoint.connections(),
            2,
            "the rebuilt bridge must carry a second connection to the host"
        );
        let probes = shim
            .trace()
            .lines()
            .filter(|line| line.contains("status client --json"))
            .count();
        assert_eq!(
            probes,
            1,
            "rebuilding the bridge must not rediscover; trace: {}",
            shim.trace()
        );
    }

    #[test]
    fn two_hosts_sharing_a_target_get_distinct_forward_sockets() {
        let first = ssh_transport("box-a", "herdr-ssh-lab", Some("lab-1"));
        let second = ssh_transport("box-b", "herdr-ssh-lab", Some("lab-1"));

        assert_ne!(first.local_socket(), second.local_socket());
        let name = first
            .local_socket()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(
            name.contains("box-a"),
            "the forward socket must carry the host id scope: {name}"
        );
    }

    #[test]
    fn dropping_the_transport_unlinks_the_forward_socket() {
        let _guard = ssh_env_lock().lock().expect("ssh env lock");
        let endpoint = FakeEndpoint::start("drop");
        let _shim = FakeSsh::install("drop", Some(1), Bridge::ProxyTo(endpoint.socket.clone()));

        let mut transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        let stream = transport.connect().expect("connect through the bridge");
        let socket = transport.local_socket().to_path_buf();
        assert!(socket.exists(), "the bridge socket was never bound");

        drop(stream);
        drop(transport);
        assert!(!socket.exists(), "the bridge socket was not unlinked");
    }

    #[test]
    fn the_description_and_timeout_are_the_remote_ones() {
        let transport = ssh_transport("lab-ssh", "herdr-ssh-lab", Some("lab-1"));
        assert_eq!(transport.describe(), "ssh herdr-ssh-lab (session lab-1)");
        assert_eq!(transport.read_timeout(), REMOTE_HANDSHAKE_READ_TIMEOUT);

        let default = ssh_transport("box", "workbox", None);
        assert_eq!(default.describe(), "ssh workbox (session default)");
    }
}
