//! `GET /pair?code=<id>.<secret>` — the one-time exchange that turns a pairing
//! URL into a per-device cookie.
//!
//! This is the only route in the epic that mints a credential, so the rules it
//! follows are worth stating:
//!
//! * **It is public, but not free.** `/pair` is a `GET` outside `/api/`, so the
//!   auth layer serves it without a principal (a phone that has never paired
//!   has nothing to present). That makes the rate limiter this handler's own
//!   responsibility: a wrong or expired code is a failed authentication and is
//!   counted against the peer exactly as a wrong bearer token is.
//! * **A code is redeemed once, by [`PairingStore::consume`].** Success and
//!   expiry delete the file; a wrong secret does not, so someone who guesses an
//!   id cannot delete a code the operator is about to use.
//! * **Nothing about the code reaches a log.** The id is logged on success,
//!   because it is the device's identity from then on; the secret, the cookie
//!   and the URL never are.
//! * **The failure answer does not say which failure it was**, beyond expired
//!   versus not: a browser needs to tell "ask for a fresh link" from "that link
//!   is wrong", and nothing finer is any client's business.
//!
//! The cookie is `HttpOnly` (script cannot read it), `SameSite=Strict` (a
//! cross-site request never carries it, in browsers that predate
//! `Sec-Fetch-Site` as well as those that do not) and `Path=/`. `Secure` is
//! added when the operator advertises an https `public_url`, or when the
//! request arrived from loopback carrying `X-Forwarded-Proto: https` — which is
//! what `tailscale serve` in front of a loopback gateway looks like.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::gateway::auth::{unix_now, PairingError, PairingStore};
use crate::gateway::http::ApiError;
use crate::gateway::middleware::DEVICE_COOKIE_NAME;
use crate::gateway::server::{AppState, AuthState};

/// How long a device cookie lives without being renewed: one year, the same
/// order as the pairing it replaces. Revocation is `rotate-token`, not expiry.
const COOKIE_MAX_AGE_SECS: u64 = 31_536_000;

/// Where a paired browser is sent: the app, on the same origin it just paired
/// with. Relative on purpose — a redirect to a configured absolute URL would
/// be one more thing that can point somewhere else.
const PAIRED_LOCATION: &str = "/";

/// `GET /pair`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/pair", get(pair_handler))
}

#[derive(Debug, Deserialize)]
struct PairQuery {
    /// `<id>.<secret>`; absent when someone opened `/pair` by hand.
    code: Option<String>,
}

async fn pair_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    query: Result<Query<PairQuery>, QueryRejection>,
) -> Response {
    let peer_ip = peer.ip();
    let auth = Arc::clone(&state.auth);

    // The auth layer skips the limiter for a public path, so this route runs
    // it itself: redeeming a code is an authentication attempt.
    if let Err(retry_after) = check_limiter(&auth, peer_ip, Instant::now()) {
        tracing::warn!(
            target: "gateway",
            peer = %peer_ip,
            retry_after_secs = retry_after.as_secs(),
            "refusing a pairing attempt from a peer that failed too often"
        );
        return ApiError::too_many_requests(retry_after).into_response();
    }

    // A malformed query string and a missing `code` are the same thing to a
    // human: this URL is not a pairing link. Neither is a guess, so neither is
    // counted.
    let code = match query {
        Ok(Query(query)) => query.code.unwrap_or_default(),
        Err(_) => String::new(),
    };
    if code.is_empty() {
        return ApiError::new(StatusCode::BAD_REQUEST, "pairing_required")
            .with_message("open the pairing link `herdr gateway pair` printed")
            .into_response();
    }

    let secure = auth.secure_cookies || forwarded_https(&headers, peer_ip);
    let exchanged = tokio::task::spawn_blocking(move || exchange(&auth, &code)).await;

    match exchanged {
        Ok(Ok(paired)) => {
            tracing::info!(
                target: "gateway",
                peer = %peer_ip,
                device = %paired.device_id,
                scope = paired.scope,
                secure,
                "paired a device"
            );
            paired_response(&paired.cookie_value, secure)
        }
        Ok(Err(error)) => {
            // A code that did not redeem is a failed credential, exactly like a
            // wrong bearer token: count it, so guessing is bounded.
            record_failure(&state.auth, peer_ip, Instant::now());
            tracing::warn!(
                target: "gateway",
                peer = %peer_ip,
                code = error.code(),
                "refusing a pairing code"
            );
            error.into_response()
        }
        Err(error) => {
            tracing::error!(target: "gateway", error = %error, "the pairing exchange task failed");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error").into_response()
        }
    }
}

/// What a successful exchange produced.
struct Paired {
    /// `<id>.<secret>`, handed to the browser once and never recoverable.
    cookie_value: String,
    device_id: String,
    scope: &'static str,
}

/// Redeem `code` and mint a device. Blocking: it reads and writes the store.
fn exchange(auth: &AuthState, code: &str) -> Result<Paired, ApiError> {
    let store = PairingStore::new(&auth.gateway_dir);
    let now = unix_now();
    // Redeem *before* sweeping: a code that has just expired must be answered
    // as expired ("ask for a new link"), not as invalid ("that link is wrong"),
    // and a sweep would have deleted it first. Only the holder of the whole
    // code can see that difference — `consume` compares the secret before it
    // looks at the clock — so it tells an attacker nothing.
    let redeemed = store.consume(code, now);
    // Every entry point sweeps, so a store that is never redeemed from still
    // does not accumulate files.
    if let Err(error) = store.sweep_expired(now) {
        tracing::warn!(target: "gateway", error = %error, "could not sweep expired pairing codes");
    }

    let redeemed = match redeemed {
        Ok(redeemed) => redeemed,
        Err(PairingError::Expired) => {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "pairing_expired")
                .with_message("that pairing link has expired; ask for a new one"))
        }
        Err(PairingError::NotFound | PairingError::Invalid) => {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "pairing_invalid")
                .with_message("that pairing link is not valid"))
        }
        Err(PairingError::Io(error)) => {
            tracing::error!(target: "gateway", error = %error, "the pairing store could not be read");
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ));
        }
    };

    let mut devices = auth.devices.lock().unwrap_or_else(|err| err.into_inner());
    // Another process may have revoked devices since this one last looked;
    // minting on top of a stale set would write the revoked ones back.
    devices.refresh();
    let cookie_value = devices
        .insert(redeemed.scope, &redeemed.label, now)
        .map_err(|error| {
            tracing::error!(target: "gateway", error = %error, "could not record the paired device");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        })?;
    let device_id = cookie_value
        .split_once('.')
        .map(|(id, _)| id.to_string())
        .unwrap_or_default();
    Ok(Paired {
        cookie_value,
        device_id,
        scope: redeemed.scope.as_str(),
    })
}

/// `303 See Other` to the app, carrying the cookie.
///
/// A redirect rather than a body: the browser lands on the app with the cookie
/// already set, and the pairing URL — which is in the address bar, in history
/// and possibly in a screenshot — is replaced by one that carries no secret.
fn paired_response(cookie_value: &str, secure: bool) -> Response {
    let cookie = build_cookie(cookie_value, secure);
    let Ok(cookie) = HeaderValue::from_str(&cookie) else {
        // Unreachable: every part of the value is hex or a fixed attribute.
        tracing::error!(target: "gateway", "a device cookie was not a valid header value");
        return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error").into_response();
    };

    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let headers = response.headers_mut();
    headers.insert(header::LOCATION, HeaderValue::from_static(PAIRED_LOCATION));
    headers.insert(header::SET_COOKIE, cookie);
    // The answer contains a credential; nothing may keep a copy.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// The `Set-Cookie` value for a minted device.
///
/// Pure, so the attributes are pinned by a test rather than by reading a live
/// response: they are the whole cross-site defence for browsers that do not
/// send `Sec-Fetch-Site`.
pub(crate) fn build_cookie(cookie_value: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{DEVICE_COOKIE_NAME}={cookie_value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={COOKIE_MAX_AGE_SECS}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Whether a loopback proxy says the browser's leg was https.
///
/// Only from loopback: `X-Forwarded-Proto` is a header any client can set, and
/// the only proxy this gateway is designed to sit behind (`tailscale serve`)
/// reaches it over loopback. Getting this wrong in the permissive direction
/// marks a cookie `Secure` that a plain-http client then never sends back —
/// annoying, not dangerous — which is why the loopback test is the whole gate.
fn forwarded_https(headers: &HeaderMap, peer: IpAddr) -> bool {
    if !peer.is_loopback() {
        return false;
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .next()
                .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
        })
}

fn check_limiter(auth: &AuthState, peer: IpAddr, now: Instant) -> Result<(), std::time::Duration> {
    let mut limiter = auth.limiter.lock().unwrap_or_else(|err| err.into_inner());
    limiter.check(peer, now)
}

fn record_failure(auth: &AuthState, peer: IpAddr, now: Instant) {
    let mut limiter = auth.limiter.lock().unwrap_or_else(|err| err.into_inner());
    limiter.record_failure(peer, now);
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::gateway::auth::TokenScope;
    use crate::gateway::server::tests::TestServer;

    /// Mint a code in a running test server's own store and return the text a
    /// pairing URL would carry.
    fn create_code(server: &TestServer, scope: TokenScope, ttl_secs: u64, label: &str) -> String {
        PairingStore::new(&server.dir)
            .create(scope, label, ttl_secs, unix_now())
            .expect("create a pairing code")
            .0
    }

    fn cookie_header(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("a cookie pair")
            .to_string()
    }

    #[test]
    fn the_cookie_carries_every_attribute_that_defends_it() {
        let cookie = build_cookie("aa.bb", false);
        assert!(
            cookie.starts_with("herdr_gateway_device=aa.bb; "),
            "{cookie}"
        );
        for attribute in ["Path=/", "HttpOnly", "SameSite=Strict", "Max-Age=31536000"] {
            assert!(cookie.contains(attribute), "{attribute} missing: {cookie}");
        }
        assert!(
            !cookie.contains("Secure"),
            "a plain-http gateway must not mint a cookie the browser will not send back: {cookie}"
        );
        assert!(build_cookie("aa.bb", true).ends_with("; Secure"));
    }

    #[test]
    fn a_forwarded_https_header_only_counts_from_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(forwarded_https(&headers, "127.0.0.1".parse().expect("ip")));
        assert!(forwarded_https(&headers, "::1".parse().expect("ip")));
        assert!(!forwarded_https(
            &headers,
            "192.168.1.10".parse().expect("ip")
        ));

        // A proxy chain lists the client's leg first.
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(forwarded_https(&headers, "127.0.0.1".parse().expect("ip")));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert!(!forwarded_https(&headers, "127.0.0.1".parse().expect("ip")));
        assert!(!forwarded_https(
            &HeaderMap::new(),
            "::1".parse().expect("ip")
        ));
    }

    #[tokio::test]
    async fn a_valid_code_mints_a_cookie_that_authorizes_the_api() {
        let server = TestServer::start("pair-valid").await;
        let code = create_code(&server, TokenScope::Read, 600, "test phone");

        let response = server.get(&format!("/pair?code={code}"), &[]).await;
        assert_eq!(response.status, 303, "{}", response.body);
        assert_eq!(response.header("location"), Some("/"));
        assert_eq!(response.header("cache-control"), Some("no-store"));
        let set_cookie = response.header("set-cookie").expect("a Set-Cookie header");
        assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
        assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");

        let cookie = cookie_header(set_cookie);
        let fleet = server.get("/api/fleet", &[("Cookie", &cookie)]).await;
        assert_eq!(fleet.status, 200, "{}", fleet.body);

        let info = server.get("/api/gateway", &[("Cookie", &cookie)]).await;
        assert_eq!(info.status, 200, "{}", info.body);
        let json = info.json();
        assert_eq!(json["scope"].as_str(), Some("read"));
        assert_eq!(json["via"].as_str(), Some("device"));
        assert_eq!(json["device"]["label"].as_str(), Some("test phone"));
        assert_eq!(
            json["device"]["id"].as_str().map(str::len),
            Some(64),
            "{json}"
        );
        assert!(
            json["features"]
                .as_array()
                .is_some_and(|features| features.iter().any(|name| name == "pairing")),
            "{json}"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_control_code_mints_a_control_device() {
        let server = TestServer::start("pair-control").await;
        let code = create_code(&server, TokenScope::Control, 600, "");

        let response = server.get(&format!("/pair?code={code}"), &[]).await;
        assert_eq!(response.status, 303, "{}", response.body);
        let cookie = cookie_header(response.header("set-cookie").expect("a Set-Cookie header"));

        let info = server.get("/api/gateway", &[("Cookie", &cookie)]).await;
        assert_eq!(info.json()["scope"].as_str(), Some("control"));
        // An empty label stays empty rather than becoming a guess.
        assert_eq!(info.json()["device"]["label"].as_str(), Some(""));

        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_code_is_redeemable_exactly_once() {
        let server = TestServer::start("pair-once").await;
        let code = create_code(&server, TokenScope::Read, 600, "");

        assert_eq!(
            server.get(&format!("/pair?code={code}"), &[]).await.status,
            303
        );
        let again = server.get(&format!("/pair?code={code}"), &[]).await;
        assert_eq!(again.status, 403, "{}", again.body);
        assert_eq!(again.json()["error"].as_str(), Some("pairing_invalid"));
        assert!(again.header("set-cookie").is_none());

        server.shutdown().await;
    }

    #[tokio::test]
    async fn an_expired_code_says_so_and_is_swept() {
        let server = TestServer::start("pair-expired").await;
        let store = PairingStore::new(&server.dir);
        let (code, _) = store
            .create(TokenScope::Read, "", 30, unix_now() - 60)
            .expect("create an already expired code");

        let response = server.get(&format!("/pair?code={code}"), &[]).await;
        assert_eq!(response.status, 403, "{}", response.body);
        assert_eq!(response.json()["error"].as_str(), Some("pairing_expired"));
        assert_eq!(store.pending(unix_now()).expect("pending"), 0);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_missing_or_malformed_code_is_a_bad_request_not_a_guess() {
        let server = TestServer::start("pair-missing").await;

        for path in ["/pair", "/pair?code=", "/pair?other=1"] {
            let response = server.get(path, &[]).await;
            assert_eq!(response.status, 400, "{path}: {}", response.body);
            assert_eq!(response.json()["error"].as_str(), Some("pairing_required"));
        }

        // None of those counted, so a real code still works afterwards.
        let code = create_code(&server, TokenScope::Read, 600, "");
        assert_eq!(
            server.get(&format!("/pair?code={code}"), &[]).await.status,
            303
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn wrong_codes_are_rate_limited_like_wrong_tokens() {
        let server = TestServer::start("pair-limited").await;
        let wrong = format!("{}.{}", "a".repeat(64), "b".repeat(64));

        let mut statuses = Vec::new();
        for _ in 0..5 {
            statuses.push(server.get(&format!("/pair?code={wrong}"), &[]).await.status);
        }
        assert_eq!(statuses, vec![403, 403, 403, 403, 403], "{statuses:?}");

        // The sixth attempt is refused before the code is looked at, and so is
        // a real code from the same peer: the limiter is per peer, not per
        // credential.
        let blocked = server.get(&format!("/pair?code={wrong}"), &[]).await;
        assert_eq!(blocked.status, 429, "{}", blocked.body);
        assert!(blocked.header("retry-after").is_some());
        let code = create_code(&server, TokenScope::Read, 600, "");
        assert_eq!(
            server.get(&format!("/pair?code={code}"), &[]).await.status,
            429
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_revoked_device_cookie_stops_working_without_a_restart() {
        let server = TestServer::start("pair-revoked").await;
        let code = create_code(&server, TokenScope::Read, 600, "");
        let response = server.get(&format!("/pair?code={code}"), &[]).await;
        let cookie = cookie_header(response.header("set-cookie").expect("a Set-Cookie header"));
        assert_eq!(
            server
                .get("/api/fleet", &[("Cookie", &cookie)])
                .await
                .status,
            200
        );

        // What `herdr gateway rotate-token read` does, from another process:
        // rewrite `devices.json` without this scope.
        {
            let mut devices =
                crate::gateway::auth::DeviceStore::load(&server.dir).expect("load devices");
            assert_eq!(devices.revoke_scope(TokenScope::Read).expect("revoke"), 1);
        }

        let refused = server.get("/api/fleet", &[("Cookie", &cookie)]).await;
        assert_eq!(refused.status, 401, "{}", refused.body);

        server.shutdown().await;
    }
}
