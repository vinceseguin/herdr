//! Socket-level tests for the terminal session.
//!
//! Unix-only, like the rest of the fleet's socket tests: they bind a local
//! socket by path and half-close it. The fake endpoint speaks exactly the
//! frozen exchange a herdr server speaks — `TerminalHello` → `Welcome` →
//! `ObserveTerminal`/`ControlTerminal` → `Terminal` frames — so a change to
//! either side of that contract fails here rather than against a lab.

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
    /// `Welcome { TerminalAnsi }`, then the server's refusal of a second
    /// controller — verbatim from `headless.rs`.
    AttachTaken,
    /// `Welcome { TerminalAnsi }`, then the shutdown an evicted controller
    /// gets when somebody else attaches with `takeover: true`.
    AttachTakenOver,
    /// `Welcome { TerminalAnsi }`, then nothing: a host that accepted the
    /// connection and has not drawn anything yet.
    Silent,
}

/// What the fake endpoint saw and sent.
#[derive(Default)]
struct Seen {
    hello: Option<(u16, u16)>,
    hello_cells: Option<(u32, u32)>,
    /// The `ObserveTerminal`/`ControlTerminal` that fixed the mode.
    attach: Option<ClientMessage>,
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
            let Ok(ClientMessage::TerminalHello {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                ..
            }) = protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE)
            else {
                return;
            };
            {
                let mut seen = lock(&thread_seen);
                seen.hello = Some((cols, rows));
                seen.hello_cells = Some((cell_width_px, cell_height_px));
            }

            let welcome = match answer {
                Answer::Ansi
                | Answer::OversizedFrame
                | Answer::AttachTaken
                | Answer::AttachTakenOver
                | Answer::Silent => ServerMessage::Welcome {
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
            if matches!(answer, Answer::Refused | Answer::WrongEncoding) {
                return;
            }

            let attach =
                match protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE) {
                    Ok(
                        message @ (ClientMessage::ObserveTerminal { .. }
                        | ClientMessage::ControlTerminal { .. }),
                    ) => message,
                    _ => return,
                };
            lock(&thread_seen).attach = Some(attach);

            if let Some(reason) = match answer {
                Answer::AttachTaken => Some(
                    "terminal attach failed: terminal t1 already has an attached client; retry with --takeover"
                        .to_string(),
                ),
                Answer::AttachTakenOver => Some("terminal attach taken over".to_string()),
                _ => None,
            } {
                let _ = protocol::write_message(
                    &mut stream,
                    &ServerMessage::ServerShutdown {
                        reason: Some(reason),
                    },
                );
                return;
            }

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

            if !matches!(answer, Answer::Silent) {
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
                        "terminal session observe failed: terminal target w9:p9 not found"
                            .to_string(),
                    ),
                };
                if protocol::write_message(&mut stream, &shutdown).is_err() {
                    return;
                }
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
        self.open(SessionRequest {
            mode: TerminalMode::Observe,
            pane: pane.to_string(),
            cols: 80,
            rows: 24,
            takeover: false,
        })
    }

    fn control(&self, pane: &str, takeover: bool) -> io::Result<TerminalSession> {
        self.open(SessionRequest {
            mode: TerminalMode::Control,
            pane: pane.to_string(),
            cols: 80,
            rows: 24,
            takeover,
        })
    }

    fn open(&self, request: SessionRequest) -> io::Result<TerminalSession> {
        negotiate(
            self.connect()?,
            READ_TIMEOUT,
            &request,
            StreamLease::detached(),
        )
    }

    fn hello(&self) -> Option<(u16, u16)> {
        lock(&self.seen).hello
    }

    /// The cell geometry the terminal hello declared: always zero, which is
    /// what a resize must not undo.
    fn hello_cells(&self) -> Option<(u32, u32)> {
        lock(&self.seen).hello_cells
    }

    fn attach(&self) -> Option<ClientMessage> {
        lock(&self.seen).attach.clone()
    }

    fn observed_target(&self) -> Option<String> {
        match lock(&self.seen).attach.clone() {
            Some(ClientMessage::ObserveTerminal { target })
            | Some(ClientMessage::ControlTerminal { target, .. }) => Some(target),
            _ => None,
        }
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

/// A stop latch that is never set. The sender is returned so the caller keeps
/// it alive: a dropped sender is itself a stop signal.
fn running() -> (watch::Sender<bool>, watch::Receiver<bool>) {
    watch::channel(false)
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
    let policy = SessionPolicy::new(TerminalMode::Observe, TokenScope::Read)
        .expect("a read credential may observe");

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

/// The attach message is the whole difference between watching and driving,
/// and `takeover` is the whole difference between asking and taking. Both go
/// on the wire exactly as the client asked.
#[test]
fn a_control_session_claims_the_attach_slot_with_the_flag_it_was_given() {
    for takeover in [false, true] {
        let endpoint = FakeEndpoint::start(&format!("control-{takeover}"), Answer::Ansi);
        let _session = endpoint.control("w1:p1", takeover).expect("control opens");
        assert!(wait_until(Duration::from_secs(5), || endpoint
            .attach()
            .is_some()));
        match endpoint.attach() {
            Some(ClientMessage::ControlTerminal {
                target,
                takeover: sent,
            }) => {
                assert_eq!(target, "w1:p1");
                assert_eq!(sent, takeover);
            }
            other => panic!("expected a ControlTerminal, got {other:?}"),
        }
    }
}

/// An observer asks for the observe slot, never the attach slot — a mode
/// mix-up here would take somebody's pane away just by watching it.
#[test]
fn an_observe_session_never_claims_the_attach_slot() {
    let endpoint = FakeEndpoint::start("observe-attach", Answer::Ansi);
    let _session = endpoint.observe("w1:p1").expect("observe opens");
    assert!(wait_until(Duration::from_secs(5), || endpoint
        .attach()
        .is_some()));
    assert!(
        matches!(
            endpoint.attach(),
            Some(ClientMessage::ObserveTerminal { .. })
        ),
        "an observer claimed the attach slot: {:?}",
        endpoint.attach()
    );
}

/// The control path end to end, through the real gate: what reaches the host,
/// byte for byte, and what still does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_control_session_forwards_input_byte_exact() {
    let endpoint = FakeEndpoint::start("control-input", Answer::Ansi);
    let mut session = endpoint.control("w1:p1", false).expect("control opens");
    let policy = SessionPolicy::new(TerminalMode::Control, TokenScope::Control)
        .expect("a control credential may control");

    // Text is UTF-8 bytes verbatim; `bytes` is base64 of the same, and the two
    // spellings must produce the same PTY input.
    for raw in [
        r#"{"type":"terminal.input","text":"echo hi\n"}"#,
        r#"{"type":"terminal.input","bytes":"ZWNobyBoaQo="}"#,
    ] {
        assert_eq!(
            handle_client_text(&policy, &mut session, raw).await,
            ClientOutcome::Continue,
            "{raw}"
        );
    }
    // A resize from a controller is the real PTY size, so it still goes.
    assert_eq!(
        handle_client_text(
            &policy,
            &mut session,
            r#"{"type":"terminal.resize","cols":60,"rows":20}"#
        )
        .await,
        ClientOutcome::Continue
    );
    // The ceiling is the controller's too: a host must not be asked to render
    // a viewport no terminal has.
    match handle_client_text(
        &policy,
        &mut session,
        r#"{"type":"terminal.resize","cols":65535,"rows":24}"#,
    )
    .await
    {
        ClientOutcome::Answer(text) => {
            let value: serde_json::Value = serde_json::from_str(&text).expect("json answer");
            assert_eq!(value["code"], "bad_request", "{text}");
        }
        other => panic!("an oversized resize was not refused: {other:?}"),
    }
    assert_eq!(
        handle_client_text(&policy, &mut session, r#"{"type":"terminal.release"}"#).await,
        ClientOutcome::Release
    );

    assert!(
        wait_until(Duration::from_secs(5), || endpoint
            .after()
            .iter()
            .any(|message| matches!(message, ClientMessage::Detach))),
        "the host never saw the release: {:?}",
        endpoint.after()
    );
    let after = endpoint.after();
    let inputs: Vec<Vec<u8>> = after
        .iter()
        .filter_map(|message| match message {
            ClientMessage::Input { data } => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        inputs,
        vec![b"echo hi\n".to_vec(), b"echo hi\n".to_vec()],
        "input did not reach the host byte-exact: {after:?}"
    );
    assert!(
        after.iter().any(|message| matches!(
            message,
            ClientMessage::Resize {
                cols: 60,
                rows: 20,
                ..
            }
        )),
        "the controller's resize never reached the host: {after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|message| matches!(message, ClientMessage::Resize { cols: 65535, .. })),
        "an out-of-bounds resize reached the host: {after:?}"
    );
}

/// A control-capable credential watching read-only still cannot type: mode
/// wins, so a device with the control token cannot type into a pane it opened
/// to watch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_control_credential_in_observe_mode_still_cannot_type() {
    let endpoint = FakeEndpoint::start("observe-control-token", Answer::Ansi);
    let mut session = endpoint.observe("w1:p1").expect("observe opens");
    let policy = SessionPolicy::new(TerminalMode::Observe, TokenScope::Control)
        .expect("a control credential may observe");

    match handle_client_text(
        &policy,
        &mut session,
        r#"{"type":"terminal.input","text":"echo INJECTED\n"}"#,
    )
    .await
    {
        ClientOutcome::Answer(text) => {
            let value: serde_json::Value = serde_json::from_str(&text).expect("json answer");
            assert_eq!(value["code"], "forbidden", "{text}");
        }
        other => panic!("an observer typed: {other:?}"),
    }
    assert!(
        !endpoint
            .after()
            .iter()
            .any(|message| matches!(message, ClientMessage::Input { .. })),
        "input reached the host: {:?}",
        endpoint.after()
    );
}

/// A pane somebody else is driving is `busy`, not "the host is down" and not a
/// silent close: the client is told the one thing it can act on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_attach_slot_answers_busy_before_ready() {
    let endpoint = FakeEndpoint::start("busy", Answer::AttachTaken);
    let mut session = endpoint.control("w1:p1", false).expect("control opens");
    let (_stop, mut stopping) = running();
    match await_attach(&mut session, &mut stopping).await {
        Attached::Refused { text, close } => {
            let value: serde_json::Value = serde_json::from_str(&text).expect("json answer");
            assert_eq!(value["type"], "terminal.error", "{text}");
            assert_eq!(value["code"], "busy", "{text}");
            assert_eq!(close.code, close_code::AGAIN);
        }
        Attached::Ready(_) => panic!("a taken pane must not report ready"),
    }
}

/// The evicted controller's side of a takeover. Not an error — it asked for
/// nothing wrong — but a named reason, so a UI can say who lost the pane.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_evicted_controller_is_told_it_was_taken_over() {
    let endpoint = FakeEndpoint::start("taken-over", Answer::AttachTakenOver);
    let mut session = endpoint.control("w1:p1", false).expect("control opens");
    let (_stop, mut stopping) = running();
    match await_attach(&mut session, &mut stopping).await {
        Attached::Refused { text, close } => {
            let value: serde_json::Value = serde_json::from_str(&text).expect("json answer");
            assert_eq!(value["type"], "terminal.closed", "{text}");
            assert_eq!(value["reason"], "taken_over", "{text}");
            assert_eq!(close.code, close_code::NORMAL);
        }
        Attached::Ready(_) => panic!("an evicted controller must not report ready"),
    }
}

/// The host's first frame is handed to the caller, not swallowed: it is what
/// `run_terminal` sends right after `terminal.ready`, so losing it here would
/// leave a browser with a blank terminal until the next repaint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hosts_first_frame_survives_the_wait_for_the_attach() {
    let endpoint = FakeEndpoint::start("first-frame", Answer::Ansi);
    let mut session = endpoint.observe("w1:p1").expect("observe opens");
    let (_stop, mut stopping) = running();
    match await_attach(&mut session, &mut stopping).await {
        Attached::Ready(Some(frame)) => {
            assert_eq!(frame.seq, 1);
            assert_eq!(frame.bytes, b"frame-1");
        }
        Attached::Ready(None) => panic!("the first frame was dropped"),
        Attached::Refused { text, .. } => panic!("an accepted session was refused: {text}"),
    }
    // And exactly once: the next event is the *second* frame, not a replay.
    match session.frames.recv().await {
        Some(TerminalEvent::Frame(frame)) => assert_eq!(frame.seq, 2),
        other => panic!("expected frame 2, got {}", describe(&other)),
    }
}

/// A gateway that starts stopping while a host has not answered yet must not
/// sit on its [`StreamLease`] for the whole attach timeout: `shutdown` waits
/// three seconds for the last lease, and this wait is ten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopping_gateway_does_not_wait_out_the_attach_timeout() {
    let endpoint = FakeEndpoint::start("stopping", Answer::Silent);
    let mut session = endpoint.control("w1:p1", false).expect("control opens");
    let (stop, mut stopping) = running();

    let started = Instant::now();
    let waiting = await_attach(&mut session, &mut stopping);
    tokio::pin!(waiting);
    // Nothing has happened yet: a silent host is not a refusal.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut waiting)
            .await
            .is_err(),
        "a silent host ended the session"
    );

    stop.send_replace(true);
    match waiting.await {
        Attached::Refused { text, close } => {
            let value: serde_json::Value = serde_json::from_str(&text).expect("json answer");
            assert_eq!(value["type"], "terminal.closed", "{text}");
            assert_eq!(close.code, close_code::AWAY);
        }
        Attached::Ready(_) => panic!("a stopping gateway reported ready"),
    }
    assert!(
        started.elapsed() < ATTACH_TIMEOUT,
        "the stop latch was ignored: waited {:?}",
        started.elapsed()
    );
}

/// The one failure the drain exists for: a keystroke queued and a release in
/// the same breath. Dropping the session half-closes the write side under the
/// writer thread, so without the drain the last command is written to a socket
/// the host has already been told is finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keystroke_queued_just_before_the_teardown_still_reaches_the_host() {
    let endpoint = FakeEndpoint::start("drain", Answer::Silent);
    let mut session = endpoint.control("w1:p1", false).expect("control opens");
    let policy = SessionPolicy::new(TerminalMode::Control, TokenScope::Control)
        .expect("a control credential may control");

    assert_eq!(
        handle_client_text(
            &policy,
            &mut session,
            r#"{"type":"terminal.input","text":"echo drained\n"}"#
        )
        .await,
        ClientOutcome::Continue
    );
    assert_eq!(
        handle_client_text(&policy, &mut session, r#"{"type":"terminal.release"}"#).await,
        ClientOutcome::Release
    );
    // Exactly what `run_terminal` does once its loop ends.
    session.drain().await;
    drop(session);

    assert!(
        wait_until(Duration::from_secs(5), || endpoint
            .after()
            .iter()
            .any(|message| matches!(message, ClientMessage::Detach))),
        "the host never saw the release: {:?}",
        endpoint.after()
    );
    let after = endpoint.after();
    let inputs: Vec<Vec<u8>> = after
        .iter()
        .filter_map(|message| match message {
            ClientMessage::Input { data } => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        inputs,
        vec![b"echo drained\n".to_vec()],
        "the queued keystroke was lost by the teardown: {after:?}"
    );
}

/// The gateway opens every connection with no cell geometry and no pixel
/// mouse, and a resize must not put either back: a controller's resize is the
/// *real* pty resize, so a browser-chosen pixel cell size would change what the
/// pane reports to every other client of that host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_never_carries_the_clients_pixel_geometry() {
    let endpoint = FakeEndpoint::start("cell-geometry", Answer::Silent);
    let mut session = endpoint.control("w1:p1", false).expect("control opens");
    let policy = SessionPolicy::new(TerminalMode::Control, TokenScope::Control)
        .expect("a control credential may control");
    assert_eq!(endpoint.hello_cells(), Some((0, 0)));

    assert_eq!(
        handle_client_text(
            &policy,
            &mut session,
            r#"{"type":"terminal.resize","cols":60,"rows":20,"cell_width_px":4294967295,"cell_height_px":4294967295}"#
        )
        .await,
        ClientOutcome::Continue
    );

    assert!(
        wait_until(Duration::from_secs(5), || endpoint
            .after()
            .iter()
            .any(|message| matches!(message, ClientMessage::Resize { .. }))),
        "the host never saw the resize: {:?}",
        endpoint.after()
    );
    let after = endpoint.after();
    let resizes: Vec<&ClientMessage> = after
        .iter()
        .filter(|message| matches!(message, ClientMessage::Resize { .. }))
        .collect();
    assert!(
        resizes.iter().all(|message| matches!(
            message,
            ClientMessage::Resize {
                cols: 60,
                rows: 20,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
            }
        )),
        "a client's pixel geometry reached the host: {resizes:?}"
    );
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
