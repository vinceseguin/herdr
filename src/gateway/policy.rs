//! Where the gateway may listen, and which browser origins it answers.
//!
//! Both are pure decisions over `[gateway]` and the resolved bind address, so
//! they are settled before a socket exists and are testable without one.
//!
//! The rules, from the epic's security contract:
//!
//! * Loopback is the default and needs no origin configuration — but it is
//!   still authenticated, because herdr's sockets are owner-only precisely so
//!   another local uid cannot read agent terminals.
//! * **Any** non-loopback bind is refused at startup unless
//!   `[gateway] allowed_origins` (or `public_url`) names at least one origin.
//!   A LAN or tailnet listener with no origin allowlist is one malicious page
//!   away from being driven by any browser on the machine.
//! * A request that carries an `Origin` header must match the allowlist
//!   exactly; a request without one (curl, a native client) passes this check
//!   and is decided by its token alone, because only browsers attach `Origin`.

use std::net::{IpAddr, SocketAddr};

use crate::config::{parse_gateway_origin, GatewayConfig, GatewayOrigin};

/// Whether a bind address is allowed by the configuration behind it.
pub struct BindPolicy;

impl BindPolicy {
    /// `Ok(())` when the gateway may listen on `bind`, `Err(message)` with a
    /// message naming the key to set when it may not.
    ///
    /// This is a startup decision, not a config diagnostic: the same config is
    /// valid for a loopback `--bind` and invalid for a LAN one.
    pub fn check(bind: SocketAddr, config: &GatewayConfig) -> Result<(), String> {
        if is_loopback(bind.ip()) {
            return Ok(());
        }
        if config
            .allowed_origins
            .iter()
            .any(|origin| parse_gateway_origin(origin).is_ok())
            || parse_gateway_origin(&config.public_url).is_ok()
        {
            return Ok(());
        }
        Err(format!(
            "refusing to bind {bind}: [gateway] allowed_origins is empty, so any web page could \
             drive this gateway. Set [gateway] allowed_origins (or public_url) to the origins \
             that may reach it, or bind a loopback address such as 127.0.0.1:{}.",
            bind.port()
        ))
    }
}

/// Whether an address is loopback, including an IPv4-mapped IPv6 loopback.
fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// The browser origins this gateway answers.
#[derive(Debug, Clone, Default)]
pub struct OriginAllowlist {
    origins: Vec<GatewayOrigin>,
}

impl OriginAllowlist {
    /// Build the allowlist for the address the gateway actually bound.
    ///
    /// It is `[gateway] allowed_origins`, plus `public_url` when set, plus —
    /// **only for a loopback bind** — the origins that bind serves itself
    /// (`http://127.0.0.1:<port>`, `http://localhost:<port>`,
    /// `http://[::1]:<port>` and the bound IP itself), so a local browser works
    /// out of the box without the user configuring their own address. A
    /// non-loopback bind gets no implicit origins: it already refused to start
    /// without configured ones.
    ///
    /// Pass the address after binding, not the requested one: a `--bind
    /// 127.0.0.1:0` only knows its port once the listener exists.
    pub fn for_bind(bind: SocketAddr, config: &GatewayConfig) -> Self {
        let mut origins: Vec<GatewayOrigin> = Vec::new();
        let mut push = |origin: GatewayOrigin| {
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        };

        for value in &config.allowed_origins {
            if let Ok(origin) = parse_gateway_origin(value) {
                push(origin);
            }
        }
        if let Ok(origin) = parse_gateway_origin(&config.public_url) {
            push(origin);
        }
        if is_loopback(bind.ip()) {
            // Through the parser, so the scheme's default port folds away the
            // same way it does for a browser's `Origin` header.
            let own = format!("http://{bind}");
            for value in ["127.0.0.1", "localhost", "[::1]"]
                .iter()
                .map(|host| format!("http://{host}:{}", bind.port()))
                .chain(std::iter::once(own))
            {
                if let Ok(origin) = parse_gateway_origin(&value) {
                    push(origin);
                }
            }
        }

        Self { origins }
    }

    /// Whether an `Origin` header value is allowed.
    ///
    /// Anything that is not a `scheme://host[:port]` origin — `null`, a path,
    /// a trailing slash, an unsupported scheme — fails to parse and is
    /// refused; matching is exact on scheme and port and case-insensitive on
    /// host, which is RFC 6454 origin equality.
    pub fn allows(&self, origin_header: &str) -> bool {
        let Ok(candidate) = parse_gateway_origin(origin_header) else {
            return false;
        };
        self.origins.iter().any(|allowed| {
            allowed.scheme == candidate.scheme
                && allowed.host == candidate.host
                && allowed.port == candidate.port
        })
    }

    /// Whether any origin is allowed at all. An empty allowlist refuses every
    /// browser while still serving header-less clients.
    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }

    /// The allowed origins. Nothing the gateway serves reports them — a
    /// browser learns it is refused from the `origin_not_allowed` answer, and
    /// `herdr gateway status` reads files rather than the daemon's memory — so
    /// this is the tests' window into what `for_bind` decided.
    #[cfg(test)]
    pub fn origins(&self) -> &[GatewayOrigin] {
        &self.origins
    }

    /// Whether device cookies minted for this gateway must be `Secure`: true
    /// when the advertised public origin is https.
    pub fn requires_secure_cookies(config: &GatewayConfig) -> bool {
        parse_gateway_origin(&config.public_url).is_ok_and(|origin| origin.scheme == "https")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(value: &str) -> SocketAddr {
        value.parse().expect("socket address")
    }

    fn config_with(origins: &[&str], public_url: &str) -> GatewayConfig {
        GatewayConfig {
            allowed_origins: origins.iter().map(|value| (*value).to_string()).collect(),
            public_url: public_url.to_string(),
            ..GatewayConfig::default()
        }
    }

    #[test]
    fn loopback_binds_need_no_origins() {
        let config = GatewayConfig::default();
        for bind in ["127.0.0.1:7788", "127.0.0.53:80", "[::1]:7788"] {
            BindPolicy::check(addr(bind), &config).unwrap_or_else(|err| panic!("{bind}: {err}"));
        }
        // An IPv4-mapped IPv6 loopback is the same machine.
        BindPolicy::check(addr("[::ffff:127.0.0.1]:7788"), &config).expect("mapped loopback");
    }

    #[test]
    fn non_loopback_binds_are_refused_without_origins() {
        let config = GatewayConfig::default();
        for bind in ["0.0.0.0:7788", "192.168.1.10:7788", "[::]:7788"] {
            let err = BindPolicy::check(addr(bind), &config)
                .expect_err("a non-loopback bind must be refused");
            assert!(err.contains("allowed_origins"), "{err}");
            assert!(
                err.contains(bind),
                "the message must name the address: {err}"
            );
        }
    }

    #[test]
    fn an_origin_or_a_public_url_unlocks_a_non_loopback_bind() {
        let bind = addr("0.0.0.0:7788");
        BindPolicy::check(bind, &config_with(&["https://fleet.example.ts.net"], ""))
            .expect("allowed_origins is enough");
        BindPolicy::check(bind, &config_with(&[], "https://fleet.example.ts.net"))
            .expect("public_url is enough");
        // An unparseable origin is not an origin.
        BindPolicy::check(bind, &config_with(&["not an origin"], ""))
            .expect_err("a malformed origin must not unlock a public bind");
    }

    #[test]
    fn a_loopback_bind_allows_its_own_origins_only() {
        let allowlist =
            OriginAllowlist::for_bind(addr("127.0.0.1:7788"), &GatewayConfig::default());
        assert!(allowlist.allows("http://127.0.0.1:7788"));
        assert!(allowlist.allows("http://localhost:7788"));
        assert!(allowlist.allows("http://[::1]:7788"));
        assert!(
            allowlist.allows("HTTP://LocalHost:7788"),
            "host case is ignored"
        );
        assert!(
            !allowlist.allows("http://localhost:7789"),
            "the port is exact"
        );
        assert!(
            !allowlist.allows("https://localhost:7788"),
            "the scheme is exact"
        );
        assert!(
            !allowlist.allows("http://localhost"),
            "a missing port is not the bound port"
        );
        assert!(
            !allowlist.allows("http://127.0.0.53:7788"),
            "another loopback address is not the bound one"
        );
        assert_eq!(allowlist.origins().len(), 3, "{:?}", allowlist.origins());
    }

    #[test]
    fn a_loopback_bind_allows_its_own_address_and_default_port() {
        let allowlist =
            OriginAllowlist::for_bind(addr("127.0.0.53:7788"), &GatewayConfig::default());
        assert!(allowlist.allows("http://127.0.0.53:7788"));
        assert!(allowlist.allows("http://127.0.0.1:7788"));
        assert!(!allowlist.allows("http://127.0.0.54:7788"));

        // On port 80 a browser sends `http://localhost`, never `:80`.
        let allowlist = OriginAllowlist::for_bind(addr("127.0.0.1:80"), &GatewayConfig::default());
        assert!(allowlist.allows("http://localhost"));
        assert!(allowlist.allows("http://localhost:80"));
        assert!(!allowlist.allows("https://localhost"));

        let allowlist = OriginAllowlist::for_bind(addr("[::1]:7788"), &GatewayConfig::default());
        assert!(allowlist.allows("http://[::1]:7788"));
        assert!(allowlist.allows("http://[0:0:0:0:0:0:0:1]:7788"));
    }

    #[test]
    fn a_public_bind_gets_no_implicit_origins() {
        let config = config_with(&["https://fleet.example.ts.net"], "");
        let allowlist = OriginAllowlist::for_bind(addr("0.0.0.0:7788"), &config);
        assert!(allowlist.allows("https://fleet.example.ts.net"));
        assert!(!allowlist.allows("http://127.0.0.1:7788"));
        assert!(!allowlist.allows("http://0.0.0.0:7788"));
    }

    #[test]
    fn nothing_that_is_not_an_exact_origin_is_allowed() {
        let config = config_with(&["https://fleet.example.ts.net"], "");
        let allowlist = OriginAllowlist::for_bind(addr("0.0.0.0:7788"), &config);
        for candidate in [
            "null",
            "",
            "https://fleet.example.ts.net/",
            "https://fleet.example.ts.net/app",
            "https://evil.fleet.example.ts.net",
            "https://fleet.example.ts.net.evil.test",
            "https://fleet.example.ts.net:8443",
            "https://fleet.example.ts.net:80",
            "http://fleet.example.ts.net",
            "http://fleet.example.ts.net:443",
            "https://fleet.example.ts.net.",
            "file://fleet.example.ts.net",
            "https://user@fleet.example.ts.net",
            "https://fleet.example.ts.net\u{0}",
            "https://fleet.example.ts.net ",
            " https://fleet.example.ts.net",
            "fleet.example.ts.net",
        ] {
            assert!(
                !allowlist.allows(candidate),
                "{candidate:?} must not be allowed"
            );
        }
        // The scheme's default port is the same origin, exactly as a browser
        // (which never sends `:443` for https) sees it.
        assert!(allowlist.allows("https://fleet.example.ts.net:443"));
        assert!(allowlist.allows("HTTPS://FLEET.EXAMPLE.TS.NET"));
    }

    #[test]
    fn the_public_url_is_implicitly_allowed_and_selects_secure_cookies() {
        let config = config_with(&[], "https://fleet.example.ts.net");
        let allowlist = OriginAllowlist::for_bind(addr("127.0.0.1:7788"), &config);
        assert!(allowlist.allows("https://fleet.example.ts.net"));
        assert!(
            allowlist.allows("http://127.0.0.1:7788"),
            "loopback still works"
        );
        assert!(OriginAllowlist::requires_secure_cookies(&config));
        assert!(!OriginAllowlist::requires_secure_cookies(&config_with(
            &[],
            "http://fleet.example.test"
        )));
        assert!(!OriginAllowlist::requires_secure_cookies(
            &GatewayConfig::default()
        ));
    }

    #[test]
    fn duplicate_and_malformed_origins_are_folded_out() {
        let config = config_with(
            &[
                "https://a.test",
                "HTTPS://A.test",
                "nope",
                "http://b.test:8080",
            ],
            "https://a.test",
        );
        let allowlist = OriginAllowlist::for_bind(addr("0.0.0.0:7788"), &config);
        assert_eq!(allowlist.origins().len(), 2, "{:?}", allowlist.origins());
        assert!(allowlist.allows("https://a.test"));
        assert!(allowlist.allows("http://b.test:8080"));
        assert!(!allowlist.is_empty());
        assert!(OriginAllowlist::default().is_empty());
    }
}
