//! `GET /api/terminal/{host}/{pane}` — one pane's rendered ANSI, live, over a
//! WebSocket.
//!
//! The gateway does not re-render anything. A herdr server already produces
//! diffed ANSI for a terminal client ([`ServerMessage::Terminal`]), so a
//! session here is a pipe with a policy on it: open a *separate* client
//! connection to that host through [`HostTransports`], put it in observe or
//! control mode, and forward every frame to the browser as a binary message
//! with a 14-byte header. Frames are never decoded, re-encoded or buffered
//! beyond two.
//!
//! **Why a separate connection.** A client socket's mode is fixed for its
//! lifetime by its first message, so a terminal cannot ride the fleet
//! connector's endpoint stream. It gets its own, in the `gateway` socket scope.
//!
//! **Backpressure.** The reader thread hands frames to the socket task over a
//! capacity-2 channel and blocks when it is full. The herdr server's own
//! per-client render lane has capacity one and coalesces, so a browser that
//! cannot keep up makes its host skip intermediate frames instead of growing a
//! queue. Memory per session is two frames.
//!
//! **Scope.** Observe never carries input, and only a `control` credential can
//! open a control session at all. Both halves live in [`SessionPolicy`]: the
//! `(control mode, read scope)` pair cannot be constructed, and
//! [`SessionPolicy::admit`] re-checks *every* message, because a route-level
//! gate decided once at the handshake cannot police a socket that lives for
//! hours. The route's `require(&principal, TokenScope::Read)` is the floor for
//! reaching the endpoint, never the gate on what the session may do.
//!
//! **Ownership.** A herdr server gives a pane's PTY to one attached client at
//! a time. A control open on a pane somebody else holds is answered
//! `terminal.error {code:"busy"}`; `terminal.open {takeover:true}` evicts that
//! owner, whose session then ends with `terminal.closed {reason:"taken_over"}`.
//! `terminal.release` (or the socket closing) hands the pane back.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use tokio::sync::mpsc;

use crate::client::terminal_control_command_from_json;
use crate::fleet::hosts::{HostId, HostSpec};
use crate::fleet::refs::is_valid_resource_id;
use crate::gateway::auth::{Credential, Principal, TokenScope};
use crate::gateway::http::ApiError;
use crate::gateway::middleware::{require, Authed};
use crate::gateway::protocol::{
    check_geometry, classify_shutdown_reason, encode_frame, parse_terminal_open, terminal_closed,
    terminal_error, terminal_ready, SessionPolicy, TerminalClose, TerminalErrorCode, TerminalMode,
    CLOSED_RELEASED, CLOSED_TAKEN_OVER, MAX_CLIENT_MESSAGE_BYTES,
};
use crate::gateway::server::AppState;
use crate::gateway::transports::{HostTransports, OpenStream, StreamLease, HOST_BUSY_KIND};
use crate::ipc::LocalStream;
use crate::protocol::{
    self, ClientMessage, FramingError, RenderEncoding, ServerMessage, TerminalFrame,
    MAX_FRAME_SIZE, PROTOCOL_VERSION,
};

/// How long the client has to send its `terminal.open`.
///
/// Long enough for a phone waking up on a slow link, short enough that an
/// upgraded connection that never says what it wants does not hold a socket.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Frames buffered between the host and the browser. See the module docs.
const FRAME_CHANNEL_CAPACITY: usize = 2;
/// Commands buffered between the browser and the host.
const COMMAND_CHANNEL_CAPACITY: usize = 16;
/// How long the gateway waits for the host to confirm the session before it
/// tells the client the terminal is ready.
///
/// A herdr server answers an accepted observe or attach with a full redraw and
/// a refused one with a `ServerShutdown`, so the first event decides. The bound
/// exists only so a host that says neither still produces a usable session
/// rather than a socket that hangs: on timeout the client gets its
/// `terminal.ready` and the stream continues.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// The terminal stream. Merged by [`crate::gateway::server::router`], which is
/// what puts it behind the auth layer.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/terminal/{host}/{pane}", get(terminal_ws))
}

/// Validate the target, then upgrade.
///
/// A bad host id or pane id is a `404` **before** the upgrade: a client that
/// asked for something that cannot exist should learn it from HTTP, not from a
/// WebSocket that opens and immediately errors.
async fn terminal_ws(
    State(state): State<AppState>,
    Authed(principal): Authed,
    Path((host, pane)): Path<(String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    // The route states the weakest scope it can be used with; the session's own
    // policy decides everything after that, per message.
    if let Err(error) = require(&principal, TokenScope::Read) {
        return error.into_response();
    }
    let Some(host) = parse_target(&host, &pane) else {
        return ApiError::not_found().into_response();
    };

    ws.max_message_size(MAX_CLIENT_MESSAGE_BYTES)
        .max_frame_size(MAX_CLIENT_MESSAGE_BYTES)
        .on_upgrade(move |socket| run_terminal(socket, state, principal, host, pane))
}

/// Validate the path pair, or `None` for a target that cannot exist.
///
/// Pure, and refused *before* the upgrade: a host id never contains `/` and a
/// pane id never does either, which is exactly what makes `host/pane` an
/// unambiguous fleet reference. A spelling that would break that parse is a
/// `404`, not a reference the gateway mints anyway.
///
/// A control character is refused too: no server-side id contains one, and
/// the pane id is logged as a field on every open and close, so a `%0a` in
/// the path would otherwise be a line of the operator's choosing in the log.
fn parse_target(host: &str, pane: &str) -> Option<HostId> {
    let host = HostId::new(host).ok()?;
    (is_valid_resource_id(pane) && !pane.chars().any(char::is_control)).then_some(host)
}

/// One session, from `terminal.open` to close.
async fn run_terminal(
    mut socket: WebSocket,
    state: AppState,
    principal: Principal,
    host: HostId,
    pane: String,
) {
    // Subscribed before `connect`, on purpose: a watch receiver only sees
    // latches that happen after it exists, and `connect` refuses once the
    // latch is set. Together they leave no window in which a stopping gateway
    // has a session that nothing will end.
    let mut stopping = state.transports.stopping();

    let open = match read_open(&mut socket).await {
        Ok(open) => open,
        Err(failure) => return failure.emit(&mut socket).await,
    };

    // The session's own gate, decided before a host is touched: a credential
    // that cannot hold this mode never opens a connection at all. `Read` at
    // the route is the floor for reaching the endpoint, never this.
    let Some(policy) = SessionPolicy::new(open.mode, principal.scope) else {
        return Failure::new(
            TerminalErrorCode::Forbidden,
            format!(
                "terminal mode {} needs the {} scope",
                open.mode.as_str(),
                open.mode.required_scope().as_str()
            ),
            close_code::POLICY,
        )
        .emit(&mut socket)
        .await;
    };
    let mode = policy.mode;

    // Host failure is local to this socket: never a 5xx, never another host's
    // problem. `None` from either lookup means the host is not configured.
    match state.fleet.host_connection(&host) {
        Some(connection) if connection.is_connected() => {}
        Some(connection) => {
            let reason = connection.reason().unwrap_or("the host is not connected");
            return Failure::new(
                TerminalErrorCode::HostUnavailable,
                format!("host {host} is {}: {reason}", connection.state_name()),
                close_code::ERROR,
            )
            .emit(&mut socket)
            .await;
        }
        None => {
            return Failure::new(
                TerminalErrorCode::HostUnavailable,
                format!("no fleet host named {host}"),
                close_code::ERROR,
            )
            .emit(&mut socket)
            .await
        }
    }
    let Some(spec) = state.fleet.host_spec(&host) else {
        return Failure::new(
            TerminalErrorCode::HostUnavailable,
            format!("no fleet host named {host}"),
            close_code::ERROR,
        )
        .emit(&mut socket)
        .await;
    };

    // Blocking: ssh discovery, a socket connect and two round trips.
    let transports = Arc::clone(&state.transports);
    let (cols, rows) = (open.cols, open.rows);
    let request = SessionRequest {
        mode,
        pane: pane.clone(),
        cols,
        rows,
        takeover: open.takeover,
    };
    let opened =
        tokio::task::spawn_blocking(move || open_session(&transports, &spec, &request)).await;
    let mut session = match opened {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => {
            tracing::info!(
                target: "gateway",
                host = %host,
                pane = %pane,
                error = %error,
                "could not open a terminal session"
            );
            // A taken slot is not a host that is down: say so with a code the
            // client can retry on, and the close code that means the same.
            let (code, close) = if error.kind() == HOST_BUSY_KIND {
                (TerminalErrorCode::HostBusy, close_code::AGAIN)
            } else {
                (TerminalErrorCode::HostUnavailable, close_code::ERROR)
            };
            return Failure::new(
                code,
                format!("could not open a terminal on {host}: {error}"),
                close,
            )
            .emit(&mut socket)
            .await;
        }
        // The blocking task panicked or the runtime is going away; say so
        // rather than leave the browser waiting.
        Err(error) => {
            tracing::error!(target: "gateway", host = %host, pane = %pane, error = %error, "the terminal open task failed");
            return Failure::new(
                TerminalErrorCode::Internal,
                "the terminal session could not be started".to_string(),
                close_code::ERROR,
            )
            .emit(&mut socket)
            .await;
        }
    };

    // The host's first word decides whether this session exists. An accepted
    // observe or attach answers with a full redraw; a refused one answers with
    // a shutdown — a pane that is not there, an attach slot somebody else
    // holds. Saying `terminal.ready` before that would make a client open a
    // terminal it does not have and then take it away again.
    let first_frame = match await_attach(&mut session).await {
        Attached::Ready(frame) => frame,
        Attached::Refused { text, close } => {
            tracing::info!(
                target: "gateway",
                host = %host,
                pane = %pane,
                mode = mode.as_str(),
                "the host refused the terminal session"
            );
            drop(session);
            let _ = socket.send(Message::Text(text.into())).await;
            let _ = socket.send(Message::Close(Some(close))).await;
            return;
        }
    };

    tracing::info!(
        target: "gateway",
        host = %host,
        pane = %pane,
        mode = mode.as_str(),
        takeover = open.takeover,
        credential = credential_kind(&principal.via),
        "terminal session opened"
    );
    if socket
        .send(Message::Text(
            terminal_ready(mode, &host, &pane, cols, rows).into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    if let Some(frame) = first_frame {
        if socket
            .send(Message::Binary(encode_frame(&frame).into()))
            .await
            .is_err()
        {
            return;
        }
    }

    let mut close: Option<CloseFrame> = None;
    loop {
        tokio::select! {
            // A stopping gateway ends its sessions rather than waiting for
            // every browser to notice; the transports cannot be torn down
            // while a stream is open.
            changed = stopping.changed() => {
                if changed.is_err() || *stopping.borrow_and_update() {
                    let _ = socket.send(Message::Text(terminal_closed(Some("the gateway is stopping")).into())).await;
                    close = Some(frame(close_code::AWAY, "gateway stopping"));
                    break;
                }
            }
            event = session.frames.recv() => match event {
                Some(TerminalEvent::Frame(frame)) => {
                    if socket.send(Message::Binary(encode_frame(&frame).into())).await.is_err() {
                        break;
                    }
                }
                Some(TerminalEvent::Closed(reason)) => {
                    let answer = ending_answer(Ending::Closed(reason));
                    let _ = socket.send(Message::Text(answer.text.into())).await;
                    close = Some(answer.close);
                    break;
                }
                // The stream broke in a way that is nobody's request: a frame
                // over the cap, a framing error, a read error. Named, so a
                // client can tell it from the host closing the pane.
                Some(TerminalEvent::Failed(error)) => {
                    tracing::info!(target: "gateway", host = %host, pane = %pane, error = %error, "terminal stream failed");
                    let answer = ending_answer(Ending::Failed(error));
                    let _ = socket.send(Message::Text(answer.text.into())).await;
                    close = Some(answer.close);
                    break;
                }
                // The reader thread ended without a reason: the host hung up.
                None => {
                    let answer = ending_answer(Ending::Eof);
                    let _ = socket.send(Message::Text(answer.text.into())).await;
                    close = Some(answer.close);
                    break;
                }
            },
            message = socket.recv() => match message {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(text))) => {
                    match handle_client_text(&policy, &mut session, text.as_str()).await {
                        ClientOutcome::Continue => {}
                        ClientOutcome::Answer(text) => {
                            if socket.send(Message::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                        ClientOutcome::Release => {
                            // The client's own doing, so the gateway names it
                            // rather than relaying the host's "detached": a
                            // release and a pane that exited are not the same
                            // event to whatever drew the terminal.
                            let _ = socket
                                .send(Message::Text(
                                    terminal_closed(Some(CLOSED_RELEASED)).into(),
                                ))
                                .await;
                            close = Some(frame(close_code::NORMAL, "released"));
                            break;
                        }
                    }
                }
                // The client half is JSON text only; a binary message is a
                // client bug, not a frame the gateway should try to interpret.
                Some(Ok(Message::Binary(_))) => {
                    let answer = terminal_error(
                        TerminalErrorCode::BadRequest,
                        "terminal commands are JSON text messages",
                    );
                    if socket.send(Message::Text(answer.into())).await.is_err() {
                        break;
                    }
                }
                // Ping and pong are answered by the WebSocket layer.
                Some(Ok(_)) => {}
            },
        }
    }

    let _ = socket.send(Message::Close(close)).await;
    drop(session);
    tracing::info!(
        target: "gateway",
        host = %host,
        pane = %pane,
        mode = mode.as_str(),
        "terminal session closed"
    );
}

/// How a credential was presented, for the log. Never the secret, never a
/// device id: only which of the two ways it arrived.
fn credential_kind(credential: &Credential) -> &'static str {
    match credential {
        Credential::Bearer => "bearer",
        Credential::Device { .. } => "device",
    }
}

/// The host's answer to the opening request.
enum Attached {
    /// The session is live. The frame is the host's first redraw, or `None`
    /// when it said nothing within [`ATTACH_TIMEOUT`].
    Ready(Option<TerminalFrame>),
    /// The host ended the session before it began; this is what the client
    /// hears instead of `terminal.ready`.
    Refused { text: String, close: CloseFrame },
}

/// Wait for the host to accept or refuse the session.
async fn await_attach(session: &mut TerminalSession) -> Attached {
    match tokio::time::timeout(ATTACH_TIMEOUT, session.frames.recv()).await {
        Ok(Some(TerminalEvent::Frame(frame))) => Attached::Ready(Some(frame)),
        Ok(Some(TerminalEvent::Closed(reason))) => {
            Attached::from(ending_answer(Ending::Closed(reason)))
        }
        Ok(Some(TerminalEvent::Failed(error))) => {
            Attached::from(ending_answer(Ending::Failed(error)))
        }
        Ok(None) => Attached::from(ending_answer(Ending::Eof)),
        // Nothing yet. The connection is open and the policy is settled, so
        // this is a slow host, not a refusal: let the session run.
        Err(_) => Attached::Ready(None),
    }
}

impl From<Answer> for Attached {
    fn from(answer: Answer) -> Self {
        Attached::Refused {
            text: answer.text,
            close: answer.close,
        }
    }
}

/// How a session ended on the host's side.
enum Ending {
    /// A `ServerShutdown`, with the host's reason.
    Closed(Option<String>),
    /// The stream broke.
    Failed(String),
    /// The reader ended without a word.
    Eof,
}

/// What the client is told, and how the socket closes.
struct Answer {
    text: String,
    close: CloseFrame,
}

/// Translate a host-side ending into this stream's vocabulary.
///
/// One place, because an ending can arrive before `terminal.ready` (the host
/// refusing the open) or hours later (a takeover, the pane exiting), and the
/// two must not drift apart.
fn ending_answer(ending: Ending) -> Answer {
    let (text, close) = match ending {
        Ending::Closed(reason) => match classify_shutdown_reason(reason.as_deref()) {
            TerminalClose::PaneNotFound(reason) => (
                terminal_error(TerminalErrorCode::PaneNotFound, &reason),
                frame(close_code::NORMAL, "pane not found"),
            ),
            // Somebody else holds this pane. Retryable — with `takeover` —
            // so it closes like the gateway's own busy transport does.
            TerminalClose::Busy(reason) => (
                terminal_error(TerminalErrorCode::Busy, &reason),
                frame(close_code::AGAIN, "busy"),
            ),
            // Not an error: this client did nothing wrong, it simply no
            // longer owns the pane.
            TerminalClose::TakenOver => (
                terminal_closed(Some(CLOSED_TAKEN_OVER)),
                frame(close_code::NORMAL, "taken over"),
            ),
            TerminalClose::Closed(reason) => (
                terminal_closed(reason.as_deref()),
                frame(close_code::NORMAL, "closed"),
            ),
        },
        Ending::Failed(error) => (
            terminal_error(TerminalErrorCode::Internal, &error),
            frame(close_code::ERROR, "stream failed"),
        ),
        Ending::Eof => (terminal_closed(None), frame(close_code::NORMAL, "closed")),
    };
    Answer { text, close }
}

fn frame(code: u16, reason: &'static str) -> CloseFrame {
    CloseFrame {
        code,
        reason: reason.into(),
    }
}

/// What a client text message did.
#[derive(Debug, PartialEq, Eq)]
enum ClientOutcome {
    /// Forwarded, or ignored; nothing to say.
    Continue,
    /// Send this text back (an error the session survives).
    Answer(String),
    /// `terminal.release`: the session ends.
    Release,
}

/// Decide one client message and forward it if the policy admits it.
///
/// Split out so the gate is testable without a socket: the caller only owns
/// the sending.
async fn handle_client_text(
    policy: &SessionPolicy,
    session: &mut TerminalSession,
    text: &str,
) -> ClientOutcome {
    if text.len() > MAX_CLIENT_MESSAGE_BYTES {
        return ClientOutcome::Answer(terminal_error(
            TerminalErrorCode::BadRequest,
            "the command is too large",
        ));
    }
    let message = match terminal_control_command_from_json(text) {
        Ok(message) => message,
        Err(error) => {
            return ClientOutcome::Answer(terminal_error(TerminalErrorCode::BadRequest, &error))
        }
    };
    if let Err(code) = policy.admit(&message) {
        return ClientOutcome::Answer(terminal_error(code, refusal(code, policy.mode)));
    }
    // The CLI vocabulary bounds a resize below only; the host renders this
    // session at whatever it asks for, so the ceiling is applied here, the
    // same one `terminal.open` was held to.
    if let ClientMessage::Resize { cols, rows, .. } = &message {
        if let Err(error) = check_geometry(*cols, *rows) {
            return ClientOutcome::Answer(terminal_error(
                TerminalErrorCode::BadRequest,
                &format!("terminal.resize {error}"),
            ));
        }
    }
    let release = matches!(message, ClientMessage::Detach);
    if session.send(message).await.is_err() {
        // The writer thread is gone; the frame arm will end the session.
        return ClientOutcome::Continue;
    }
    if release {
        ClientOutcome::Release
    } else {
        ClientOutcome::Continue
    }
}

/// The human half of a refusal. Fixed strings: nothing a client sent is echoed.
fn refusal(code: TerminalErrorCode, mode: TerminalMode) -> &'static str {
    match (code, mode) {
        (TerminalErrorCode::Forbidden, TerminalMode::Observe) => {
            "an observe session cannot send input; open the terminal with mode \"control\""
        }
        (TerminalErrorCode::Forbidden, TerminalMode::Control) => {
            "this credential does not have the control scope"
        }
        (TerminalErrorCode::Unsupported, _) => {
            "the host ignores scrollback commands from an observer; scroll in the client instead"
        }
        _ => "the command is not allowed on this session",
    }
}

/// A refusal that ends the session before it started.
struct Failure {
    code: TerminalErrorCode,
    message: String,
    close: u16,
}

impl Failure {
    fn new(code: TerminalErrorCode, message: String, close: u16) -> Self {
        Self {
            code,
            message,
            close,
        }
    }

    async fn emit(self, socket: &mut WebSocket) {
        let _ = socket
            .send(Message::Text(
                terminal_error(self.code, &self.message).into(),
            ))
            .await;
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: self.close,
                reason: self.code.as_str().into(),
            })))
            .await;
    }
}

/// Read the opening message, tolerating pings while it waits.
async fn read_open(
    socket: &mut WebSocket,
) -> Result<crate::gateway::protocol::TerminalOpen, Failure> {
    let deadline = Instant::now() + OPEN_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Failure::new(
                TerminalErrorCode::BadRequest,
                "no terminal.open arrived".to_string(),
                close_code::PROTOCOL,
            ));
        }
        match tokio::time::timeout(remaining, socket.recv()).await {
            Err(_) | Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => {
                return Err(Failure::new(
                    TerminalErrorCode::BadRequest,
                    "no terminal.open arrived".to_string(),
                    close_code::PROTOCOL,
                ))
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                return parse_terminal_open(text.as_str()).map_err(|error| {
                    Failure::new(TerminalErrorCode::BadRequest, error, close_code::PROTOCOL)
                })
            }
            Ok(Some(Ok(Message::Binary(_)))) => {
                return Err(Failure::new(
                    TerminalErrorCode::BadRequest,
                    "the first message must be a terminal.open text message".to_string(),
                    close_code::PROTOCOL,
                ))
            }
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// Everything the client asked for, owned so it can cross into
/// `spawn_blocking`.
#[derive(Debug, Clone)]
struct SessionRequest {
    mode: TerminalMode,
    pane: String,
    cols: u16,
    rows: u16,
    takeover: bool,
}

impl SessionRequest {
    /// The message that fixes this connection's mode for its whole life.
    ///
    /// The only place `takeover` is read: an observer owns nothing, so there
    /// is nothing for it to take, and the server has no observe equivalent.
    fn attach(&self) -> ClientMessage {
        match self.mode {
            TerminalMode::Observe => ClientMessage::ObserveTerminal {
                target: self.pane.clone(),
            },
            TerminalMode::Control => ClientMessage::ControlTerminal {
                target: self.pane.clone(),
                takeover: self.takeover,
            },
        }
    }
}

/// Open a client connection to `spec`'s host and put it in the asked-for mode.
///
/// **Blocking.** Every failure is one host's: the caller turns it into a
/// `terminal.error` on this socket and nothing else.
fn open_session(
    transports: &HostTransports,
    spec: &HostSpec,
    request: &SessionRequest,
) -> io::Result<TerminalSession> {
    let OpenStream {
        stream,
        read_timeout,
        lease,
    } = transports.connect(spec)?;
    negotiate(stream, read_timeout, request, lease)
}

/// The terminal handshake on an already-open stream. **Blocking.**
///
/// Split from [`open_session`] so the two frozen exchanges — hello/welcome and
/// `ObserveTerminal`/`ControlTerminal` — are testable against a fake endpoint
/// without a transport, a config directory or a session socket.
fn negotiate(
    mut stream: LocalStream,
    read_timeout: Duration,
    request: &SessionRequest,
    lease: StreamLease,
) -> io::Result<TerminalSession> {
    // The terminal hello, not the endpoint hello: this connection is a
    // terminal client for its whole life. No cell geometry and no pixel mouse —
    // the gateway renders nothing itself and never enables graphics, so every
    // frame stays inside `MAX_FRAME_SIZE`.
    write(
        &mut stream,
        &ClientMessage::TerminalHello {
            version: PROTOCOL_VERSION,
            cols: request.cols,
            rows: request.rows,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        },
    )?;

    set_recv_timeout(&stream, Some(read_timeout))?;
    let welcome = protocol::read_message::<_, ServerMessage>(&mut stream, MAX_FRAME_SIZE);
    // Clear the deadline even when the read failed, so a stream that is kept
    // for a moment longer does not carry a stale timeout into framing.
    let cleared = set_recv_timeout(&stream, None);
    let welcome = welcome.map_err(framing)?;
    cleared?;
    match welcome {
        ServerMessage::Welcome {
            error: Some(error), ..
        } => {
            return Err(io::Error::other(format!(
                "the host refused the terminal session: {error}"
            )))
        }
        ServerMessage::Welcome {
            encoding: RenderEncoding::TerminalAnsi,
            ..
        } => {}
        ServerMessage::Welcome { encoding, .. } => {
            return Err(io::Error::other(format!(
                "the host negotiated {encoding:?} rather than terminal ansi"
            )))
        }
        _ => {
            return Err(io::Error::other(
                "the host did not answer the terminal hello with a welcome",
            ))
        }
    }

    write(&mut stream, &request.attach())?;

    TerminalSession::start(stream, lease)
}

/// What the reader thread hands the socket task.
enum TerminalEvent {
    Frame(TerminalFrame),
    /// The host said why it is ending the session.
    Closed(Option<String>),
    /// The stream ended on an error rather than on the host's say-so.
    Failed(String),
}

/// One open observe/control connection: two threads and the channels to them.
///
/// Dropping it half-closes the write side, which makes the host hang up and
/// both threads exit; the [`StreamLease`] is released only once they have, so a
/// stopping gateway never drops a transport under a live ssh bridge.
struct TerminalSession {
    frames: mpsc::Receiver<TerminalEvent>,
    commands: Option<mpsc::Sender<ClientMessage>>,
    /// A third handle on the same socket, kept only to half-close it.
    close: LocalStream,
    _lease: StreamLease,
}

impl TerminalSession {
    fn start(stream: LocalStream, lease: StreamLease) -> io::Result<Self> {
        let read_stream = stream.try_clone()?;
        let close = stream.try_clone()?;
        let (frames_tx, frames_rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);

        let reader_lease = lease.clone();
        std::thread::Builder::new()
            .name("herdr-gw-term-r".to_string())
            .spawn(move || {
                let _lease = reader_lease;
                read_frames(read_stream, &frames_tx);
            })?;

        let writer_lease = lease.clone();
        std::thread::Builder::new()
            .name("herdr-gw-term-w".to_string())
            .spawn(move || {
                let _lease = writer_lease;
                write_commands(stream, commands_rx);
            })?;

        Ok(Self {
            frames: frames_rx,
            commands: Some(commands_tx),
            close,
            _lease: lease,
        })
    }

    /// Queue one message for the host. `Err` means the writer thread is gone.
    async fn send(&mut self, message: ClientMessage) -> Result<(), ()> {
        let Some(commands) = &self.commands else {
            return Err(());
        };
        commands.send(message).await.map_err(|_| ())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Order matters. Dropping the command sender ends the writer thread's
        // `blocking_recv`; half-closing the write side makes the host hang up,
        // which ends the reader thread's blocking read. Dropping `frames` (as
        // part of this drop) releases a reader blocked on a full channel.
        self.commands = None;
        half_close_write(&self.close);
    }
}

/// Forward every terminal frame until the host stops or nobody is listening.
fn read_frames(mut stream: LocalStream, frames: &mpsc::Sender<TerminalEvent>) {
    loop {
        // `MAX_FRAME_SIZE`, not the graphics cap: the gateway never enables
        // direct graphics, so a larger frame is not something it asked for.
        match protocol::read_message::<_, ServerMessage>(&mut stream, MAX_FRAME_SIZE) {
            Ok(ServerMessage::Terminal(frame)) => {
                // Blocks while the browser is behind: that is the backpressure.
                if frames.blocking_send(TerminalEvent::Frame(frame)).is_err() {
                    return;
                }
            }
            Ok(ServerMessage::ServerShutdown { reason }) => {
                let _ = frames.blocking_send(TerminalEvent::Closed(reason));
                return;
            }
            // Graphics and every shell-lane message are not part of this
            // connection's vocabulary; drop them the way the CLI twin does.
            Ok(_) => {}
            Err(FramingError::UnexpectedEof) => return,
            // Oversized, malformed, or a read error: the session cannot go on,
            // and the browser should hear why rather than see a plain close.
            Err(error) => {
                let _ = frames.blocking_send(TerminalEvent::Failed(format!(
                    "terminal frame read ended: {error}"
                )));
                return;
            }
        }
    }
}

/// Write every admitted command until the session ends.
fn write_commands(mut stream: LocalStream, mut commands: mpsc::Receiver<ClientMessage>) {
    while let Some(message) = commands.blocking_recv() {
        let release = matches!(message, ClientMessage::Detach);
        if let Err(error) = protocol::write_message(&mut stream, &message) {
            tracing::debug!(target: "gateway", error = %error, "terminal command write failed");
            break;
        }
        if release {
            break;
        }
    }
    // Tell the host we are done even when the session ended without a
    // `terminal.release`, so it drops this observer promptly.
    half_close_write(&stream);
}

fn write(stream: &mut LocalStream, message: &ClientMessage) -> io::Result<()> {
    protocol::write_message(stream, message).map_err(framing)
}

fn framing(error: FramingError) -> io::Error {
    io::Error::other(error.to_string())
}

fn set_recv_timeout(stream: &LocalStream, timeout: Option<Duration>) -> io::Result<()> {
    match stream.set_recv_timeout(timeout) {
        Ok(()) => Ok(()),
        // A platform without socket deadlines still works; the session simply
        // waits on the host as long as the host takes.
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(()),
        Err(error) => Err(error),
    }
}

/// Half-close the write side so the host hangs up and the reader unblocks.
///
/// Windows named pipes have no equivalent, exactly as in the fleet connector:
/// the reader there ends on its next message or with the process.
#[cfg(unix)]
fn half_close_write(stream: &LocalStream) {
    match stream {
        LocalStream::UdSocket(stream) => {
            let _ = stream.inner().shutdown(std::net::Shutdown::Write);
        }
    }
}

#[cfg(not(unix))]
fn half_close_write(_stream: &LocalStream) {}

#[cfg(all(test, unix))]
mod tests;
