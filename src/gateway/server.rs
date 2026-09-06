//! The HTTP server: shared state, the router, and how it stops.
//!
//! [`AppState`] is the only thing a handler sees, and it is cheap to clone: a
//! [`FleetHandle`] (an `Arc` pair), an `Arc<AuthState>` and an
//! `Arc<GatewayInfo>`. Nothing per-request is allocated here and no handler
//! ever holds a lock across an `.await`.
//!
//! [`router`] is the merge point every later PR extends: each area module
//! exposes `routes() -> Router<AppState>` and is merged in, so the auth layer
//! and the body limit apply to it without that PR touching this function.

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::DefaultBodyLimit;
use axum::Router;
use tokio::net::TcpListener;

use crate::config::GatewayConfig;
use crate::gateway::auth::{AuthLimiter, DeviceStore, TokenStore};
use crate::gateway::fleet::FleetHandle;
use crate::gateway::policy::OriginAllowlist;
use crate::gateway::transports::HostTransports;
use crate::gateway::{assets, events, http, middleware, terminal};

/// Largest request body the gateway accepts.
///
/// Every request it answers today is a `GET`; the limit exists so a future
/// body-carrying route inherits a bound rather than choosing one, and so a
/// client cannot make the process buffer more than this.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;

/// How long a stopping gateway waits for in-flight connections before it
/// drops them.
///
/// Graceful shutdown alone waits for every open connection, and a client that
/// opened one and never finished its request would hold the process — and the
/// fleet supervisors behind it — until a supervisor lost patience and killed
/// it. A bound turns that into a warning and a clean exit.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// Everything a handler needs, and nothing more.
#[derive(Clone)]
pub(crate) struct AppState {
    /// The merged fleet. Cloning is two `Arc` bumps; every method is sync.
    pub(crate) fleet: FleetHandle,
    pub(crate) auth: Arc<AuthState>,
    pub(crate) info: Arc<GatewayInfo>,
    /// How a terminal stream opens its own connection to one host.
    ///
    /// Separate from the fleet connector on purpose: a client socket's mode is
    /// fixed by its first message, so a terminal cannot ride the aggregator's
    /// stream. Lazy — a gateway nobody opens a terminal on connects nothing
    /// extra. `run` keeps the other half of this `Arc` so it can stop the
    /// sessions and drop the transports in that order.
    pub(crate) transports: Arc<HostTransports>,
}

/// Credentials, devices, the failure limiter and the origin allowlist.
///
/// The two mutexes are `std::sync::Mutex` on purpose: every critical section
/// is a few statements of pure data work, none is held across an `.await`, and
/// the one write to disk runs on the blocking pool.
pub(crate) struct AuthState {
    /// Bearer token digests. Immutable for the life of the process: PR 8's
    /// `rotate-token` rewrites the files and the operator restarts, which is
    /// also what makes rotation an unambiguous revocation.
    pub(crate) tokens: TokenStore,
    pub(crate) devices: Mutex<DeviceStore>,
    pub(crate) limiter: Mutex<AuthLimiter>,
    pub(crate) origins: OriginAllowlist,
    /// When a device `last_seen` was last written, so the middleware can
    /// debounce that write.
    pub(crate) devices_persisted_at: Mutex<Option<Instant>>,
}

impl AuthState {
    pub(crate) fn new(
        tokens: TokenStore,
        devices: DeviceStore,
        config: &GatewayConfig,
        origins: OriginAllowlist,
    ) -> Self {
        Self {
            tokens,
            devices: Mutex::new(devices),
            limiter: Mutex::new(AuthLimiter::from_config(config)),
            origins,
            devices_persisted_at: Mutex::new(None),
        }
    }
}

/// What `GET /api/gateway` reports: facts about this gateway, not about the
/// fleet behind it.
pub(crate) struct GatewayInfo {
    pub(crate) client_version: String,
    /// Whether the bound address is loopback. A client uses it to decide
    /// whether it is talking to something reachable from elsewhere.
    pub(crate) loopback: bool,
    /// `[gateway] public_url`, or empty.
    pub(crate) public_url: String,
    /// Capability names, appended by later PRs and never removed.
    pub(crate) features: Vec<&'static str>,
}

impl GatewayInfo {
    /// The features this build serves.
    ///
    /// PR 8 appends `"pairing"`.
    pub(crate) fn features() -> Vec<&'static str> {
        vec!["fleet", "events", "terminal"]
    }

    pub(crate) fn new(bind: SocketAddr, config: &GatewayConfig) -> Self {
        Self {
            client_version: crate::build_info::version(),
            loopback: bind.ip().is_loopback(),
            public_url: config.public_url.clone(),
            features: Self::features(),
        }
    }
}

/// Every route, behind the auth layer and the body limit.
///
/// Layer order matters: `DefaultBodyLimit` is outermost so a body is bounded
/// before anything reads it, and the auth layer wraps every route including
/// the asset fallback, so no route can be added outside it by accident.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .merge(http::routes())
        .merge(events::routes())
        .merge(terminal::routes())
        .fallback(assets::serve)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::authenticate,
        ))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state)
}

/// Serve until `shutdown` resolves, then drain in-flight requests.
///
/// `into_make_service_with_connect_info` is what puts the peer address in the
/// request extensions; the auth layer's failure limiter is per peer, so this
/// is not optional.
pub(crate) async fn serve<F>(listener: TcpListener, state: AppState, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_with_drain(listener, state, shutdown, SHUTDOWN_DRAIN).await
}

/// [`serve`] with an explicit drain bound, so a test does not wait five
/// seconds to see it hold.
async fn serve_with_drain<F>(
    listener: TcpListener,
    state: AppState,
    shutdown: F,
    drain: Duration,
) -> io::Result<()>
where
    F: Send + Future<Output = ()> + 'static,
{
    let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel::<()>();
    let graceful = async move {
        shutdown.await;
        let _ = stopping_tx.send(());
    };
    // `into_future()` because `WithGracefulShutdown` is `IntoFuture`, not
    // `Future`: it only becomes pollable — and only then spawns the task that
    // watches the signal — once it is converted.
    let server = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(graceful)
    .into_future();
    tokio::pin!(server);

    let deadline = async move {
        // An `Err` means the graceful future was dropped without firing,
        // which only happens once the server has already returned.
        if stopping_rx.await.is_ok() {
            tokio::time::sleep(drain).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        outcome = &mut server => outcome,
        () = deadline => {
            tracing::warn!(
                target: "gateway",
                drain_secs = drain.as_secs_f64(),
                "connections were still open after the drain period; dropping them"
            );
            Ok(())
        }
    }
}

/// A future that resolves on `SIGINT` (Ctrl-C) or `SIGTERM` (what systemd and
/// `kill` send).
///
/// **Registration happens in this call, not on the first poll.** The caller
/// prints its listen address before it starts serving, and a supervisor — or a
/// test — that stops the process the instant it sees that line would otherwise
/// race a lazily installed handler and hit the default disposition, killing
/// the gateway instead of draining it.
///
/// A signal that cannot be registered is logged and then never fires, so a
/// broken handler looks like "no shutdown request", never like one.
#[cfg(unix)]
pub(crate) fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
    use tokio::signal::unix::{signal, Signal, SignalKind};

    fn register(kind: SignalKind, name: &'static str) -> Option<Signal> {
        match signal(kind) {
            Ok(stream) => Some(stream),
            Err(error) => {
                tracing::warn!(
                    target: "gateway",
                    error = %error,
                    signal = name,
                    "could not listen for this signal; it will not stop the gateway"
                );
                None
            }
        }
    }

    let mut interrupt = register(SignalKind::interrupt(), "SIGINT");
    let mut terminate = register(SignalKind::terminate(), "SIGTERM");

    async move {
        match (interrupt.as_mut(), terminate.as_mut()) {
            (Some(interrupt), Some(terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
            }
            (Some(only), None) | (None, Some(only)) => {
                only.recv().await;
            }
            (None, None) => std::future::pending::<()>().await,
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
    async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(target: "gateway", error = %error, "could not listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    use crate::config::Config;
    use crate::gateway::auth::{unix_now, TokenScope};
    use crate::gateway::fleet::FleetRuntime;
    use crate::gateway::middleware::DEVICE_COOKIE_NAME;

    /// A router on a real loopback port, with real tokens in a throwaway
    /// directory and an empty fleet.
    ///
    /// Real sockets rather than a `tower::Service` call: the epic adds no
    /// dev-dependency, and the properties under test (a peer address for the
    /// limiter, header parsing, status codes) are exactly the ones a
    /// service-level call would fake.
    pub(crate) struct TestServer {
        pub(crate) addr: SocketAddr,
        pub(crate) dir: std::path::PathBuf,
        pub(crate) read_token: String,
        pub(crate) control_token: String,
        /// A `read`-scope device cookie value (`<id>.<secret>`), when the
        /// server was started with one paired.
        pub(crate) device_cookie: Option<String>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        served: Option<tokio::task::JoinHandle<()>>,
        fleet: Option<FleetRuntime>,
    }

    impl TestServer {
        pub(crate) async fn start(name: &str) -> Self {
            Self::start_with(name, GatewayConfig::default(), false, SHUTDOWN_DRAIN).await
        }

        /// A server with one `read`-scope device already paired, as PR 8's
        /// exchange would leave it.
        pub(crate) async fn start_paired(name: &str) -> Self {
            Self::start_with(name, GatewayConfig::default(), true, SHUTDOWN_DRAIN).await
        }

        pub(crate) async fn start_with(
            name: &str,
            gateway: GatewayConfig,
            pair_device: bool,
            drain: Duration,
        ) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-gateway-server-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let tokens = TokenStore::load_or_create(&dir).expect("token store");
            let read_token = std::fs::read_to_string(dir.join("read.token")).expect("read token");
            let control_token =
                std::fs::read_to_string(dir.join("control.token")).expect("control token");
            let mut devices = DeviceStore::load(&dir).expect("device store");
            let device_cookie = pair_device.then(|| {
                devices
                    .insert(TokenScope::Read, "test phone", unix_now())
                    .expect("pair a device")
            });

            // An empty fleet: no host is configured, so the runtime starts no
            // supervisor, opens no socket and still exercises the real
            // `FleetHandle` the handlers use.
            let mut config = Config::default();
            config.fleet.include_local = false;
            config.gateway = gateway;
            let fleet = FleetRuntime::start(&config).expect("an empty fleet resolves");

            let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
            let addr = listener.local_addr().expect("local addr");
            let origins = OriginAllowlist::for_bind(addr, &config.gateway);
            let state = AppState {
                fleet: fleet.handle(),
                auth: Arc::new(AuthState::new(tokens, devices, &config.gateway, origins)),
                info: Arc::new(GatewayInfo::new(addr, &config.gateway)),
                transports: Arc::new(HostTransports::new(&config)),
            };
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let served = tokio::spawn(async move {
                let _ = serve_with_drain(
                    listener,
                    state,
                    async {
                        let _ = stopped.await;
                    },
                    drain,
                )
                .await;
            });

            Self {
                addr,
                dir,
                read_token,
                control_token,
                device_cookie,
                stop: Some(stop),
                served: Some(served),
                fleet: Some(fleet),
            }
        }

        /// A raw HTTP/1.1 GET, on the runtime's blocking pool so the server
        /// task keeps running.
        pub(crate) async fn get(&self, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
            let addr = self.addr;
            let mut request =
                format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
            for (name, value) in headers {
                request.push_str(&format!("{name}: {value}\r\n"));
            }
            request.push_str("\r\n");
            tokio::task::spawn_blocking(move || {
                let mut stream = TcpStream::connect(addr).expect("connect");
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("read timeout");
                stream.write_all(request.as_bytes()).expect("write request");
                let mut raw = Vec::new();
                stream.read_to_end(&mut raw).expect("read response");
                HttpResponse::parse(&raw)
            })
            .await
            .expect("the request task")
        }

        pub(crate) async fn shutdown(mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(served) = self.served.take() {
                let _ = served.await;
            }
            if let Some(fleet) = self.fleet.take() {
                fleet.shutdown().await;
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) struct HttpResponse {
        pub(crate) status: u16,
        pub(crate) headers: Vec<(String, String)>,
        pub(crate) body: String,
    }

    impl HttpResponse {
        fn parse(raw: &[u8]) -> Self {
            let text = String::from_utf8_lossy(raw).to_string();
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
            let mut lines = head.split("\r\n");
            let status = lines
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|code| code.parse().ok())
                .unwrap_or(0);
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
                .collect();
            Self {
                status,
                headers,
                body: body.to_string(),
            }
        }

        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }

        pub(crate) fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body)
                .unwrap_or_else(|err| panic!("body is not JSON ({err}): {}", self.body))
        }
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {}", token.trim())
    }

    fn device_cookie(server: &TestServer) -> String {
        format!(
            "{DEVICE_COOKIE_NAME}={}",
            server.device_cookie.as_deref().expect("a paired device")
        )
    }

    #[tokio::test]
    async fn health_is_served_without_a_credential() {
        let server = TestServer::start("health").await;
        let response = server.get("/health", &[]).await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(response.json()["ok"].as_bool(), Some(true));
        assert_eq!(response.header("cache-control"), Some("no-store"));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn the_fleet_report_needs_a_token_and_serves_the_status_schema() {
        let server = TestServer::start("fleet").await;

        let bare = server.get("/api/fleet", &[]).await;
        assert_eq!(bare.status, 401, "{}", bare.body);
        assert_eq!(bare.json()["error"].as_str(), Some("unauthorized"));

        for token in [&server.read_token, &server.control_token] {
            let response = server
                .get("/api/fleet", &[("Authorization", &bearer(token))])
                .await;
            assert_eq!(response.status, 200, "{}", response.body);
            let json = response.json();
            assert_eq!(
                json["schema"].as_str(),
                Some(crate::fleet::report::FLEET_STATUS_SCHEMA)
            );
            assert_eq!(json["hosts"].as_array().map(Vec::len), Some(0));
            assert_eq!(response.header("content-type"), Some("application/json"));
            assert_eq!(response.header("cache-control"), Some("no-store"));
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn gateway_info_reports_the_scope_the_caller_proved() {
        let server = TestServer::start("info").await;
        for (token, scope) in [
            (&server.read_token, TokenScope::Read),
            (&server.control_token, TokenScope::Control),
        ] {
            let response = server
                .get("/api/gateway", &[("Authorization", &bearer(token))])
                .await;
            assert_eq!(response.status, 200, "{}", response.body);
            let json = response.json();
            assert_eq!(json["schema"].as_str(), Some(http::GATEWAY_INFO_SCHEMA));
            assert_eq!(json["scope"].as_str(), Some(scope.as_str()));
            assert_eq!(json["loopback"].as_bool(), Some(true));
            // Capability names are appended as PRs land them and never
            // removed, so this asserts presence rather than an exact list a
            // later PR would have to edit for no behavioural reason.
            let features: Vec<&str> = json["features"]
                .as_array()
                .map(|features| features.iter().filter_map(|f| f.as_str()).collect())
                .unwrap_or_default();
            for name in ["fleet", "events", "terminal"] {
                assert!(features.contains(&name), "features: {features:?}");
            }
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_foreign_origin_is_refused_even_with_a_valid_token() {
        let server = TestServer::start("origin").await;
        let token = bearer(&server.read_token);

        let refused = server
            .get(
                "/api/fleet",
                &[
                    ("Authorization", &token),
                    ("Origin", "https://evil.example"),
                ],
            )
            .await;
        assert_eq!(refused.status, 403, "{}", refused.body);
        assert_eq!(refused.json()["error"].as_str(), Some("origin_not_allowed"));

        // The gateway's own loopback origin is implicitly allowed.
        let own = format!("http://{}", server.addr);
        let allowed = server
            .get("/api/fleet", &[("Authorization", &token), ("Origin", &own)])
            .await;
        assert_eq!(allowed.status, 200, "{}", allowed.body);

        server.shutdown().await;
    }

    /// A page that cannot read the response can still make the browser send
    /// one; an origin refusal must therefore not feed the failure limiter, or
    /// any web site could lock the operator out of their own gateway.
    #[tokio::test]
    async fn a_refused_origin_does_not_lock_the_peer_out() {
        let server = TestServer::start("origin-limit").await;
        let token = bearer(&server.read_token);
        for _ in 0..8 {
            let response = server
                .get(
                    "/api/fleet",
                    &[
                        ("Authorization", &token),
                        ("Origin", "https://evil.example"),
                    ],
                )
                .await;
            assert_eq!(response.status, 403, "{}", response.body);
        }
        let response = server.get("/api/fleet", &[("Authorization", &token)]).await;
        assert_eq!(response.status, 200, "{}", response.body);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn repeated_bad_tokens_are_rate_limited_per_peer() {
        let server = TestServer::start("limit").await;
        let mut statuses = Vec::new();
        for _ in 0..6 {
            statuses.push(
                server
                    .get("/api/fleet", &[("Authorization", "Bearer 00")])
                    .await
                    .status,
            );
        }
        assert_eq!(statuses, vec![401, 401, 401, 401, 401, 429], "{statuses:?}");

        let blocked = server
            .get("/api/fleet", &[("Authorization", "Bearer 00")])
            .await;
        assert_eq!(blocked.status, 429);
        assert!(
            blocked.header("retry-after").is_some(),
            "{:?}",
            blocked.headers
        );
        assert_eq!(blocked.json()["error"].as_str(), Some("too_many_requests"));

        // `/health` stays reachable: liveness is not a credential guess.
        assert_eq!(server.get("/health", &[]).await.status, 200);
        server.shutdown().await;
    }

    /// The limiter is keyed on the peer address, so one peer's failures must
    /// not answer for another. The test's second "peer" is the same loopback
    /// address, so it asserts the shape the other way round: a *different*
    /// limiter (a fresh server) still answers 401, not 429.
    #[tokio::test]
    async fn another_peer_still_gets_the_ordinary_refusal() {
        let blocked = TestServer::start("limit-a").await;
        for _ in 0..6 {
            let _ = blocked
                .get("/api/fleet", &[("Authorization", "Bearer 00")])
                .await;
        }
        assert_eq!(
            blocked
                .get("/api/fleet", &[("Authorization", "Bearer 00")])
                .await
                .status,
            429
        );

        let fresh = TestServer::start("limit-b").await;
        assert_eq!(
            fresh
                .get("/api/fleet", &[("Authorization", "Bearer 00")])
                .await
                .status,
            401
        );

        blocked.shutdown().await;
        fresh.shutdown().await;
    }

    #[tokio::test]
    async fn a_token_in_the_query_string_is_ignored() {
        let server = TestServer::start("query").await;
        let path = format!("/api/fleet?token={}", server.read_token.trim());
        let response = server.get(&path, &[]).await;
        assert_eq!(response.status, 401, "{}", response.body);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn the_app_shell_is_served_at_the_root_and_on_app_routes() {
        let server = TestServer::start("assets").await;

        for path in ["/", "/settings"] {
            let response = server.get(path, &[]).await;
            assert_eq!(response.status, 200, "{path}: {}", response.body);
            assert_eq!(
                response.header("content-type"),
                Some("text/html; charset=utf-8"),
                "{path}"
            );
            assert!(response.body.contains("Herdr Fleet"), "{path}");
        }

        let missing = server.get("/nope.png", &[]).await;
        assert_eq!(missing.status, 404, "{}", missing.body);
        assert_eq!(missing.json()["error"].as_str(), Some("not_found"));

        // An unknown API path answers JSON, never the HTML shell.
        let unknown_api = server
            .get(
                "/api/nope",
                &[("Authorization", &bearer(&server.read_token))],
            )
            .await;
        assert_eq!(unknown_api.status, 404);
        assert_eq!(unknown_api.json()["error"].as_str(), Some("not_found"));

        server.shutdown().await;
    }

    /// A non-loopback bind gets no implicit origins, so its own address is not
    /// silently trusted; only the configured allowlist is.
    #[tokio::test]
    async fn configured_origins_are_the_only_ones_a_browser_may_use() {
        let gateway = GatewayConfig {
            allowed_origins: vec!["https://fleet.example".to_string()],
            ..GatewayConfig::default()
        };
        let server =
            TestServer::start_with("configured-origin", gateway, false, SHUTDOWN_DRAIN).await;
        let token = bearer(&server.read_token);

        let allowed = server
            .get(
                "/api/fleet",
                &[
                    ("Authorization", &token),
                    ("Origin", "https://fleet.example"),
                ],
            )
            .await;
        assert_eq!(allowed.status, 200, "{}", allowed.body);

        let refused = server
            .get(
                "/api/fleet",
                &[
                    ("Authorization", &token),
                    ("Origin", "http://fleet.example"),
                ],
            )
            .await;
        assert_eq!(refused.status, 403, "{}", refused.body);

        server.shutdown().await;
    }

    /// A request with no credential at all is refused but is not a guess: a
    /// page can point five `<img>` tags at the gateway with no `Origin` and
    /// no header of its choosing, and that must not lock the operator's own
    /// address out.
    #[tokio::test]
    async fn credential_less_requests_do_not_feed_the_limiter() {
        let server = TestServer::start("no-credential").await;
        for _ in 0..8 {
            let response = server.get("/api/fleet", &[]).await;
            assert_eq!(response.status, 401, "{}", response.body);
        }
        let response = server
            .get(
                "/api/fleet",
                &[("Authorization", &bearer(&server.read_token))],
            )
            .await;
        assert_eq!(response.status, 200, "{}", response.body);

        // A presented-and-wrong credential of either kind still counts.
        for _ in 0..5 {
            let _ = server
                .get(
                    "/api/fleet",
                    &[("Cookie", "herdr_gateway_device=nope.nope")],
                )
                .await;
        }
        let blocked = server
            .get(
                "/api/fleet",
                &[("Authorization", &bearer(&server.read_token))],
            )
            .await;
        assert_eq!(blocked.status, 429, "{}", blocked.body);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_paired_device_cookie_authenticates_with_read_scope() {
        let server = TestServer::start_paired("cookie").await;
        let cookie = device_cookie(&server);

        for headers in [
            vec![("Cookie", cookie.as_str())],
            vec![
                ("Cookie", cookie.as_str()),
                ("Sec-Fetch-Site", "same-origin"),
            ],
            vec![("Cookie", cookie.as_str()), ("Sec-Fetch-Site", "none")],
        ] {
            let response = server.get("/api/gateway", &headers).await;
            assert_eq!(response.status, 200, "{headers:?}: {}", response.body);
            assert_eq!(response.json()["scope"].as_str(), Some("read"));
        }

        // The same cookie with a wrong secret is an ordinary failure.
        let wrong = format!("{DEVICE_COOKIE_NAME}=nope.nope");
        let response = server.get("/api/gateway", &[("Cookie", &wrong)]).await;
        assert_eq!(response.status, 401, "{}", response.body);
        server.shutdown().await;
    }

    /// A page on another site can make the browser send the operator's cookie
    /// without an `Origin` (an `<img>`, a form, a link). The browser marks
    /// those requests, and the cookie is refused on them before it is
    /// compared — and, like an origin refusal, without feeding the limiter.
    #[tokio::test]
    async fn a_device_cookie_is_refused_on_a_cross_site_request_without_an_origin() {
        let server = TestServer::start_paired("cookie-cross-site").await;
        let cookie = device_cookie(&server);

        for site in ["cross-site", "same-site"] {
            for _ in 0..4 {
                let response = server
                    .get(
                        "/api/gateway",
                        &[("Cookie", &cookie), ("Sec-Fetch-Site", site)],
                    )
                    .await;
                assert_eq!(response.status, 403, "{site}: {}", response.body);
                assert_eq!(
                    response.json()["error"].as_str(),
                    Some("origin_not_allowed"),
                    "{site}"
                );
            }
        }

        // An allowed Origin vouches for the page, whatever the site relation.
        let own = format!("http://{}", server.addr);
        let response = server
            .get(
                "/api/gateway",
                &[
                    ("Cookie", &cookie),
                    ("Origin", &own),
                    ("Sec-Fetch-Site", "cross-site"),
                ],
            )
            .await;
        assert_eq!(response.status, 200, "{}", response.body);

        // A bearer token is not a browser-managed credential and is unaffected.
        let response = server
            .get(
                "/api/gateway",
                &[
                    ("Authorization", &bearer(&server.read_token)),
                    ("Sec-Fetch-Site", "cross-site"),
                ],
            )
            .await;
        assert_eq!(response.status, 200, "{}", response.body);

        // Eight refusals above, and the peer is not locked out.
        let response = server.get("/api/gateway", &[("Cookie", &cookie)]).await;
        assert_eq!(response.status, 200, "{}", response.body);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn two_origin_headers_are_refused_even_when_one_is_allowed() {
        let server = TestServer::start("two-origins").await;
        let own = format!("http://{}", server.addr);
        let response = server
            .get(
                "/api/fleet",
                &[
                    ("Authorization", &bearer(&server.read_token)),
                    ("Origin", &own),
                    ("Origin", "https://evil.example"),
                ],
            )
            .await;
        assert_eq!(response.status, 403, "{}", response.body);
        assert_eq!(
            response.json()["error"].as_str(),
            Some("origin_not_allowed")
        );
        server.shutdown().await;
    }

    /// Axum does not fold percent-encoded or dotted spellings before routing
    /// — probed, not assumed — so a spelling never reaches an API handler on
    /// its own. The auth layer normalizes anyway, so no spelling of an API
    /// path can be classified public either, and none of them answers with
    /// the HTML shell an API client could not tell from a real reply.
    #[tokio::test]
    async fn non_canonical_api_spellings_never_reach_an_api_handler() {
        let server = TestServer::start("spellings").await;
        for path in [
            "/api/fleet",
            "//api/fleet",
            "/./api/fleet",
            "/health/../api/fleet",
            "/api%2Ffleet",
            "/api%2ffleet",
            "/api/./fleet",
            "/api/x/../fleet",
            "/api/fleet/",
            "/api/fleet;x",
            "/api/nope",
        ] {
            let response = server.get(path, &[]).await;
            assert_eq!(response.status, 401, "{path}: {}", response.body);
            assert_eq!(
                response.json()["error"].as_str(),
                Some("unauthorized"),
                "{path}"
            );
            assert!(!response.body.contains("herdr.fleet.status"), "{path}");
        }

        // Case is not folded: HTTP paths are case-sensitive and so is the
        // router, so this is one of the app's own routes, not the API.
        let response = server.get("/API/fleet", &[]).await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(
            response.header("content-type"),
            Some("text/html; charset=utf-8")
        );

        // With a credential, only the canonical spelling produces a report;
        // the rest are an honest JSON 404, never the shell.
        let token = bearer(&server.read_token);
        for path in ["//api/fleet", "/api%2Ffleet", "/api/fleet/"] {
            let response = server.get(path, &[("Authorization", &token)]).await;
            assert_eq!(response.status, 404, "{path}: {}", response.body);
            assert_eq!(
                response.json()["error"].as_str(),
                Some("not_found"),
                "{path}"
            );
        }
        let response = server.get("/api/fleet", &[("Authorization", &token)]).await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(
            response.json()["schema"].as_str(),
            Some(crate::fleet::report::FLEET_STATUS_SCHEMA)
        );

        server.shutdown().await;
    }

    /// A connection that never finishes its request must not hold a stopping
    /// gateway open for good.
    #[tokio::test]
    async fn shutdown_drops_a_stalled_connection_after_the_drain_period() {
        let server = TestServer::start_with(
            "drain",
            GatewayConfig::default(),
            false,
            Duration::from_millis(200),
        )
        .await;
        let addr = server.addr;
        let stalled = tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .write_all(b"GET /health HTT")
                .expect("write a partial request");
            stream
        })
        .await
        .expect("the stalled client");

        let started = Instant::now();
        server.shutdown().await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "shutdown took {:?}",
            started.elapsed()
        );
        drop(stalled);
    }

    #[tokio::test]
    async fn no_server_header_is_advertised() {
        let server = TestServer::start("no-server-header").await;
        let response = server.get("/health", &[]).await;
        assert!(
            response.header("server").is_none(),
            "{:?}",
            response.headers
        );
        server.shutdown().await;
    }
}
