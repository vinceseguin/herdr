//! Claude account profiles (fork).
//!
//! An "account" is one Claude config directory (`CLAUDE_CONFIG_DIR`) with its
//! own credentials, so choosing an account for an agent is choosing which
//! directory its `claude` process runs against. Usage limits are per account,
//! so herdr has to be able to say which agent runs where, and move a running
//! agent to another account without losing its conversation.
//!
//! Layering: [`config`] is the `[[accounts]]` section, [`store`] the
//! CLI-managed `<config>/accounts/profiles.toml`, and [`profile`] merges the
//! two into the one view every caller reads. [`layout`] owns what a profile
//! directory is made of and how healthy one is, [`launch`] the per-shell
//! environment line a client types to apply a profile, and [`tokens`] the
//! metadata vocabulary reported back to the server.
//!
//! All six are pure: sync, socket-free, and free of `tokio`, `ratatui` and the
//! client. Only the runtime drivers that arrive with later PRs speak to a
//! server, and the architecture test below keeps that true.

pub mod config;
pub mod launch;
pub mod layout;
pub mod profile;
pub mod store;
pub mod tokens;

#[cfg(test)]
mod tests {
    /// Modules that must stay pure data, and the paths that would end that.
    /// Precedent: `scripts/test_ui_hot_path_architecture.py` guards the render
    /// hot path the same way.
    const PURE_MODULES: [(&str, &str); 6] = [
        ("config.rs", include_str!("config.rs")),
        ("launch.rs", include_str!("launch.rs")),
        ("layout.rs", include_str!("layout.rs")),
        ("profile.rs", include_str!("profile.rs")),
        ("store.rs", include_str!("store.rs")),
        ("tokens.rs", include_str!("tokens.rs")),
    ];

    const FORBIDDEN: [&str; 7] = [
        "tokio",
        "ratatui",
        "interprocess",
        "crate::ipc",
        "crate::remote",
        "crate::client",
        "crate::api::client",
    ];

    /// The production half of a module: everything before its `#[cfg(test)]`
    /// block, with `//` comments removed so the module docs may still *name*
    /// the runtimes they promise not to use. Block comments are not stripped;
    /// these modules document with `///` and `//!`.
    fn production_code(source: &str) -> String {
        source
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(source)
            .lines()
            .map(|line| match line.find("//") {
                Some(comment) => &line[..comment],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The path tree of a `use` / `pub use` statement, if `statement` is one.
    fn use_tree(statement: &str) -> Option<&str> {
        const USE: &str = "use ";
        let mut search = 0;
        while let Some(offset) = statement[search..].find(USE) {
            let at = search + offset;
            let starts_a_token = !statement[..at]
                .chars()
                .next_back()
                .is_some_and(|previous| previous.is_alphanumeric() || previous == '_');
            if starts_a_token {
                return Some(statement[at + USE.len()..].trim());
            }
            search = at + USE.len();
        }
        None
    }

    /// `a::{b, c::{d, e}}` → `a::b`, `a::c::d`, `a::c::e`.
    fn expand_use_tree(tree: &str) -> Vec<String> {
        let tree = tree.trim();
        let Some(open) = tree.find('{') else {
            return vec![tree.to_string()];
        };
        let prefix = &tree[..open];
        let mut depth = 0usize;
        let mut close = None;
        for (index, character) in tree[open..].char_indices().map(|(at, c)| (at + open, c)) {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        // Unbalanced braces: hand the raw text back so it is still scanned.
        let Some(close) = close else {
            return vec![tree.to_string()];
        };
        let inner = &tree[open + 1..close];
        let mut items = Vec::new();
        let mut depth = 0usize;
        let mut start = 0usize;
        for (index, character) in inner.char_indices() {
            match character {
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    items.push(&inner[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
        items.push(&inner[start..]);
        let mut expanded = Vec::new();
        for item in items {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            for leaf in expand_use_tree(item) {
                expanded.push(format!("{prefix}{leaf}"));
            }
        }
        expanded
    }

    /// Every imported path in `production`, brace groups expanded to leaves.
    ///
    /// A grouped import names no forbidden path on any single line — in
    /// `use crate::{fleet::hosts::HostId, ipc::connect_local_stream};` the text
    /// `crate::ipc` exists only once the prefix and the leaf are joined — so a
    /// line scan alone would miss it.
    fn imported_paths(production: &str) -> Vec<String> {
        let mut paths = Vec::new();
        for statement in production.split(';') {
            let statement = statement.split_whitespace().collect::<Vec<_>>().join(" ");
            if let Some(tree) = use_tree(&statement) {
                paths.extend(expand_use_tree(tree));
            }
        }
        paths
    }

    /// The first forbidden reference in `source`: what it names, and where.
    fn forbidden_reference(source: &str) -> Option<(&'static str, String)> {
        let production = production_code(source);
        // Flat imports, and fully-qualified paths written without a `use`.
        for line in production.lines() {
            for forbidden in FORBIDDEN {
                if line.contains(forbidden) {
                    return Some((forbidden, line.trim().to_string()));
                }
            }
        }
        for path in imported_paths(&production) {
            for forbidden in FORBIDDEN {
                if path.contains(forbidden) {
                    return Some((forbidden, path));
                }
            }
        }
        None
    }

    #[test]
    fn the_pure_account_modules_import_no_runtime() {
        for (name, source) in PURE_MODULES {
            if let Some((forbidden, evidence)) = forbidden_reference(source) {
                panic!("{name} must stay pure data, but names {forbidden}: {evidence}");
            }
        }
    }

    #[test]
    fn the_purity_guard_catches_the_imports_it_exists_for() {
        for source in [
            "use tokio::sync::mpsc;",
            "use crate::ipc::connect_local_stream;",
            "use crate::client::ClientError as Error;",
            "use crate::{\n    fleet::hosts::HostId,\n    ipc::connect_local_stream,\n};",
            "use crate::{accounts::profile::Profiles, remote::attach::RemoteSsh};",
            "use crate::{accounts::tokens::AccountState, client::{shell::state::ClientShellState}};",
            "fn draw() { let _ = ratatui::layout::Rect::default(); }",
            "fn spawn() { tokio::spawn(async {}); }",
        ] {
            assert!(
                forbidden_reference(source).is_some(),
                "the purity guard missed {source:?}"
            );
        }
    }

    #[test]
    fn the_purity_guard_allows_prose_and_test_only_code() {
        for source in [
            "//! Pure: no sockets, no async, no ratatui, no tokio.\nuse std::fmt;",
            "/// Mirrors `crate::client::shell::state`.\nuse std::fmt;",
            "use crate::accounts::profile::Profiles;",
            "use crate::{api::schema::AgentStatus, accounts::tokens::AccountState};",
            "#[cfg(test)]\nmod tests {\n    use tokio::runtime::Runtime;\n}",
        ] {
            assert_eq!(
                forbidden_reference(source),
                None,
                "the purity guard tripped on {source:?}"
            );
        }
    }
}
