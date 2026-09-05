//! How the connector reaches one host.
//!
//! A transport is the smallest thing a supervisor thread needs: something that
//! hands back an open, blocking [`LocalStream`] carrying the endpoint protocol.
//! Local hosts connect to a session socket; ssh hosts (PR 6) connect to a
//! private forward socket fed by the shared stdio bridge. Everything above this
//! trait — handshake, snapshots, frames, reconnect — is identical for both.

use std::io;
use std::time::Duration;

use crate::fleet::hosts::{HostKind, HostSpec};
use crate::ipc::LocalStream;

pub mod local;

pub use local::LocalTransport;

/// Reason reported for an ssh host until PR 6 lands its transport.
///
/// PR 6 replaces the `HostKind::Ssh` arm of [`transport_for`] with the real
/// [`crate::fleet::transport`]`::ssh::SshTransport` and deletes this constant.
const SSH_PENDING_REASON: &str = "ssh hosts land in PR 6";

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
}

/// Build the transport for one host spec.
///
/// `Err` is a host-local reason string: the supervisor reports it as
/// `Unavailable` and keeps backing off, exactly as it does for a refused
/// socket. It never aborts the fleet.
pub fn transport_for(
    spec: &HostSpec,
    _options: &crate::fleet::connector::FleetConnectorOptions,
) -> Result<Box<dyn HostTransport>, String> {
    match &spec.kind {
        HostKind::Local { session } => Ok(Box::new(LocalTransport::new(session.clone()))),
        HostKind::Ssh { .. } => Err(SSH_PENDING_REASON.to_string()),
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

    #[test]
    fn ssh_hosts_are_reported_unavailable_until_pr_6() {
        let options = FleetConnectorOptions::default();
        let error = transport_for(
            &spec(HostKind::Ssh {
                target: "workbox".to_string(),
                session: None,
            }),
            &options,
        )
        .err()
        .expect("ssh transport is not implemented yet");
        assert_eq!(error, SSH_PENDING_REASON);
    }
}
