//! The one place a whole `Config` becomes fleet host specs.
//!
//! `[fleet]` is only half the fleet once `include_machines` is on: the other
//! half is the machines saved by `herdr machine add`, which live in the
//! client's endpoint catalog. Every fleet consumer — `herdr fleet status`
//! ([`crate::fleet::oneshot`]) and the gateway (`src/gateway/fleet.rs`) —
//! resolves hosts through [`hosts_for_config`] so both always see the same
//! fleet.
//!
//! This is deliberately the only module under `src/fleet/` that names
//! `crate::client`: the mapping itself is pure and lives in
//! [`crate::fleet::machines`], so the catalog is a filesystem detail confined
//! to one function here.

use crate::config::Config;
use crate::fleet::hosts::{resolve_hosts, HostSpec};
use crate::fleet::machines::{machine_host_specs, MachineProfile};

/// Resolve `[fleet]`, then append the saved machines when they are opted in.
///
/// `Err` carries every diagnostic, all-or-nothing: an invalid `[fleet]`
/// section or an unusable machine catalog is an operator error the caller
/// reports and exits on, never a host failure.
pub fn hosts_for_config(config: &Config) -> Result<Vec<HostSpec>, Vec<String>> {
    hosts_from(config, load_saved_machines)
}

/// [`hosts_for_config`] over an injectable catalog, so the merge rules are
/// testable without a state directory.
fn hosts_from(
    config: &Config,
    load: impl FnOnce() -> Result<Vec<MachineProfile>, String>,
) -> Result<Vec<HostSpec>, Vec<String>> {
    let mut specs = resolve_hosts(&config.fleet)?;
    if !config.fleet.include_machines {
        return Ok(specs);
    }
    let profiles = load().map_err(|error| {
        vec![format!(
            "saved machines unavailable: {error}; repair or remove the endpoint catalog, or set fleet.include_machines = false; ignoring [fleet] hosts"
        )]
    })?;
    let machines = machine_host_specs(&profiles, &specs)?;
    specs.extend(machines);
    Ok(specs)
}

/// The saved machines, as the fleet sees them.
///
/// A missing catalog is an empty list, not an error: `include_machines = true`
/// before the first `herdr machine add` is a valid configuration.
///
/// The catalog validates every profile as it loads (32-hex profile ids,
/// labels without control characters, `--remote`-shaped targets, session
/// names), so a corrupt or hand-edited file is one `Err` here rather than a
/// profile with an unexpected shape reaching the mapper. The error names the
/// file: upstream's parse error does not, and the user has to find it.
fn load_saved_machines() -> Result<Vec<MachineProfile>, String> {
    use crate::client::endpoint::{catalog_path, EndpointCatalog};

    Ok(EndpointCatalog::load_profiles()
        .map_err(|error| format!("{error}; catalog: {}", catalog_path().display()))?
        .into_iter()
        .map(|profile| MachineProfile {
            id: profile.id.as_str().to_string(),
            label: profile.label,
            target: profile.target,
            session: profile.session,
            enabled: profile.enabled,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `kind = "local"` host, optionally with the machine source on.
    fn config(include_machines: bool) -> Config {
        let toml = format!(
            r#"
[fleet]
include_local = false
include_machines = {include_machines}

[[fleet.hosts]]
name = "scratch"
kind = "local"
session = "scratch"
"#
        );
        toml::from_str::<Config>(&toml).expect("config fixture parses")
    }

    fn profile(label: &str, session: &str) -> MachineProfile {
        MachineProfile {
            id: format!("{:032x}", label.len()),
            label: label.to_string(),
            target: format!("{}.example", label.replace(' ', "-").to_ascii_lowercase()),
            session: session.to_string(),
            enabled: true,
        }
    }

    fn ids(specs: &[HostSpec]) -> Vec<&str> {
        specs.iter().map(|spec| spec.id.as_str()).collect()
    }

    #[test]
    fn machines_are_off_by_default() {
        assert!(
            !crate::config::FleetConfig::default().include_machines,
            "include_machines must default to false"
        );
        let specs = hosts_from(&config(false), || {
            panic!("the catalog must not be read when include_machines is off")
        })
        .expect("valid config");
        assert_eq!(ids(&specs), vec!["scratch"]);
    }

    #[test]
    fn machines_are_appended_after_the_configured_hosts() {
        let specs = hosts_from(&config(true), || {
            Ok(vec![
                profile("workbox", "agents"),
                profile("Build Box", "ci"),
            ])
        })
        .expect("valid config");
        assert_eq!(ids(&specs), vec!["scratch", "workbox", "build-box"]);
        assert_eq!(specs[1].kind.as_str(), "ssh");
        assert_eq!(specs[1].kind.target(), Some("workbox.example"));
        assert_eq!(specs[1].kind.session(), Some("agents"));
        assert_eq!(specs[2].kind.session(), Some("ci"));
    }

    #[test]
    fn an_unreadable_catalog_is_one_diagnostic() {
        let diagnostics = hosts_from(&config(true), || {
            Err("stored endpoint catalog is invalid: expected value".to_string())
        })
        .expect_err("a broken catalog is an operator error");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(
            diagnostics[0].contains("saved machines unavailable")
                && diagnostics[0].contains("fleet.include_machines = false"),
            "{}",
            diagnostics[0]
        );
    }

    #[test]
    fn a_machine_colliding_with_a_configured_host_fails_the_whole_fleet() {
        let diagnostics = hosts_from(&config(true), || Ok(vec![profile("scratch", "agents")]))
            .expect_err("the name is taken");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(
            diagnostics[0].contains("duplicate saved machine host name"),
            "{}",
            diagnostics[0]
        );
    }

    #[test]
    fn an_invalid_fleet_section_never_reaches_the_catalog() {
        let invalid = toml::from_str::<Config>(
            r#"
[fleet]
include_local = true
include_machines = true

[[fleet.hosts]]
name = "local"
kind = "local"
session = "agents"
"#,
        )
        .expect("config fixture parses");
        let diagnostics = hosts_from(&invalid, || {
            panic!("the catalog must not be read for an invalid [fleet] section")
        })
        .expect_err("reserved host name");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("reserved fleet host name")),
            "{diagnostics:#?}"
        );
    }

    /// Puts `XDG_STATE_HOME` back the way it was, on the happy path and when
    /// the test body panics, so one failed assertion cannot leave a later
    /// test in the same process reading the developer's real state directory.
    struct RestoreStateHome {
        previous: Option<std::ffi::OsString>,
        root: std::path::PathBuf,
    }

    impl Drop for RestoreStateHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Run `body` with `XDG_STATE_HOME` pointed at a throwaway directory.
    ///
    /// `XDG_STATE_HOME` is process-global. `just ci` runs nextest, which gives
    /// every test its own process, but a plain `cargo test` does not, so this
    /// takes the crate-wide `test_config_env_lock` that every other test
    /// mutating `XDG_*` variables takes. The user's real catalog
    /// (`~/.local/state/herdr*/client/endpoints.json`) is never read or
    /// written by these tests: `hosts_for_config` only ever reads, and it
    /// reads under the override.
    fn with_state_home<T>(name: &str, body: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = match crate::config::test_config_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let root = std::env::temp_dir().join(format!(
            "herdr-fleet-machines-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        // The release build reads `herdr/`, a debug build `herdr-dev/`; the
        // test must not depend on which one it was compiled as.
        for app in APP_DIRS {
            std::fs::create_dir_all(root.join(app).join("client"))
                .expect("state directory is creatable");
        }
        let _restore = RestoreStateHome {
            previous: std::env::var_os("XDG_STATE_HOME"),
            root: root.clone(),
        };
        std::env::set_var("XDG_STATE_HOME", &root);
        assert_eq!(
            crate::config::state_dir().parent(),
            Some(root.as_path()),
            "the override must be what state_dir() reads"
        );
        body(&root)
    }

    const APP_DIRS: [&str; 2] = ["herdr", "herdr-dev"];

    fn write_catalog(root: &std::path::Path, content: &str) {
        for app in APP_DIRS {
            std::fs::write(
                root.join(app).join("client").join("endpoints.json"),
                content,
            )
            .expect("catalog is writable");
        }
    }

    #[test]
    fn the_real_catalog_is_read_only_when_it_is_opted_in() {
        let catalog = r#"{"version":1,"selected_profile":null,"ssh":[
            {"id":"0123456789abcdef0123456789abcdef","label":"Lab SSH","target":"lab.example","session":"lab-1","enabled":true},
            {"id":"fedcba9876543210fedcba9876543210","label":"workbox","target":"workbox","session":"agents","enabled":false}
        ]}"#;

        with_state_home("optin", |root| {
            write_catalog(root, catalog);

            let specs = hosts_for_config(&config(false)).expect("valid config");
            assert_eq!(ids(&specs), vec!["scratch"], "the opt-in really is off");

            let specs = hosts_for_config(&config(true)).expect("valid config");
            assert_eq!(ids(&specs), vec!["scratch", "lab-ssh", "workbox"]);
            assert_eq!(specs[1].kind.as_str(), "ssh");
            assert_eq!(specs[1].kind.target(), Some("lab.example"));
            assert_eq!(specs[1].kind.session(), Some("lab-1"));
            assert!(specs[1].enabled);
            assert!(!specs[2].enabled, "a disabled machine stays disabled");
        });
    }

    #[test]
    fn a_missing_catalog_is_an_empty_machine_list() {
        with_state_home("missing", |_| {
            let specs = hosts_for_config(&config(true)).expect("no catalog is not an error");
            assert_eq!(ids(&specs), vec!["scratch"]);
        });
    }

    #[test]
    fn a_corrupt_catalog_is_one_diagnostic() {
        with_state_home("corrupt", |root| {
            write_catalog(root, "{ not json");
            let diagnostics =
                hosts_for_config(&config(true)).expect_err("a corrupt catalog is an error");
            assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
            assert!(
                diagnostics[0].contains("saved machines unavailable"),
                "{}",
                diagnostics[0]
            );
        });
    }
}
