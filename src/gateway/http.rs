//! The JSON surface: `/health`, `/api/gateway`, `/api/fleet`, and the error
//! shape every route in the epic answers with.
//!
//! Each area module of the gateway exposes one `routes() -> Router<AppState>`
//! that [`crate::gateway::server::router`] merges, so a later PR adds a route
//! without touching this file's handlers. Every `/api/*` handler takes
//! [`Authed`], which only exists once the auth middleware has decided who the
//! caller is — a route that forgets authentication does not compile.
//!
//! Nothing here caches. `/api/fleet` costs one `FleetStatusReport::from_state`
//! per request (O(hosts × agents)), which is cheap and, unlike a cache, cannot
//! race the delta stream PR 5 adds.

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use serde_json::json;

use crate::gateway::auth::{Credential, TokenScope};
use crate::gateway::middleware::{require, Authed};
use crate::gateway::server::AppState;

/// `/health`, `/api/gateway` and `/api/fleet`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/api/gateway", get(gateway_info))
        .route("/api/fleet", get(fleet_report))
}

/// Liveness only, and deliberately unauthenticated: a supervisor (systemd, a
/// container probe, the operator's `curl`) must be able to tell "the process
/// is up" from "the process is wedged" without holding a token. It carries no
/// fleet fact — not a host name, not a version — so it leaks nothing.
async fn health() -> Response {
    json_response(StatusCode::OK, &json!({ "ok": true }))
}

/// Schema marker of [`gateway_info`]'s body.
pub(crate) const GATEWAY_INFO_SCHEMA: &str = "herdr.gateway.info.v1";

/// What this gateway is and what the caller may do with it.
///
/// `features` is the capability advertisement the web app and every later
/// client reads: names are appended as PRs land them (`events`, `terminal`,
/// `pairing`) and never removed, so an old client that does not know a name
/// simply does not use it.
async fn gateway_info(State(state): State<AppState>, Authed(principal): Authed) -> Response {
    // Every route states the scope it needs, even when that scope is the
    // weakest one: a later PR that changes what a route does must change this
    // line too, rather than inherit "read" by omission.
    if let Err(error) = require(&principal, TokenScope::Read) {
        return error.into_response();
    }
    let info = &state.info;
    let mut body = json!({
        "schema": GATEWAY_INFO_SCHEMA,
        "client_version": info.client_version,
        "scope": principal.scope.as_str(),
        "via": credential_name(&principal.via),
        "loopback": info.loopback,
        "public_url": info.public_url,
        "features": info.features,
    });
    // A paired browser is told which device it is, so a settings screen can
    // name the entry the operator would revoke. A bearer client gets no
    // `device` field at all rather than a null one.
    if let Credential::Device { id } = &principal.via {
        if let Some(device) = device_summary(&state, id) {
            body["device"] = device;
        }
    }
    json_response(StatusCode::OK, &body)
}

/// How a principal proved itself, in the neutral vocabulary the API uses.
fn credential_name(credential: &Credential) -> &'static str {
    match credential {
        Credential::Bearer => "bearer",
        Credential::Device { .. } => "device",
    }
}

/// `{"id", "label"}` for the calling device, or `None` when it has been
/// revoked between the credential check and this lookup.
fn device_summary(state: &AppState, id: &str) -> Option<serde_json::Value> {
    let devices = state
        .auth
        .devices
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    devices
        .devices()
        .iter()
        .find(|record| record.id == id)
        .map(|record| json!({ "id": record.id, "label": record.label }))
}

/// The fleet, in exactly the shape `herdr fleet status --json` prints.
///
/// An unreachable host is data in this body (`connection.state ==
/// "unavailable"`), never a 5xx: host failure is local to that host.
async fn fleet_report(State(state): State<AppState>, Authed(principal): Authed) -> Response {
    if let Err(error) = require(&principal, TokenScope::Read) {
        return error.into_response();
    }
    json_response(StatusCode::OK, &state.fleet.report())
}

/// Serialize `body` as a JSON response that is never stored anywhere.
///
/// `no-store` on every API answer: the fleet changes continuously and the
/// bodies describe someone's machines, so neither a browser nor an
/// intermediary should keep one. `nosniff` because the body is JSON and must
/// not be re-interpreted as anything else.
fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    let encoded = match serde_json::to_vec(body) {
        Ok(encoded) => encoded,
        // Unreachable for the shapes above; a daemon must not panic if it ever
        // stops being, and the caller gets an honest error instead of a
        // truncated body.
        Err(error) => {
            tracing::error!(target: "gateway", error = %error, "a response body would not serialize");
            return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
                .into_response();
        }
    };
    let mut response = Response::new(axum::body::Body::from(encoded));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Every error the gateway answers with: `{"error":"<snake_case_code>"}` plus
/// optional `message` and `needed` fields.
///
/// The code is the contract (a client switches on it); the message is for a
/// human and is never derived from a credential, a header value or a
/// filesystem path a caller controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: Option<String>,
    /// The scope the caller would have needed, for `forbidden`.
    needed: Option<&'static str>,
    /// Seconds for a `Retry-After` header, for `too_many_requests`.
    retry_after_secs: Option<u64>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            message: None,
            needed: None,
            retry_after_secs: None,
        }
    }

    /// No credential, or one that proved nothing.
    ///
    /// Deliberately identical whether the token was absent, malformed, or
    /// wrong: telling the two apart is a probing aid and no client needs it.
    pub(crate) fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    /// A valid credential whose scope is too narrow.
    pub(crate) fn forbidden(needed: TokenScope) -> Self {
        Self {
            needed: Some(needed.as_str()),
            ..Self::new(StatusCode::FORBIDDEN, "forbidden")
        }
    }

    /// A browser origin that is not on the allowlist.
    pub(crate) fn origin_not_allowed() -> Self {
        Self::new(StatusCode::FORBIDDEN, "origin_not_allowed")
    }

    /// The peer has failed authentication too often in the window.
    pub(crate) fn too_many_requests(retry_after: std::time::Duration) -> Self {
        Self {
            // At least a second: `Retry-After: 0` reads as "retry immediately".
            retry_after_secs: Some(retry_after.as_secs().max(1)),
            ..Self::new(StatusCode::TOO_MANY_REQUESTS, "too_many_requests")
        }
    }

    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found")
    }

    pub(crate) fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// The stable machine-readable half of the error, for the routes that log
    /// what they refused and for the tests.
    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    #[allow(dead_code)]
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = serde_json::Map::new();
        body.insert("error".to_string(), json!(self.code));
        if let Some(message) = &self.message {
            body.insert("message".to_string(), json!(message));
        }
        if let Some(needed) = self.needed {
            body.insert("needed".to_string(), json!(needed));
        }
        let mut response = json_error_response(self.status, &serde_json::Value::Object(body));
        if let Some(seconds) = self.retry_after_secs {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        // A bearer-authenticated API needs to say so on a 401, or a browser
        // fetch has no way to tell "log in" from "you may not".
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"herdr gateway\""),
            );
        }
        response
    }
}

/// Like [`json_response`], without the recursion an error inside it would
/// cause: the body here is a `serde_json::Value`, which always serializes.
fn json_error_response(status: StatusCode, body: &serde_json::Value) -> Response {
    let encoded = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec());
    let mut response = Response::new(axum::body::Body::from(encoded));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::to_bytes;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read the body");
        serde_json::from_slice(&bytes).expect("the body is JSON")
    }

    #[tokio::test]
    async fn every_error_body_carries_a_snake_case_code() {
        let cases = [
            ApiError::unauthorized(),
            ApiError::forbidden(TokenScope::Control),
            ApiError::origin_not_allowed(),
            ApiError::too_many_requests(std::time::Duration::from_secs(30)),
            ApiError::not_found(),
        ];
        for error in cases {
            let code = error.code();
            let json = body_json(error.into_response()).await;
            assert_eq!(json["error"].as_str(), Some(code));
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "not snake_case: {code}"
            );
        }
    }

    #[tokio::test]
    async fn forbidden_names_the_scope_the_caller_lacked() {
        let json = body_json(ApiError::forbidden(TokenScope::Control).into_response()).await;
        assert_eq!(json["error"].as_str(), Some("forbidden"));
        assert_eq!(json["needed"].as_str(), Some("control"));
    }

    #[test]
    fn a_rate_limited_answer_carries_a_retry_after_of_at_least_one_second() {
        let response =
            ApiError::too_many_requests(std::time::Duration::from_millis(10)).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("1")
        );
    }

    #[test]
    fn unauthorized_advertises_bearer_authentication() {
        let response = ApiError::unauthorized().into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
    }

    #[tokio::test]
    async fn a_message_is_optional_and_never_present_by_default() {
        let json = body_json(ApiError::unauthorized().into_response()).await;
        assert!(json.get("message").is_none(), "{json}");
        let json = body_json(
            ApiError::not_found()
                .with_message("no such host")
                .into_response(),
        )
        .await;
        assert_eq!(json["message"].as_str(), Some("no such host"));
    }

    #[test]
    fn api_bodies_are_never_stored() {
        let response = json_response(StatusCode::OK, &json!({ "ok": true }));
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }
}
