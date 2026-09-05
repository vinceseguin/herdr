//! Where a client write goes (fork).
//!
//! The client event loop has exactly one write path. In the single-host client
//! that path is the server socket it handshook on; in the Fleet console
//! (E2 PR 4) it is one explicit host of a [`FleetConnector`]. Both live behind
//! [`ServerLink`], so every one of the loop's `write_to_server` call sites
//! keeps its shape and no caller can invent a second, unrouted write.
//!
//! The active host is never implicit: [`FleetLink`] holds the one [`HostId`]
//! the loop set, and [`FleetLink::set_active`] is the only way it changes. A
//! write that lands on the wrong machine is the failure this module exists to
//! make impossible.

use std::io;
use std::rc::Rc;

use tracing::debug;

use crate::fleet::connector::{FleetConnector, FleetEvent, HostCommand, HostSendError};
use crate::fleet::hosts::HostId;
use crate::ipc::LocalStream;
use crate::protocol::{self, ClientMessage};

/// Why a write did not reach a server.
#[derive(Debug)]
pub(super) enum LinkWriteError {
    /// The write failed and the connection is presumed lost.
    Io(io::Error),
    /// A console detach: there is no server to tell, the loop is the thing
    /// that stops. Only [`ServerLink::Fleet`] produces it.
    // Produced by the fleet link, whose only constructor lands in E2 PR 4.
    #[allow(dead_code)]
    Detached,
}

/// The client loop's write half: one local server, or one host of a fleet.
pub(super) enum ServerLink {
    Single(LocalStream),
    /// Built by the Fleet console (E2 PR 4); unreachable until then.
    #[allow(dead_code)]
    Fleet(FleetLink),
}

/// One fleet host, chosen explicitly.
///
/// The connector is shared (`send` takes `&self`) because the loop owns the
/// event receiver separately, through `FleetConnector::take_events`. `Rc`, not
/// `Arc`: a `FleetConnector` is not `Sync` (it owns the supervisors' completion
/// receiver), and the console shares it only between the link and the loop's
/// own fleet state, on the one thread the client loop runs on.
pub(super) struct FleetLink {
    connector: Rc<FleetConnector>,
    active: HostId,
}

impl FleetLink {
    // Constructed by the fleet console (E2 PR 4).
    #[allow(dead_code)]
    pub(super) fn new(connector: Rc<FleetConnector>, active: HostId) -> Self {
        Self { connector, active }
    }

    /// The host every write currently goes to.
    // Read by the fleet console (E2 PR 4/5) when it switches hosts.
    #[allow(dead_code)]
    pub(super) fn active(&self) -> &HostId {
        &self.active
    }

    fn send(&self, command: HostCommand) -> Result<(), LinkWriteError> {
        match self.connector.send(&self.active, command) {
            Ok(()) => Ok(()),
            // Host-local and transient: the supervisor reconnects, and the
            // console keeps running. E2 PR 8 makes it visible in the pane area.
            Err(error @ (HostSendError::NotConnected | HostSendError::Io(_))) => {
                debug!(host = %self.active, %error, "dropping a write for a fleet host that is not connected");
                Ok(())
            }
            // A bug in this client, not a host fact: surfaced.
            Err(error @ (HostSendError::UnknownHost | HostSendError::Refused(_))) => {
                Err(LinkWriteError::Io(io::Error::other(error.to_string())))
            }
        }
    }
}

impl ServerLink {
    /// Writes one client message to whichever server this link addresses.
    pub(super) fn write(&mut self, msg: &ClientMessage) -> Result<(), LinkWriteError> {
        match self {
            Self::Single(stream) => write_stream_message(stream, msg).map_err(LinkWriteError::Io),
            Self::Fleet(link) => match msg {
                // The console owns detaching: telling one host would leave the
                // other N connections to the connector's own shutdown anyway.
                ClientMessage::Detach => Err(LinkWriteError::Detached),
                other => link.send(HostCommand::Raw(Box::new(other.clone()))),
            },
        }
    }

    /// Writes one endpoint request, correlated the way this link needs.
    ///
    /// A single-host client carries the boot id on the wire; a fleet host's
    /// lane is owned by the connector, which correlates the answer by
    /// `request_id` and replays it as one reassembled response.
    pub(super) fn write_endpoint_request(
        &mut self,
        boot_id: &str,
        request_id: &str,
        request: String,
    ) -> Result<(), LinkWriteError> {
        match self {
            Self::Single(stream) => write_stream_message(
                stream,
                &ClientMessage::ClientShellEndpointRequest {
                    boot_id: boot_id.to_string(),
                    request,
                },
            )
            .map_err(LinkWriteError::Io),
            Self::Fleet(link) => link.send(HostCommand::Endpoint {
                request_id: request_id.to_string(),
                request,
            }),
        }
    }

    /// Points a fleet link at another host. A no-op for a single-host client,
    /// which has exactly one server for its whole life.
    // Called by the fleet console's host switch (E2 PR 4/5/6).
    #[allow(dead_code)]
    pub(super) fn set_active(&mut self, host: HostId) {
        match self {
            Self::Single(_) => debug!(%host, "ignoring a host switch on a single-server link"),
            Self::Fleet(link) => link.active = host,
        }
    }
}

/// Everything the client loop needs on its server side.
///
/// One parameter rather than two so the loop's signature says what it is: the
/// write half, plus — for a fleet console — the merged inbound event stream the
/// connector owns.
pub(super) struct ClientLink {
    pub(super) link: ServerLink,
    /// `None` for a single-host client, whose inbound messages come from its
    /// own reader thread.
    pub(super) fleet_events: Option<tokio::sync::mpsc::Receiver<FleetEvent>>,
}

impl ClientLink {
    /// The single-host client: one socket, no fleet events.
    pub(super) fn single(stream: LocalStream) -> Self {
        Self {
            link: ServerLink::Single(stream),
            fleet_events: None,
        }
    }
}

/// Writes one message to a raw server stream (blocking).
///
/// The single-host client's whole write path, kept as its own function because
/// `herdr terminal session observe|control` writes to a stream it owns rather
/// than through the event loop's link.
pub(super) fn write_stream_message(
    stream: &mut LocalStream,
    msg: &ClientMessage,
) -> io::Result<()> {
    protocol::write_message(stream, msg).map_err(|e| io::Error::other(e.to_string()))
}

/// The loop's io-shaped view of a link write.
///
/// A console detach is a local stop, not a connection failure: the call sites
/// that write `Detach` return from the loop immediately afterwards.
pub(super) fn io_result(result: Result<(), LinkWriteError>) -> io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(LinkWriteError::Io(error)) => Err(error),
        Err(LinkWriteError::Detached) => {
            debug!("fleet console detach: no server write, the console stops");
            Ok(())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use std::time::Duration;

    use interprocess::local_socket::traits::Stream as _;

    use crate::fleet::connector::test_support::{
        connected_with_snapshot, drain_until, fake_connector, scratch_dir, snapshot,
        snapshot_message, Behaviour, FakeHost,
    };
    use crate::fleet::connector::FleetConnectorOptions;
    use crate::fleet::state::FleetState;
    use crate::protocol::ClientPaneInputEvent;

    fn host_id(id: &str) -> HostId {
        HostId::new(id).expect("valid host id")
    }

    fn pane_input(pane: &str) -> ClientMessage {
        ClientMessage::ClientShellPaneInput {
            pane_id: pane.to_string(),
            events: vec![ClientPaneInputEvent::TextCommit("x".to_string())],
        }
    }

    fn pane_inputs(messages: &[ClientMessage]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                ClientMessage::ClientShellPaneInput { pane_id, .. } => Some(pane_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Two connected fake hosts and a connector that has seen both snapshots.
    fn two_hosts(name: &str) -> (FakeHost, FakeHost, FleetConnector) {
        let dir = scratch_dir(name);
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-alpha", 1))]),
        );
        let beta = FakeHost::start(
            &dir,
            "beta",
            Behaviour::Serve(vec![snapshot_message(&snapshot("boot-beta", 1))]),
        );
        let mut connector = fake_connector(
            &[("alpha", &alpha), ("beta", &beta)],
            FleetConnectorOptions::default(),
        );
        let specs = vec![alpha.spec("alpha"), beta.spec("beta")];
        let mut state = FleetState::new(specs);
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| {
                connected_with_snapshot(state, "alpha") && connected_with_snapshot(state, "beta")
            },
        );
        assert!(connected_with_snapshot(&state, "alpha"), "alpha connected");
        assert!(connected_with_snapshot(&state, "beta"), "beta connected");
        (alpha, beta, connector)
    }

    #[test]
    fn a_single_link_writes_exactly_the_wire_bytes() {
        let dir = scratch_dir("link-single");
        let host = FakeHost::start(&dir, "solo", Behaviour::Serve(Vec::new()));
        let stream =
            crate::ipc::connect_local_stream(&host.socket).expect("connect to the fake host");
        stream.set_nonblocking(false).expect("blocking stream");
        let mut link = ServerLink::Single(stream);

        link.write(&pane_input("w1:p1")).expect("write");

        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(5), || {
                pane_inputs(&host.received()) == vec!["w1:p1".to_string()]
            }),
            "the fake host read exactly the message that was written: {:?}",
            host.received()
        );
    }

    #[test]
    fn a_fleet_write_reaches_only_the_active_host() {
        let (alpha, beta, connector) = two_hosts("link-active");
        let mut link = ServerLink::Fleet(FleetLink::new(Rc::new(connector), host_id("alpha")));

        link.write(&pane_input("w1:p1")).expect("write to alpha");

        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(5), || {
                pane_inputs(&alpha.received()) == vec!["w1:p1".to_string()]
            }),
            "alpha received the input: {:?}",
            alpha.received()
        );
        assert!(
            pane_inputs(&beta.received()).is_empty(),
            "beta received no input: {:?}",
            beta.received()
        );
    }

    #[test]
    fn set_active_redirects_the_next_write() {
        let (alpha, beta, connector) = two_hosts("link-switch");
        let mut link = ServerLink::Fleet(FleetLink::new(Rc::new(connector), host_id("alpha")));

        link.write(&pane_input("w1:p1")).expect("write to alpha");
        link.set_active(host_id("beta"));
        link.write(&pane_input("w2:p2")).expect("write to beta");

        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(5), || {
                pane_inputs(&beta.received()) == vec!["w2:p2".to_string()]
            }),
            "beta received only the second input: {:?}",
            beta.received()
        );
        assert_eq!(
            pane_inputs(&alpha.received()),
            vec!["w1:p1".to_string()],
            "alpha kept only the first input"
        );
    }

    #[test]
    fn a_fleet_link_refuses_detach_instead_of_telling_one_host() {
        let (alpha, _beta, connector) = two_hosts("link-detach");
        let mut link = ServerLink::Fleet(FleetLink::new(Rc::new(connector), host_id("alpha")));

        let error = link.write(&ClientMessage::Detach);

        assert!(
            matches!(error, Err(LinkWriteError::Detached)),
            "detach is a console-local stop: {error:?}"
        );
        assert!(
            io_result(link.write(&ClientMessage::Detach)).is_ok(),
            "the loop reads a detach as a clean stop"
        );
        assert!(
            !alpha
                .received()
                .iter()
                .any(|message| matches!(message, ClientMessage::Detach)),
            "no host was told to detach: {:?}",
            alpha.received()
        );
    }

    #[test]
    fn a_write_to_a_disconnected_host_is_dropped_not_fatal() {
        let dir = scratch_dir("link-down");
        // Accepts the connection and never answers the hello: connected at the
        // transport level, never usable, so `send` reports NotConnected.
        let silent = FakeHost::start(&dir, "silent", Behaviour::Silent);
        let connector = fake_connector(&[("silent", &silent)], FleetConnectorOptions::default());
        let mut link = ServerLink::Fleet(FleetLink::new(Rc::new(connector), host_id("silent")));

        assert!(
            link.write(&pane_input("w1:p1")).is_ok(),
            "a host that is down is host-local, not a console failure"
        );
    }

    #[test]
    fn an_unknown_host_is_a_surfaced_bug() {
        let dir = scratch_dir("link-unknown");
        let host = FakeHost::start(&dir, "alpha", Behaviour::Serve(Vec::new()));
        let connector = fake_connector(&[("alpha", &host)], FleetConnectorOptions::default());
        let mut link = ServerLink::Fleet(FleetLink::new(Rc::new(connector), host_id("ghost")));

        let error = link.write(&pane_input("w1:p1"));

        assert!(
            matches!(error, Err(LinkWriteError::Io(_))),
            "an unrouted write must not be swallowed: {error:?}"
        );
    }

    #[test]
    fn a_fleet_endpoint_request_goes_through_the_connector_lane() {
        let dir = scratch_dir("link-endpoint");
        let alpha = FakeHost::start(
            &dir,
            "alpha",
            Behaviour::Answer {
                messages: vec![snapshot_message(&snapshot("boot-alpha", 1))],
                reply_boot: None,
            },
        );
        let mut connector = fake_connector(&[("alpha", &alpha)], FleetConnectorOptions::default());
        let mut state = FleetState::new(vec![alpha.spec("alpha")]);
        drain_until(
            &mut connector,
            &mut state,
            Duration::from_secs(10),
            |state| connected_with_snapshot(state, "alpha"),
        );
        let mut link = ServerLink::Fleet(FleetLink::new(Rc::new(connector), host_id("alpha")));

        link.write_endpoint_request(
            "boot-alpha",
            "request-1",
            "{\"id\":\"request-1\",\"method\":\"session.snapshot\"}".to_string(),
        )
        .expect("endpoint request accepted");

        assert!(
            crate::fleet::connector::test_support::wait_for(Duration::from_secs(5), || {
                alpha.received().iter().any(|message| {
                    matches!(
                        message,
                        ClientMessage::ClientShellEndpointRequest { request, .. }
                            if request.contains("request-1")
                    )
                })
            }),
            "the request reached the host through the connector's lane: {:?}",
            alpha.received()
        );
    }

    #[test]
    fn a_single_link_ignores_a_host_switch() {
        let dir = scratch_dir("link-single-switch");
        let host = FakeHost::start(&dir, "solo", Behaviour::Serve(Vec::new()));
        let stream =
            crate::ipc::connect_local_stream(&host.socket).expect("connect to the fake host");
        let mut link = ServerLink::Single(stream);

        link.set_active(host_id("elsewhere"));

        assert!(
            matches!(link, ServerLink::Single(_)),
            "a single-server link has exactly one server for its whole life"
        );
    }
}
