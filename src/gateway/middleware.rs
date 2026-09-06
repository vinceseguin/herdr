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
//!    constant time. A credential that was presented and proved nothing
//!    records a failure and answers `401`; a request that presented **no**
//!    credential answers `401` without recording one. Only a guess counts:
//!    a page cannot attach a header to an `<img>` or a form it points at the
//!    gateway, but it can make the browser send those requests — with no
//!    `Origin` — five times, and counting them would let any web site lock
//!    the operator's own address out.
//!
//! The cookie has one more gate. A browser that sends no `Origin` still says
//! where a request came from in `Sec-Fetch-Site`; a device cookie on a
//! request marked `cross-site` or `same-site` is a page on another site
//! riding the operator's browser (an `<img>`, a form, a link), so it is
//! refused before the cookie is compared and is not counted either. The
//! cookie PR 8 mints must also be `SameSite=Strict` and `HttpOnly`, which
//! closes the same door in browsers that predate `Sec-Fetch-Site`.
//!
//! What the layer never does: read a token from the query string or from any
//! header a page can set cross-origin (which is why only `Authorization` and
//! `Cookie` are consulted), log a credential, or say *why* a credential failed.
//!
//! Every path decision here is made on the **normalized** path
//! ([`normalized_path`]), never on the raw request target: `/api%2ffleet`,
//! `//api/fleet` and `/x/../api/fleet` must all be treated as `/api/fleet`.
//! Today axum's router does not fold them together — measured, not assumed —
//! so none of them reaches a handler; deciding on the raw path anyway would
//! mean a future router that *does* fold them turns this layer's "public"
//! answer into an authentication bypass. Normalizing here can only ever demand
//! a credential for more paths than the router serves, which is the safe
//! direction to be wrong in.

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
    let path = normalized_path(request.uri().path());
    let peer_ip = peer.ip();

    // 1. Origin.
    let origin = match origin_header(&request) {
        Ok(origin) => origin,
        Err(reason) => {
            tracing::warn!(
                target: "gateway",
                peer = %peer_ip,
                path = %path,
                reason,
                "refusing a request whose Origin header cannot be trusted"
            );
            return ApiError::origin_not_allowed().into_response();
        }
    };
    if let Some(origin) = &origin {
        if !auth.origins.allows(origin) {
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
    let principal = match verify(&auth, &request, origin.is_some()) {
        Verification::Verified(principal) => principal,
        Verification::Rejected => {
            record_failure(&auth, peer_ip, now);
            tracing::warn!(
                target: "gateway",
                peer = %peer_ip,
                path = %path,
                "refusing a request whose credential did not verify"
            );
            return ApiError::unauthorized().into_response();
        }
        Verification::Absent => {
            // Not a guess, so not a failure: the app shell itself asks
            // `/api/gateway` without a credential on every load.
            tracing::debug!(
                target: "gateway",
                peer = %peer_ip,
                path = %path,
                "refusing a request with no credential"
            );
            return ApiError::unauthorized().into_response();
        }
        Verification::CrossSiteCookie => {
            tracing::warn!(
                target: "gateway",
                peer = %peer_ip,
                path = %path,
                "refusing a device cookie on a cross-site request that carries no Origin"
            );
            return ApiError::origin_not_allowed().into_response();
        }
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

/// Whether a path is served without a principal. Takes a [`normalized_path`].
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

/// A request path with percent-escapes decoded, empty and `.` segments
/// dropped, and `..` segments resolved.
///
/// Pure, one bounded allocation per request, and it never touches the
/// filesystem — this is a string decision, not a lookup. A `..` that would
/// escape the root is dropped rather than kept, so the result always starts at
/// `/`. An escape that decodes to bytes that are not UTF-8 leaves the path
/// exactly as written: it cannot match a route either way, and guessing at it
/// would invent a second spelling of the same path.
pub(crate) fn normalized_path(path: &str) -> String {
    let decoded = percent_decode(path);
    let mut segments: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut normalized = String::with_capacity(decoded.len() + 1);
    for segment in segments {
        normalized.push('/');
        normalized.push_str(segment);
    }
    if normalized.is_empty() {
        normalized.push('/');
    }
    normalized
}

/// Decode `%XX` escapes, leaving anything that is not a well-formed escape —
/// or that does not decode to UTF-8 — exactly as it was written.
fn percent_decode(path: &str) -> String {
    if !path.contains('%') {
        return path.to_string();
    }
    let bytes = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| path.to_string())
}

/// What looking at a request's credentials decided.
enum Verification {
    /// A credential verified.
    Verified(Principal),
    /// A credential was presented and proved nothing: a guess, counted
    /// against the peer.
    Rejected,
    /// Neither an `Authorization` header nor the device cookie: refused, but
    /// nothing was guessed.
    Absent,
    /// A device cookie on a request the browser marked as coming from another
    /// site while carrying no `Origin`: refused before the cookie is
    /// compared, and not counted.
    CrossSiteCookie,
}

/// The credential a request proves, if any.
///
/// Bearer first, then the device cookie: a request that presents a bad bearer
/// token is not silently upgraded by a cookie it also happens to carry, which
/// would make a failed token look like a success in the logs. `origin_present`
/// says whether step 1 already vouched for the page behind a browser request;
/// without it, the cookie is only honoured when `Sec-Fetch-Site` does not say
/// the request came from another site.
fn verify(auth: &AuthState, request: &Request, origin_present: bool) -> Verification {
    if let Some(value) = header_str(request, header::AUTHORIZATION) {
        let Some(presented) = bearer_token(&value) else {
            return Verification::Rejected;
        };
        let mut tokens = auth.tokens.lock().unwrap_or_else(|err| err.into_inner());
        // A `rotate-token` in another process must be in force by the next
        // request, not by the next restart: two `stat`s, and a read only when
        // a file was actually replaced.
        tokens.refresh();
        return match tokens.verify_bearer(presented) {
            Some(scope) => Verification::Verified(Principal::bearer(scope)),
            None => Verification::Rejected,
        };
    }
    let Some(cookie) = header_str(request, header::COOKIE) else {
        return Verification::Absent;
    };
    let Some(value) = cookie_value(&cookie, DEVICE_COOKIE_NAME) else {
        return Verification::Absent;
    };
    if !origin_present && is_cross_site(request) {
        return Verification::CrossSiteCookie;
    }
    let mut devices = auth.devices.lock().unwrap_or_else(|err| err.into_inner());
    // Same reason as the tokens above: `rotate-token` revokes this scope's
    // devices from another process.
    devices.refresh();
    match devices.verify_cookie(&value) {
        Some((scope, id)) => Verification::Verified(Principal::device(scope, id)),
        None => Verification::Rejected,
    }
}

/// Whether the browser says this request was made by a page on another site.
///
/// `Sec-Fetch-Site` is set by the browser and cannot be set by a page. Only
/// `same-origin` and `none` (a URL the user typed, a bookmark) are the
/// operator's own doing; `same-site` is refused along with `cross-site`
/// because a sibling host on the operator's domain is not the gateway. A
/// request without the header (curl, an old browser) is not marked either
/// way and is decided by its credential alone.
fn is_cross_site(request: &Request) -> bool {
    request
        .headers()
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|site| {
            let site = site.trim();
            site.eq_ignore_ascii_case("cross-site") || site.eq_ignore_ascii_case("same-site")
        })
}

/// The one `Origin` header, or `Ok(None)` when there is none.
///
/// A browser sends at most one. Two of them, or one that is not readable
/// ASCII, is not something to pick the friendliest value out of: it is an
/// `Err`, and the request is refused as if the origin were foreign.
fn origin_header(request: &Request) -> Result<Option<String>, &'static str> {
    let mut values = request.headers().get_all(header::ORIGIN).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("more than one Origin header");
    }
    first
        .to_str()
        .map(|value| Some(value.to_string()))
        .map_err(|_| "Origin header is not visible ASCII")
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
        // Never write back a set of records another process has already
        // rewritten: refresh first, so a device revoked between the cookie
        // check and this write stays revoked.
        devices.refresh();
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
        let mut devices = auth.devices.lock().unwrap_or_else(|err| err.into_inner());
        // The file can have been rewritten — a `rotate-token` revocation —
        // while this task waited for the blocking pool. Writing the copy this
        // process holds would put the records that revocation removed back.
        if devices.refresh() {
            return;
        }
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

    /// Every spelling of the fleet route normalizes to the one path the layer
    /// decides on, so none of them can be classified public.
    #[test]
    fn encoded_and_dotted_spellings_of_an_api_path_are_never_public() {
        for spelling in [
            "/api%2ffleet",
            "/api%2Ffleet",
            "//api/fleet",
            "/./api/fleet",
            "/x/../api/fleet",
            "/api/./fleet",
            "/api/x/../fleet",
            "/%61pi/fleet",
        ] {
            let normalized = normalized_path(spelling);
            assert_eq!(normalized, "/api/fleet", "spelling: {spelling}");
            assert!(
                !is_public(&normalized, &Method::GET),
                "spelling: {spelling}"
            );
        }
    }

    #[test]
    fn normalization_keeps_ordinary_paths_and_cannot_escape_the_root() {
        assert_eq!(normalized_path("/"), "/");
        assert_eq!(normalized_path(""), "/");
        assert_eq!(normalized_path("/index.html"), "/index.html");
        assert_eq!(normalized_path("/a/b/c"), "/a/b/c");
        assert_eq!(normalized_path("/a/b/"), "/a/b");
        // A `..` past the root is dropped, never kept.
        assert_eq!(normalized_path("/../../etc/passwd"), "/etc/passwd");
        assert_eq!(normalized_path("/.."), "/");
        // A malformed escape is left alone rather than guessed at.
        assert_eq!(normalized_path("/a%zz"), "/a%zz");
        assert_eq!(normalized_path("/a%2"), "/a%2");
        // An escape that is not UTF-8 leaves the whole path as written.
        assert_eq!(normalized_path("/a%ff"), "/a%ff");
        // A space stays a space; it does not become a separator.
        assert_eq!(normalized_path("/api/fleet%20"), "/api/fleet ");
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

    fn request_with(headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri("/api/fleet");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(axum::body::Body::empty()).expect("request")
    }

    #[test]
    fn only_the_browser_marking_another_site_counts_as_cross_site() {
        assert!(is_cross_site(&request_with(&[(
            "Sec-Fetch-Site",
            "cross-site"
        )])));
        assert!(is_cross_site(&request_with(&[(
            "Sec-Fetch-Site",
            "same-site"
        )])));
        assert!(is_cross_site(&request_with(&[(
            "Sec-Fetch-Site",
            "Cross-Site"
        )])));
        assert!(!is_cross_site(&request_with(&[(
            "Sec-Fetch-Site",
            "same-origin"
        )])));
        assert!(!is_cross_site(&request_with(&[("Sec-Fetch-Site", "none")])));
        assert!(!is_cross_site(&request_with(&[])));
    }

    #[test]
    fn a_single_readable_origin_is_the_only_acceptable_shape() {
        assert_eq!(origin_header(&request_with(&[])), Ok(None));
        assert_eq!(
            origin_header(&request_with(&[("Origin", "https://fleet.example")])),
            Ok(Some("https://fleet.example".to_string()))
        );
        assert!(origin_header(&request_with(&[
            ("Origin", "https://fleet.example"),
            ("Origin", "https://evil.example"),
        ]))
        .is_err());
        let mut request = request_with(&[]);
        request.headers_mut().insert(
            header::ORIGIN,
            axum::http::HeaderValue::from_bytes(b"https://\xffevil.example").expect("opaque"),
        );
        assert!(origin_header(&request).is_err());
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
