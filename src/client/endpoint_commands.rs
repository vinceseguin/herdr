use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::api::client::ApiClientError;
use crate::api::schema::{Request, ResponseResult};

use super::link::{LinkWriteError, ServerLink};
use super::shell::ClientShellEndpointError;

const ENDPOINT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

struct InFlightCommand {
    boot_id: String,
    request_id: String,
    response: Vec<u8>,
    sent_at: Instant,
    timed_out: bool,
    /// Why the fleet host never took this request. Such a command has no
    /// answer coming, so [`EndpointCommands::expire`] fails it on the next
    /// tick and releases the lane. Always `None` for a single-host client,
    /// whose write either reached the socket or lost the connection.
    unavailable: Option<String>,
}

pub(super) struct EndpointCommandResult {
    pub(super) boot_id: String,
    pub(super) request_id: String,
    pub(super) result: Result<ResponseResult, ClientShellEndpointError>,
}

#[derive(Default)]
pub(super) struct EndpointCommands {
    queued: VecDeque<(String, Box<Request>)>,
    in_flight: Option<InFlightCommand>,
}

impl EndpointCommands {
    pub(super) fn enqueue(&mut self, boot_id: String, request: Box<Request>) {
        self.queued.push_back((boot_id, request));
    }

    pub(super) fn send_next(&mut self, link: &mut ServerLink) -> io::Result<()> {
        if self.in_flight.is_some() {
            return Ok(());
        }
        let Some((boot_id, request)) = self.queued.pop_front() else {
            return Ok(());
        };
        let request_id = request.id.clone();
        let request = serde_json::to_string(&request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // The link decides how the request is correlated: on the wire for a
        // single host, through the connector's per-host lane for a fleet.
        let unavailable = match link.write_endpoint_request(&boot_id, &request_id, request) {
            // The fleet host did not take it, so nothing will answer: the
            // command still occupies the lane, but only until the next tick
            // reports the failure and releases it.
            Err(LinkWriteError::HostUnavailable(reason)) => {
                debug!(
                    request_id,
                    reason, "endpoint request not accepted by the fleet host"
                );
                Some(reason)
            }
            other => {
                super::link::io_result(other)?;
                None
            }
        };
        self.in_flight = Some(InFlightCommand {
            boot_id,
            request_id,
            response: Vec::new(),
            sent_at: Instant::now(),
            timed_out: false,
            unavailable,
        });
        Ok(())
    }

    /// Completes the in-flight command from an already reassembled answer.
    ///
    /// The fleet connector owns one endpoint lane per host: it correlates the
    /// answer by `request_id` and hands it over whole, so there is no boot id
    /// on the wire to check here. An answer for anything but the in-flight
    /// request is ignored — a stale host's late reply must not release the
    /// lane of the request that replaced it.
    pub(super) fn complete(
        &mut self,
        request_id: &str,
        result: Result<Vec<u8>, String>,
    ) -> Option<EndpointCommandResult> {
        let in_flight = self.in_flight.as_ref()?;
        if in_flight.request_id != request_id {
            debug!(
                request_id,
                in_flight = in_flight.request_id,
                "ignoring an endpoint answer for another request"
            );
            return None;
        }
        let in_flight = self.in_flight.take()?;
        let result = match result {
            Ok(response) => match String::from_utf8(response) {
                Ok(response) => parse_response(&in_flight.request_id, &response),
                Err(error) => Err(ClientShellEndpointError {
                    code: None,
                    message: format!("invalid endpoint response: {error}"),
                }),
            },
            Err(message) => Err(ClientShellEndpointError {
                code: Some("endpoint_unavailable".into()),
                message,
            }),
        };
        Some(EndpointCommandResult {
            boot_id: in_flight.boot_id,
            request_id: in_flight.request_id,
            result,
        })
    }

    /// Drop the queue and the in-flight command.
    ///
    /// The Fleet console calls this when it switches host: the connector
    /// answers only the host it sent a request to, and `fleet::translate`
    /// drops an inactive host's answer, so an in-flight command would hold
    /// this single lane until its 60 s timeout — and its answer, if it did
    /// arrive, would be applied against the machine the console moved to.
    pub(super) fn reset(&mut self) {
        if let Some(in_flight) = self.in_flight.take() {
            debug!(
                request_id = in_flight.request_id,
                "dropping an endpoint command in flight to the previous host"
            );
        }
        self.queued.clear();
    }

    /// Whether the single lane is free and nothing is waiting for it.
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.in_flight.is_none() && self.queued.is_empty()
    }

    pub(super) fn expire(&mut self, now: Instant) -> Option<EndpointCommandResult> {
        if let Some(reason) = self.in_flight.as_mut()?.unavailable.take() {
            // Never accepted, so unlike a timeout there is no late answer to
            // wait for: the lane is released with the failure.
            let command = self.in_flight.take()?;
            return Some(EndpointCommandResult {
                boot_id: command.boot_id,
                request_id: command.request_id,
                result: Err(ClientShellEndpointError {
                    code: Some("endpoint_unavailable".into()),
                    message: reason,
                }),
            });
        }
        let command = self.in_flight.as_mut()?;
        if command.timed_out
            || now.saturating_duration_since(command.sent_at) < ENDPOINT_COMMAND_TIMEOUT
        {
            return None;
        }
        command.timed_out = true;
        Some(EndpointCommandResult {
            boot_id: command.boot_id.clone(),
            request_id: command.request_id.clone(),
            result: Err(ClientShellEndpointError {
                code: Some("endpoint_timeout".into()),
                message: "this server did not respond to the action".into(),
            }),
        })
    }

    pub(super) fn receive_chunk(
        &mut self,
        response_boot_id: &str,
        response_request_id: &str,
        final_chunk: bool,
        data: Vec<u8>,
    ) -> io::Result<Option<EndpointCommandResult>> {
        let Some(in_flight) = self.in_flight.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "endpoint response arrived without an in-flight command",
            ));
        };
        if response_boot_id != in_flight.boot_id || response_request_id != in_flight.request_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "endpoint response correlation did not match the in-flight command",
            ));
        }
        in_flight.response.extend(data);
        if !final_chunk {
            return Ok(None);
        }

        let in_flight = self.in_flight.take().expect("checked in-flight command");
        let response = String::from_utf8(in_flight.response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let result = parse_response(&in_flight.request_id, &response);
        Ok(Some(EndpointCommandResult {
            boot_id: in_flight.boot_id,
            request_id: in_flight.request_id,
            result,
        }))
    }
}

fn parse_response(
    expected_id: &str,
    response: &str,
) -> Result<ResponseResult, ClientShellEndpointError> {
    let value = serde_json::from_str(response).map_err(|error| ClientShellEndpointError {
        code: None,
        message: format!("invalid endpoint response: {error}"),
    })?;
    match crate::api::client::parse_response_value(value) {
        Ok(response) if response.id == expected_id => Ok(response.result),
        Ok(response) => Err(ClientShellEndpointError {
            code: None,
            message: format!(
                "endpoint response id {:?} did not match {expected_id:?}",
                response.id
            ),
        }),
        Err(ApiClientError::ErrorResponse(response)) if response.id == expected_id => {
            Err(ClientShellEndpointError {
                code: Some(response.error.code),
                message: response.error.message,
            })
        }
        Err(ApiClientError::ErrorResponse(response)) => Err(ClientShellEndpointError {
            code: None,
            message: format!(
                "endpoint error id {:?} did not match {expected_id:?}",
                response.id
            ),
        }),
        Err(error) => Err(ClientShellEndpointError {
            code: None,
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ResponseResult, SuccessResponse};

    fn commands_with_in_flight() -> EndpointCommands {
        EndpointCommands {
            in_flight: Some(InFlightCommand {
                boot_id: "boot-a".into(),
                request_id: "request-a".into(),
                response: Vec::new(),
                sent_at: Instant::now(),
                timed_out: false,
                unavailable: None,
            }),
            ..EndpointCommands::default()
        }
    }

    #[test]
    fn a_request_the_fleet_host_never_took_fails_at_once_and_releases_the_lane() {
        let mut commands = EndpointCommands {
            in_flight: Some(InFlightCommand {
                boot_id: "boot-a".into(),
                request_id: "request-a".into(),
                response: Vec::new(),
                sent_at: Instant::now(),
                timed_out: false,
                unavailable: Some("fleet host is not connected".into()),
            }),
            ..EndpointCommands::default()
        };

        let failed = commands
            .expire(Instant::now())
            .expect("an unaccepted request fails on the next tick, not after the timeout");

        assert_eq!(failed.boot_id, "boot-a");
        assert_eq!(failed.request_id, "request-a");
        assert!(matches!(
            failed.result,
            Err(ClientShellEndpointError { code: Some(code), message })
                if code == "endpoint_unavailable" && message == "fleet host is not connected"
        ));
        assert!(
            commands.in_flight.is_none(),
            "no answer can come, so the lane is released rather than kept for one"
        );
        assert!(commands.expire(Instant::now()).is_none());
    }

    #[test]
    fn a_sent_request_is_not_touched_by_the_unavailable_path() {
        let mut commands = commands_with_in_flight();

        assert!(
            commands.expire(Instant::now()).is_none(),
            "a request the host took waits for its answer or the timeout"
        );
        assert!(commands.in_flight.is_some());
    }

    #[test]
    fn chunked_response_completion_is_correlated_and_clears_the_lane() {
        let mut commands = commands_with_in_flight();
        let response = serde_json::to_string(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .unwrap();
        let split = response.len() / 2;

        assert!(commands
            .receive_chunk(
                "boot-a",
                "request-a",
                false,
                response.as_bytes()[..split].to_vec(),
            )
            .unwrap()
            .is_none());
        let completed = commands
            .receive_chunk(
                "boot-a",
                "request-a",
                true,
                response.as_bytes()[split..].to_vec(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(completed.boot_id, "boot-a");
        assert_eq!(completed.request_id, "request-a");
        assert!(matches!(completed.result, Ok(ResponseResult::Ok {})));
        assert!(commands.in_flight.is_none());
    }

    #[test]
    fn large_selection_response_reassembles_without_truncation() {
        let mut commands = commands_with_in_flight();
        let selection = "selected".repeat(160_000);
        let response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::PaneSelection {
                pane_id: "w1:p1".into(),
                text: selection.clone(),
            },
        })
        .unwrap();
        let chunk_count = response.len().div_ceil(128 * 1024);
        let mut completed = None;
        for (index, chunk) in response.chunks(128 * 1024).enumerate() {
            completed = commands
                .receive_chunk(
                    "boot-a",
                    "request-a",
                    index + 1 == chunk_count,
                    chunk.to_vec(),
                )
                .unwrap();
        }

        assert!(matches!(
            completed.expect("final selection response").result,
            Ok(ResponseResult::PaneSelection { text, .. }) if text == selection
        ));
    }

    #[test]
    fn in_flight_endpoint_command_expires_and_releases_the_lane() {
        let mut commands = commands_with_in_flight();
        let expired = commands
            .expire(std::time::Instant::now() + ENDPOINT_COMMAND_TIMEOUT)
            .expect("expired endpoint command");

        assert_eq!(expired.boot_id, "boot-a");
        assert_eq!(expired.request_id, "request-a");
        assert!(matches!(
            expired.result,
            Err(ClientShellEndpointError {
                code: Some(code),
                ..
            }) if code == "endpoint_timeout"
        ));
        assert!(commands.in_flight.is_some());
        assert!(commands
            .expire(std::time::Instant::now() + ENDPOINT_COMMAND_TIMEOUT)
            .is_none());
        let late_response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .unwrap();
        assert!(commands
            .receive_chunk("boot-a", "request-a", true, late_response)
            .expect("late response releases the synchronized lane")
            .is_some());
        assert!(commands.in_flight.is_none());
    }

    #[test]
    fn a_reassembled_fleet_answer_completes_the_in_flight_command() {
        let mut commands = commands_with_in_flight();
        let response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .unwrap();

        let completed = commands
            .complete("request-a", Ok(response))
            .expect("the in-flight command is completed");

        assert_eq!(completed.boot_id, "boot-a");
        assert_eq!(completed.request_id, "request-a");
        assert!(matches!(completed.result, Ok(ResponseResult::Ok {})));
        assert!(commands.in_flight.is_none(), "the lane is released");
    }

    #[test]
    fn a_fleet_answer_for_another_request_never_releases_the_lane() {
        let mut commands = commands_with_in_flight();

        assert!(commands.complete("request-b", Ok(b"{}".to_vec())).is_none());
        assert!(
            commands.in_flight.is_some(),
            "the request that is actually in flight keeps its lane"
        );
    }

    #[test]
    fn a_fleet_host_failure_completes_the_command_as_an_error() {
        let mut commands = commands_with_in_flight();

        let completed = commands
            .complete("request-a", Err("host went away".into()))
            .expect("a failure still answers the command");

        assert!(matches!(
            completed.result,
            Err(ClientShellEndpointError { code: Some(code), message })
                if code == "endpoint_unavailable" && message == "host went away"
        ));
        assert!(commands.in_flight.is_none());
    }

    #[test]
    fn an_answer_with_no_command_in_flight_is_ignored() {
        let mut commands = EndpointCommands::default();

        assert!(commands.complete("request-a", Ok(b"{}".to_vec())).is_none());
    }

    #[test]
    fn response_from_another_boot_is_rejected() {
        let mut commands = commands_with_in_flight();

        let Err(error) = commands.receive_chunk("boot-b", "request-a", true, b"{}".to_vec()) else {
            panic!("mismatched boot should fail");
        };

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}

#[cfg(all(test, unix))]
mod fleet_tests {
    use super::*;

    use std::rc::Rc;
    use std::time::Duration;

    use crate::api::schema::{Method, PingParams, Request};
    use crate::fleet::connector::test_support::{
        connected_with_snapshot, drain_until, fake_connector, scratch_dir, snapshot,
        snapshot_message, wait_for, Behaviour, FakeHost,
    };
    use crate::fleet::connector::FleetConnectorOptions;
    use crate::fleet::hosts::HostId;
    use crate::fleet::state::FleetState;
    use crate::protocol::ClientMessage;

    use super::super::link::{FleetLink, ServerLink};

    /// A queued request sent through a fleet link reaches the host as an
    /// endpoint request on that host's own lane, correlated by request id.
    #[test]
    fn send_next_routes_an_endpoint_request_through_the_active_host() {
        let dir = scratch_dir("endpoint-commands-fleet");
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
        let mut link = ServerLink::Fleet(FleetLink::new(
            Rc::new(connector),
            HostId::new("alpha").expect("valid host id"),
        ));

        let mut commands = EndpointCommands::default();
        commands.enqueue(
            "boot-alpha".into(),
            Box::new(Request {
                id: "request-1".into(),
                method: Method::Ping(PingParams::default()),
            }),
        );
        commands.send_next(&mut link).expect("request sent");

        assert_eq!(
            commands
                .in_flight
                .as_ref()
                .map(|command| command.request_id.clone()),
            Some("request-1".to_string()),
            "the lane tracks the request the host was given"
        );
        assert!(
            wait_for(Duration::from_secs(5), || {
                alpha.received().iter().any(|message| {
                    matches!(
                        message,
                        ClientMessage::ClientShellEndpointRequest { request, .. }
                            if request.contains("\"request-1\"")
                    )
                })
            }),
            "the active host received the request: {:?}",
            alpha.received()
        );
    }

    /// A request to a host that is down is not silently parked in the lane:
    /// the connector never answers requests it did not accept, so the next
    /// tick fails the command and the lane is free for the next one.
    #[test]
    fn send_next_to_a_host_that_is_down_fails_the_command_instead_of_stalling_the_lane() {
        let dir = scratch_dir("endpoint-commands-fleet-down");
        // Accepts the connection and never answers the hello: connected at the
        // transport level, never usable, so the connector reports NotConnected.
        let silent = FakeHost::start(&dir, "silent", Behaviour::Silent);
        let connector = fake_connector(&[("silent", &silent)], FleetConnectorOptions::default());
        let mut link = ServerLink::Fleet(FleetLink::new(
            Rc::new(connector),
            HostId::new("silent").expect("valid host id"),
        ));

        let mut commands = EndpointCommands::default();
        commands.enqueue(
            "boot-silent".into(),
            Box::new(Request {
                id: "request-1".into(),
                method: Method::Ping(PingParams::default()),
            }),
        );
        commands
            .send_next(&mut link)
            .expect("a host that is down is not a console failure");

        let failed = commands
            .expire(std::time::Instant::now())
            .expect("the unaccepted request fails on the next tick");
        assert_eq!(failed.request_id, "request-1");
        assert!(matches!(
            failed.result,
            Err(ClientShellEndpointError { code: Some(code), .. }) if code == "endpoint_unavailable"
        ));
        assert!(commands.in_flight.is_none(), "the lane is released");

        commands.enqueue(
            "boot-silent".into(),
            Box::new(Request {
                id: "request-2".into(),
                method: Method::Ping(PingParams::default()),
            }),
        );
        commands
            .send_next(&mut link)
            .expect("the next request is attempted");
        assert_eq!(
            commands
                .in_flight
                .as_ref()
                .map(|command| command.request_id.clone()),
            Some("request-2".to_string()),
            "the lane took the next request rather than staying blocked on the first"
        );
    }
}
