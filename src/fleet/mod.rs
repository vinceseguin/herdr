//! Fleet runtime model (fork).
//!
//! One client process aggregates several herdr servers — this machine's
//! default session plus configured local or ssh hosts — into a single merged
//! view. This module is the fork-owned home for that work; nothing here
//! changes the wire protocol or the endpoint contract.
//!
//! Layering: [`hosts`] turns `[fleet]` configuration into typed host specs and
//! is pure (no sockets, no async, no ratatui). Later PRs of the epic add the
//! merged state, the report and the connector on top of it.

// The config-to-spec layer lands before its first production consumer (the
// fleet connector and `herdr fleet status`). Unit tests exercise every item in
// `hosts`, but test-only use does not satisfy the dead-code lint, so allow it
// there until the connector calls into this module. Scoped to the one module
// that needs it: later modules must make the same choice deliberately instead
// of inheriting a crate-module-wide allow.
#[allow(dead_code)]
pub mod hosts;
