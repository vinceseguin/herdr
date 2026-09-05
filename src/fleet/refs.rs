//! Host-qualified ids for fleet resources.
//!
//! Every server in the fleet numbers its own workspaces, tabs and panes, so
//! `w1:p1` alone is ambiguous once more than one server is attached. A fleet
//! reference is the host id, a `/`, and the server-side id — `workbox/w1:p1`.
//! Host ids never contain `/` (see [`HostId`]), so `split_once('/')` is an
//! unambiguous parse.
//!
//! Pure: no sockets, no async, no ratatui.

use std::fmt;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::fleet::hosts::HostId;

/// Whether a server-side id can appear on the right of a fleet reference.
///
/// Herdr numbers resources `w1`, `w1:t1`, `w1:p1`, so a well-behaved server
/// never sends `/` or an empty id. Snapshot ingestion drops the agents of any
/// server that does, rather than minting a reference whose string form would
/// parse back into a different host.
pub fn is_valid_resource_id(id: &str) -> bool {
    !id.is_empty() && !id.contains('/')
}

macro_rules! fleet_ref {
    (
        $(#[$meta:meta])*
        $name:ident, $field:ident, $label:literal, $example:literal
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            /// Host the resource lives on.
            pub host: HostId,
            #[doc = concat!("Server-side ", $label, " id, for example `", $example, "`.")]
            pub $field: String,
        }

        impl $name {
            #[doc = concat!("Build a ", $label, " reference from a host id and a server-side id.")]
            ///
            /// Infallible on purpose: references are built while ingesting a
            /// snapshot, where ids that would break the string form are
            /// filtered out with [`is_valid_resource_id`] instead of failing
            /// the whole snapshot.
            pub fn new(host: HostId, id: impl Into<String>) -> Self {
                Self {
                    host,
                    $field: id.into(),
                }
            }

            /// Host half of the reference.
            // The fields are public, so nothing in the fork needs the
            // accessors yet; E2's sidebar and E3's API read references
            // through them, and a reference is exactly the type where a
            // uniform call site is worth keeping.
            #[allow(dead_code)]
            pub fn host(&self) -> &HostId {
                &self.host
            }

            /// Server-side half of the reference.
            #[allow(dead_code)]
            pub fn id(&self) -> &str {
                &self.$field
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}/{}", self.host, self.$field)
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let Some((host, id)) = value.split_once('/') else {
                    return Err(format!(
                        concat!($label, " reference must look like \"host/", $example, "\": {:?}"),
                        value
                    ));
                };
                let host = HostId::new(host)
                    .map_err(|reason| format!(concat!($label, " reference host: {}"), reason))?;
                if !is_valid_resource_id(id) {
                    return Err(format!(
                        concat!($label, " reference must look like \"host/", $example, "\": {:?}"),
                        value
                    ));
                }
                Ok(Self {
                    host,
                    $field: id.to_string(),
                })
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse().map_err(de::Error::custom)
            }
        }
    };
}

fleet_ref!(
    /// A pane on one host, `host/w1:p1`.
    FleetPaneRef,
    pane_id,
    "pane",
    "w1:p1"
);

fleet_ref!(
    /// A tab on one host, `host/w1:t1`.
    FleetTabRef,
    tab_id,
    "tab",
    "w1:t1"
);

fleet_ref!(
    /// A workspace on one host, `host/w1`.
    FleetWorkspaceRef,
    workspace_id,
    "workspace",
    "w1"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> HostId {
        HostId::new(name).expect("valid host name")
    }

    #[test]
    fn references_round_trip_through_their_string_form() {
        let pane = FleetPaneRef::new(host("workbox"), "w1:p1");
        assert_eq!(pane.to_string(), "workbox/w1:p1");
        assert_eq!("workbox/w1:p1".parse::<FleetPaneRef>(), Ok(pane));

        let tab = FleetTabRef::new(host("local"), "w2:t3");
        assert_eq!(tab.to_string(), "local/w2:t3");
        assert_eq!("local/w2:t3".parse::<FleetTabRef>(), Ok(tab));

        let workspace = FleetWorkspaceRef::new(host("box-1"), "w1");
        assert_eq!(workspace.to_string(), "box-1/w1");
        assert_eq!("box-1/w1".parse::<FleetWorkspaceRef>(), Ok(workspace));
    }

    #[test]
    fn accessors_expose_both_halves() {
        let pane = FleetPaneRef::new(host("workbox"), "w1:p1");
        assert_eq!(pane.host().as_str(), "workbox");
        assert_eq!(pane.id(), "w1:p1");
    }

    #[test]
    fn parsing_rejects_missing_host_empty_and_nested_ids() {
        assert!("w1:p1".parse::<FleetPaneRef>().is_err());
        assert!("/w1:p1".parse::<FleetPaneRef>().is_err());
        assert!("workbox/".parse::<FleetPaneRef>().is_err());
        assert!("workbox/w1/p1".parse::<FleetPaneRef>().is_err());
        assert!("has space/w1:p1".parse::<FleetPaneRef>().is_err());
        assert!("".parse::<FleetWorkspaceRef>().is_err());
    }

    #[test]
    fn parse_errors_name_the_reference_kind() {
        let err = "w1:p1".parse::<FleetPaneRef>().expect_err("no host half");
        assert!(err.starts_with("pane reference must look like"), "{err}");
        let err = "/w1".parse::<FleetWorkspaceRef>().expect_err("empty host");
        assert!(err.starts_with("workspace reference host:"), "{err}");
    }

    #[test]
    fn serde_uses_the_string_form() {
        let pane = FleetPaneRef::new(host("workbox"), "w1:p1");
        let json = serde_json::to_string(&pane).expect("pane reference serializes");
        assert_eq!(json, "\"workbox/w1:p1\"");
        let decoded: FleetPaneRef = serde_json::from_str(&json).expect("pane reference decodes");
        assert_eq!(decoded, pane);

        let invalid = serde_json::from_str::<FleetPaneRef>("\"w1:p1\"");
        assert!(
            invalid.is_err(),
            "a reference without a host must not decode"
        );
    }

    #[test]
    fn resource_ids_that_break_the_string_form_are_rejected() {
        assert!(is_valid_resource_id("w1:p1"));
        assert!(!is_valid_resource_id(""));
        assert!(!is_valid_resource_id("w1/p1"));
    }

    #[test]
    fn references_order_by_host_then_id() {
        let mut refs = [
            FleetPaneRef::new(host("b"), "w1:p1"),
            FleetPaneRef::new(host("a"), "w2:p1"),
            FleetPaneRef::new(host("a"), "w1:p1"),
        ];
        refs.sort();
        assert_eq!(
            refs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["a/w1:p1", "a/w2:p1", "b/w1:p1"]
        );
    }
}
