//! The message shapes the gateway's WebSocket surfaces speak.
//!
//! Pure data: `serde` types, constants and their encoding, with no tokio, no
//! axum and no sockets. That is deliberate — the ordering and wire shape of a
//! stream is the part a client depends on, so it is testable without a
//! runtime, and the socket half only has to decide *when* to send what this
//! module knows how to spell.
//!
//! Each area of the gateway owns a section below and appends to it; nothing
//! removes or reinterprets a field a released client may already read.

// ---- events (PR 5) ----

use serde::Serialize;

use crate::fleet::report::FleetStatusReport;
use crate::gateway::auth::TokenScope;

/// Schema marker of every message on `/api/events`.
///
/// The contract it names: the first message is `hello`, the second is `fleet`
/// carrying a whole [`FleetStatusReport`], and every message after that is
/// either one `FleetChange` verbatim or a `resync` followed by a fresh
/// `fleet`. A reader that meets a `kind` it does not know skips it — new
/// change kinds are added to `FleetChange`, not to this schema.
pub(crate) const EVENTS_SCHEMA: &str = "herdr.fleet.events.v1";

/// First message on `/api/events`: what the client is talking to, and what it
/// is allowed to do.
///
/// `scope` is the scope the *credential* proved, not the scope this socket
/// needs — a control-token client reads the same stream, and knowing its scope
/// up front is what lets a UI decide whether to offer input at all.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct EventsHello {
    kind: &'static str,
    schema: &'static str,
    client_version: String,
    scope: &'static str,
}

impl EventsHello {
    pub(crate) fn new(client_version: impl Into<String>, scope: TokenScope) -> Self {
        Self {
            kind: "hello",
            schema: EVENTS_SCHEMA,
            client_version: client_version.into(),
            scope: scope.as_str(),
        }
    }
}

/// The whole fleet, in exactly the shape `GET /api/fleet` answers with.
///
/// Sent once after `hello`, and again after every [`Resync`]: a client always
/// has a complete state to apply deltas to, and never has to guess whether it
/// missed one.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct EventsFleet {
    kind: &'static str,
    report: FleetStatusReport,
}

impl EventsFleet {
    pub(crate) fn new(report: FleetStatusReport) -> Self {
        Self {
            kind: "fleet",
            report,
        }
    }
}

/// "You fell behind; the deltas you missed are gone."
///
/// Always immediately followed by a fresh [`EventsFleet`], so a client can
/// treat the pair as "throw away what you have and start from this".
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Resync {
    kind: &'static str,
}

impl Resync {
    pub(crate) fn new() -> Self {
        Self { kind: "resync" }
    }
}

/// Encode one message as a single newline-free JSON line.
///
/// A WebSocket text message has no framing problem of its own, but the epic's
/// tooling (`scripts/fork/ws-client.py`, and every log that echoes a message)
/// is line-oriented, so a newline inside a payload would break a reader that
/// is not at fault. `serde_json::to_string` never emits one for these shapes;
/// the assertion is a cheap guard on that staying true.
pub(crate) fn encode<T: Serialize>(message: &T) -> Result<String, serde_json::Error> {
    let encoded = serde_json::to_string(message)?;
    debug_assert!(
        !encoded.contains('\n') && !encoded.contains('\r'),
        "a gateway message must be newline-free"
    );
    Ok(encoded)
}

#[cfg(test)]
mod events_tests {
    use super::*;

    fn kind_of(json: &str) -> String {
        serde_json::from_str::<serde_json::Value>(json)
            .expect("the message is JSON")
            .get("kind")
            .and_then(|kind| kind.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| panic!("no kind in {json}"))
    }

    /// The schema name is a published constant: a client keys its whole
    /// decoder off it, so changing it means a new name, not an edit.
    #[test]
    fn the_events_schema_name_is_frozen() {
        assert_eq!(EVENTS_SCHEMA, "herdr.fleet.events.v1");
    }

    #[test]
    fn every_message_carries_its_kind_and_no_newline() {
        let hello = encode(&EventsHello::new("0.0.0-test", TokenScope::Read)).expect("hello");
        assert_eq!(kind_of(&hello), "hello");
        assert!(!hello.contains('\n'), "{hello}");

        let resync = encode(&Resync::new()).expect("resync");
        assert_eq!(kind_of(&resync), "resync");
        assert!(!resync.contains('\n'), "{resync}");
    }

    /// The hello states the schema and the scope the caller proved, so a UI
    /// can decide what to offer before a single delta arrives.
    #[test]
    fn the_hello_states_the_schema_the_version_and_the_scope() {
        let json: serde_json::Value = serde_json::from_str(
            &encode(&EventsHello::new("0.8.2-fork", TokenScope::Control)).expect("hello"),
        )
        .expect("JSON");
        assert_eq!(json["schema"].as_str(), Some(EVENTS_SCHEMA));
        assert_eq!(json["client_version"].as_str(), Some("0.8.2-fork"));
        assert_eq!(json["scope"].as_str(), Some("control"));
    }

    /// The report is nested under `report`, not flattened, so a `kind` inside
    /// the fleet shape can never be mistaken for the message's own.
    #[test]
    fn the_fleet_message_nests_the_whole_report() {
        let mut state = crate::fleet::state::FleetState::new(Vec::new());
        let report = FleetStatusReport::from_state(&mut state, "0.0.0-test");
        let json: serde_json::Value =
            serde_json::from_str(&encode(&EventsFleet::new(report)).expect("fleet")).expect("JSON");
        assert_eq!(json["kind"].as_str(), Some("fleet"));
        assert!(json["report"].is_object(), "{json}");
        assert_eq!(
            json["report"]["schema"].as_str(),
            Some("herdr.fleet.status.v1"),
            "{json}"
        );
    }
}

// ---- terminal (PR 6) ----

use serde::Deserialize;

use crate::fleet::hosts::HostId;
use crate::fleet::refs::FleetPaneRef;
use crate::protocol::{ClientMessage, TerminalFrame};

/// Schema marker of the `/api/terminal/{host}/{pane}` vocabulary.
///
/// Sent on `terminal.ready` so a client that reconnects to an older or newer
/// gateway can tell which contract it is speaking before it decodes a frame.
pub(crate) const TERMINAL_SCHEMA: &str = "herdr.fleet.terminal.v1";

/// Bytes before the payload of a binary terminal message.
///
/// `[kind u8][seq u64 LE][width u16 LE][height u16 LE][full u8]` — 14 bytes,
/// then the frame's ANSI bytes verbatim. Fixed width on purpose: a browser
/// reads the header with one `DataView` and hands `frame.slice(14)` to
/// xterm.js without copying or parsing JSON per frame.
pub(crate) const FRAME_HEADER_LEN: usize = 14;

/// The only binary message kind the terminal stream sends today.
///
/// A client must ignore a binary message whose first byte it does not know, so
/// a later kind (graphics, a heartbeat) can be added without breaking it.
pub(crate) const FRAME_KIND_TERMINAL: u8 = 0x01;

/// Largest inbound WebSocket message the terminal stream accepts.
///
/// A control message is a small JSON object; 64 KiB is generous enough for a
/// paste of pathological size and small enough that a client cannot make the
/// gateway buffer meaningfully.
pub(crate) const MAX_CLIENT_MESSAGE_BYTES: usize = 64 * 1024;

/// Largest viewport a terminal session may ask a host for, in each dimension.
///
/// The server clamps a client's geometry to a *minimum* only, and renders an
/// observer's frames at whatever size it declared — so without a ceiling a
/// read credential could make a host render a 65535×65535 viewport on every
/// repaint. 1024×1024 is far beyond any browser terminal (a 5K display at a
/// 6 px font is ~850 columns) and keeps a plain-text full frame inside
/// `MAX_FRAME_SIZE`.
pub(crate) const MAX_TERMINAL_COLS: u16 = 1024;
pub(crate) const MAX_TERMINAL_ROWS: u16 = 1024;

/// Whether a geometry is one the gateway will relay to a host.
///
/// Applied to `terminal.open` and to every `terminal.resize`: the server
/// accepts either without an upper bound, so the bound has to live here.
pub(crate) fn check_geometry(cols: u16, rows: u16) -> Result<(), String> {
    if cols == 0 || rows == 0 {
        return Err("cols and rows must be greater than 0".to_string());
    }
    if cols > MAX_TERMINAL_COLS || rows > MAX_TERMINAL_ROWS {
        return Err(format!(
            "cols and rows must be at most {MAX_TERMINAL_COLS}x{MAX_TERMINAL_ROWS}"
        ));
    }
    Ok(())
}

/// What the client asked for in its first message.
///
/// Deliberately strict: an unknown field is a typo the client should learn
/// about now rather than a silently ignored intent (a mistyped `mode` must not
/// quietly downgrade a control session to observe).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalOpen {
    /// `"terminal.open"`, kept so the first message is parsed by the same
    /// tagged vocabulary as every later one.
    #[serde(rename = "type")]
    pub(crate) kind: TerminalOpenTag,
    pub(crate) mode: TerminalMode,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    /// Only meaningful for `mode: "control"` (PR 7); ignored in observe mode.
    #[serde(default)]
    pub(crate) takeover: bool,
}

/// The literal `"terminal.open"` tag, as a type so serde rejects anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum TerminalOpenTag {
    #[serde(rename = "terminal.open")]
    Open,
}

/// Whether the client wants to watch or to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TerminalMode {
    Observe,
    Control,
}

impl TerminalMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TerminalMode::Observe => "observe",
            TerminalMode::Control => "control",
        }
    }

    /// The credential scope a session in this mode requires.
    pub(crate) fn required_scope(self) -> TokenScope {
        match self {
            TerminalMode::Observe => TokenScope::Read,
            TerminalMode::Control => TokenScope::Control,
        }
    }
}

/// Parse the opening message.
///
/// The error string is for the gateway's own log and for the `message` field
/// of a `terminal.error`; it never repeats a caller-supplied value verbatim
/// beyond what serde already names.
pub(crate) fn parse_terminal_open(raw: &str) -> Result<TerminalOpen, String> {
    if raw.len() > MAX_CLIENT_MESSAGE_BYTES {
        return Err("terminal.open is too large".to_string());
    }
    let open: TerminalOpen =
        serde_json::from_str(raw).map_err(|error| format!("invalid terminal.open: {error}"))?;
    check_geometry(open.cols, open.rows).map_err(|error| format!("terminal.open {error}"))?;
    Ok(open)
}

/// Every `terminal.error` code a client may see.
///
/// The code is the contract a client switches on; the message is for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalErrorCode {
    /// The client sent something the vocabulary does not allow.
    BadRequest,
    /// The credential's scope is too narrow for what was asked.
    Forbidden,
    /// The host is not configured, not connected, or incompatible.
    HostUnavailable,
    /// The host is up but its one terminal stream slot is taken (an ssh
    /// host's bridge serves one at a time); the client may retry later.
    HostBusy,
    /// The host is reachable but has no such pane.
    PaneNotFound,
    /// The vocabulary allows it but this session cannot do it.
    Unsupported,
    /// The gateway failed for a reason that is not the client's fault.
    Internal,
}

impl TerminalErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TerminalErrorCode::BadRequest => "bad_request",
            TerminalErrorCode::Forbidden => "forbidden",
            TerminalErrorCode::HostUnavailable => "host_unavailable",
            TerminalErrorCode::HostBusy => "host_busy",
            TerminalErrorCode::PaneNotFound => "pane_not_found",
            TerminalErrorCode::Unsupported => "unsupported",
            TerminalErrorCode::Internal => "internal",
        }
    }
}

/// What a session in a given mode, held by a given scope, may send onward.
///
/// Pure and re-checked on **every** message rather than only at the handshake:
/// a route-level scope gate cannot police what a long-lived socket does later,
/// and this is the one place a byte can become terminal input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionPolicy {
    pub(crate) mode: TerminalMode,
    pub(crate) scope: TokenScope,
}

impl SessionPolicy {
    pub(crate) fn new(mode: TerminalMode, scope: TokenScope) -> Self {
        Self { mode, scope }
    }

    /// Whether this session may forward `message` to the host.
    pub(crate) fn admit(&self, message: &ClientMessage) -> Result<(), TerminalErrorCode> {
        match message {
            // Input needs *both* halves: a control-mode session opened with a
            // read credential must never reach this arm, and a control
            // credential must not smuggle input through an observe session.
            ClientMessage::Input { .. } => {
                if self.mode == TerminalMode::Control && self.scope.allows(TokenScope::Control) {
                    Ok(())
                } else {
                    Err(TerminalErrorCode::Forbidden)
                }
            }
            // An observer's resize is client-local by server design
            // (`headless.rs` only resizes the PTY for an attached client), so
            // every observer may size its own viewport.
            ClientMessage::Resize { .. } => Ok(()),
            // The server's scroll handler requires `TerminalAttach`; an
            // observer's scroll is dropped on the floor, and the shared
            // scrollback it moves for a controller is not client-local either.
            // Answer honestly rather than pretend it worked.
            ClientMessage::AttachScroll { .. } => match self.mode {
                TerminalMode::Observe => Err(TerminalErrorCode::Unsupported),
                TerminalMode::Control => {
                    if self.scope.allows(TokenScope::Control) {
                        Ok(())
                    } else {
                        Err(TerminalErrorCode::Forbidden)
                    }
                }
            },
            // Releasing is always allowed: it only ends this session.
            ClientMessage::Detach => Ok(()),
            // The JSON vocabulary produces nothing else; a future variant must
            // opt in here rather than inherit permission.
            _ => Err(TerminalErrorCode::BadRequest),
        }
    }
}

/// Encode one rendered frame as a binary WebSocket message.
///
/// The bytes are the server's already-diffed ANSI, copied once behind the
/// header and never re-encoded: a frame costs one allocation per session per
/// frame, which is the whole per-terminal cost of the stream.
pub(crate) fn encode_frame(frame: &TerminalFrame) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(FRAME_HEADER_LEN + frame.bytes.len());
    encoded.push(FRAME_KIND_TERMINAL);
    encoded.extend_from_slice(&frame.seq.to_le_bytes());
    encoded.extend_from_slice(&frame.width.to_le_bytes());
    encoded.extend_from_slice(&frame.height.to_le_bytes());
    encoded.push(u8::from(frame.full));
    encoded.extend_from_slice(&frame.bytes);
    encoded
}

/// The fixed part of a binary terminal message.
///
/// Test-only: the gateway writes headers and never reads one back. It exists so
/// the layout has a single definition that a round-trip test pins, rather than
/// a comment the encoder could drift away from.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    pub(crate) kind: u8,
    pub(crate) seq: u64,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) full: bool,
}

/// Read the header of a binary terminal message, or `None` if it is too short.
///
/// Test-only, for the same reason as [`FrameHeader`]: it is the reader half of
/// the layout E4's browser code implements in TypeScript.
#[cfg(test)]
pub(crate) fn decode_frame_header(message: &[u8]) -> Option<FrameHeader> {
    if message.len() < FRAME_HEADER_LEN {
        return None;
    }
    let seq = u64::from_le_bytes(message.get(1..9)?.try_into().ok()?);
    let width = u16::from_le_bytes(message.get(9..11)?.try_into().ok()?);
    let height = u16::from_le_bytes(message.get(11..13)?.try_into().ok()?);
    Some(FrameHeader {
        kind: *message.first()?,
        seq,
        width,
        height,
        full: *message.get(13)? != 0,
    })
}

/// `terminal.ready` — the session is open and frames are coming.
///
/// Carries both halves of the identity the client asked for *and* the
/// host-qualified `ref`, so a client that opened several terminals can route a
/// frame without keeping its own map from socket to pane.
pub(crate) fn terminal_ready(
    mode: TerminalMode,
    host: &HostId,
    pane: &str,
    cols: u16,
    rows: u16,
) -> String {
    json_line(&serde_json::json!({
        "type": "terminal.ready",
        "schema": TERMINAL_SCHEMA,
        "mode": mode.as_str(),
        "host": host.as_str(),
        "pane": pane,
        "ref": FleetPaneRef::new(host.clone(), pane).to_string(),
        "cols": cols,
        "rows": rows,
    }))
}

/// `terminal.error` — something this session asked for did not happen.
///
/// A code the client switches on, plus a message for a human. The message is
/// the gateway's own words or a host's reason; it never echoes a credential.
pub(crate) fn terminal_error(code: TerminalErrorCode, message: &str) -> String {
    json_line(&serde_json::json!({
        "type": "terminal.error",
        "code": code.as_str(),
        "message": message,
    }))
}

/// `terminal.closed` — the host ended the session, and why.
pub(crate) fn terminal_closed(reason: Option<&str>) -> String {
    json_line(&serde_json::json!({
        "type": "terminal.closed",
        "reason": reason,
    }))
}

/// What a `ServerShutdown` on a terminal connection means to the client.
///
/// The server disconnects an observe request whose target it cannot resolve,
/// which is the only way a client learns that a pane it named is gone. That is
/// a `terminal.error` with a code, not a plain close, so a UI can tell "no such
/// pane" from "the pane exited".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalClose {
    /// The host could not resolve the pane this session named.
    PaneNotFound(String),
    /// An ordinary end: the pane exited, the server stopped, a takeover.
    Closed(Option<String>),
}

pub(crate) fn classify_shutdown_reason(reason: Option<&str>) -> TerminalClose {
    match reason {
        // `headless.rs`: "terminal session observe failed: terminal target
        // <target> not found". Matched on both halves so an unrelated reason
        // containing "not found" is still an ordinary close.
        Some(text) if text.contains("terminal target") && text.contains("not found") => {
            TerminalClose::PaneNotFound(text.to_string())
        }
        other => TerminalClose::Closed(other.map(str::to_string)),
    }
}

/// One JSON object on one line, with no newline in it.
///
/// Every text message on these sockets is exactly one object, so a client may
/// treat a message boundary as a record boundary.
fn json_line(value: &serde_json::Value) -> String {
    value.to_string()
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    fn frame(seq: u64, full: bool, bytes: Vec<u8>) -> TerminalFrame {
        TerminalFrame {
            seq,
            width: 80,
            height: 24,
            full,
            bytes,
        }
    }

    #[test]
    fn a_frame_round_trips_through_the_header() {
        let original = frame(7, true, b"\x1b[2Jhello".to_vec());
        let encoded = encode_frame(&original);
        let header = decode_frame_header(&encoded).expect("a full header");
        assert_eq!(header.kind, FRAME_KIND_TERMINAL);
        assert_eq!(header.seq, 7);
        assert_eq!(header.width, 80);
        assert_eq!(header.height, 24);
        assert!(header.full);
        assert_eq!(&encoded[FRAME_HEADER_LEN..], original.bytes.as_slice());
    }

    #[test]
    fn an_incremental_frame_says_so() {
        let encoded = encode_frame(&frame(u64::MAX, false, Vec::new()));
        let header = decode_frame_header(&encoded).expect("a full header");
        assert_eq!(header.seq, u64::MAX);
        assert!(!header.full);
        assert_eq!(encoded.len(), FRAME_HEADER_LEN);
    }

    /// The server's cap for a non-graphics frame is 2 MiB, so the encoder has
    /// to survive one without truncating or reallocating the payload.
    #[test]
    fn a_two_mebibyte_frame_survives_the_header() {
        let payload = vec![b'x'; crate::protocol::MAX_FRAME_SIZE];
        let encoded = encode_frame(&frame(1, false, payload.clone()));
        assert_eq!(encoded.len(), FRAME_HEADER_LEN + payload.len());
        let header = decode_frame_header(&encoded).expect("a full header");
        assert_eq!(header.seq, 1);
        assert_eq!(&encoded[FRAME_HEADER_LEN..], payload.as_slice());
    }

    #[test]
    fn a_short_message_has_no_header() {
        assert_eq!(decode_frame_header(&[]), None);
        assert_eq!(decode_frame_header(&[0u8; FRAME_HEADER_LEN - 1]), None);
        assert!(decode_frame_header(&[0u8; FRAME_HEADER_LEN]).is_some());
    }

    #[test]
    fn a_well_formed_open_parses_in_both_modes() {
        let open =
            parse_terminal_open(r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24}"#)
                .expect("observe parses");
        assert_eq!(open.mode, TerminalMode::Observe);
        assert_eq!((open.cols, open.rows), (80, 24));
        assert!(!open.takeover);

        let open = parse_terminal_open(
            r#"{"type":"terminal.open","mode":"control","cols":100,"rows":30,"takeover":true}"#,
        )
        .expect("control parses");
        assert_eq!(open.mode, TerminalMode::Control);
        assert!(open.takeover);
    }

    #[test]
    fn a_malformed_open_is_an_error_not_a_default() {
        for raw in [
            // Missing geometry.
            r#"{"type":"terminal.open","mode":"observe","rows":24}"#,
            r#"{"type":"terminal.open","mode":"observe","cols":80}"#,
            // Zero geometry: a 0-column terminal is not a terminal.
            r#"{"type":"terminal.open","mode":"observe","cols":0,"rows":24}"#,
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":0}"#,
            // A viewport the host would have to render on every repaint.
            r#"{"type":"terminal.open","mode":"observe","cols":65535,"rows":24}"#,
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":65535}"#,
            r#"{"type":"terminal.open","mode":"observe","cols":1025,"rows":24}"#,
            // An unknown mode must never fall back to observe.
            r#"{"type":"terminal.open","mode":"drive","cols":80,"rows":24}"#,
            // Another message cannot open a session.
            r#"{"type":"terminal.input","text":"x"}"#,
            // A typo'd field is a client bug, not a silent default.
            r#"{"type":"terminal.open","mode":"observe","cols":80,"rows":24,"mdoe":"control"}"#,
            "not json at all",
        ] {
            assert!(parse_terminal_open(raw).is_err(), "accepted: {raw}");
        }
    }

    /// The bound is inclusive on both edges and shared with `terminal.resize`.
    #[test]
    fn the_geometry_bound_admits_the_edges_and_refuses_beyond_them() {
        assert!(check_geometry(1, 1).is_ok());
        assert!(check_geometry(MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS).is_ok());
        assert!(check_geometry(0, 1).is_err());
        assert!(check_geometry(1, 0).is_err());
        assert!(check_geometry(MAX_TERMINAL_COLS + 1, 1).is_err());
        assert!(check_geometry(1, MAX_TERMINAL_ROWS + 1).is_err());
        assert!(check_geometry(u16::MAX, u16::MAX).is_err());
        let largest = parse_terminal_open(&format!(
            r#"{{"type":"terminal.open","mode":"observe","cols":{MAX_TERMINAL_COLS},"rows":{MAX_TERMINAL_ROWS}}}"#
        ))
        .expect("the largest allowed viewport parses");
        assert_eq!(
            (largest.cols, largest.rows),
            (MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS)
        );
    }

    #[test]
    fn an_oversized_open_is_refused_before_it_is_parsed() {
        let raw = format!(
            r#"{{"type":"terminal.open","mode":"observe","cols":80,"rows":24,"pad":"{}"}}"#,
            "x".repeat(MAX_CLIENT_MESSAGE_BYTES)
        );
        assert!(parse_terminal_open(&raw).is_err());
    }

    #[test]
    fn each_mode_states_the_scope_it_needs() {
        assert_eq!(TerminalMode::Observe.required_scope(), TokenScope::Read);
        assert_eq!(TerminalMode::Control.required_scope(), TokenScope::Control);
        assert_eq!(TerminalMode::Observe.as_str(), "observe");
        assert_eq!(TerminalMode::Control.as_str(), "control");
    }

    fn input() -> ClientMessage {
        ClientMessage::Input {
            data: b"x".to_vec(),
        }
    }

    fn resize() -> ClientMessage {
        ClientMessage::Resize {
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        }
    }

    fn scroll() -> ClientMessage {
        ClientMessage::AttachScroll {
            source: crate::protocol::AttachScrollSource::Wheel,
            direction: crate::protocol::AttachScrollDirection::Up,
            lines: 3,
            column: None,
            row: None,
            modifiers: 0,
        }
    }

    /// The whole gate in one table. A row that changes here changes what a
    /// credential can do to somebody's machine.
    #[test]
    fn the_session_policy_decides_every_message_by_mode_and_scope() {
        let cases: [(
            TerminalMode,
            TokenScope,
            ClientMessage,
            Option<TerminalErrorCode>,
        ); 12] = [
            // Observe never admits input, whatever the credential proves.
            (
                TerminalMode::Observe,
                TokenScope::Read,
                input(),
                Some(TerminalErrorCode::Forbidden),
            ),
            (
                TerminalMode::Observe,
                TokenScope::Control,
                input(),
                Some(TerminalErrorCode::Forbidden),
            ),
            // Control-mode input needs the control scope.
            (
                TerminalMode::Control,
                TokenScope::Read,
                input(),
                Some(TerminalErrorCode::Forbidden),
            ),
            (TerminalMode::Control, TokenScope::Control, input(), None),
            // Resize is client-local for an observer and the PTY for a
            // controller; both are allowed.
            (TerminalMode::Observe, TokenScope::Read, resize(), None),
            (TerminalMode::Control, TokenScope::Control, resize(), None),
            // Scroll: the server ignores it for an observer.
            (
                TerminalMode::Observe,
                TokenScope::Read,
                scroll(),
                Some(TerminalErrorCode::Unsupported),
            ),
            (
                TerminalMode::Observe,
                TokenScope::Control,
                scroll(),
                Some(TerminalErrorCode::Unsupported),
            ),
            (
                TerminalMode::Control,
                TokenScope::Read,
                scroll(),
                Some(TerminalErrorCode::Forbidden),
            ),
            (TerminalMode::Control, TokenScope::Control, scroll(), None),
            // Releasing only ends this session.
            (
                TerminalMode::Observe,
                TokenScope::Read,
                ClientMessage::Detach,
                None,
            ),
            // Nothing outside the vocabulary is admitted by omission.
            (
                TerminalMode::Control,
                TokenScope::Control,
                ClientMessage::ObserveTerminal {
                    target: "w1:p1".to_string(),
                },
                Some(TerminalErrorCode::BadRequest),
            ),
        ];

        for (mode, scope, message, expected) in cases {
            let policy = SessionPolicy::new(mode, scope);
            assert_eq!(
                policy.admit(&message).err(),
                expected,
                "mode {mode:?} scope {scope:?} message {message:?}"
            );
        }
    }

    #[test]
    fn every_error_code_is_snake_case_and_distinct() {
        let codes = [
            TerminalErrorCode::BadRequest,
            TerminalErrorCode::Forbidden,
            TerminalErrorCode::HostUnavailable,
            TerminalErrorCode::HostBusy,
            TerminalErrorCode::PaneNotFound,
            TerminalErrorCode::Unsupported,
            TerminalErrorCode::Internal,
        ]
        .map(TerminalErrorCode::as_str);
        for code in codes {
            assert!(
                code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "not snake_case: {code}"
            );
        }
        let unique: std::collections::BTreeSet<&str> = codes.into_iter().collect();
        assert_eq!(unique.len(), codes.len());
    }
}

#[cfg(test)]
mod terminal_message_tests {
    use super::*;

    fn host(id: &str) -> HostId {
        HostId::new(id).expect("valid host id")
    }

    #[test]
    fn ready_names_the_host_the_pane_and_the_reference() {
        let line = terminal_ready(TerminalMode::Observe, &host("lab-2"), "w1:p1", 80, 24);
        assert!(!line.contains('\n'), "{line}");
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["type"], "terminal.ready");
        assert_eq!(value["schema"], TERMINAL_SCHEMA);
        assert_eq!(value["mode"], "observe");
        assert_eq!(value["host"], "lab-2");
        assert_eq!(value["pane"], "w1:p1");
        assert_eq!(value["ref"], "lab-2/w1:p1");
        assert_eq!(value["cols"], 80);
        assert_eq!(value["rows"], 24);
    }

    #[test]
    fn an_error_carries_its_code_and_a_message() {
        let line = terminal_error(TerminalErrorCode::Forbidden, "observe sessions cannot type");
        let value: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["type"], "terminal.error");
        assert_eq!(value["code"], "forbidden");
        assert_eq!(value["message"], "observe sessions cannot type");
    }

    #[test]
    fn a_close_keeps_the_hosts_reason_or_says_there_was_none() {
        let value: serde_json::Value =
            serde_json::from_str(&terminal_closed(Some("pane exited"))).expect("json");
        assert_eq!(value["type"], "terminal.closed");
        assert_eq!(value["reason"], "pane exited");

        let value: serde_json::Value = serde_json::from_str(&terminal_closed(None)).expect("json");
        assert!(value["reason"].is_null(), "{value}");
    }

    /// The server's own wording, verbatim from `headless.rs`.
    #[test]
    fn an_unresolvable_target_is_classified_as_a_missing_pane() {
        let reason = "terminal session observe failed: terminal target w9:p9 not found";
        assert_eq!(
            classify_shutdown_reason(Some(reason)),
            TerminalClose::PaneNotFound(reason.to_string())
        );
    }

    #[test]
    fn every_other_shutdown_is_an_ordinary_close() {
        for reason in [
            Some("server is shutting down"),
            Some("terminal attach failed: terminal t1 already has an attached client; retry with --takeover"),
            // "not found" alone is not enough: only the target wording counts.
            Some("workspace not found"),
            None,
        ] {
            assert_eq!(
                classify_shutdown_reason(reason),
                TerminalClose::Closed(reason.map(str::to_string)),
                "misclassified: {reason:?}"
            );
        }
    }
}
