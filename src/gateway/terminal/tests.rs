//! Socket-level tests for the terminal session.
//!
//! Unix-only, like the rest of the fleet's socket tests: they bind a local
//! socket by path and half-close it. The fake endpoint speaks exactly the
//! frozen exchange a herdr server speaks — `TerminalHello` → `Welcome` →
//! `ObserveTerminal` → `Terminal` frames — so a change to either side of that
//! contract fails here rather than against a lab.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Listener as _;

use super::*;
use crate::ipc::{bind_local_listener, connect_local_stream};
use crate::protocol::{RenderEncoding, TerminalFrame};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

/// How the fake endpoint should answer the terminal hello.
#[derive(Clone, Copy)]
enum Answer {
    /// `Welcome { TerminalAnsi, error: None }`, then frames.
    Ansi,
    /// `Welcome { error: Some(..) }` — a server that refuses the session.
    Refused,
    /// A welcome for a different encoding, which this connection cannot use.
    WrongEncoding,
    /// `Welcome { TerminalAnsi }`, then one frame larger than `MAX_FRAME_SIZE`.
    OversizedFrame,
}

/// What the fake endpoint saw and sent.
#[derive(Default)]
struct Seen {
    hello: Option<(u16, u16)>,
    observe_target: Option<String>,
    after: Vec<ClientMessage>,
    /// The client's half-close (or close) reached the fake's read loop.
    closed: bool,
}

struct FakeEndpoint {
    socket: PathBuf,
    seen: Arc<Mutex<Seen>>,
    stop: Arc<AtomicBool>,
}

impl FakeEndpoint {
    fn start(name: &str, answer: Answer) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let socket = std::env::temp_dir().join(format!(
            "herdr-gw-term-{name}-{}-{nanos}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = bind_local_listener(&socket).expect("bind the fake endpoint");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_seen = Arc::clone(&seen);
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let Ok(mut stream) = listener.accept() else {
                return;
            };
            let Ok(ClientMessage::TerminalHello { cols, rows, .. }) =
                protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE)
            else {
                return;
            };
            lock(&thread_seen).hello = Some((cols, rows));

            let welcome = match answer {
                Answer::Ansi | Answer::OversizedFrame => ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    encoding: RenderEncoding::TerminalAnsi,
                    error: None,
                },
                Answer::Refused => ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    encoding: RenderEncoding::TerminalAnsi,
                    error: Some("terminal sessions are disabled on this host".to_string()),
                },
                Answer::WrongEncoding => ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    encoding: RenderEncoding::SemanticFrame,
                    error: None,
                },
            };
            if protocol::write_message(&mut stream, &welcome).is_err() {
                return;
            }
            if !matches!(answer, Answer::Ansi | Answer::OversizedFrame) {
                return;
            }

            let Ok(ClientMessage::ObserveTerminal { target }) =
                protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE)
            else {
                return;
            };
            lock(&thread_seen).observe_target = Some(target);

            if matches!(answer, Answer::OversizedFrame) {
                // The payload is the frame's bytes plus bincode's header, so
                // exactly `MAX_FRAME_SIZE` bytes of body is already over the
                // cap the gateway reads with. The write may block once the
                // socket buffer fills; it ends when the client closes.
                let frame = ServerMessage::Terminal(TerminalFrame {
                    seq: 1,
                    width: 80,
                    height: 24,
                    full: true,
                    bytes: vec![b'x'; MAX_FRAME_SIZE],
                });
                let _ = protocol::write_message(&mut stream, &frame);
                return;
            }

            for (seq, full) in [(1u64, true), (2, false)] {
                let frame = ServerMessage::Terminal(TerminalFrame {
                    seq,
                    width: 80,
                    height: 24,
                    full,
                    bytes: format!("frame-{seq}").into_bytes(),
                });
                if protocol::write_message(&mut stream, &frame).is_err() {
                    return;
                }
            }
            let shutdown = ServerMessage::ServerShutdown {
                reason: Some(
                    "terminal session observe failed: terminal target w9:p9 not found".to_string(),
                ),
            };
            if protocol::write_message(&mut stream, &shutdown).is_err() {
                return;
            }

            while !thread_stop.load(Ordering::Acquire) {
                match protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE) {
                    Ok(message) => lock(&thread_seen).after.push(message),
                    Err(_) => {
                        lock(&thread_seen).closed = true;
                        return;
                    }
                }
            }
        });

        Self { socket, seen, stop }
    }

    fn connect(&self) -> io::Result<LocalStream> {
        let stream = connect_local_stream(&self.socket)?;
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    fn observe(&self, pane: &str) -> io::Result<TerminalSession> {
        negotiate_observe(
            self.connect()?,
            READ_TIMEOUT,
            pane,
            80,
            24,
            StreamLease::detached(),
        )
    }

    fn hello(&self) -> Option<(u16, u16)> {
        lock(&self.seen).hello
    }

    fn observed_target(&self) -> Option<String> {
        lock(&self.seen).observe_target.clone()
    }

    fn after(&self) -> Vec<ClientMessage> {
        lock(&self.seen).after.clone()
    }

    fn closed(&self) -> bool {
        lock(&self.seen).closed
    }
}

impl Drop for FakeEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The whole happy path: the frozen hello, the target the client asked for,
/// both frames in order, then the host's reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observe_session_yields_every_frame_in_order_then_the_hosts_reason() {
    let endpoint = FakeEndpoint::start("frames", Answer::Ansi);
    let mut session = endpoint.observe("w1:p1").expect("observe opens");

    assert_eq!(endpoint.hello(), Some((80, 24)));
    assert!(wait_until(Duration::from_secs(5), || endpoint
        .observed_target()
        == Some("w1:p1".to_string())));

    for (seq, full, body) in [(1u64, true, "frame-1"), (2, false, "frame-2")] {
        match session.frames.recv().await {
            Some(TerminalEvent::Frame(frame)) => {
                assert_eq!(frame.seq, seq);
                assert!(frame.full == full, "frame {seq} full flag");
                assert_eq!(frame.bytes, body.as_bytes());
                // What the browser actually receives.
                let encoded = encode_frame(&frame);
                let header =
                    crate::gateway::protocol::decode_frame_header(&encoded).expect("a full header");
                assert_eq!(header.seq, seq);
                assert_eq!((header.width, header.height), (80, 24));
                assert_eq!(header.full, full);
            }
            other => panic!("expected frame {seq}, got {}", describe(&other)),
        }
    }

    match session.frames.recv().await {
        Some(TerminalEvent::Closed(reason)) => {
            let reason = reason.expect("the host said why");
            assert!(matches!(
                classify_shutdown_reason(Some(&reason)),
                TerminalClose::PaneNotFound(_)
            ));
        }
        other => panic!("expected a close, got {}", describe(&other)),
    }
    // The reader ends after the shutdown message.
    assert!(session.frames.recv().await.is_none());
}

fn describe(event: &Option<TerminalEvent>) -> String {
    match event {
        Some(TerminalEvent::Frame(frame)) => format!("frame {}", frame.seq),
        Some(TerminalEvent::Closed(reason)) => format!("closed {reason:?}"),
        Some(TerminalEvent::Failed(error)) => format!("failed {error}"),
        None => "end of stream".to_string(),
    }
}

/// A frame over the cap ends the session with a named failure — never a
/// silent close a client could mistake for the pane exiting, and never an
/// attempt to buffer it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_over_the_cap_fails_the_session_by_name() {
    let endpoint = FakeEndpoint::start("oversized", Answer::OversizedFrame);
    let mut session = endpoint.observe("w1:p1").expect("observe opens");
    match session.frames.recv().await {
        Some(TerminalEvent::Failed(error)) => {
            assert!(error.contains("terminal frame read ended"), "{error}");
        }
        other => panic!("expected a failure, got {}", describe(&other)),
    }
    assert!(session.frames.recv().await.is_none());
}

/// A release has to reach the host, or an observer lingers on the server until
/// the socket dies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_reaches_the_host() {
    let endpoint = FakeEndpoint::start("release", Answer::Ansi);
    let mut session = endpoint.observe("w1:p1").expect("observe opens");
    session
        .send(ClientMessage::Detach)
        .await
        .expect("the writer accepts a release");

    assert!(
        wait_until(Duration::from_secs(5), || endpoint
            .after()
            .iter()
            .any(|message| matches!(message, ClientMessage::Detach))),
        "the host never saw the release: {:?}",
        endpoint.after()
    );
}

/// A resize is client-local on the server, but it still has to be sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_reaches_the_host() {
    let endpoint = FakeEndpoint::start("resize", Answer::Ansi);
    let mut session = endpoint.observe("w1:p1").expect("observe opens");
    session
        .send(ClientMessage::Resize {
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        })
        .await
        .expect("the writer accepts a resize");

    assert!(
        wait_until(Duration::from_secs(5), || endpoint.after().iter().any(
            |message| matches!(
                message,
                ClientMessage::Resize {
                    cols: 100,
                    rows: 30,
                    ..
                }
            )
        )),
        "the host never saw the resize: {:?}",
        endpoint.after()
    );
}

/// The text path end to end, through the real gate: what an observe session
/// forwards, what it answers, and what never reaches the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observe_session_forwards_only_what_the_policy_admits() {
    let endpoint = FakeEndpoint::start("gate", Answer::Ansi);
    let mut session = endpoint.observe("w1:p1").expect("observe opens");
    let policy = SessionPolicy::new(TerminalMode::Observe, TokenScope::Read);

    let answer = |outcome: ClientOutcome| match outcome {
        ClientOutcome::Answer(text) => {
            serde_json::from_str::<serde_json::Value>(&text).expect("json answer")
        }
        other => panic!("expected an answer, got {other:?}"),
    };

    // Input is refused by the policy, whatever it says.
    for raw in [
        r#"{"type":"terminal.input","text":"echo INJECTED\n"}"#,
        r#"{"type":"terminal.input","bytes":"AQID"}"#,
        r#"{"type":"terminal.input"}"#,
    ] {
        let value = answer(handle_client_text(&policy, &mut session, raw).await);
        assert_eq!(value["type"], "terminal.error", "{raw}");
        assert_eq!(value["code"], "forbidden", "{raw}");
    }
    // Scroll is not something the host honors for an observer.
    let value = answer(
        handle_client_text(
            &policy,
            &mut session,
            r#"{"type":"terminal.scroll","direction":"up","lines":3}"#,
        )
        .await,
    );
    assert_eq!(value["code"], "unsupported");
    // A second open, garbage, and a viewport over the ceiling are bad requests.
    for raw in [
        r#"{"type":"terminal.open","mode":"control","cols":80,"rows":24}"#,
        "not json",
        r#"{"type":"terminal.resize","cols":65535,"rows":24}"#,
        r#"{"type":"terminal.resize","cols":80,"rows":0}"#,
    ] {
        let value = answer(handle_client_text(&policy, &mut session, raw).await);
        assert_eq!(value["code"], "bad_request", "{raw}");
    }
    // A resize inside the bound is forwarded silently.
    assert_eq!(
        handle_client_text(
            &policy,
            &mut session,
            r#"{"type":"terminal.resize","cols":100,"rows":30}"#
        )
        .await,
        ClientOutcome::Continue
    );
    // A release is forwarded and ends the session.
    assert_eq!(
        handle_client_text(&policy, &mut session, r#"{"type":"terminal.release"}"#).await,
        ClientOutcome::Release
    );

    assert!(wait_until(Duration::from_secs(5), || endpoint
        .after()
        .iter()
        .any(|message| matches!(message, ClientMessage::Detach))));
    let after = endpoint.after();
    assert!(
        !after
            .iter()
            .any(|message| matches!(message, ClientMessage::Input { .. })),
        "input reached the host: {after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|message| matches!(message, ClientMessage::AttachScroll { .. })),
        "scroll reached the host: {after:?}"
    );
    assert!(
        !after.iter().any(|message| matches!(
            message,
            ClientMessage::Resize { cols: 65535, .. } | ClientMessage::Resize { rows: 0, .. }
        )),
        "an out-of-bounds resize reached the host: {after:?}"
    );
    assert!(after.iter().any(|message| matches!(
        message,
        ClientMessage::Resize {
            cols: 100,
            rows: 30,
            ..
        }
    )));
}

/// Dropping a session closes the connection, so a browser that goes away does
/// not leave an observer attached to somebody's pane.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_session_ends_the_host_connection() {
    let endpoint = FakeEndpoint::start("drop", Answer::Ansi);
    let session = endpoint.observe("w1:p1").expect("observe opens");
    assert!(wait_until(Duration::from_secs(5), || endpoint
        .observed_target()
        .is_some()));
    drop(session);

    // The fake's read loop sees the client's half-close: that is what makes a
    // real host drop the observer rather than keep it attached until the
    // process exits.
    assert!(
        wait_until(Duration::from_secs(5), || endpoint.closed()),
        "the host never saw the session end"
    );
    assert!(!endpoint
        .after()
        .iter()
        .any(|message| matches!(message, ClientMessage::Input { .. })));
}

/// A host that refuses the session must produce a named error, not a stream
/// that silently produces nothing.
#[test]
fn a_refused_welcome_names_the_hosts_reason() {
    let endpoint = FakeEndpoint::start("refused", Answer::Refused);
    let Err(error) = endpoint.observe("w1:p1") else {
        panic!("the host refused, so no session should open");
    };
    assert!(
        error.to_string().contains("terminal sessions are disabled"),
        "unexpected error: {error}"
    );
}

/// A welcome for an encoding this connection cannot read is a failure, never a
/// session that forwards frames nobody can interpret.
#[test]
fn a_welcome_for_another_encoding_is_refused() {
    let endpoint = FakeEndpoint::start("encoding", Answer::WrongEncoding);
    let Err(error) = endpoint.observe("w1:p1") else {
        panic!("an unusable encoding must not open a session");
    };
    assert!(
        error.to_string().contains("rather than terminal ansi"),
        "unexpected error: {error}"
    );
}

/// The target pair a request names, decided before anything is upgraded.
#[test]
fn a_valid_target_is_a_host_and_a_server_side_id() {
    let host = parse_target("lab-2", "w1:p1").expect("a well-formed target");
    assert_eq!(host.as_str(), "lab-2");
    // An agent target and a raw terminal id are server-side ids too; the
    // gateway does not second-guess what a server can resolve.
    assert!(parse_target("lab-2", "t7").is_some());
    assert!(parse_target("lab-2", "claude").is_some());
}

#[test]
fn a_target_that_could_not_be_a_reference_is_refused() {
    for (host, pane) in [
        // Not in a host id's alphabet.
        ("bad host", "w1:p1"),
        ("lab/1", "w1:p1"),
        ("", "w1:p1"),
        // A pane id with a slash would parse back into a different host.
        ("lab-1", "w1/p1"),
        ("lab-1", ""),
        // A control character is not an id; it is a line in somebody's log.
        ("lab-1", "w1:p1\nforged log line"),
        ("lab-1", "w1:p1\u{7}"),
    ] {
        assert!(
            parse_target(host, pane).is_none(),
            "accepted {host:?}/{pane:?}"
        );
    }
}

/// Route-level facts, through the real router and a real socket.
///
/// The auth layer answers before any extractor runs, so an unauthenticated
/// terminal request is plain HTTP and needs no WebSocket client. Everything
/// after the upgrade is covered by the integration test against the lab.
mod route {
    use crate::gateway::server::tests::TestServer;

    /// `/api/*` is never public, WebSocket or not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_terminal_without_a_credential_is_unauthorized() {
        let server = TestServer::start("terminal-unauth").await;
        let response = server.get("/api/terminal/lab-1/w1:p1", &[]).await;
        assert_eq!(response.status, 401, "{}", response.body);
        assert_eq!(response.json()["error"].as_str(), Some("unauthorized"));
        assert!(
            response.header("www-authenticate").is_some(),
            "a 401 must say how to authenticate: {:?}",
            response.headers
        );
        server.shutdown().await;
    }

    /// A read token is enough to *reach* the terminal route; what it may do
    /// once the socket is open is the session policy's decision.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_token_is_not_refused_by_the_route() {
        let server = TestServer::start("terminal-read-scope").await;
        let response = server
            .get(
                "/api/terminal/lab-1/w1:p1",
                &[("Authorization", &format!("Bearer {}", server.read_token))],
            )
            .await;
        // Not an auth failure: the WebSocket extractor refuses the missing
        // upgrade, which means the request got past the scope gate.
        assert_ne!(response.status, 401, "{}", response.body);
        assert_ne!(response.status, 403, "{}", response.body);
        server.shutdown().await;
    }
}
