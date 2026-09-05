//! Fleet runtime model (fork).
//!
//! One client process aggregates several herdr servers — this machine's
//! default session plus configured local or ssh hosts — into a single merged
//! view. This module is the fork-owned home for that work; nothing here
//! changes the wire protocol or the endpoint contract.
//!
//! Layering: [`hosts`] turns `[fleet]` configuration into typed host specs,
//! [`refs`] gives every server-side id a host-qualified form, [`state`] merges
//! the hosts' snapshots into one ordered view and [`report`] serializes that
//! view. All four are pure (no sockets, no async, no ratatui); a later PR adds
//! the connector that feeds [`state::FleetState`] from real servers.

// These layers land before their first production consumer (the fleet
// connector and `herdr fleet status`). Unit tests exercise every item, but
// test-only use does not satisfy the dead-code lint, so it is allowed here
// until the connector calls into them; that PR removes these attributes.
// Scoped per module so a later module makes the same choice deliberately
// instead of inheriting a crate-module-wide allow.
#[allow(dead_code)]
pub mod hosts;
#[allow(dead_code)]
pub mod refs;
#[allow(dead_code)]
pub mod report;
#[allow(dead_code)]
pub mod state;

#[cfg(test)]
mod tests {
    /// Modules that must stay pure data, and the `use` paths that would end
    /// that. Precedent: `scripts/test_ui_hot_path_architecture.py` guards the
    /// render hot path the same way.
    const PURE_MODULES: [(&str, &str); 4] = [
        ("hosts.rs", include_str!("hosts.rs")),
        ("refs.rs", include_str!("refs.rs")),
        ("report.rs", include_str!("report.rs")),
        ("state.rs", include_str!("state.rs")),
    ];

    const FORBIDDEN: [&str; 6] = [
        "tokio",
        "ratatui",
        "interprocess",
        "crate::ipc",
        "crate::remote",
        "crate::client",
    ];

    #[test]
    fn the_pure_fleet_modules_import_no_runtime() {
        for (name, source) in PURE_MODULES {
            // Tests may name a runtime crate in a string; only production code
            // is guarded, so stop at the file's test module.
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for line in production.lines() {
                let line = line.trim_start();
                if !line.starts_with("use ") && !line.starts_with("pub use ") {
                    continue;
                }
                for forbidden in FORBIDDEN {
                    assert!(
                        !line.contains(forbidden),
                        "{name} must stay pure data, but imports {forbidden}: {line}"
                    );
                }
            }
        }
    }
}
