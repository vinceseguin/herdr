//! A herdr server on this machine, reached through its session socket.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;

use super::HostTransport;
use crate::fleet::handshake::LOCAL_HANDSHAKE_READ_TIMEOUT;
use crate::ipc::{connect_local_stream, LocalStream};

/// The client socket of one local session.
///
/// The path is resolved once, at construction: `[fleet]` is read in the same
/// process, so a host's socket cannot move under it, and re-deriving it on
/// every reconnect would mean re-reading the environment from a thread.
#[derive(Debug, Clone)]
pub struct LocalTransport {
    session: Option<String>,
    socket: PathBuf,
}

impl LocalTransport {
    /// `None` is this machine's default session, `Some(name)` a named one.
    pub fn new(session: Option<String>) -> Self {
        let socket = crate::session::client_socket_path_for(session.as_deref());
        Self { session, socket }
    }

    /// A transport pointing at an explicit socket, for tests that stand up a
    /// fake endpoint outside the session directory.
    #[cfg(test)]
    pub fn with_socket(socket: std::path::PathBuf) -> Self {
        Self {
            session: None,
            socket,
        }
    }

    /// The socket this transport connects to.
    #[cfg(test)]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket
    }

    fn session_label(&self) -> &str {
        self.session.as_deref().unwrap_or("default")
    }
}

impl HostTransport for LocalTransport {
    fn connect(&mut self) -> io::Result<LocalStream> {
        match connect_local_stream(&self.socket) {
            Ok(stream) => {
                // The supervisor reads with blocking `read_message`; a stream
                // left non-blocking would spin on `WouldBlock`.
                stream.set_nonblocking(false)?;
                Ok(stream)
            }
            // A stopped server is the common case, not an anomaly: report it
            // as the operator-facing reason rather than a raw errno.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Err(io::Error::new(
                    error.kind(),
                    format!(
                        "no herdr server for session {} at {}",
                        self.session_label(),
                        self.socket.display()
                    ),
                ))
            }
            Err(error) => Err(error),
        }
    }

    fn read_timeout(&self) -> Duration {
        LOCAL_HANDSHAKE_READ_TIMEOUT
    }

    fn describe(&self) -> String {
        format!("local session {}", self.session_label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_session_uses_the_config_dir_socket() {
        let transport = LocalTransport::new(None);
        assert_eq!(
            transport.socket_path(),
            crate::config::config_dir().join("herdr-client.sock")
        );
        assert_eq!(transport.describe(), "local session default");
    }

    #[test]
    fn a_named_session_uses_its_session_socket() {
        let transport = LocalTransport::new(Some("alpha".to_string()));
        assert_eq!(
            transport.socket_path(),
            crate::config::config_dir()
                .join("sessions")
                .join("alpha")
                .join("herdr-client.sock")
        );
        assert_eq!(transport.describe(), "local session alpha");
    }

    #[test]
    fn a_missing_server_reports_the_session_and_the_path() {
        let mut transport = LocalTransport::new(Some("herdr-fleet-missing".to_string()));
        let error = transport.connect().expect_err("no server is listening");
        let message = error.to_string();
        assert!(
            message.contains("no herdr server for session herdr-fleet-missing"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("herdr-client.sock"),
            "unexpected error: {message}"
        );
    }
}
