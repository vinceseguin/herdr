//! The endpoint generation-1 client handshake, for one fleet host.
//!
//! `crate::client` owns the same exchange, but its version exits the process
//! on failure and is wired into a single-connection shell. The fleet needs a
//! handshake that is *data*: it returns what the server answered so the
//! connector can turn a rejection into one host's [`HostConnection`] and keep
//! every other host running.
//!
//! Nothing here changes the wire: it sends the same `endpoint.hello.v1` a
//! stock client sends and accepts only a generation-1 welcome carrying the
//! four `*_V1` codec names.

use std::io;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;

use crate::ipc::LocalStream;
use crate::protocol::endpoint::{
    EndpointClientHello, EndpointServerWelcome, BLOB_CODEC_V1, ENDPOINT_HELLO_KIND,
    ENDPOINT_PROTOCOL_GENERATION, ENDPOINT_WELCOME_KIND, INPUT_CODEC_V1, SNAPSHOT_CODEC_V1,
    SURFACE_CODEC_V1,
};
use crate::protocol::{
    self, ClientMessage, ClientSurfaceSize, FramingError, ServerMessage, MAX_FRAME_SIZE,
};

/// Welcome deadline for a host on this machine.
///
/// Mirrors `client::handshake::LOCAL_HANDSHAKE_READ_TIMEOUT`: a local server is
/// already up, so five seconds is generous.
pub const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Welcome deadline for a host behind ssh.
///
/// Mirrors `client::handshake::REMOTE_HANDSHAKE_READ_TIMEOUT`: the first frame
/// travels behind a cold ssh connection.
// The ssh transport that returns it from `HostTransport::read_timeout` lands in
// PR 6; the constant belongs next to the local one it is paired with.
#[allow(dead_code)]
pub const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// What the fleet client tells a host about itself.
///
/// `direct_graphics` and `endpoint_keybindings` are always false: the fleet
/// client renders nothing itself and every host uses local keybindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeParams {
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub surface_size: ClientSurfaceSize,
    pub pixel_mouse: bool,
    pub mouse_capture: bool,
    pub read_timeout: Duration,
}

impl HandshakeParams {
    /// A read-only client at `surface_size`, with the local welcome deadline.
    pub fn read_only(surface_size: ClientSurfaceSize) -> Self {
        Self {
            cell_width_px: 0,
            cell_height_px: 0,
            surface_size,
            pixel_mouse: false,
            mouse_capture: false,
            read_timeout: LOCAL_HANDSHAKE_READ_TIMEOUT,
        }
    }
}

/// How one host answered the hello.
///
/// Every variant is a fact about *that* host: the connector turns it into a
/// [`crate::fleet::state::HostEvent`] and never lets it reach another host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// Generation 1 with the four `*_V1` codecs. Boxed because the welcome is
    /// several times the size of the other variants.
    Connected(Box<EndpointServerWelcome>),
    /// The server answered, but cannot speak endpoint generation 1.
    Incompatible {
        generation: Option<u32>,
        reason: String,
    },
    /// The server understood the hello and refused it.
    Rejected { code: String, message: String },
}

/// Run the generation-1 handshake on an open stream.
///
/// `Err` is a transport failure (the socket died, the welcome never came); the
/// caller retries with backoff. Everything the *server* said, including a
/// refusal, comes back as an [`HandshakeOutcome`].
pub fn endpoint_handshake(
    stream: &mut LocalStream,
    params: &HandshakeParams,
) -> io::Result<HandshakeOutcome> {
    stream.set_nonblocking(false)?;

    let hello = EndpointClientHello {
        generation: ENDPOINT_PROTOCOL_GENERATION,
        cell_width_px: params.cell_width_px,
        cell_height_px: params.cell_height_px,
        surface_size: params.surface_size,
        pixel_mouse: params.pixel_mouse,
        // The fleet client draws nothing on the host terminal.
        direct_graphics: false,
        // Every host keeps local keybindings; see the plan's decision list.
        endpoint_keybindings: false,
        mouse_capture: params.mouse_capture,
        snapshot_codecs: vec![SNAPSHOT_CODEC_V1.into()],
        surface_codecs: vec![SURFACE_CODEC_V1.into()],
        input_codecs: vec![INPUT_CODEC_V1.into()],
        blob_codecs: vec![BLOB_CODEC_V1.into()],
    };
    let hello = ClientMessage::EndpointControl {
        kind: ENDPOINT_HELLO_KIND.into(),
        data: serde_json::to_string(&hello)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    };
    protocol::write_message(stream, &hello).map_err(framing_error)?;

    set_recv_timeout(stream, Some(params.read_timeout))?;
    let welcome = protocol::read_message::<_, ServerMessage>(stream, MAX_FRAME_SIZE);
    // Clear the deadline even when the read failed: the caller may keep the
    // stream to report on, and a leftover timeout would break framing later.
    let cleared = set_recv_timeout(stream, None);
    let welcome = welcome.map_err(framing_error)?;
    cleared?;

    Ok(classify_welcome(welcome))
}

/// Turn the server's first message into an outcome.
///
/// Split out so the decision table is testable without a socket.
fn classify_welcome(message: ServerMessage) -> HandshakeOutcome {
    let ServerMessage::EndpointControl { kind, data } = message else {
        return HandshakeOutcome::Incompatible {
            generation: None,
            reason: "server does not speak the stable endpoint protocol; update that host"
                .to_string(),
        };
    };
    if kind != ENDPOINT_WELCOME_KIND {
        return HandshakeOutcome::Incompatible {
            generation: None,
            reason: format!("expected an endpoint welcome, got control {kind}"),
        };
    }
    let welcome: EndpointServerWelcome = match serde_json::from_str(&data) {
        Ok(welcome) => welcome,
        Err(error) => {
            return HandshakeOutcome::Incompatible {
                generation: None,
                reason: format!("invalid endpoint welcome: {error}"),
            }
        }
    };
    if let Some(error) = welcome.error {
        return HandshakeOutcome::Rejected {
            code: error.code,
            message: error.message,
        };
    }
    if welcome.generation != ENDPOINT_PROTOCOL_GENERATION {
        return HandshakeOutcome::Incompatible {
            generation: Some(welcome.generation),
            reason: format!(
                "host speaks endpoint generation {}; this client speaks {ENDPOINT_PROTOCOL_GENERATION}",
                welcome.generation
            ),
        };
    }
    if welcome.snapshot_codec != SNAPSHOT_CODEC_V1
        || welcome.surface_codec != SURFACE_CODEC_V1
        || welcome.input_codec != INPUT_CODEC_V1
        || welcome.blob_codec != BLOB_CODEC_V1
    {
        return HandshakeOutcome::Incompatible {
            generation: Some(welcome.generation),
            reason: format!(
                "host has no compatible endpoint core (snapshot {}, surface {}, input {}, blob {})",
                welcome.snapshot_codec,
                welcome.surface_codec,
                welcome.input_codec,
                welcome.blob_codec
            ),
        };
    }
    HandshakeOutcome::Connected(Box::new(welcome))
}

/// A framing failure as an `io::Error`, keeping the original kind when there
/// is one so the connector can describe the reason accurately.
pub(crate) fn framing_error(error: FramingError) -> io::Error {
    match error {
        FramingError::Io(error) => error,
        FramingError::UnexpectedEof => {
            io::Error::new(io::ErrorKind::UnexpectedEof, "host closed the connection")
        }
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// Set (or clear) the receive deadline, tolerating platforms without one.
///
/// Windows named pipes report `Unsupported`; the client does the same thing
/// there rather than refusing to connect.
fn set_recv_timeout(stream: &LocalStream, timeout: Option<Duration>) -> io::Result<()> {
    match stream.set_recv_timeout(timeout) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            tracing::debug!(error = %error, "host socket receive timeout unavailable");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::endpoint::{EndpointHandshakeError, ENDPOINT_SNAPSHOT_KIND};
    use crate::protocol::{RenderEncoding, ServerMessage};

    fn welcome(welcome: EndpointServerWelcome) -> ServerMessage {
        ServerMessage::EndpointControl {
            kind: ENDPOINT_WELCOME_KIND.to_string(),
            data: serde_json::to_string(&welcome).expect("welcome encodes"),
        }
    }

    fn compatible() -> EndpointServerWelcome {
        EndpointServerWelcome::compatible(vec!["pane.write".to_string()])
    }

    #[test]
    fn a_generation_one_welcome_connects() {
        let outcome = classify_welcome(welcome(compatible()));
        let HandshakeOutcome::Connected(welcome) = outcome else {
            panic!("expected a connected outcome, got {outcome:?}");
        };
        assert_eq!(welcome.generation, ENDPOINT_PROTOCOL_GENERATION);
        assert_eq!(welcome.methods, vec!["pane.write".to_string()]);
    }

    #[test]
    fn a_newer_generation_is_incompatible_and_carries_its_number() {
        let mut newer = compatible();
        newer.generation = 2;
        let outcome = classify_welcome(welcome(newer));
        let HandshakeOutcome::Incompatible { generation, reason } = outcome else {
            panic!("expected an incompatible outcome, got {outcome:?}");
        };
        assert_eq!(generation, Some(2));
        assert!(
            reason.contains("generation 2"),
            "unexpected reason: {reason}"
        );
    }

    #[test]
    fn one_wrong_codec_name_is_incompatible() {
        for mutate in [
            (|welcome: &mut EndpointServerWelcome| {
                welcome.snapshot_codec = "shell.snapshot.v2".to_string()
            }) as fn(&mut EndpointServerWelcome),
            |welcome| welcome.surface_codec = "shell.surface.v2".to_string(),
            |welcome| welcome.input_codec = "shell.input.semantic.v2".to_string(),
            |welcome| welcome.blob_codec = "shell.blob.v2".to_string(),
        ] {
            let mut wrong = compatible();
            mutate(&mut wrong);
            let outcome = classify_welcome(welcome(wrong));
            let HandshakeOutcome::Incompatible { generation, reason } = outcome else {
                panic!("expected an incompatible outcome, got {outcome:?}");
            };
            assert_eq!(generation, Some(ENDPOINT_PROTOCOL_GENERATION));
            assert!(
                reason.contains("endpoint core"),
                "unexpected reason: {reason}"
            );
        }
    }

    #[test]
    fn a_welcome_error_is_a_rejection() {
        let mut refused = compatible();
        refused.error = Some(EndpointHandshakeError {
            code: "busy".to_string(),
            message: "another client owns this shell".to_string(),
        });
        let outcome = classify_welcome(welcome(refused));
        let HandshakeOutcome::Rejected { code, message } = outcome else {
            panic!("expected a rejection, got {outcome:?}");
        };
        assert_eq!(code, "busy");
        assert_eq!(message, "another client owns this shell");
    }

    #[test]
    fn anything_but_an_endpoint_welcome_is_incompatible() {
        for message in [
            ServerMessage::Welcome {
                version: 22,
                encoding: RenderEncoding::SemanticFrame,
                error: None,
            },
            ServerMessage::EndpointControl {
                kind: ENDPOINT_SNAPSHOT_KIND.to_string(),
                data: "{}".to_string(),
            },
            ServerMessage::EndpointControl {
                kind: ENDPOINT_WELCOME_KIND.to_string(),
                data: "not json".to_string(),
            },
        ] {
            let outcome = classify_welcome(message);
            assert!(
                matches!(outcome, HandshakeOutcome::Incompatible { .. }),
                "expected an incompatible outcome, got {outcome:?}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod socket_tests {
    use super::*;
    use crate::ipc::{bind_local_listener, connect_local_stream};
    use crate::protocol::endpoint::{EndpointClientHello, EndpointServerWelcome};
    use crate::protocol::{ClientMessage, ServerMessage};
    use interprocess::local_socket::traits::Listener as _;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn scratch_socket(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "herdr-handshake-{name}-{}-{nanos}.sock",
            std::process::id()
        ))
    }

    #[test]
    fn the_hello_carries_generation_one_and_the_four_codecs() {
        let socket = scratch_socket("hello");
        let listener = bind_local_listener(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept");
            let hello = protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE)
                .expect("hello");
            let welcome = ServerMessage::EndpointControl {
                kind: ENDPOINT_WELCOME_KIND.to_string(),
                data: serde_json::to_string(&EndpointServerWelcome::compatible(vec![
                    "pane.write".to_string()
                ]))
                .expect("welcome encodes"),
            };
            protocol::write_message(&mut stream, &welcome).expect("welcome");
            hello
        });

        let mut stream = connect_local_stream(&socket).expect("connect");
        let params = HandshakeParams::read_only(ClientSurfaceSize { cols: 20, rows: 5 });
        let outcome = endpoint_handshake(&mut stream, &params).expect("handshake");
        assert!(matches!(outcome, HandshakeOutcome::Connected(_)));

        let hello = server.join().expect("server thread");
        let ClientMessage::EndpointControl { kind, data } = hello else {
            panic!("expected an endpoint hello, got {hello:?}");
        };
        assert_eq!(kind, ENDPOINT_HELLO_KIND);
        let hello: EndpointClientHello = serde_json::from_str(&data).expect("hello decodes");
        assert_eq!(hello.generation, ENDPOINT_PROTOCOL_GENERATION);
        assert_eq!(hello.surface_size, ClientSurfaceSize { cols: 20, rows: 5 });
        assert!(!hello.direct_graphics);
        assert!(!hello.endpoint_keybindings);
        assert_eq!(hello.snapshot_codecs, vec![SNAPSHOT_CODEC_V1.to_string()]);
        assert_eq!(hello.surface_codecs, vec![SURFACE_CODEC_V1.to_string()]);
        assert_eq!(hello.input_codecs, vec![INPUT_CODEC_V1.to_string()]);
        assert_eq!(hello.blob_codecs, vec![BLOB_CODEC_V1.to_string()]);
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn a_host_that_never_answers_times_out() {
        let socket = scratch_socket("silent");
        let listener = bind_local_listener(&socket).expect("bind");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let _held = listener.accept().expect("accept");
            while !server_stop.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let mut stream = connect_local_stream(&socket).expect("connect");
        let mut params = HandshakeParams::read_only(ClientSurfaceSize { cols: 20, rows: 5 });
        params.read_timeout = Duration::from_millis(200);
        let error = endpoint_handshake(&mut stream, &params).expect_err("no welcome arrives");
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error: {error} ({:?})",
            error.kind()
        );

        stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = server.join();
        let _ = std::fs::remove_file(&socket);
    }
}
