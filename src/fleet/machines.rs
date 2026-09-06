//! Saved machine profiles (`herdr machine …`) as fleet hosts.
//!
//! Pure: no sockets, no async, no filesystem. Reading the catalog is
//! [`super::hosts_source`]'s job; this module only decides what the profiles it
//! is handed mean for the fleet, so the id derivation and every diagnostic are
//! testable without a state directory.
//!
//! A fleet host id addresses a machine: it is the `host` half of every
//! `host/w1:p1` reference, so the wrong mapping sends a terminal to the wrong
//! machine. The derivation is therefore deliberately boring and **frozen**:
//! the label when [`HostId::new`] accepts it verbatim, otherwise the label
//! lowercased with every run of other characters folded to a single `-`. Two
//! machines that derive the same id, a machine that derives a `[[fleet.hosts]]`
//! name, and a machine that derives the reserved `local` are all refused rather
//! than silently merged.

use std::collections::BTreeMap;

use crate::config::validate_fleet_host_target;
use crate::fleet::hosts::{HostId, HostKind, HostSpec};

/// One machine saved by `herdr machine add`, as the fleet sees it.
///
/// A fleet-owned mirror of upstream's `SavedSshEndpoint` so nothing under
/// `src/fleet/` outside [`super::hosts_source`] depends on `crate::client`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineProfile {
    /// Catalog profile id — what `herdr machine rename` takes, so every
    /// diagnostic can name the exact command that fixes it.
    pub id: String,
    pub label: String,
    pub target: String,
    pub session: String,
    pub enabled: bool,
}

/// The fleet host id a machine label maps to.
///
/// The label wins when it is already a valid host name (so `workbox` stays
/// `workbox`, case included); otherwise it is lowercased and every run of
/// characters outside `[A-Za-z0-9._-]` becomes a single `-`, with leading and
/// trailing runs dropped. `Err` carries the reason the derived id is still not
/// a host name, and never a fix — the caller knows which machine it was.
pub fn machine_host_id(label: &str) -> Result<HostId, String> {
    if let Ok(id) = HostId::new(label) {
        return Ok(id);
    }
    HostId::new(&slug(label))
}

/// The derived id text, whether or not it is a valid host name.
///
/// Kept separate from [`machine_host_id`] so a diagnostic can quote what the
/// label folded to.
fn derived_id(label: &str) -> String {
    if HostId::new(label).is_ok() {
        return label.to_string();
    }
    slug(label)
}

/// `"My Laptop (home)"` → `"my-laptop-home"`; `"///"` → `""`.
fn slug(label: &str) -> String {
    let mut slug = String::with_capacity(label.len());
    let mut separator_pending = false;
    for character in label.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            // A pending run before the first kept character is a leading run:
            // dropped, not turned into a leading '-'.
            if separator_pending && !slug.is_empty() {
                slug.push('-');
            }
            separator_pending = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            separator_pending = true;
        }
    }
    // A trailing run is never flushed, so it is dropped the same way.
    slug
}

/// Turn saved machines into ssh host specs, in catalog order.
///
/// `existing` is what `[fleet]` already resolved to; machines are appended
/// after it and may never take one of its ids. Validation is all-or-nothing
/// like [`crate::fleet::hosts::resolve_hosts`]: **any** diagnostic, including
/// one on a disabled machine, means no machine becomes a host, so a bad label
/// can never quietly re-point a host id at another machine.
pub fn machine_host_specs(
    profiles: &[MachineProfile],
    existing: &[HostSpec],
) -> Result<Vec<HostSpec>, Vec<String>> {
    let mut diagnostics = Vec::new();
    let mut specs = Vec::with_capacity(profiles.len());
    // Derived id → the machine that claimed it first, so a collision names both.
    let mut claimed: BTreeMap<String, &MachineProfile> = BTreeMap::new();

    for profile in profiles {
        let named = |what: &str, detail: String, fix: &str| {
            format!(
                "{what}: machine {:?} ({}) {detail}; {fix}; ignoring [fleet] hosts",
                profile.label, profile.id
            )
        };
        let rename = format!(
            "rename it with: herdr machine rename {} --label <name>",
            profile.id
        );
        let readd = format!("remove it with: herdr machine remove {}", profile.id);

        let mut id = match machine_host_id(&profile.label) {
            Ok(id) => Some(id),
            Err(reason) => {
                diagnostics.push(named(
                    "invalid saved machine host name",
                    format!(
                        "derives the fleet host id {:?}; {reason}",
                        derived_id(&profile.label)
                    ),
                    &rename,
                ));
                None
            }
        };

        if let Some(candidate) = id.as_ref() {
            if candidate.is_local() {
                diagnostics.push(named(
                    "reserved saved machine host name",
                    format!(
                        "derives the fleet host id {:?}, which is reserved for this machine's default session",
                        HostId::LOCAL
                    ),
                    &rename,
                ));
                id = None;
            }
        }

        if let Some(candidate) = id.as_ref() {
            if existing.iter().any(|spec| &spec.id == candidate) {
                diagnostics.push(named(
                    "duplicate saved machine host name",
                    format!(
                        "derives the fleet host id {:?}, which is already a [fleet] host name",
                        candidate.as_str()
                    ),
                    &rename,
                ));
                id = None;
            }
        }

        if let Some(candidate) = id.as_ref() {
            match claimed.get(candidate.as_str()) {
                Some(first) => {
                    diagnostics.push(format!(
                        "duplicate saved machine host name: machines {:?} ({}) and {:?} ({}) both derive the fleet host id {:?}; {rename}; ignoring [fleet] hosts",
                        first.label,
                        first.id,
                        profile.label,
                        profile.id,
                        candidate.as_str()
                    ));
                    id = None;
                }
                None => {
                    claimed.insert(candidate.as_str().to_string(), profile);
                }
            }
        }

        // The catalog validates targets with `--remote`'s rule, which is
        // weaker than the fleet's: it allows whitespace, and an ssh
        // destination is one argv element. Re-check rather than trust it.
        if let Err(reason) = validate_fleet_host_target(&profile.target) {
            diagnostics.push(named(
                "invalid saved machine target",
                format!("has the ssh target {:?}; {reason}", profile.target),
                &readd,
            ));
            id = None;
        }

        if let Err(reason) = crate::session::validate_name(&profile.session) {
            diagnostics.push(named(
                "invalid saved machine session",
                format!("has the session {:?}; {reason}", profile.session),
                &readd,
            ));
            id = None;
        }

        let Some(id) = id else {
            continue;
        };
        specs.push(HostSpec {
            id,
            kind: HostKind::Ssh {
                target: profile.target.clone(),
                session: Some(profile.session.clone()),
            },
            enabled: profile.enabled,
        });
    }

    if diagnostics.is_empty() {
        Ok(specs)
    } else {
        Err(diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fleet::state::FleetState;

    fn profile(label: &str) -> MachineProfile {
        MachineProfile {
            id: format!("{:032x}", label.len()),
            label: label.to_string(),
            target: "example.invalid".to_string(),
            session: "agents".to_string(),
            enabled: true,
        }
    }

    fn config_host(name: &str) -> HostSpec {
        HostSpec {
            id: HostId::new(name).expect("valid host name"),
            kind: HostKind::Local {
                session: Some(name.to_string()),
            },
            enabled: true,
        }
    }

    #[test]
    fn a_valid_label_is_the_host_id_verbatim() {
        for label in ["workbox", "box-1", "box_1", "box.1", "A9", "Workbox"] {
            assert_eq!(
                machine_host_id(label).expect("valid label").as_str(),
                label,
                "{label} should be used as-is"
            );
        }
    }

    #[test]
    fn an_invalid_label_folds_to_a_slug() {
        for (label, expected) in [
            ("My Laptop (home)", "my-laptop-home"),
            ("  spaced  out  ", "spaced-out"),
            ("build@ci", "build-ci"),
            ("a///b", "a-b"),
            ("Ünïcødé box", "n-c-d-box"),
        ] {
            assert_eq!(
                machine_host_id(label).expect("label folds").as_str(),
                expected,
                "{label:?} folded wrong"
            );
        }
    }

    #[test]
    fn a_label_that_folds_to_nothing_is_an_error() {
        for label in ["///", "", "   ", "。。"] {
            assert!(
                machine_host_id(label).is_err(),
                "{label:?} should not derive a host id"
            );
        }
        // `..` survives the fold (both characters are kept) and is still not a
        // valid host name.
        assert!(machine_host_id("..").is_err());
        assert!(machine_host_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn machines_become_ssh_specs_in_catalog_order() {
        let profiles = vec![
            MachineProfile {
                enabled: false,
                ..profile("workbox")
            },
            MachineProfile {
                target: "user@build.example".to_string(),
                session: "ci".to_string(),
                ..profile("Build Box")
            },
        ];
        let specs = machine_host_specs(&profiles, &[]).expect("valid machines");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id.as_str(), "workbox");
        assert_eq!(specs[0].kind.as_str(), "ssh");
        assert_eq!(specs[0].kind.target(), Some("example.invalid"));
        assert_eq!(specs[0].kind.session(), Some("agents"));
        assert!(
            !specs[0].enabled,
            "a disabled machine stays a disabled host"
        );
        assert_eq!(specs[1].id.as_str(), "build-box");
        assert_eq!(specs[1].kind.target(), Some("user@build.example"));
        assert_eq!(specs[1].kind.session(), Some("ci"));
        assert!(specs[1].enabled);
    }

    #[test]
    fn no_machines_is_no_specs() {
        assert_eq!(machine_host_specs(&[], &[]), Ok(Vec::new()));
    }

    #[test]
    fn an_underivable_id_names_the_machine_and_the_rename_command() {
        let diagnostics =
            machine_host_specs(&[profile("///")], &[]).expect_err("no host id derivable");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        let diagnostic = &diagnostics[0];
        assert!(
            diagnostic.contains("invalid saved machine host name"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("\"///\""), "{diagnostic}");
        assert!(
            diagnostic.contains("herdr machine rename 00000000000000000000000000000003 --label"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("ignoring [fleet] hosts"),
            "{diagnostic}"
        );
    }

    #[test]
    fn the_reserved_local_id_is_refused() {
        for label in ["local", "LOCAL "] {
            let diagnostics =
                machine_host_specs(&[profile(label)], &[]).expect_err("local is reserved");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
            assert!(
                diagnostics[0].contains("reserved saved machine host name")
                    && diagnostics[0].contains("\"local\""),
                "{}",
                diagnostics[0]
            );
        }
    }

    #[test]
    fn two_machines_folding_to_one_id_name_both() {
        let profiles = vec![profile("Build Box"), profile("build-box!")];
        let diagnostics = machine_host_specs(&profiles, &[]).expect_err("ids collide");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        let diagnostic = &diagnostics[0];
        assert!(
            diagnostic.contains("duplicate saved machine host name"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("\"Build Box\""), "{diagnostic}");
        assert!(diagnostic.contains("\"build-box!\""), "{diagnostic}");
        assert!(diagnostic.contains("\"build-box\""), "{diagnostic}");
    }

    #[test]
    fn a_machine_may_not_take_a_configured_host_name() {
        let existing = vec![config_host("workbox")];
        let diagnostics =
            machine_host_specs(&[profile("workbox")], &existing).expect_err("name is taken");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(
            diagnostics[0].contains("already a [fleet] host name"),
            "{}",
            diagnostics[0]
        );
    }

    #[test]
    fn a_malformed_target_or_session_is_refused() {
        let diagnostics = machine_host_specs(
            &[MachineProfile {
                target: "-oProxyCommand=id".to_string(),
                ..profile("workbox")
            }],
            &[],
        )
        .expect_err("targets are re-validated");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(
            diagnostics[0].contains("invalid saved machine target")
                && diagnostics[0].contains("herdr machine remove"),
            "{}",
            diagnostics[0]
        );

        let diagnostics = machine_host_specs(
            &[MachineProfile {
                target: "build box".to_string(),
                session: "a/b".to_string(),
                ..profile("workbox")
            }],
            &[],
        )
        .expect_err("sessions are re-validated");
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("invalid saved machine target")),
            "{diagnostics:#?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("invalid saved machine session")),
            "{diagnostics:#?}"
        );
    }

    #[test]
    fn every_problem_is_reported_and_nothing_resolves() {
        let profiles = vec![
            profile("workbox"),
            profile("///"),
            MachineProfile {
                enabled: false,
                ..profile("local")
            },
        ];
        let diagnostics = machine_host_specs(&profiles, &[]).expect_err("two machines are invalid");
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    }

    #[test]
    fn a_mixed_fleet_of_config_hosts_and_machines_keeps_its_invariants() {
        let mut existing = vec![HostSpec::local_default(), config_host("scratch")];
        let machines = machine_host_specs(&[profile("workbox"), profile("Build Box")], &existing)
            .expect("valid machines");
        existing.extend(machines);
        let ids: Vec<&str> = existing.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids, vec!["local", "scratch", "workbox", "build-box"]);

        let state = FleetState::new(existing);
        state.assert_invariants_for_test();
    }
}
