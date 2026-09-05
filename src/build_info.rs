//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

/// The build channel used by forks of Herdr that must never self-update from
/// the upstream release manifest.
pub const FORK_CHANNEL: &str = "fork";

/// Pure predicate over a channel name, so guards are testable without a
/// recompile (`is_fork` only ever sees this build's own channel).
pub fn is_fork_channel(channel: &str) -> bool {
    channel == FORK_CHANNEL
}

pub fn is_fork() -> bool {
    is_fork_channel(channel())
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!version().is_empty());
    }

    #[test]
    fn is_fork_channel_only_matches_fork() {
        assert!(is_fork_channel("fork"));
        assert!(!is_fork_channel("stable"));
        assert!(!is_fork_channel("preview"));
        assert!(!is_fork_channel(""));
        assert!(!is_fork_channel("Fork"));
        assert!(!is_fork_channel("forked"));
    }

    #[test]
    fn is_fork_agrees_with_the_compiled_channel() {
        assert_eq!(is_fork(), channel() == FORK_CHANNEL);
    }

    #[test]
    fn version_carries_the_channel_suffix_for_non_stable_builds() {
        let version = version();
        if channel() == "stable" {
            assert_eq!(version, BASE_VERSION);
        } else {
            let expected_prefix = format!("{BASE_VERSION}-{}", channel());
            assert!(
                version == expected_prefix || version.starts_with(&format!("{expected_prefix}.")),
                "version {version} does not carry channel {}",
                channel()
            );
        }
    }

    #[test]
    fn fork_builds_render_a_fork_version() {
        if !is_fork() {
            return;
        }
        assert!(
            version().starts_with(&format!("{BASE_VERSION}-fork")),
            "fork build reported version {}",
            version()
        );
    }
}
