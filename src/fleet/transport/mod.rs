//! How the connector reaches one host.
//!
//! A transport is the smallest thing a supervisor thread needs: something that
//! hands back an open, blocking [`LocalStream`] carrying the endpoint protocol.
//! Local hosts connect to a session socket; ssh hosts connect to a private
//! forward socket fed by the shared stdio bridge. Everything above this
//! trait — handshake, snapshots, frames, reconnect — is identical for both.

use std::io;
use std::time::Duration;

use crate::fleet::hosts::{HostKind, HostSpec};
use crate::ipc::LocalStream;

pub mod local;
pub mod ssh;

pub use local::LocalTransport;
pub use ssh::SshTransport;

/// One host's connection factory.
///
/// `connect` is called on that host's supervisor thread only, and may block.
/// It must never panic and must never affect another host.
pub trait HostTransport: Send {
    /// Open a fresh connection. Called again for every reconnect.
    fn connect(&mut self) -> io::Result<LocalStream>;
    /// How long to wait for the endpoint welcome on this transport.
    fn read_timeout(&self) -> Duration;
    /// Operator-facing description, used in log fields and failure reasons.
    fn describe(&self) -> String;
    /// The ssh policy this transport was built with, or `None` for a transport
    /// that never runs ssh.
    ///
    /// Test-only: [`transport_for`] is the single place the daemon switch
    /// reaches an ssh host, and a dropped argument there would hand a gateway
    /// an interactive ssh child with no other symptom.
    #[cfg(test)]
    fn ssh_noninteractive_for_test(&self) -> Option<bool> {
        None
    }
}

/// Build the transport for one host spec.
///
/// `Err` is a host-local reason string the supervisor reports **once** before
/// it stops: it means no transport for this host exists in this build, which a
/// retry cannot change. Everything that a retry *could* fix — an unreachable
/// host, a missing remote herdr, a dead bridge — is an `Err` from `connect`
/// instead, so the host keeps its backoff and reconnects.
pub fn transport_for(
    spec: &HostSpec,
    options: &crate::fleet::connector::FleetConnectorOptions,
) -> Result<Box<dyn HostTransport>, String> {
    transport_for_scoped(spec, options, CONNECTOR_TRANSPORT_SCOPE)
}

/// The connector's socket scope: the host id alone, exactly as E1 named it.
pub const CONNECTOR_TRANSPORT_SCOPE: &str = "";

/// [`transport_for`], for a consumer that needs its own connection to a host
/// the connector already holds.
///
/// An ssh host's bridge binds one forward socket per (pid, scope, target,
/// session), so two transports built with the same scope in one process
/// collide with `AddrInUse`. `scope` is how a second consumer — the gateway's
/// terminal streams — gets a socket of its own; [`CONNECTOR_TRANSPORT_SCOPE`]
/// keeps the connector's names unchanged. A local host has no forward socket,
/// so the scope does not reach it.
pub fn transport_for_scoped(
    spec: &HostSpec,
    options: &crate::fleet::connector::FleetConnectorOptions,
    scope: &str,
) -> Result<Box<dyn HostTransport>, String> {
    match &spec.kind {
        HostKind::Local { session } => Ok(Box::new(LocalTransport::new(session.clone()))),
        HostKind::Ssh { target, session } => Ok(Box::new(SshTransport::new_scoped(
            spec.id.clone(),
            target.clone(),
            session.clone(),
            options.manage_ssh_config,
            // Carried exactly as `transport_for` carries it: a dropped
            // argument here hands a daemon an interactive ssh child with no
            // other symptom, which is what the wiring test below pins.
            options.ssh_noninteractive,
            scope,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::connector::FleetConnectorOptions;
    use crate::fleet::hosts::HostId;

    fn spec(kind: HostKind) -> HostSpec {
        HostSpec {
            id: HostId::new("host").expect("valid host id"),
            kind,
            enabled: true,
        }
    }

    #[test]
    fn a_local_transport_runs_no_ssh() {
        let options = FleetConnectorOptions::default();
        let transport = transport_for(&spec(HostKind::Local { session: None }), &options)
            .expect("local transport");
        assert_eq!(transport.ssh_noninteractive_for_test(), None);
    }

    #[test]
    fn local_hosts_get_a_local_transport() {
        let options = FleetConnectorOptions::default();
        let transport = transport_for(&spec(HostKind::Local { session: None }), &options)
            .expect("local transport");
        assert!(
            transport.describe().starts_with("local "),
            "unexpected description: {}",
            transport.describe()
        );
    }

    /// `transport_for` is the only place the daemon switch reaches an ssh
    /// transport, so a dropped field here would silently give a gateway an
    /// interactive ssh child.
    #[test]
    fn the_daemon_ssh_policy_reaches_the_transport() {
        let spec = spec(HostKind::Ssh {
            target: "workbox".to_string(),
            session: None,
        });
        for noninteractive in [false, true] {
            let options = FleetConnectorOptions {
                ssh_noninteractive: noninteractive,
                ..FleetConnectorOptions::default()
            };
            let transport = transport_for(&spec, &options).expect("ssh transport");
            assert_eq!(
                transport.ssh_noninteractive_for_test(),
                Some(noninteractive),
                "the daemon ssh policy did not reach the transport"
            );
            let scoped =
                transport_for_scoped(&spec, &options, "gateway").expect("scoped ssh transport");
            assert_eq!(
                scoped.ssh_noninteractive_for_test(),
                Some(noninteractive),
                "the daemon ssh policy did not reach the scoped transport"
            );
        }
    }

    /// A scoped transport must not answer on the socket the connector binds,
    /// or the second one to start loses to `AddrInUse` and that host goes dark.
    #[cfg(unix)]
    #[test]
    fn a_scoped_ssh_transport_binds_a_socket_of_its_own() {
        use crate::remote::local_forward_socket_path_scoped;

        let options = FleetConnectorOptions::default();
        let spec = spec(HostKind::Ssh {
            target: "workbox".to_string(),
            session: Some("agents".to_string()),
        });

        let connector = SshTransport::new_scoped(
            spec.id.clone(),
            "workbox".to_string(),
            Some("agents".to_string()),
            false,
            options.ssh_noninteractive,
            CONNECTOR_TRANSPORT_SCOPE,
        );
        let gateway = SshTransport::new_scoped(
            spec.id.clone(),
            "workbox".to_string(),
            Some("agents".to_string()),
            false,
            options.ssh_noninteractive,
            "gateway",
        );

        assert_ne!(connector.local_socket(), gateway.local_socket());
        // E1's names are unchanged: the connector's socket is still the one
        // `SshTransport::new` derived from the host id alone.
        assert_eq!(
            connector.local_socket(),
            local_forward_socket_path_scoped("host", "workbox", "agents")
        );
        // And neither is `herdr --remote`'s unscoped name.
        let unscoped = local_forward_socket_path_scoped("", "workbox", "agents");
        assert_ne!(connector.local_socket(), unscoped);
        assert_ne!(gateway.local_socket(), unscoped);
    }

    #[test]
    fn ssh_hosts_get_an_ssh_transport() {
        let options = FleetConnectorOptions::default();
        // Building a transport must do no I/O: nothing here reaches ssh.
        let transport = transport_for(
            &spec(HostKind::Ssh {
                target: "workbox".to_string(),
                session: Some("agents".to_string()),
            }),
            &options,
        )
        .expect("every configured ssh host gets a transport");
        assert_eq!(transport.describe(), "ssh workbox (session agents)");
        assert_eq!(
            transport.read_timeout(),
            crate::fleet::handshake::REMOTE_HANDSHAKE_READ_TIMEOUT
        );
    }
}
