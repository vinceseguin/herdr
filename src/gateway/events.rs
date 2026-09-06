//! `GET /api/events` — the live fleet feed.
//!
//! A client that only polls `/api/fleet` is either late or wasteful. This
//! socket makes the fleet push: it opens with a `hello` and one whole
//! [`FleetStatusReport`], then forwards every `FleetChange` the runtime folds,
//! as the pre-serialized JSON the runtime already produced. Nothing here
//! re-serializes a change, so fan-out to N clients costs N pointer clones and
//! N writes, not N encodings.
//!
//! **Gaplessness is the property to preserve.** The subscription and its
//! starting report are taken together under the fleet's state lock
//! ([`FleetHandle::subscribe_with_report`]), so for any change either the
//! report already contains it or it arrives on the stream — never neither. A
//! subscriber that falls behind the 256-deep channel is not silently short a
//! delta: it gets `resync` and a fresh report/stream pair taken the same way.
//!
//! The socket half is deliberately thin, and the ordering logic lives behind
//! [`EventSink`] so it is tested with a `Vec<String>` instead of a real
//! WebSocket — the axum implementation is the only part that needs a server.

use std::future::Future;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::gateway::auth::TokenScope;
use crate::gateway::fleet::{ChangeItem, FleetHandle};
use crate::gateway::middleware::{require, Authed};
use crate::gateway::protocol::{encode, EventsFleet, EventsHello, Resync};
use crate::gateway::server::AppState;

/// Largest message the gateway will read from an events client.
///
/// The client is not supposed to say anything at all here; the cap exists so a
/// browser bug or a hostile peer cannot make the process buffer. A message
/// above it is a protocol error to the WebSocket layer, which ends the
/// connection — the socket is a fleet *feed*, so there is nothing to recover.
const MAX_INBOUND_MESSAGE_BYTES: usize = 4 * 1024;

/// How often an idle socket is pinged.
///
/// A LAN NAT or a phone that slept drops a TCP connection without either end
/// noticing. Without this, a gateway would hold a task and a fleet subscriber
/// per dead browser tab until the process restarted.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// How many pings may go unanswered before the socket is closed.
///
/// Two: one missed pong is a stalled phone, two is a connection that is gone.
const MISSED_PONG_LIMIT: u32 = 2;

/// WebSocket close code for "this end is going away" (RFC 6455 §7.4.1), which
/// is what a stopping gateway is. A client tells it apart from a network drop
/// and can stop reconnecting.
const CLOSE_GOING_AWAY: u16 = 1001;

/// `/api/events`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/events", get(events_ws))
}

/// Upgrade to a WebSocket, once the caller has proved it may read.
///
/// [`Authed`] cannot be constructed outside the auth middleware, so an
/// unauthenticated request never reaches the upgrade; [`require`] then states
/// the scope explicitly rather than inheriting `read` by omission. The scope
/// travels into the task because the `hello` reports it.
async fn events_ws(
    State(state): State<AppState>,
    Authed(principal): Authed,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(error) = require(&principal, TokenScope::Read) {
        return error.into_response();
    }
    let scope = principal.scope;
    upgrade
        .max_message_size(MAX_INBOUND_MESSAGE_BYTES)
        .max_frame_size(MAX_INBOUND_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            run_events(
                &mut SocketSink(socket),
                &state.fleet,
                &state.info.client_version,
                scope,
                PING_INTERVAL,
            )
            .await;
        })
}

/// What the events loop needs from its socket.
///
/// One trait so the loop is exercised without a server: the axum
/// implementation is [`SocketSink`], and the tests use a recorder. Every
/// method returns an explicitly `Send` future, because the loop runs in a task
/// axum spawns.
trait EventSink {
    /// One text message. `Err` means the peer is gone; the loop then stops.
    fn send_text(&mut self, text: String) -> impl Future<Output = Result<(), Gone>> + Send;

    /// A keepalive ping.
    fn send_ping(&mut self) -> impl Future<Output = Result<(), Gone>> + Send;

    /// Say goodbye with a close code. Best effort: the peer may already be
    /// gone, and that is not an error worth reporting.
    fn close(&mut self, code: u16, reason: &'static str) -> impl Future<Output = ()> + Send;

    /// The next thing the client did, reduced to what the loop cares about.
    ///
    /// Must be cancel-safe: it sits in a `select!` arm and is dropped whenever
    /// another arm wins.
    fn recv(&mut self) -> impl Future<Output = ClientEvent> + Send;
}

/// The peer is no longer reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Gone;

/// What arriving client traffic means to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientEvent {
    /// Any message at all: the connection is alive. The content is ignored —
    /// `/api/events` is a feed, and a client with something to say uses the
    /// HTTP API or (from PR 6) a terminal socket.
    Alive,
    /// A close frame, a transport error, or the end of the stream.
    Closed,
}

/// The axum socket.
struct SocketSink(WebSocket);

impl EventSink for SocketSink {
    async fn send_text(&mut self, text: String) -> Result<(), Gone> {
        self.0
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| Gone)
    }

    async fn send_ping(&mut self) -> Result<(), Gone> {
        self.0
            .send(Message::Ping(Default::default()))
            .await
            .map_err(|_| Gone)
    }

    async fn close(&mut self, code: u16, reason: &'static str) {
        let _ = self
            .0
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })))
            .await;
    }

    async fn recv(&mut self) -> ClientEvent {
        // Cancel-safe: the partial read lives in the socket's own buffer, not
        // in this future, so dropping it in a `select!` loses nothing.
        match self.0.recv().await {
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => ClientEvent::Closed,
            Some(Ok(_)) => ClientEvent::Alive,
        }
    }
}

/// One step of the loop, so the `select!` borrows the sink for exactly the
/// `recv` arm and nothing else.
enum Step {
    Change(ChangeItem),
    /// The runtime stopped and the stream is drained.
    FleetEnded,
    Client(ClientEvent),
    Ping,
}

/// Send `hello`, the fleet, then every change, until either end stops.
///
/// Returns when the socket is finished with; the caller drops it, which is
/// what actually closes the connection.
async fn run_events<S: EventSink>(
    sink: &mut S,
    fleet: &FleetHandle,
    client_version: &str,
    scope: TokenScope,
    ping_interval: Duration,
) {
    let (mut stream, report) = fleet.subscribe_with_report();
    if send(sink, &EventsHello::new(client_version, scope))
        .await
        .is_err()
    {
        return;
    }
    if send(sink, &EventsFleet::new(report)).await.is_err() {
        return;
    }

    let mut ping = tokio::time::interval(ping_interval);
    // `Delay`, not the default `Burst`: a socket whose peer read slowly for a
    // while (a phone on a bad link, a browser tab that was throttled) delays
    // this loop past several periods, and a bursting interval would then fire
    // every missed tick back to back — closing a client that had answered
    // every ping it was actually given a chance to answer. `Delay` restarts
    // the period from the tick it did deliver, so the limit below always
    // counts pings the peer had a whole interval to answer.
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately; the first tick is consumed here so a fresh
    // socket is not pinged before it has been idle for a whole period.
    ping.tick().await;
    let mut unanswered_pings = 0u32;

    loop {
        let step = tokio::select! {
            item = stream.next() => match item {
                Some(item) => Step::Change(item),
                None => Step::FleetEnded,
            },
            event = sink.recv() => Step::Client(event),
            _ = ping.tick() => Step::Ping,
        };

        match step {
            Step::Change(ChangeItem::Change(json)) => {
                // The runtime already serialized this exactly once, for every
                // subscriber. Forward it verbatim; wrapping it would re-encode
                // per client and change the shape a reader decodes.
                if sink.send_text(json.to_string()).await.is_err() {
                    return;
                }
            }
            Step::Change(ChangeItem::Lagged) => {
                // A fresh subscription and its report are taken together under
                // the state lock, so the pair the client resumes from has no
                // gap of its own — unlike re-reading a report on the old
                // stream, which could miss a change applied in between.
                let (fresh, report) = fleet.subscribe_with_report();
                stream = fresh;
                if send(sink, &Resync::new()).await.is_err() {
                    return;
                }
                if send(sink, &EventsFleet::new(report)).await.is_err() {
                    return;
                }
            }
            Step::FleetEnded => {
                sink.close(CLOSE_GOING_AWAY, "fleet runtime stopped").await;
                return;
            }
            Step::Client(ClientEvent::Alive) => unanswered_pings = 0,
            Step::Client(ClientEvent::Closed) => return,
            Step::Ping => {
                if unanswered_pings >= MISSED_PONG_LIMIT {
                    tracing::debug!(
                        target: "gateway",
                        "closing an events socket that stopped answering pings"
                    );
                    return;
                }
                if sink.send_ping().await.is_err() {
                    return;
                }
                unanswered_pings += 1;
            }
        }
    }
}

/// Encode and send one message, treating an unencodable one as a dead socket.
///
/// A daemon must not panic on a serialization failure, and it must not pretend
/// the client got a message it did not: dropping the connection is the honest
/// outcome, and the client reconnects into a fresh report.
async fn send<S: EventSink, T: serde::Serialize>(sink: &mut S, message: &T) -> Result<(), Gone> {
    match encode(message) {
        Ok(text) => sink.send_text(text).await,
        Err(error) => {
            tracing::error!(
                target: "gateway",
                error = %error,
                "an events message would not serialize; closing the socket"
            );
            Err(Gone)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::watch;

    use crate::fleet::hosts::{HostId, HostKind, HostSpec};
    use crate::fleet::state::{FleetChange, FleetState, HostEvent};

    /// A sink that records what the loop sent and never says anything back.
    ///
    /// It can also fold changes into the fleet at a chosen point in the
    /// conversation. That is not a convenience: [`run_events`] subscribes
    /// *inside* itself, so a change applied before the call would already be in
    /// the opening report and never reach the stream. Injecting from the sink
    /// is what makes a delta — and an overflow — reproducible.
    #[derive(Default)]
    struct Recorder {
        sent: Vec<String>,
        pings: u32,
        closed: Option<(u16, &'static str)>,
        /// Once this many text messages have been sent, the peer is "gone", so
        /// a test ends the loop deterministically instead of on a timeout.
        gone_after: Option<usize>,
        /// `(after N messages, fleet, events to fold)`.
        inject: Option<(usize, FleetHandle, Vec<HostEvent>)>,
        /// A peer that answers every ping, until this many have been sent —
        /// then it hangs up, so the test ends without a timeout.
        answer_pings: Option<u32>,
        /// How many pings this peer has already answered.
        answered: u32,
    }

    impl Recorder {
        fn kinds(&self) -> Vec<String> {
            self.sent
                .iter()
                .map(|json| {
                    serde_json::from_str::<serde_json::Value>(json)
                        .ok()
                        .and_then(|value| value.get("kind")?.as_str().map(str::to_string))
                        .unwrap_or_else(|| format!("undecodable: {json}"))
                })
                .collect()
        }

        fn inject_if_due(&mut self) {
            let sent = self.sent.len();
            let Some((at, fleet, events)) = self.inject.as_mut() else {
                return;
            };
            if sent != *at {
                return;
            }
            for event in std::mem::take(events) {
                fleet.test_apply(&host("alpha"), event);
            }
        }
    }

    impl EventSink for Recorder {
        async fn send_text(&mut self, text: String) -> Result<(), Gone> {
            self.sent.push(text);
            self.inject_if_due();
            match self.gone_after {
                Some(limit) if self.sent.len() >= limit => Err(Gone),
                _ => Ok(()),
            }
        }

        async fn send_ping(&mut self) -> Result<(), Gone> {
            self.pings += 1;
            Ok(())
        }

        async fn close(&mut self, code: u16, reason: &'static str) {
            self.closed = Some((code, reason));
        }

        async fn recv(&mut self) -> ClientEvent {
            if let Some(limit) = self.answer_pings {
                if self.pings >= limit {
                    return ClientEvent::Closed;
                }
                if self.pings > self.answered {
                    // A pong is ordinary inbound traffic to the loop, which is
                    // the whole point: axum surfaces `Message::Pong` on
                    // `recv`, so answering is what proves a peer alive.
                    self.answered = self.pings;
                    return ClientEvent::Alive;
                }
            }
            // A silent client: the loop must be driven by the fleet and the
            // ping timer, never by this arm resolving spuriously.
            std::future::pending().await
        }
    }

    fn host(id: &str) -> HostId {
        HostId::new(id).expect("valid host id")
    }

    fn spec(id: &str) -> HostSpec {
        HostSpec {
            id: host(id),
            kind: HostKind::Local {
                session: Some(id.to_string()),
            },
            enabled: true,
        }
    }

    fn connected() -> HostEvent {
        HostEvent::Connected {
            server_version: "0.8.2-fork".to_string(),
            methods: Vec::new(),
        }
    }

    fn unavailable() -> HostEvent {
        HostEvent::Unavailable {
            reason: "host went away".to_string(),
            retry_in: Some(Duration::from_secs(1)),
        }
    }

    /// A handle plus the stop sender that keeps its streams alive.
    fn handle(capacity: usize) -> (FleetHandle, watch::Sender<bool>) {
        FleetHandle::test_new(FleetState::new(vec![spec("alpha")]), capacity)
    }

    /// Drive the loop to completion with a long ping interval, so only the
    /// fleet and the sink decide when it ends.
    async fn drive(sink: &mut Recorder, fleet: &FleetHandle) {
        run_events(
            sink,
            fleet,
            "0.0.0-test",
            TokenScope::Read,
            Duration::from_secs(600),
        )
        .await;
    }

    /// The order clients depend on: identity, a whole fleet, then deltas.
    #[tokio::test]
    async fn a_client_gets_hello_then_the_fleet_then_changes() {
        let (fleet, _stop) = handle(16);
        let mut sink = Recorder {
            // hello, fleet, and one delta.
            gone_after: Some(3),
            inject: Some((2, fleet.clone(), vec![connected()])),
            ..Recorder::default()
        };

        drive(&mut sink, &fleet).await;

        assert_eq!(
            sink.kinds(),
            vec!["hello", "fleet", "host_connection"],
            "{:?}",
            sink.sent
        );
    }

    /// The delta is the runtime's own JSON, forwarded byte for byte: a reader
    /// decodes it with the same `FleetChange` type the CLI's watch uses.
    #[tokio::test]
    async fn a_change_is_forwarded_verbatim() {
        let (fleet, _stop) = handle(16);
        let mut sink = Recorder {
            gone_after: Some(3),
            inject: Some((2, fleet.clone(), vec![unavailable()])),
            ..Recorder::default()
        };

        drive(&mut sink, &fleet).await;

        let delta = sink.sent.last().expect("a delta was sent");
        let change: FleetChange =
            serde_json::from_str(delta).unwrap_or_else(|err| panic!("{err}: {delta}"));
        assert!(
            matches!(change, FleetChange::HostConnection { .. }),
            "{change:?}"
        );
    }

    /// A subscriber that overflowed is told, and handed a whole new fleet — a
    /// silent hole in the delta stream would leave its state permanently wrong.
    #[tokio::test]
    async fn a_lagging_client_is_resynced_with_a_fresh_report() {
        // Capacity 1, and two changes land before the loop reads either.
        let (fleet, _stop) = handle(1);
        let mut sink = Recorder {
            // hello, fleet, resync, fleet.
            gone_after: Some(4),
            inject: Some((2, fleet.clone(), vec![connected(), unavailable()])),
            ..Recorder::default()
        };

        drive(&mut sink, &fleet).await;

        assert_eq!(
            sink.kinds(),
            vec!["hello", "fleet", "resync", "fleet"],
            "{:?}",
            sink.sent
        );
        // The report after the resync is current, not the one from the open.
        let resynced: serde_json::Value =
            serde_json::from_str(&sink.sent[3]).expect("the second fleet message is JSON");
        assert_eq!(
            resynced["report"]["hosts"][0]["connection"]["state"].as_str(),
            Some("unavailable"),
            "{resynced}"
        );
    }

    /// A stopping runtime closes the socket with "going away" rather than
    /// leaving a client waiting on a fleet that no longer exists.
    #[tokio::test]
    async fn a_stopped_runtime_closes_the_socket() {
        let (fleet, stop) = handle(16);
        let mut sink = Recorder::default();
        assert!(stop.send(true).is_ok());

        drive(&mut sink, &fleet).await;

        assert_eq!(sink.kinds(), vec!["hello", "fleet"], "{:?}", sink.sent);
        assert_eq!(
            sink.closed,
            Some((CLOSE_GOING_AWAY, "fleet runtime stopped"))
        );
    }

    /// A client that never answers a ping is dropped after the second one,
    /// instead of holding a task and a fleet subscription forever.
    #[tokio::test]
    async fn a_silent_client_is_dropped_after_two_unanswered_pings() {
        let (fleet, _stop) = handle(16);
        let mut sink = Recorder::default();

        run_events(
            &mut sink,
            &fleet,
            "0.0.0-test",
            TokenScope::Read,
            Duration::from_millis(20),
        )
        .await;

        assert_eq!(sink.pings, MISSED_PONG_LIMIT);
        assert_eq!(sink.kinds(), vec!["hello", "fleet"], "{:?}", sink.sent);
        assert_eq!(sink.closed, None, "a dead peer gets no close frame");
    }

    /// A peer that vanishes during the opening messages ends the loop right
    /// there: no delta is read, and nothing is left subscribed.
    #[tokio::test]
    async fn a_peer_that_goes_away_during_the_handshake_stops_the_loop() {
        let (fleet, _stop) = handle(16);
        let mut sink = Recorder {
            gone_after: Some(1),
            ..Recorder::default()
        };

        drive(&mut sink, &fleet).await;

        assert_eq!(sink.kinds(), vec!["hello"], "{:?}", sink.sent);
        assert_eq!(sink.closed, None, "a gone peer gets no close frame");
    }

    /// A client that answers its pings is never dropped, however long it has
    /// nothing else to say. The counter has to be *reset* by inbound traffic,
    /// not merely incremented slower than the limit — a browser tab that is
    /// only watching sends nothing but pongs for hours.
    #[tokio::test]
    async fn a_client_that_answers_pings_is_kept() {
        const PINGS: u32 = MISSED_PONG_LIMIT * 3;
        let (fleet, _stop) = handle(16);
        let mut sink = Recorder {
            answer_pings: Some(PINGS),
            ..Recorder::default()
        };

        run_events(
            &mut sink,
            &fleet,
            "0.0.0-test",
            TokenScope::Read,
            Duration::from_millis(5),
        )
        .await;

        // Without the reset the loop would have given up at ping
        // `MISSED_PONG_LIMIT + 1`; it got well past that and ended only
        // because this peer hung up.
        assert_eq!(sink.pings, PINGS);
        assert_eq!(sink.kinds(), vec!["hello", "fleet"], "{:?}", sink.sent);
    }

    /// A message that will not serialize ends the socket instead of panicking
    /// or silently pretending the client received it: a daemon must survive a
    /// shape it cannot spell, and a client that reconnects gets a fresh report.
    #[tokio::test]
    async fn a_message_that_will_not_serialize_closes_the_socket() {
        struct Unserializable;

        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("this shape has no JSON"))
            }
        }

        let mut sink = Recorder::default();

        assert_eq!(send(&mut sink, &Unserializable).await, Err(Gone));
        assert!(sink.sent.is_empty(), "{:?}", sink.sent);
        assert_eq!(sink.closed, None);
    }

    /// The `Arc<str>` a change arrives as is forwarded, not rebuilt: this pins
    /// the "serialize once, fan out N pointers" property the module docs
    /// promise.
    #[test]
    fn a_change_item_carries_shared_json() {
        let json: Arc<str> = Arc::from(r#"{"kind":"snapshot"}"#);
        let item = ChangeItem::Change(Arc::clone(&json));
        match item {
            ChangeItem::Change(shared) => assert!(Arc::ptr_eq(&shared, &json)),
            ChangeItem::Lagged => panic!("not a change"),
        }
    }
}
