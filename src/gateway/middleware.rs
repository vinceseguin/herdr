//! Who is calling, and whether they may.
//!
//! One layer wraps the whole router, so a route cannot be added without going
//! through it. It answers four questions in a fixed order, and the order is
//! the security property:
//!
//! 1. **Origin.** A request that carries an `Origin` header was made by a
//!    browser page. It must be on the allowlist, or it is refused — before any
//!    credential is looked at, because a hostile page can carry the victim's
//!    cookie. An origin failure deliberately does **not** count against the
//!    failure limiter: it is not a credential guess, and letting a page drive
//!    the limiter would let any web site lock the operator out of their own
//!    gateway. Requests with no `Origin` (curl, a native client) pass this
//!    step and are decided by their token alone; only browsers send it.
//! 2. **Public paths.** `/health` and the static app are served without a
//!    principal. Everything under `/api/` needs one.
//! 3. **Rate limit.** A peer that has failed too often in the window is
//!    refused with `429` and a `Retry-After`, before its next guess is
//!    compared against anything.
//! 4. **Credential.** A `Authorization: Bearer <token>` first, then the
//!    `herdr_gateway_device` cookie PR 8 mints. Both compare digests in
//!    constant time. A miss records a failure and answers `401`.
//!
//! What the layer never does: read a token from the query string or from any
//! header a page can set cross-origin (which is why only `Authorization` and
//! `Cookie` are consulted), log a credential, or say *why* a credential failed.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{header, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::gateway::auth::{unix_now, Credential, Principal, TokenScope};
use crate::gateway::http::ApiError;
use crate::gateway::server::{AppState, AuthState};

/// Cookie a paired device presents, as `<id>.<secret>` (PR 8 mints it).
pub(crate) const DEVICE_COOKIE_NAME: &str = "herdr_gateway_device";

/// How rarely a device's `last_seen` reaches the disk. The gateway must not
/// write a file per request; a minute of drift on a "last seen" timestamp is
/// invisible to a human and costs one small write.
const DEVICE_TOUCH_PERSIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The authenticated caller, put into the request's extensions by
/// [`authenticate`] and pulled back out by every `/api/*` handler.
///
/// A handler that takes this argument cannot be reached unauthenticated: the
/// extension is only ever inserted after a credential verified, and the
/// extractor's rejection is a `401`. That is the route-level half of the scope
/// gate; [`require`] is the other half.
#[derive(Debug, Clone)]
pub(crate) struct Authed(pub(crate) Principal);

impl<S> FromRequestParts<S> for Authed
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Authed>()
            .cloned()
            .ok_or_else(ApiError::unauthorized)
    }
}

/// Refuse a principal whose scope is narrower than `needed`.
///
/// The route-level gate. A stream that keeps deciding after the handshake
/// (PR 7's terminal input) must re-check inside its own state machine rather
/// than trust this one call.
pub(crate) fn require(principal: &Principal, needed: TokenScope) -> Result<(), ApiError> {
    if principal.allows(needed) {
        Ok(())
    } else {
        Err(ApiError::forbidden(needed))
    }
}

/// The auth layer. See the module docs for the order and why it is that order.
pub(crate) async fn authenticate(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth = Arc::clone(&state.auth);
    let path = request.uri().path().to_string();
    let peer_ip = peer.ip();

    // 1. Origin.
    if let Some(origin) = header_str(&request, header::ORIGIN) {
        if !auth.origins.allows(&origin) {
            tracing::warn!(
                target: "gateway",
                peer = %peer_ip,
                origin = %origin,
                path = %path,
                "refusing a request from an origin that is not allowed"
            );
            return ApiError::origin_not_allowed().into_response();
        }
    }

    // 2. Public paths.
    if is_public(&path, request.method()) {
        return next.run(request).await;
    }

    // 3. Rate limit.
    let now = Instant::now();
    if let Err(retry_after) = check_limiter(&auth, peer_ip, now) {
        tracing::warn!(
            target: "gateway",
            peer = %peer_ip,
            path = %path,
            retry_after_secs = retry_after.as_secs(),
            "refusing a peer that failed authentication too often"
        );
        return ApiError::too_many_requests(retry_after).into_response();
    }

    // 4. Credential.
    let Some(principal) = verify(&auth, &request) else {
        record_failure(&auth, peer_ip, now);
        tracing::warn!(
            target: "gateway",
            peer = %peer_ip,
            path = %path,
            "refusing a request with no valid credential"
        );
        return ApiError::unauthorized().into_response();
    };

    if let Credential::Device { id } = &principal.via {
        touch_device(&state, id);
    }
    tracing::debug!(
        target: "gateway",
        peer = %peer_ip,
        path = %path,
        scope = principal.scope.as_str(),
        "authenticated request"
    );
    request.extensions_mut().insert(Authed(principal));
    next.run(request).await
}

/// Whether a path is served without a principal.
///
/// `/health` is liveness for a supervisor. Everything else public is the
/// embedded app itself — a `GET`/`HEAD` outside `/api/`, which is how PR 8's
/// `/pair` page is reachable too. Anything under `/api/` always needs a
/// principal, and so does any non-`GET` method.
fn is_public(path: &str, method: &Method) -> bool {
    if path == "/health" {
        return true;
    }
    if path == "/api" || path.starts_with("/api/") {
        return false;
    }
    matches!(*method, Method::GET | Method::HEAD)
}

/// The credential a request proves, if any.
///
/// Bearer first, then the device cookie: a request that presents a bad bearer
/// token is not silently upgraded by a cookie it also happens to carry, which
/// would make a failed token look like a success in the logs.
fn verify(auth: &AuthState, request: &Request) -> Option<Principal> {
    if let Some(value) = header_str(request, header::AUTHORIZATION) {
        let presented = bearer_token(&value)?;
        return auth.tokens.verify_bearer(presented).map(Principal::bearer);
    }
    let cookie = header_str(request, header::COOKIE)?;
    let value = cookie_value(&cookie, DEVICE_COOKIE_NAME)?;
    let devices = auth.devices.lock().unwrap_or_else(|err| err.into_inner());
    devices
        .verify_cookie(&value)
        .map(|(scope, id)| Principal::device(scope, id))
}

/// The token from an `Authorization` header, or `None` for any other scheme.
///
/// The scheme is matched case-insensitively (RFC 7235 says it is
/// case-insensitive) but the token itself is taken verbatim: trimming or
/// unquoting it would make two different strings authenticate the same.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim_start_matches(' ');
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// One cookie's value out of a `Cookie` header.
///
/// Names are matched exactly; a value is taken up to the next `;` and its
/// surrounding whitespace is trimmed, which is all RFC 6265 allows between
/// pairs.
fn cookie_value(header: &str, name: &str) -> Option<String> {
    for pair in header.split(';') {
        let pair = pair.trim();
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key.trim() == name {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn header_str(request: &Request, name: header::HeaderName) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn check_limiter(auth: &AuthState, peer: IpAddr, now: Instant) -> Result<(), std::time::Duration> {
    let mut limiter = auth.limiter.lock().unwrap_or_else(|err| err.into_inner());
    limiter.check(peer, now)
}

fn record_failure(auth: &AuthState, peer: IpAddr, now: Instant) {
    let mut limiter = auth.limiter.lock().unwrap_or_else(|err| err.into_inner());
    limiter.record_failure(peer, now);
}

/// Record that a paired device was seen, and persist that at most once a
/// minute.
///
/// The write runs on the blocking pool: it is a `fsync`ed rename, and a
/// request in flight must never wait on a disk.
fn touch_device(state: &AppState, id: &str) {
    let auth = Arc::clone(&state.auth);
    let now_unix = unix_now();
    let should_persist = {
        let mut devices = auth.devices.lock().unwrap_or_else(|err| err.into_inner());
        if !devices.touch(id, now_unix) {
            return;
        }
        drop(devices);
        let mut last = auth
            .devices_persisted_at
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let due = last.is_none_or(|at| at.elapsed() >= DEVICE_TOUCH_PERSIST_INTERVAL);
        if due {
            *last = Some(Instant::now());
        }
        due
    };
    if !should_persist {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let devices = auth.devices.lock().unwrap_or_else(|err| err.into_inner());
        if let Err(error) = devices.persist() {
            tracing::warn!(target: "gateway", error = %error, "could not record device activity");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_and_the_app_are_public_but_the_api_is_not() {
        assert!(is_public("/health", &Method::GET));
        assert!(is_public("/", &Method::GET));
        assert!(is_public("/settings", &Method::GET));
        assert!(is_public("/pair", &Method::GET));
        assert!(is_public("/index.html", &Method::HEAD));

        assert!(!is_public("/api/fleet", &Method::GET));
        assert!(!is_public("/api/gateway", &Method::GET));
        assert!(!is_public("/api", &Method::GET));
        // A write to the app's own path is not a static read.
        assert!(!is_public("/", &Method::POST));
        assert!(!is_public("/pair", &Method::POST));
    }

    /// `/apirandom` is not under `/api/`; it must not be mistaken for it in
    /// either direction.
    #[test]
    fn the_api_prefix_is_a_path_segment_not_a_string_prefix() {
        assert!(is_public("/apidocs", &Method::GET));
        assert!(!is_public("/api/", &Method::GET));
    }

    #[test]
    fn bearer_tokens_are_taken_verbatim_after_a_case_insensitive_scheme() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER  abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer abc def"), Some("abc def"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token(""), None);
    }

    #[test]
    fn cookie_values_are_matched_on_the_exact_name() {
        let header = "a=1; herdr_gateway_device=id.secret; b=2";
        assert_eq!(
            cookie_value(header, DEVICE_COOKIE_NAME).as_deref(),
            Some("id.secret")
        );
        assert_eq!(cookie_value(header, "a").as_deref(), Some("1"));
        assert_eq!(cookie_value(header, "herdr_gateway").as_deref(), None);
        assert_eq!(cookie_value("", DEVICE_COOKIE_NAME), None);
        assert_eq!(cookie_value("novalue", DEVICE_COOKIE_NAME), None);
        // A suffix match must not win.
        assert_eq!(
            cookie_value("xherdr_gateway_device=nope", DEVICE_COOKIE_NAME),
            None
        );
    }

    #[test]
    fn require_gates_on_the_scope_a_principal_proved() {
        let read = Principal::bearer(TokenScope::Read);
        let control = Principal::bearer(TokenScope::Control);
        assert!(require(&read, TokenScope::Read).is_ok());
        assert!(require(&control, TokenScope::Read).is_ok());
        assert!(require(&control, TokenScope::Control).is_ok());

        let error = require(&read, TokenScope::Control).expect_err("read may not control");
        assert_eq!(error.code(), "forbidden");
        assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
    }
}
