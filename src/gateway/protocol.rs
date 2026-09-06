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
mod tests {
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
