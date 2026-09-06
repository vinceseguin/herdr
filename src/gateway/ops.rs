//! `herdr gateway pair`, `status` and `rotate-token` — the operator's side of
//! the gateway, run from a terminal while the daemon runs somewhere else.
//!
//! None of these commands talks to the running gateway over a private channel:
//! they read and write the same `0600` files under `<config>/gateway/`, and the
//! daemon notices. That is deliberate — `<config>/gateway/` is already the
//! contract between the two, it works when the daemon is not running, and it
//! adds no socket to a process whose whole point is to be the only network
//! surface. The one thing the daemon is asked for is `GET /health`, which needs
//! no credential.
//!
//! What each command touches:
//!
//! * `pair` writes one file into `pairings/` and prints the URL that redeems
//!   it. It never reads a token: a pairing code is its own secret.
//! * `status` reads `gateway.json`, `devices.json` and `pairings/`, and probes
//!   `/health`. It writes nothing except the sweep of expired codes.
//! * `rotate-token` rewrites one token file, then removes every device **and**
//!   every pending pairing code of that scope. Rotation is a revocation; a
//!   pending code for the scope being rotated would hand out the authority the
//!   operator just took back.
//!
//! Exit codes: 0 success, 1 a refusal (unreadable store, no address to
//! advertise), 2 a usage error, 3 `status` when no gateway is running.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use crate::config::{parse_gateway_origin, Config};
use crate::gateway::auth::{unix_now, DeviceStore, PairingStore, TokenScope, TokenStore};
use crate::gateway::paths;

/// Success.
const EXIT_OK: i32 = 0;
/// A refusal: a store that cannot be used, or no address to put in a URL.
const EXIT_REFUSED: i32 = 1;
/// A usage error, matching the rest of the gateway CLI.
const EXIT_USAGE: i32 = 2;
/// `status` found no running gateway.
const EXIT_NOT_RUNNING: i32 = 3;

/// Inclusive bounds of `--ttl-secs`, the same as `[gateway] pairing_ttl_secs`.
const MIN_TTL_SECS: u64 = crate::config::MIN_GATEWAY_PAIRING_TTL_SECS;
const MAX_TTL_SECS: u64 = crate::config::MAX_GATEWAY_PAIRING_TTL_SECS;

/// How long `status` waits for `/health`. A gateway that cannot answer a
/// credential-less liveness check in this long is not healthy by any useful
/// definition, and the command must not hang.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// `herdr gateway pair`
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct PairArgs {
    control: bool,
    ttl_secs: Option<u64>,
    label: String,
    no_qr: bool,
    invert: bool,
    json: bool,
}

impl PairArgs {
    fn scope(&self) -> TokenScope {
        if self.control {
            TokenScope::Control
        } else {
            TokenScope::Read
        }
    }
}

fn parse_pair_args(args: &[String]) -> Result<PairArgs, String> {
    let mut parsed = PairArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--control" => parsed.control = true,
            "--no-qr" => parsed.no_qr = true,
            "--invert" => parsed.invert = true,
            "--json" => parsed.json = true,
            "--ttl-secs" | "--label" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("{arg} needs a value"));
                };
                assign_pair_value(&mut parsed, arg, value)?;
                index += 1;
            }
            other => {
                if let Some((name, value)) = other.split_once('=') {
                    if matches!(name, "--ttl-secs" | "--label") {
                        assign_pair_value(&mut parsed, name, value)?;
                        index += 1;
                        continue;
                    }
                }
                return Err(unknown_argument(other));
            }
        }
        index += 1;
    }
    Ok(parsed)
}

fn assign_pair_value(parsed: &mut PairArgs, name: &str, value: &str) -> Result<(), String> {
    match name {
        "--ttl-secs" => {
            let secs: u64 = value
                .parse()
                .map_err(|_| format!("--ttl-secs must be a whole number of seconds: {value}"))?;
            if !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&secs) {
                return Err(format!(
                    "--ttl-secs must be {MIN_TTL_SECS}..={MAX_TTL_SECS} seconds: {secs}"
                ));
            }
            parsed.ttl_secs = Some(secs);
        }
        "--label" => parsed.label = value.to_string(),
        _ => return Err(unknown_argument(name)),
    }
    Ok(())
}

pub(crate) fn pair_command(args: &[String]) -> io::Result<i32> {
    let parsed = match parse_pair_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            return usage_error(
                &error,
                "pair [--control] [--ttl-secs N] [--label TEXT] [--no-qr] [--invert] [--json]",
            )
        }
    };

    let config = load_config();
    let scope = parsed.scope();
    let ttl_secs = parsed
        .ttl_secs
        .unwrap_or_else(|| config.gateway.effective_pairing_ttl_secs());

    let gateway_dir = paths::gateway_dir();
    let marker = read_runtime_marker(&gateway_dir);
    let base_url = match resolve_base_url(&config, marker.as_ref()) {
        Ok(base_url) => base_url,
        Err(error) => {
            eprintln!("{error}");
            return Ok(EXIT_REFUSED);
        }
    };

    let store = PairingStore::new(&gateway_dir);
    // Every entry point sweeps: a code nobody redeemed must not sit in the
    // directory until the next exchange happens to notice it.
    if let Err(error) = store.sweep_expired(unix_now()) {
        eprintln!("warning: could not sweep expired pairing codes: {error}");
    }
    let (code, record) = match store.create(scope, &parsed.label, ttl_secs, unix_now()) {
        Ok(created) => created,
        Err(error) => {
            eprintln!("cannot create a pairing code: {error}");
            return Ok(EXIT_REFUSED);
        }
    };
    let url = format!("{base_url}/pair?code={code}");

    if parsed.json {
        // Exactly three keys: a client reads `url` and shows the rest.
        let body = serde_json::json!({
            "url": url,
            "scope": scope.as_str(),
            "expires_unix": record.expires_unix,
        });
        println!("{body}");
        return Ok(EXIT_OK);
    }

    if marker.is_none() {
        eprintln!("warning: no gateway is running; start one with `herdr gateway` before opening this link");
    }
    if is_loopback_url(&base_url) {
        eprintln!(
            "warning: this URL points at loopback, so only a browser on this machine can open it; set [gateway] public_url for a phone"
        );
    }

    println!(
        "Pair this device with Herdr Fleet ({} scope, valid {}):",
        scope.as_str(),
        format_duration(ttl_secs)
    );
    println!("  {url}");
    if !parsed.no_qr {
        match qr_text(&url, parsed.invert) {
            Ok(qr) => {
                println!("{qr}");
                println!(
                    "If a scanner cannot read this code, re-run with --invert (dark terminals)."
                );
            }
            Err(error) => eprintln!("warning: {error}"),
        }
    }
    println!("The link works once and then expires.");
    Ok(EXIT_OK)
}

/// The QR code for `url`, as text a terminal can print.
///
/// `Dense1x2` packs two module rows into one line, which is what makes the
/// result roughly square in a terminal cell grid — a scanner needs square
/// modules. The quiet zone is the crate's four modules: trimming it saves four
/// lines and costs reliability on exactly the phones this exists for.
///
/// `invert` swaps which modules are drawn as blocks. A QR code is dark modules
/// on a light field, and a terminal draws the blocks in its *foreground*
/// colour — so the default is right on a light terminal and inverted on a dark
/// one, where the same code must be drawn the other way round to scan.
pub(crate) fn qr_text(url: &str, invert: bool) -> Result<String, String> {
    use qrcode::render::unicode::Dense1x2;

    let code = qrcode::QrCode::new(url)
        .map_err(|error| format!("cannot encode a QR code for this URL: {error}"))?;
    let (dark, light) = if invert {
        (Dense1x2::Light, Dense1x2::Dark)
    } else {
        (Dense1x2::Dark, Dense1x2::Light)
    };
    Ok(code
        .render::<Dense1x2>()
        .dark_color(dark)
        .light_color(light)
        .quiet_zone(true)
        .build())
}

/// The origin a pairing URL advertises.
///
/// In order: `[gateway] public_url` (what the operator says the world calls
/// this gateway), then the address a running gateway actually bound (which is
/// the only source that knows the port after `--bind …:0`), then the
/// configured bind. An address nobody can dial — a wildcard bind, or port 0
/// with no running gateway — is a refusal naming `public_url`, never a URL
/// that silently does not work.
fn resolve_base_url(config: &Config, marker: Option<&RuntimeMarker>) -> Result<String, String> {
    if !config.gateway.public_url.is_empty() {
        return match parse_gateway_origin(&config.gateway.public_url) {
            // Through the parser so a trailing slash, a path or odd casing
            // cannot reach the URL.
            Ok(origin) => Ok(origin.to_header_value()),
            Err(error) => Err(format!(
                "cannot use [gateway] public_url as a pairing address: {error}"
            )),
        };
    }

    let listen = marker
        .map(|marker| marker.listen)
        .unwrap_or_else(|| config.gateway.effective_bind_addr());
    if listen.ip().is_unspecified() || listen.port() == 0 {
        return Err(format!(
            "cannot build a pairing URL for {listen}: set [gateway] public_url to the address devices should open, or bind a concrete address"
        ));
    }
    Ok(format!("http://{listen}"))
}

fn is_loopback_url(base_url: &str) -> bool {
    parse_gateway_origin(base_url).is_ok_and(|origin| {
        origin
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or_else(|_| origin.host == "localhost")
    })
}

/// A duration a human reads at a glance: whole hours, whole minutes, or
/// seconds.
fn format_duration(secs: u64) -> String {
    if secs >= 3600 && secs.is_multiple_of(3600) {
        let hours = secs / 3600;
        return format!("{hours} h");
    }
    if secs >= 60 && secs.is_multiple_of(60) {
        let minutes = secs / 60;
        return format!("{minutes} min");
    }
    format!("{secs} s")
}

// ---------------------------------------------------------------------------
// `herdr gateway status`
// ---------------------------------------------------------------------------

/// What `<config>/gateway/gateway.json` says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeMarker {
    pid: u32,
    listen: SocketAddr,
}

/// The marker of a gateway that is **actually running**.
///
/// A crash leaves the file behind, so the pid is checked before the file is
/// believed: a stale marker must not make `status` say "running", and must not
/// give `pair` a port that nothing is listening on.
fn read_runtime_marker(gateway_dir: &Path) -> Option<RuntimeMarker> {
    let path = gateway_dir.join(paths::RUNTIME_FILE);
    let bytes = paths::read_private_file(&path).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let pid = u32::try_from(json.get("pid")?.as_u64()?).ok()?;
    let listen = json.get("listen")?.as_str()?.parse().ok()?;
    if !crate::platform::process_exists(pid) {
        tracing::debug!(
            target: "gateway",
            path = %path.display(),
            pid,
            "ignoring a runtime marker whose process is gone"
        );
        return None;
    }
    Some(RuntimeMarker { pid, listen })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StatusArgs {
    json: bool,
}

fn parse_status_args(args: &[String]) -> Result<StatusArgs, String> {
    let mut parsed = StatusArgs::default();
    for arg in args {
        match arg.as_str() {
            "--json" => parsed.json = true,
            other => return Err(unknown_argument(other)),
        }
    }
    Ok(parsed)
}

/// Schema marker of `herdr gateway status --json`.
pub(crate) const GATEWAY_STATUS_SCHEMA: &str = "herdr.gateway.status.v1";

pub(crate) fn status_command(args: &[String]) -> io::Result<i32> {
    let parsed = match parse_status_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return usage_error(&error, "status [--json]"),
    };

    let gateway_dir = paths::gateway_dir();
    let marker = read_runtime_marker(&gateway_dir);

    let devices = match DeviceStore::load(&gateway_dir) {
        Ok(devices) => devices,
        Err(error) => {
            eprintln!("cannot read the paired devices: {error}");
            return Ok(EXIT_REFUSED);
        }
    };
    let mut read_devices = 0;
    let mut control_devices = 0;
    for device in devices.devices() {
        match device.scope {
            TokenScope::Read => read_devices += 1,
            TokenScope::Control => control_devices += 1,
        }
    }

    let store = PairingStore::new(&gateway_dir);
    if let Err(error) = store.sweep_expired(unix_now()) {
        eprintln!("warning: could not sweep expired pairing codes: {error}");
    }
    let pending = store.pending(unix_now()).unwrap_or_else(|error| {
        eprintln!("warning: could not count pairing codes: {error}");
        0
    });

    let healthy = marker
        .as_ref()
        .is_some_and(|marker| probe_health(marker.listen));

    if parsed.json {
        let body = serde_json::json!({
            "schema": GATEWAY_STATUS_SCHEMA,
            "running": marker.is_some(),
            "healthy": healthy,
            "pid": marker.as_ref().map(|marker| marker.pid),
            "listen": marker.as_ref().map(|marker| marker.listen.to_string()),
            "devices": { "read": read_devices, "control": control_devices },
            "pairings_pending": pending,
        });
        println!("{body}");
    } else {
        match &marker {
            Some(marker) => {
                println!("gateway:  running (pid {})", marker.pid);
                println!("listen:   {}", marker.listen);
                println!("health:   {}", if healthy { "ok" } else { "no answer" });
            }
            None => println!("gateway:  not running"),
        }
        println!("devices:  read {read_devices}, control {control_devices}");
        println!("pairings: {pending} pending");
    }

    Ok(if marker.is_some() {
        EXIT_OK
    } else {
        EXIT_NOT_RUNNING
    })
}

/// `GET /health` on the address the marker recorded.
///
/// Hand-rolled over `TcpStream` rather than through an HTTP client: the epic's
/// dependency budget has no client in it, the request is one fixed line, and
/// the only thing read back is the status code. A wildcard bind is dialled on
/// loopback, which is the one address it is certain to answer on.
fn probe_health(listen: SocketAddr) -> bool {
    use std::io::{Read, Write};

    let target = if listen.ip().is_unspecified() {
        let loopback = match listen.ip() {
            IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        };
        SocketAddr::new(loopback, listen.port())
    } else {
        listen
    };

    let probe = || -> io::Result<bool> {
        let mut stream = std::net::TcpStream::connect_timeout(&target, HEALTH_TIMEOUT)?;
        stream.set_read_timeout(Some(HEALTH_TIMEOUT))?;
        stream.set_write_timeout(Some(HEALTH_TIMEOUT))?;
        stream.write_all(
            format!("GET /health HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )?;
        // The status line is all that matters, and it is the first bytes on the
        // socket; a bounded read keeps a wedged peer from holding the command.
        let mut raw = [0u8; 64];
        let read = stream.read(&mut raw)?;
        Ok(String::from_utf8_lossy(&raw[..read]).starts_with("HTTP/1.1 200"))
    };
    probe().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// `herdr gateway rotate-token`
// ---------------------------------------------------------------------------

fn parse_rotate_args(args: &[String]) -> Result<TokenScope, String> {
    let mut scope = None;
    for arg in args {
        match arg.as_str() {
            "read" if scope.is_none() => scope = Some(TokenScope::Read),
            "control" if scope.is_none() => scope = Some(TokenScope::Control),
            other => return Err(unknown_argument(other)),
        }
    }
    scope.ok_or_else(|| "rotate-token needs a scope: read or control".to_string())
}

pub(crate) fn rotate_token_command(args: &[String]) -> io::Result<i32> {
    let scope = match parse_rotate_args(args) {
        Ok(scope) => scope,
        Err(error) => return usage_error(&error, "rotate-token <read|control>"),
    };

    let gateway_dir = paths::gateway_dir();
    // `load_or_create` rather than `load`: rotating a token in a store that was
    // never created is a reasonable way to create one, and it refuses the same
    // untrusted modes the daemon does.
    let before = match TokenStore::load_or_create(&gateway_dir) {
        Ok(tokens) => tokens,
        Err(error) => {
            eprintln!("cannot use the gateway token store: {error}");
            return Ok(EXIT_REFUSED);
        }
    };
    if let Err(error) = TokenStore::rotate(&gateway_dir, scope) {
        eprintln!("cannot rotate the {} token: {error}", scope.as_str());
        return Ok(EXIT_REFUSED);
    }
    // Read it back: a rotation that did not change the file would revoke every
    // device for nothing, and the operator would believe the old token is dead.
    match TokenStore::load(&gateway_dir) {
        Ok(after) if after.digest(scope).ct_eq(&before.digest(scope)) => {
            eprintln!(
                "the {} token file did not change; nothing was revoked",
                scope.as_str()
            );
            return Ok(EXIT_REFUSED);
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("the rotated token store cannot be read back: {error}");
            return Ok(EXIT_REFUSED);
        }
    }

    let mut devices = match DeviceStore::load(&gateway_dir) {
        Ok(devices) => devices,
        Err(error) => {
            eprintln!(
                "the {} token was rotated, but the paired devices could not be read: {error}",
                scope.as_str()
            );
            return Ok(EXIT_REFUSED);
        }
    };
    let revoked = match devices.revoke_scope(scope) {
        Ok(revoked) => revoked,
        Err(error) => {
            eprintln!(
                "the {} token was rotated, but its devices could not be revoked: {error}",
                scope.as_str()
            );
            return Ok(EXIT_REFUSED);
        }
    };
    // A pending code for this scope would mint a device with the authority
    // that was just revoked, so it goes too.
    let codes = PairingStore::new(&gateway_dir)
        .revoke_scope(scope)
        .unwrap_or_else(|error| {
            eprintln!("warning: could not revoke pending pairing codes: {error}");
            0
        });

    println!(
        "rotated the {} token; revoked {} and {}",
        scope.as_str(),
        plural(revoked, "device", "devices"),
        plural(codes, "pending pairing code", "pending pairing codes")
    );
    if read_runtime_marker(&gateway_dir).is_some() {
        println!("the running gateway applies this on its next request; no restart is needed.");
    }
    Ok(EXIT_OK)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// Load the config the way the daemon does, reporting the same diagnostics.
///
/// A bad `[gateway]` value is never fatal here either: the documented fallback
/// applies and the operator is told which key to fix.
fn load_config() -> Config {
    let loaded = Config::load();
    for diagnostic in &loaded.diagnostics {
        eprintln!("{diagnostic}");
    }
    loaded.config
}

fn unknown_argument(argument: &str) -> String {
    if argument.starts_with('-') {
        format!("unknown option: {argument}")
    } else {
        format!("unexpected argument: {argument}")
    }
}

fn usage_error(error: &str, usage: &str) -> io::Result<i32> {
    eprintln!("{error}");
    eprintln!("usage: herdr gateway {usage}");
    Ok(EXIT_USAGE)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::GatewayConfig;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn config_with(bind: &str, public_url: &str) -> Config {
        Config {
            gateway: GatewayConfig {
                bind: bind.to_string(),
                public_url: public_url.to_string(),
                ..GatewayConfig::default()
            },
            ..Config::default()
        }
    }

    fn marker(listen: &str) -> RuntimeMarker {
        RuntimeMarker {
            pid: std::process::id(),
            listen: listen.parse().expect("socket address"),
        }
    }

    #[test]
    fn pair_arguments_parse_in_both_spellings() {
        assert_eq!(parse_pair_args(&[]), Ok(PairArgs::default()));
        let parsed = parse_pair_args(&args(&[
            "--control",
            "--ttl-secs",
            "60",
            "--label",
            "kitchen tablet",
            "--no-qr",
            "--invert",
            "--json",
        ]))
        .expect("parse");
        assert_eq!(
            parsed,
            PairArgs {
                control: true,
                ttl_secs: Some(60),
                label: "kitchen tablet".to_string(),
                no_qr: true,
                invert: true,
                json: true,
            }
        );
        assert_eq!(parsed.scope(), TokenScope::Control);

        let equals = parse_pair_args(&args(&["--ttl-secs=90", "--label=phone"])).expect("parse");
        assert_eq!(equals.ttl_secs, Some(90));
        assert_eq!(equals.label, "phone");
        assert_eq!(PairArgs::default().scope(), TokenScope::Read);
    }

    #[test]
    fn the_pairing_ttl_has_the_same_bounds_as_the_config_key() {
        for value in ["29", "86401", "0", "nope", ""] {
            let error = parse_pair_args(&args(&["--ttl-secs", value]))
                .expect_err("out-of-bounds ttl must be refused");
            assert!(error.contains("--ttl-secs"), "{error}");
        }
        for value in ["30", "600", "86400"] {
            assert!(
                parse_pair_args(&args(&["--ttl-secs", value])).is_ok(),
                "{value}"
            );
        }
    }

    #[test]
    fn unknown_pair_arguments_are_usage_errors() {
        for argument in [
            vec!["--nope"],
            vec!["extra"],
            vec!["--ttl-secs"],
            vec!["--label"],
        ] {
            assert!(
                parse_pair_args(&args(&argument)).is_err(),
                "expected a usage error for {argument:?}"
            );
        }
    }

    #[test]
    fn status_and_rotate_arguments_are_exact() {
        assert_eq!(parse_status_args(&[]), Ok(StatusArgs { json: false }));
        assert_eq!(
            parse_status_args(&args(&["--json"])),
            Ok(StatusArgs { json: true })
        );
        assert!(parse_status_args(&args(&["--nope"])).is_err());

        assert_eq!(parse_rotate_args(&args(&["read"])), Ok(TokenScope::Read));
        assert_eq!(
            parse_rotate_args(&args(&["control"])),
            Ok(TokenScope::Control)
        );
        // A scope is required, and exactly one of them.
        assert!(parse_rotate_args(&[]).is_err());
        assert!(parse_rotate_args(&args(&["read", "control"])).is_err());
        assert!(parse_rotate_args(&args(&["both"])).is_err());
        assert!(parse_rotate_args(&args(&["--json"])).is_err());
    }

    #[test]
    fn a_public_url_wins_and_is_normalized() {
        // Casing folds and the scheme's default port disappears, exactly as
        // they do for the `Origin` header this must match.
        let config = config_with("127.0.0.1:7788", "HTTPS://Fleet.Example.ts.net:443");
        assert_eq!(
            resolve_base_url(&config, Some(&marker("127.0.0.1:9999"))),
            Ok("https://fleet.example.ts.net".to_string())
        );
        assert_eq!(
            resolve_base_url(
                &config_with("127.0.0.1:7788", "http://fleet.lan:7788"),
                None
            ),
            Ok("http://fleet.lan:7788".to_string())
        );
        // A value the origin parser refuses (a path, a trailing slash, a
        // scheme nobody serves) names the key rather than producing a URL that
        // would not work.
        for public_url in [
            "not an origin",
            "https://fleet.example.ts.net/",
            "ftp://fleet.lan",
        ] {
            let broken = config_with("127.0.0.1:7788", public_url);
            let error = resolve_base_url(&broken, None).expect_err("a bad public_url is a refusal");
            assert!(error.contains("public_url"), "{public_url}: {error}");
        }
    }

    #[test]
    fn a_running_gateway_supplies_the_port_the_config_cannot() {
        // `--bind 127.0.0.1:0` is only knowable from the marker.
        let config = config_with("127.0.0.1:0", "");
        assert_eq!(
            resolve_base_url(&config, Some(&marker("127.0.0.1:45123"))),
            Ok("http://127.0.0.1:45123".to_string())
        );
        assert_eq!(
            resolve_base_url(&config_with("127.0.0.1:7788", ""), None),
            Ok("http://127.0.0.1:7788".to_string())
        );
        assert_eq!(
            resolve_base_url(&config_with("[::1]:7788", ""), None),
            Ok("http://[::1]:7788".to_string())
        );
    }

    #[test]
    fn an_address_nobody_can_dial_is_refused_by_name() {
        for bind in ["0.0.0.0:7788", "[::]:7788", "127.0.0.1:0"] {
            let error = resolve_base_url(&config_with(bind, ""), None)
                .expect_err("a wildcard or port-0 bind cannot make a URL");
            assert!(error.contains("public_url"), "{bind}: {error}");
        }
    }

    #[test]
    fn loopback_urls_are_recognized_so_the_warning_is_accurate() {
        for url in [
            "http://127.0.0.1:7788",
            "http://localhost:7788",
            "http://[::1]:7788",
        ] {
            assert!(is_loopback_url(url), "{url}");
        }
        for url in [
            "https://fleet.example.ts.net",
            "http://192.168.1.10:7788",
            "not an origin",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
    }

    #[test]
    fn durations_read_like_a_human_wrote_them() {
        assert_eq!(format_duration(600), "10 min");
        assert_eq!(format_duration(30), "30 s");
        assert_eq!(format_duration(3600), "1 h");
        assert_eq!(format_duration(86_400), "24 h");
        assert_eq!(format_duration(90), "90 s");
    }

    #[test]
    fn plurals_agree_with_their_counts() {
        assert_eq!(plural(0, "device", "devices"), "0 devices");
        assert_eq!(plural(1, "device", "devices"), "1 device");
        assert_eq!(plural(2, "device", "devices"), "2 devices");
    }

    /// A golden render: the QR is the only part of `pair` a human cannot check
    /// by reading, so its shape is pinned here.
    #[test]
    fn a_short_url_renders_a_stable_qr_code() {
        let qr = qr_text("http://a", false).expect("render");
        let lines: Vec<&str> = qr.lines().collect();
        // Version 1 is 21 modules plus a four-module quiet zone on each side,
        // two module rows to a line.
        assert_eq!(lines.len(), 15, "{qr}");
        for line in &lines {
            assert_eq!(line.chars().count(), 29, "{line}");
        }
        assert!(
            lines[0].chars().all(|ch| ch == ' '),
            "the quiet zone is blank: {:?}",
            lines[0]
        );
        assert!(
            qr.contains('\u{2588}') || qr.contains('\u{2580}') || qr.contains('\u{2584}'),
            "{qr}"
        );

        // Inverting swaps every module, so the same code has the same shape and
        // none of the same rows.
        let inverted = qr_text("http://a", true).expect("render inverted");
        assert_eq!(inverted.lines().count(), lines.len());
        assert_ne!(inverted, qr);
        assert!(
            inverted
                .lines()
                .next()
                .is_some_and(|line| line.chars().all(|ch| ch == '\u{2588}')),
            "an inverted quiet zone is solid: {:?}",
            inverted.lines().next()
        );
    }

    /// A real pairing URL is long enough to need a larger QR version; it must
    /// still fit an 80-column terminal.
    #[test]
    fn a_full_pairing_url_still_fits_a_terminal() {
        let url = format!(
            "http://127.0.0.1:7788/pair?code={}.{}",
            "a".repeat(64),
            "b".repeat(64)
        );
        let qr = qr_text(&url, false).expect("render");
        let width = qr
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        assert!(width <= 80, "a QR {width} columns wide does not fit");
        assert!(qr.lines().count() <= 40, "{}", qr.lines().count());
    }

    #[test]
    fn a_missing_or_stale_runtime_marker_is_not_a_running_gateway() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-gateway-ops-marker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        paths::create_private_dir(&dir).expect("create the gateway dir");
        let path = dir.join(paths::RUNTIME_FILE);

        assert_eq!(read_runtime_marker(&dir), None, "no file at all");

        let live = serde_json::json!({
            "pid": std::process::id(),
            "listen": "127.0.0.1:7788",
            "started_unix": unix_now(),
        });
        paths::write_private_file(&path, live.to_string().as_bytes()).expect("write");
        assert_eq!(
            read_runtime_marker(&dir),
            Some(RuntimeMarker {
                pid: std::process::id(),
                listen: "127.0.0.1:7788".parse().expect("addr"),
            })
        );

        // A crash leaves the file behind; pid 0 never names a live process.
        let stale = serde_json::json!({
            "pid": 0,
            "listen": "127.0.0.1:7788",
            "started_unix": unix_now(),
        });
        paths::write_private_file(&path, stale.to_string().as_bytes()).expect("write");
        assert_eq!(read_runtime_marker(&dir), None, "a stale marker is ignored");

        paths::write_private_file(&path, b"{ not json").expect("write");
        assert_eq!(
            read_runtime_marker(&dir),
            None,
            "an unreadable marker is ignored"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn health_is_probed_over_a_real_socket_and_fails_closed() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let served = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut raw = [0u8; 128];
                let _ = stream.read(&mut raw);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });

        assert!(probe_health(addr));
        // A wildcard address is dialled on loopback, which is where the test
        // server is.
        assert!(probe_health(SocketAddr::new(
            "0.0.0.0".parse().expect("ip"),
            addr.port()
        )));
        served.join().expect("the probe server thread");

        // Nothing is listening any more.
        assert!(!probe_health(addr));
    }
}
