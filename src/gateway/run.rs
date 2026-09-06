//! `herdr gateway [--bind ADDR] [--config PATH]` — startup, and the order it
//! happens in.
//!
//! The order is the contract, because each step can refuse and the cheapest
//! refusals must come first:
//!
//! 1. Parse the arguments (usage error → exit 2).
//! 2. Point config loading at `--config`, **before** anything reads config.
//! 3. Load config and print its diagnostics. A bad `[gateway]` value is a
//!    warning with a documented fallback, never fatal.
//! 4. Decide the bind address and run [`BindPolicy`] on it. This is the only
//!    thing that refuses to listen, and it refuses *before* a socket, a token
//!    or a host connection exists (exit 1).
//! 5. Create or verify the token files (exit 1 on a mode we do not trust).
//! 6. Inside a tokio runtime: start the fleet, bind, write the runtime marker,
//!    serve until `SIGTERM`/Ctrl-C, then stop the fleet and remove the marker.
//!
//! Nothing after step 6 calls `std::process::exit`: a running gateway stops by
//! returning, so the fleet's supervisors and the listening socket are always
//! released.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tokio::net::TcpListener;

use crate::config::{Config, GatewayConfig};
use crate::gateway::auth::{DeviceStore, TokenStore};
use crate::gateway::fleet::FleetRuntime;
use crate::gateway::paths;
use crate::gateway::policy::{BindPolicy, OriginAllowlist};
use crate::gateway::server::{self, AppState, AuthState, GatewayInfo};
use crate::gateway::transports::HostTransports;
use crate::gateway::GATEWAY_COMMAND_LINE;

/// Exit code for a usage error, matching the fleet CLI.
const EXIT_USAGE: i32 = 2;
/// Exit code for a refusal to start: bad config, a refused bind, an untrusted
/// token file, an address already in use.
const EXIT_REFUSED: i32 = 1;

/// What the command line asked for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RunArgs {
    /// `--bind`, which overrides `[gateway] bind`.
    pub(crate) bind: Option<SocketAddr>,
    /// `--config`, the config file to read.
    pub(crate) config: Option<PathBuf>,
}

pub(crate) fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut parsed = RunArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--bind" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--bind needs an ADDRESS:PORT value".to_string());
                };
                parsed.bind = Some(parse_bind(value)?);
                index += 1;
            }
            "--config" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--config needs a PATH value".to_string());
                };
                parsed.config = Some(parse_config_path(value)?);
                index += 1;
            }
            other => {
                if let Some(value) = other.strip_prefix("--bind=") {
                    parsed.bind = Some(parse_bind(value)?);
                } else if let Some(value) = other.strip_prefix("--config=") {
                    parsed.config = Some(parse_config_path(value)?);
                } else if other.starts_with('-') {
                    return Err(format!("unknown option: {other}"));
                } else {
                    return Err(format!("unknown subcommand: {other}"));
                }
            }
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_bind(value: &str) -> Result<SocketAddr, String> {
    value
        .parse()
        .map_err(|_| format!("--bind must be ADDRESS:PORT (for example 127.0.0.1:7788): {value}"))
}

fn parse_config_path(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err("--config needs a PATH value".to_string());
    }
    Ok(PathBuf::from(value))
}

/// Run the gateway. Returns the process exit code.
pub(crate) fn run(args: &[String]) -> io::Result<i32> {
    let parsed = match parse_run_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            crate::platform::begin_cli_output();
            eprintln!("{error}");
            eprintln!("usage: {GATEWAY_COMMAND_LINE}");
            return Ok(EXIT_USAGE);
        }
    };

    crate::platform::begin_cli_output();

    if let Some(path) = &parsed.config {
        // Before `Config::load`, and before the runtime exists: this process
        // is still single-threaded here, which is the only safe moment to set
        // a process-wide environment variable. Everything that resolves a
        // config path — including `herdr server reload-config` in a child —
        // reads the same variable, so one assignment moves the whole load.
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, path);
    }

    init_gateway_logging();

    let loaded = Config::load();
    for diagnostic in &loaded.diagnostics {
        eprintln!("{diagnostic}");
    }
    let config = loaded.config;

    let bind = parsed
        .bind
        .unwrap_or_else(|| config.gateway.effective_bind_addr());
    if let Err(error) = BindPolicy::check(bind, &config.gateway) {
        eprintln!("{error}");
        return Ok(EXIT_REFUSED);
    }

    let gateway_dir = paths::gateway_dir();
    let tokens = match TokenStore::load_or_create(&gateway_dir) {
        Ok(tokens) => tokens,
        Err(error) => {
            eprintln!("cannot use the gateway token store: {error}");
            return Ok(EXIT_REFUSED);
        }
    };
    let devices = match DeviceStore::load(&gateway_dir) {
        Ok(devices) => devices,
        Err(error) => {
            eprintln!("cannot read the paired devices: {error}");
            return Ok(EXIT_REFUSED);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve_until_signal(
        &config,
        bind,
        &gateway_dir,
        tokens,
        devices,
    ))
}

/// Logs go to stderr, which is where a supervisor collects a foreground
/// daemon's output (systemd's journal, a container's log, the operator's
/// terminal). Herdr's other long-running roles log to files because they own a
/// terminal and cannot; a gateway does not have that problem.
///
/// `HERDR_LOG` overrides the filter, which by default carries this module's
/// `gateway` target as well as `herdr` — the shared file-logging default is
/// `herdr=info` alone, which would silently drop every line below.
fn init_gateway_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("HERDR_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("herdr=info,gateway=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(true)
        .try_init();
}

/// Everything that needs the runtime: the fleet, the listener, the server.
async fn serve_until_signal(
    config: &Config,
    bind: SocketAddr,
    gateway_dir: &Path,
    tokens: TokenStore,
    devices: DeviceStore,
) -> io::Result<i32> {
    // Started before the listener so a fleet that cannot be resolved never
    // opens a port; stopped on every path out of this function.
    let fleet = match FleetRuntime::start(config) {
        Ok(fleet) => fleet,
        Err(diagnostics) => {
            for diagnostic in &diagnostics {
                eprintln!("{diagnostic}");
            }
            return Ok(EXIT_REFUSED);
        }
    };

    let listener = match TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on {bind}: {error}");
            fleet.shutdown().await;
            return Ok(EXIT_REFUSED);
        }
    };
    // The bound address, not the requested one: `--bind 127.0.0.1:0` only
    // knows its port now, and the origin allowlist and the marker both need
    // the real one.
    let listen = listener.local_addr().unwrap_or(bind);

    let origins = OriginAllowlist::for_bind(listen, &config.gateway);
    warn_if_a_browser_cannot_reach_it(listen, &config.gateway, &origins);

    // Built here rather than inside the router so `run` keeps a handle: the
    // teardown order below (sessions, then transports, then the connector) is
    // the E1 drop hazard, not a preference.
    let transports = std::sync::Arc::new(HostTransports::new(config));
    let state = AppState {
        fleet: fleet.handle(),
        auth: std::sync::Arc::new(AuthState::new(
            gateway_dir.to_path_buf(),
            tokens,
            devices,
            &config.gateway,
            origins,
        )),
        info: std::sync::Arc::new(GatewayInfo::new(listen, &config.gateway)),
        transports: std::sync::Arc::clone(&transports),
    };

    // Before the address is announced, so nothing that reacts to that line can
    // beat the signal handlers into place; the future itself is only awaited
    // by `serve`.
    let signal = server::shutdown_signal();
    let stopper = fleet.stopper();
    let shutdown = async move {
        signal.await;
        // End every open stream here rather than at the server's drain
        // deadline. An upgraded WebSocket keeps graceful shutdown waiting for
        // as long as it is open, so without this a stopping gateway would
        // stall for the whole drain and then drop its clients' connections
        // instead of telling them it is going away.
        stopper.stop();
    };

    let marker = gateway_dir.join(paths::RUNTIME_FILE);
    if let Err(error) = write_runtime_marker(&marker, listen) {
        // Not fatal: the marker is how `herdr gateway status` finds a running
        // gateway, not how the gateway serves.
        tracing::warn!(target: "gateway", error = %error, "could not write the gateway runtime marker");
    }

    println!("listening on http://{listen}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    tracing::info!(target: "gateway", listen = %listen, "gateway listening");

    let outcome = server::serve(listener, state, shutdown).await;

    tracing::info!(target: "gateway", "gateway stopping");
    // Sessions first: a terminal stream lives in its own task that the server
    // does not own, so latching the stop flag is what ends it. Then the
    // transports, whose ssh bridges unlink their forward sockets on drop and
    // would block if a stream were still open. Then the connector.
    let stopped = tokio::task::spawn_blocking(move || transports.shutdown()).await;
    if let Err(error) = stopped {
        tracing::warn!(target: "gateway", error = %error, "the transport shutdown task failed");
    }
    fleet.shutdown().await;
    if let Err(error) = remove_runtime_marker(&marker) {
        tracing::warn!(target: "gateway", error = %error, "could not remove the gateway runtime marker");
    }

    match outcome {
        Ok(()) => Ok(0),
        Err(error) => {
            eprintln!("gateway stopped: {error}");
            Ok(EXIT_REFUSED)
        }
    }
}

/// A loopback bind serves its own origins implicitly; a non-loopback one only
/// serves what was configured, and an operator who configured an origin that
/// does not resolve to this listener would otherwise find out from a browser.
fn warn_if_a_browser_cannot_reach_it(
    listen: SocketAddr,
    config: &GatewayConfig,
    origins: &OriginAllowlist,
) {
    if origins.is_empty() {
        tracing::warn!(
            target: "gateway",
            listen = %listen,
            "no browser origin is allowed; only clients that send no Origin header can reach this gateway"
        );
    }
    if !listen.ip().is_loopback() && config.public_url.is_empty() {
        tracing::info!(
            target: "gateway",
            listen = %listen,
            "no [gateway] public_url is set; pairing URLs will use the bind address"
        );
    }
}

/// `{pid, listen, started_unix}`, `0600`, for `herdr gateway status` (PR 8),
/// the systemd unit's documentation and E5.
fn write_runtime_marker(path: &Path, listen: SocketAddr) -> io::Result<()> {
    let marker = serde_json::json!({
        "pid": std::process::id(),
        "listen": listen.to_string(),
        "started_unix": crate::gateway::auth::unix_now(),
    });
    let encoded = serde_json::to_vec(&marker).map_err(io::Error::other)?;
    paths::write_private_file(path, &encoded)
}

fn remove_runtime_marker(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn no_arguments_runs_with_the_configured_defaults() {
        assert_eq!(parse_run_args(&[]), Ok(RunArgs::default()));
    }

    #[test]
    fn bind_and_config_parse_in_both_spellings() {
        let expected = RunArgs {
            bind: Some("127.0.0.1:7788".parse().expect("addr")),
            config: Some(PathBuf::from("/tmp/herdr.toml")),
        };
        assert_eq!(
            parse_run_args(&args(&[
                "--bind",
                "127.0.0.1:7788",
                "--config",
                "/tmp/herdr.toml"
            ])),
            Ok(expected.clone())
        );
        assert_eq!(
            parse_run_args(&args(&[
                "--bind=127.0.0.1:7788",
                "--config=/tmp/herdr.toml"
            ])),
            Ok(expected)
        );
    }

    #[test]
    fn ipv6_and_wildcard_addresses_parse() {
        for value in ["[::1]:7788", "0.0.0.0:7788", "[::]:0"] {
            let parsed = parse_run_args(&args(&["--bind", value])).expect(value);
            assert_eq!(parsed.bind, Some(value.parse().expect("addr")));
        }
    }

    #[test]
    fn a_missing_or_bad_value_is_a_usage_error() {
        for arguments in [
            vec!["--bind"],
            vec!["--config"],
            vec!["--bind", "127.0.0.1"],
            vec!["--bind", "not-an-address"],
            vec!["--bind="],
            vec!["--config="],
        ] {
            assert!(
                parse_run_args(&args(&arguments)).is_err(),
                "expected a usage error for {arguments:?}"
            );
        }
    }

    #[test]
    fn an_unknown_option_or_word_is_a_usage_error() {
        let error = parse_run_args(&args(&["--nope"])).expect_err("unknown option");
        assert!(error.contains("--nope"), "{error}");
        let error = parse_run_args(&args(&["pair"])).expect_err("unknown subcommand");
        assert!(error.contains("pair"), "{error}");
    }

    #[test]
    fn a_bind_message_names_the_flag_and_the_value() {
        let error = parse_run_args(&args(&["--bind", "nope"])).expect_err("bad address");
        assert!(error.contains("--bind"), "{error}");
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn the_runtime_marker_is_private_and_carries_the_listen_address() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-gateway-run-marker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        paths::create_private_dir(&dir).expect("create the gateway dir");
        let path = dir.join(paths::RUNTIME_FILE);
        let listen: SocketAddr = "127.0.0.1:7788".parse().expect("addr");

        write_runtime_marker(&path, listen).expect("write the marker");
        paths::verify_private_file(&path).expect("the marker is private");
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        assert_eq!(marker["listen"].as_str(), Some("127.0.0.1:7788"));
        assert_eq!(marker["pid"].as_u64(), Some(u64::from(std::process::id())));
        assert!(marker["started_unix"].as_u64().is_some(), "{marker}");

        remove_runtime_marker(&path).expect("remove the marker");
        assert!(!path.exists());
        // Removing a marker that is already gone is not an error: a crash and
        // a clean stop must leave the same state.
        remove_runtime_marker(&path).expect("remove again");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
