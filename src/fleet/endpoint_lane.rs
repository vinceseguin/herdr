//! One host's endpoint request/response lane.
//!
//! `crate::client::endpoint_commands` does this for the single connection a
//! stock client owns. The fleet needs one lane per host, and needs it as data:
//! a response that arrives for the wrong boot or the wrong request must fail
//! *that* request, never be handed to another host's caller.
//!
//! Correlation is `(boot_id, request_id)`, exactly as the server sends it. A
//! `boot_id` mismatch means the host restarted under the request: the answer
//! belongs to a projection that no longer exists, so it is refused rather than
//! reassembled.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long one request may stay in flight before the lane gives up.
///
/// Mirrors `client::endpoint_commands::ENDPOINT_COMMAND_TIMEOUT`.
pub const ENDPOINT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A finished request: the caller's id, and the bytes or the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneResponse {
    pub request_id: String,
    pub result: Result<Vec<u8>, String>,
}

#[derive(Debug, Clone)]
struct InFlight {
    boot_id: String,
    request_id: String,
    response: Vec<u8>,
    sent_at: Instant,
}

/// The queued and in-flight requests of one host.
///
/// One request is in flight at a time, matching what the server's shell lane
/// accepts; the rest wait in order.
#[derive(Debug, Default)]
pub struct EndpointLane {
    queued: VecDeque<(String, String)>,
    in_flight: Option<InFlight>,
}

impl EndpointLane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one already-serialized `api::schema::Request`.
    pub fn enqueue(&mut self, request_id: String, request: String) {
        self.queued.push_back((request_id, request));
    }

    /// The next request to write, if the lane is free.
    ///
    /// Returns the request id and its JSON; the caller writes the
    /// `ClientShellEndpointRequest` and calls [`EndpointLane::mark_sent`] (or
    /// [`EndpointLane::fail_in_flight`] when the write failed).
    pub fn take_next(&mut self) -> Option<(String, String)> {
        if self.in_flight.is_some() {
            return None;
        }
        self.queued.pop_front()
    }

    /// Record the request the caller just wrote for `boot_id`.
    pub fn mark_sent(&mut self, boot_id: String, request_id: String, now: Instant) {
        self.in_flight = Some(InFlight {
            boot_id,
            request_id,
            response: Vec::new(),
            sent_at: now,
        });
    }

    pub fn has_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Fold one response chunk in.
    ///
    /// `Ok(Some(_))` is a completed response, `Ok(None)` a chunk that is not
    /// the last. `Err` is a correlation failure: the host answered something
    /// this lane did not ask for, which the connector treats as a protocol
    /// error on that host alone.
    pub fn receive_chunk(
        &mut self,
        boot_id: &str,
        request_id: &str,
        final_chunk: bool,
        data: Vec<u8>,
    ) -> Result<Option<LaneResponse>, String> {
        let Some(in_flight) = self.in_flight.as_mut() else {
            return Err("endpoint response arrived without an in-flight request".to_string());
        };
        if in_flight.boot_id != boot_id {
            return Err(format!(
                "endpoint response for boot {boot_id} does not match the in-flight boot {}",
                in_flight.boot_id
            ));
        }
        if in_flight.request_id != request_id {
            return Err(format!(
                "endpoint response for request {request_id} does not match the in-flight request {}",
                in_flight.request_id
            ));
        }
        in_flight.response.extend(data);
        if !final_chunk {
            return Ok(None);
        }
        let Some(in_flight) = self.in_flight.take() else {
            // Unreachable: `as_mut` above proved it is present, and nothing
            // between the two can clear it. Reported, never unwrapped.
            return Err("endpoint response lost its in-flight request".to_string());
        };
        Ok(Some(LaneResponse {
            request_id: in_flight.request_id,
            result: Ok(in_flight.response),
        }))
    }

    /// Give up on a request that has been in flight too long.
    ///
    /// Checked when the host says anything and when a request is queued, not
    /// on a timer: the connector has no clock thread, and a host that answers
    /// nothing at all is reported through its connection state instead.
    pub fn expire(&mut self, now: Instant) -> Option<LaneResponse> {
        let in_flight = self.in_flight.as_ref()?;
        if now.saturating_duration_since(in_flight.sent_at) < ENDPOINT_REQUEST_TIMEOUT {
            return None;
        }
        let in_flight = self.in_flight.take()?;
        Some(LaneResponse {
            request_id: in_flight.request_id,
            result: Err("this host did not answer the request in time".to_string()),
        })
    }

    /// Fail the in-flight request, keeping the queue.
    pub fn fail_in_flight(&mut self, reason: &str) -> Option<LaneResponse> {
        let in_flight = self.in_flight.take()?;
        Some(LaneResponse {
            request_id: in_flight.request_id,
            result: Err(reason.to_string()),
        })
    }

    /// Fail everything: the connection is gone, so no answer is coming.
    pub fn fail_all(&mut self, reason: &str) -> Vec<LaneResponse> {
        let mut failed = Vec::new();
        if let Some(response) = self.fail_in_flight(reason) {
            failed.push(response);
        }
        while let Some((request_id, _)) = self.queued.pop_front() {
            failed.push(LaneResponse {
                request_id,
                result: Err(reason.to_string()),
            });
        }
        failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent_lane() -> EndpointLane {
        let mut lane = EndpointLane::new();
        lane.enqueue("r1".to_string(), "{}".to_string());
        let (request_id, _) = lane.take_next().expect("queued request");
        lane.mark_sent("boot-1".to_string(), request_id, Instant::now());
        lane
    }

    #[test]
    fn two_chunks_reassemble_in_order() {
        let mut lane = sent_lane();
        assert_eq!(
            lane.receive_chunk("boot-1", "r1", false, b"{\"a\":".to_vec()),
            Ok(None)
        );
        let response = lane
            .receive_chunk("boot-1", "r1", true, b"1}".to_vec())
            .expect("correlated")
            .expect("final chunk");
        assert_eq!(response.request_id, "r1");
        assert_eq!(response.result, Ok(b"{\"a\":1}".to_vec()));
        assert!(!lane.has_in_flight());
    }

    #[test]
    fn a_mismatched_boot_id_is_refused() {
        let mut lane = sent_lane();
        let error = lane
            .receive_chunk("boot-2", "r1", true, b"{}".to_vec())
            .expect_err("boot mismatch");
        assert!(error.contains("boot-2"), "unexpected error: {error}");
        assert!(
            lane.has_in_flight(),
            "the real request must stay in flight after a foreign answer"
        );
    }

    #[test]
    fn a_mismatched_request_id_is_refused() {
        let mut lane = sent_lane();
        let error = lane
            .receive_chunk("boot-1", "r2", true, b"{}".to_vec())
            .expect_err("request mismatch");
        assert!(error.contains("r2"), "unexpected error: {error}");
    }

    #[test]
    fn a_response_without_a_request_is_refused() {
        let mut lane = EndpointLane::new();
        let error = lane
            .receive_chunk("boot-1", "r1", true, b"{}".to_vec())
            .expect_err("nothing in flight");
        assert!(
            error.contains("without an in-flight"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn only_one_request_is_in_flight_at_a_time() {
        let mut lane = EndpointLane::new();
        lane.enqueue("r1".to_string(), "{}".to_string());
        lane.enqueue("r2".to_string(), "{}".to_string());
        let (first, _) = lane.take_next().expect("first request");
        lane.mark_sent("boot-1".to_string(), first, Instant::now());
        assert_eq!(lane.take_next(), None);
        let response = lane
            .receive_chunk("boot-1", "r1", true, Vec::new())
            .expect("correlated")
            .expect("final chunk");
        assert_eq!(response.request_id, "r1");
        assert_eq!(
            lane.take_next(),
            Some(("r2".to_string(), "{}".to_string())),
            "the queued request runs once the lane is free"
        );
    }

    #[test]
    fn an_old_request_expires() {
        let mut lane = EndpointLane::new();
        lane.enqueue("r1".to_string(), "{}".to_string());
        let (request_id, _) = lane.take_next().expect("queued request");
        let sent_at = Instant::now() - ENDPOINT_REQUEST_TIMEOUT - Duration::from_secs(1);
        lane.mark_sent("boot-1".to_string(), request_id, sent_at);
        assert_eq!(lane.expire(Instant::now() - ENDPOINT_REQUEST_TIMEOUT), None);
        let expired = lane.expire(Instant::now()).expect("expired request");
        assert_eq!(expired.request_id, "r1");
        assert!(expired.result.is_err());
        assert!(!lane.has_in_flight());
    }

    #[test]
    fn a_lost_connection_fails_every_request() {
        let mut lane = sent_lane();
        lane.enqueue("r2".to_string(), "{}".to_string());
        let failed = lane.fail_all("host disconnected");
        assert_eq!(
            failed
                .iter()
                .map(|response| response.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["r1", "r2"]
        );
        assert!(failed
            .iter()
            .all(|response| response.result == Err("host disconnected".to_string())));
        assert_eq!(lane.take_next(), None);
    }
}
