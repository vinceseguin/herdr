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
/// travels behind a cold ssh connection. Returned by
/// `transport::ssh::SshTransport::read_timeout`.
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
    /// Whether this client wants to be a host's *foreground* surface.
    ///
    /// Upstream #3670 added `EndpointClientHello.surface_active`. `false` tells
    /// a host "read me out, but I am not looking at you": the server never
    /// makes this endpoint the foreground client, so the host's pane geometry
    /// is left exactly as it was. Snapshots still flow. See
    /// [`crate::fleet::connector::INACTIVE_SURFACE`] for what a `true` client
    /// costs the host.
    pub surface_active: bool,
    pub read_timeout: Duration,
}

impl HandshakeParams {
    /// A read-only client at `surface_size`, with the local welcome deadline.
    ///
    /// Passive: `surface_active` is `false`, so a host running #3670 or later
    /// keeps its own geometry while this client is connected. `surface_size`
    /// is still announced, because a server that predates the field reads it.
    pub fn read_only(surface_size: ClientSurfaceSize) -> Self {
        Self {
            cell_width_px: 0,
            cell_height_px: 0,
            surface_size,
            pixel_mouse: false,
            mouse_capture: false,
            surface_active: false,
            read_timeout: LOCAL_HANDSHAKE_READ_TIMEOUT,
        }
    }

    /// A full-screen console's hello for every host.
    ///
    /// The cell geometry and mouse policy are the console terminal's real
    /// ones, because whichever host becomes active renders into that
    /// terminal. The size is deliberately the *inactive* one
    /// ([`crate::fleet::connector::INACTIVE_SURFACE`]): a host is handshaken
    /// small and resized up only when it is activated, so N-1 hosts never
    /// reflow their panes for a console that is not looking at them. The
    /// connector overrides the size (and the cell geometry) for whichever host
    /// is active when it handshakes.
    ///
    /// `read_timeout` is the local deadline; `transport.read_timeout()`
    /// replaces it for an ssh host.
    // Called by the fleet console (E2 PR 4), which owns the terminal these
    // values describe; this PR only ships the constructor it will call.
    #[allow(dead_code)]
    pub fn for_client(
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
        mouse_capture: bool,
    ) -> Self {
        Self {
            cell_width_px,
            cell_height_px,
            surface_size: crate::fleet::connector::INACTIVE_SURFACE,
            pixel_mouse,
            mouse_capture,
            // A console *is* looking at whichever host is active, and the
            // connector activates one; a foreground client is what makes the
            // active host render at the console's geometry.
            //
            // One hello serves every host, so a console's *inactive* hosts
            // announce `true` as well. They are protected by the value, not
            // the flag: they handshake at
            // [`crate::fleet::connector::INACTIVE_SURFACE`], which is the
            // geometry a headless host already uses.
            surface_active: true,
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
        // Upstream #3670: `false` lets a server drop this endpoint from surface
        // interest, which is what makes a read-only consumer passive. A server
        // that predates the field ignores it (the hello has no
        // `deny_unknown_fields`) and treats every client as active.
        surface_active: params.surface_active,
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

    /// The passivity switch, at the only two places it is decided.
    ///
    /// A read-only consumer (`herdr fleet status`, the gateway) must never
    /// become a host's foreground client; a console must, because whichever
    /// host is active renders into its terminal.
    #[test]
    fn only_a_console_hello_asks_to_be_the_foreground_client() {
        let read_only = HandshakeParams::read_only(ClientSurfaceSize { cols: 20, rows: 5 });
        assert!(!read_only.surface_active);
        let console = HandshakeParams::for_client(9, 19, false, false);
        assert!(console.surface_active);
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
        assert!(
            !hello.surface_active,
            "a read-only hello must be passive on the wire, not only in the params"
        );
        // The field is what a #3670 server reads; assert the JSON too, because
        // a rename on either side would still pass the typed check above.
        assert!(
            data.contains("\"surface_active\":false"),
            "the hello JSON must carry surface_active=false: {data}"
        );
        assert!(!hello.direct_graphics);
        assert!(!hello.endpoint_keybindings);
        assert_eq!(hello.snapshot_codecs, vec![SNAPSHOT_CODEC_V1.to_string()]);
        assert_eq!(hello.surface_codecs, vec![SURFACE_CODEC_V1.to_string()]);
        assert_eq!(hello.input_codecs, vec![INPUT_CODEC_V1.to_string()]);
        assert_eq!(hello.blob_codecs, vec![BLOB_CODEC_V1.to_string()]);
        let _ = std::fs::remove_file(&socket);
    }

    /// The console's hello: the terminal's real cell geometry and mouse
    /// policy, but the *inactive* surface size, and never direct graphics or
    /// endpoint keybindings.
    #[test]
    fn the_client_hello_carries_the_terminal_geometry_at_the_inactive_size() {
        let socket = scratch_socket("client-hello");
        let listener = bind_local_listener(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept");
            let hello = protocol::read_message::<_, ClientMessage>(&mut stream, MAX_FRAME_SIZE)
                .expect("hello");
            let welcome = ServerMessage::EndpointControl {
                kind: ENDPOINT_WELCOME_KIND.to_string(),
                data: serde_json::to_string(&EndpointServerWelcome::compatible(Vec::new()))
                    .expect("welcome encodes"),
            };
            protocol::write_message(&mut stream, &welcome).expect("welcome");
            hello
        });

        let mut stream = connect_local_stream(&socket).expect("connect");
        let params = HandshakeParams::for_client(9, 19, true, true);
        assert_eq!(params.read_timeout, LOCAL_HANDSHAKE_READ_TIMEOUT);
        let outcome = endpoint_handshake(&mut stream, &params).expect("handshake");
        assert!(matches!(outcome, HandshakeOutcome::Connected(_)));

        let hello = server.join().expect("server thread");
        let ClientMessage::EndpointControl { data, .. } = hello else {
            panic!("expected an endpoint hello, got {hello:?}");
        };
        let hello: EndpointClientHello = serde_json::from_str(&data).expect("hello decodes");
        assert_eq!(hello.cell_width_px, 9);
        assert_eq!(hello.cell_height_px, 19);
        assert!(hello.pixel_mouse);
        assert!(hello.mouse_capture);
        assert!(
            hello.surface_active,
            "a console is the foreground client of whichever host is active"
        );
        assert_eq!(
            hello.surface_size,
            crate::fleet::connector::INACTIVE_SURFACE,
            "a console handshakes every host at the inactive size"
        );
        assert!(!hello.direct_graphics, "fleet v1 draws no direct graphics");
        assert!(
            !hello.endpoint_keybindings,
            "every fleet host uses local keybindings"
        );
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
