//! Typed fleet host specs resolved from `[fleet]`.
//!
//! Pure: no sockets, no async, no filesystem. `[fleet]` is validated once by
//! [`resolve_hosts`], which is the only place configuration becomes specs.

use std::fmt;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{
    validate_fleet_host_name, FleetConfig, FleetHostConfig, FleetHostKind, FLEET_LOCAL_HOST_NAME,
};

/// Stable identifier of one host in the fleet.
///
/// Host ids are the `host` half of the `host/w1:p1` id form, so they may never
/// contain `/`. Construction always goes through [`HostId::new`], including on
/// the deserialization path, so an id read back from JSON obeys the same rule
/// as one built from config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(String);

impl HostId {
    /// Id of this machine's default session, reserved by `include_local`.
    pub const LOCAL: &str = FLEET_LOCAL_HOST_NAME;

    pub fn new(name: &str) -> Result<Self, String> {
        validate_fleet_host_name(name)?;
        // `validate_fleet_host_name` restricts the name to
        // `[A-Za-z0-9._-]`, so this is a belt-and-braces assertion of the one
        // property the `host/pane` id form depends on.
        if name.contains('/') {
            return Err("host name may not contain '/'".to_string());
        }
        Ok(Self(name.to_string()))
    }

    /// The id of this machine's default session.
    pub fn local() -> Self {
        Self(Self::LOCAL.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_local(&self) -> bool {
        self.0 == Self::LOCAL
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for HostId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for HostId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HostId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(de::Error::custom)
    }
}

/// How the fleet reaches a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKind {
    /// A herdr server on this machine. `session` is `None` for the default
    /// session and `Some(name)` for a named one.
    Local { session: Option<String> },
    /// A herdr server reached over the ssh stdio bridge.
    Ssh {
        target: String,
        session: Option<String>,
    },
}

impl HostKind {
    /// Wire/report name of the transport; matches `[[fleet.hosts]].kind`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::Ssh { .. } => "ssh",
        }
    }

    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Local { .. } => None,
            Self::Ssh { target, .. } => Some(target.as_str()),
        }
    }

    pub fn session(&self) -> Option<&str> {
        match self {
            Self::Local { session } | Self::Ssh { session, .. } => session.as_deref(),
        }
    }
}

/// One resolved host: what to connect to, and whether the fleet should.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    pub id: HostId,
    pub kind: HostKind,
    pub enabled: bool,
}

impl HostSpec {
    /// This machine's default session.
    pub fn local_default() -> Self {
        Self {
            id: HostId::local(),
            kind: HostKind::Local { session: None },
            enabled: true,
        }
    }
}

/// Turn `[fleet]` into ordered host specs.
///
/// The implicit `local` host comes first when `include_local` is set, then the
/// configured hosts in file order. Disabled hosts are kept as disabled specs so
/// callers can show them without reparsing config.
///
/// `Err` carries every diagnostic [`FleetConfig::diagnostics`] produced, so a
/// CLI can refuse to run on an invalid section while the TUI keeps showing the
/// diagnostic banner.
pub fn resolve_hosts(config: &FleetConfig) -> Result<Vec<HostSpec>, Vec<String>> {
    let diagnostics = config.diagnostics();
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let mut specs = Vec::with_capacity(config.hosts.len() + usize::from(config.include_local));
    if config.include_local {
        specs.push(HostSpec::local_default());
    }

    for (index, host) in config.hosts.iter().enumerate() {
        let Some(spec) = host_spec(host) else {
            // Unreachable: `diagnostics()` above rejects every entry this can
            // fail on. Reported rather than unwrapped so a future drift
            // between the two becomes a diagnostic, not a panic.
            return Err(vec![format!(
                "invalid fleet host: fleet.hosts[{index}].name = {:?}; ignoring [fleet] hosts",
                host.name
            )]);
        };
        specs.push(spec);
    }

    Ok(specs)
}

/// Build one spec from a `[[fleet.hosts]]` entry that
/// [`FleetConfig::diagnostics`] already accepted.
fn host_spec(host: &FleetHostConfig) -> Option<HostSpec> {
    let id = HostId::new(&host.name).ok()?;
    let kind = match host.kind {
        FleetHostKind::Local => HostKind::Local {
            session: host.session.clone(),
        },
        FleetHostKind::Ssh => HostKind::Ssh {
            target: host.target.clone()?,
            session: host.session.clone(),
        },
    };
    Some(HostSpec {
        id,
        kind,
        enabled: host.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet(toml: &str) -> FleetConfig {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Wrapper {
            fleet: FleetConfig,
        }

        toml::from_str::<Wrapper>(toml)
            .expect("fleet config fixture parses")
            .fleet
    }

    #[test]
    fn host_id_accepts_session_name_characters() {
        for name in ["local", "workbox", "box-1", "box_1", "box.1", "A9"] {
            assert_eq!(
                HostId::new(name).expect("valid host name").as_str(),
                name,
                "{name} should be accepted"
            );
        }
    }

    #[test]
    fn host_id_rejects_slashes_empty_and_overlong_names() {
        assert!(HostId::new("a/b").is_err());
        assert!(HostId::new("").is_err());
        assert!(HostId::new(&"a".repeat(65)).is_err());
        assert!(HostId::new("..").is_err());
        assert!(HostId::new("has space").is_err());
    }

    #[test]
    fn host_id_errors_talk_about_hosts_not_sessions() {
        let err = HostId::new("").expect_err("empty host name is invalid");
        assert_eq!(err, "host name cannot be empty");
        let err = HostId::new("a/b").expect_err("slash is invalid");
        assert!(err.contains("host name"), "{err}");
        assert!(!err.contains("session name"), "{err}");
    }

    #[test]
    fn host_id_round_trips_through_string_forms() {
        let id: HostId = "workbox".parse().expect("parses");
        assert_eq!(id.to_string(), "workbox");
        assert_eq!(
            serde_json::to_string(&id).expect("serializes"),
            "\"workbox\""
        );
        assert_eq!(
            serde_json::from_str::<HostId>("\"workbox\"").expect("deserializes"),
            id
        );
        assert!(serde_json::from_str::<HostId>("\"work/box\"").is_err());
        assert!(HostId::local().is_local());
        assert_eq!(HostId::LOCAL, "local");
    }

    #[test]
    fn resolve_hosts_puts_the_implicit_local_host_first() {
        let config = fleet(
            r#"
[fleet]
[[fleet.hosts]]
name = "workbox"
target = "workbox"
"#,
        );
        let specs = resolve_hosts(&config).expect("valid fleet config");
        assert_eq!(
            specs,
            vec![
                HostSpec::local_default(),
                HostSpec {
                    id: HostId::new("workbox").expect("valid"),
                    kind: HostKind::Ssh {
                        target: "workbox".to_string(),
                        session: None,
                    },
                    enabled: true,
                },
            ]
        );
    }

    #[test]
    fn resolve_hosts_honours_include_local_false() {
        let config = fleet(
            r#"
[fleet]
include_local = false
[[fleet.hosts]]
name = "local"
kind = "local"
session = "agents"
"#,
        );
        let specs = resolve_hosts(&config).expect("valid fleet config");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id.as_str(), "local");
        assert_eq!(
            specs[0].kind,
            HostKind::Local {
                session: Some("agents".to_string()),
            }
        );
    }

    #[test]
    fn resolve_hosts_keeps_disabled_hosts_as_disabled_specs() {
        let config = fleet(
            r#"
[fleet]
include_local = false
[[fleet.hosts]]
name = "workbox"
target = "workbox"
enabled = false
"#,
        );
        let specs = resolve_hosts(&config).expect("valid fleet config");
        assert_eq!(specs.len(), 1);
        assert!(!specs[0].enabled);
    }

    #[test]
    fn resolve_hosts_preserves_file_order_and_kinds() {
        let config = fleet(
            r#"
[fleet]
[[fleet.hosts]]
name = "b"
target = "b.example"
session = "agents"
[[fleet.hosts]]
name = "a"
kind = "local"
session = "side"
"#,
        );
        let specs = resolve_hosts(&config).expect("valid fleet config");
        let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids, vec!["local", "b", "a"]);
        assert_eq!(specs[1].kind.as_str(), "ssh");
        assert_eq!(specs[1].kind.target(), Some("b.example"));
        assert_eq!(specs[1].kind.session(), Some("agents"));
        assert_eq!(specs[2].kind.as_str(), "local");
        assert_eq!(specs[2].kind.target(), None);
        assert_eq!(specs[2].kind.session(), Some("side"));
    }

    #[test]
    fn resolve_hosts_returns_every_diagnostic() {
        let config = fleet(
            r#"
[fleet]
[[fleet.hosts]]
name = "local"
kind = "local"
[[fleet.hosts]]
name = "bad/name"
[[fleet.hosts]]
name = "workbox"
[[fleet.hosts]]
name = "workbox"
target = "-oProxyCommand=id"
"#,
        );
        let diagnostics = resolve_hosts(&config).expect_err("invalid fleet config");
        assert_eq!(diagnostics, config.diagnostics());
        assert_eq!(diagnostics.len(), 7, "{diagnostics:#?}");
        assert!(diagnostics
            .iter()
            .any(|d| d.contains("reserved fleet host name")));
        assert!(diagnostics
            .iter()
            .any(|d| d.contains("missing fleet host session")));
        assert!(diagnostics
            .iter()
            .any(|d| d.contains("invalid fleet host name")));
        assert!(diagnostics
            .iter()
            .any(|d| d.contains("duplicate fleet host name")));
        assert!(diagnostics
            .iter()
            .any(|d| d.contains("invalid fleet host target")));
    }

    #[test]
    fn resolve_hosts_on_defaults_yields_only_the_local_host() {
        let specs = resolve_hosts(&FleetConfig::default()).expect("defaults are valid");
        assert_eq!(specs, vec![HostSpec::local_default()]);
    }
}
