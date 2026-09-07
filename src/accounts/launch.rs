//! Turning "run under this profile" into one line typed at a shell (fork).
//!
//! Decision (b) of the E9 plan keeps stock servers untouched: instead of
//! teaching `agent.start` about environments, the client first types an
//! environment assignment into the pane's already-running interactive shell
//! and then calls the stock `agent.start`, whose typed `claude …` inherits it.
//!
//! That only works if the line is written in *that* shell's syntax, so this
//! module is where the shell families and their assignment syntax live. It is
//! pure: it maps a process name and a value to a string, or refuses.
//!
//! An unrecognised shell is a hard error everywhere it is used. Typing an
//! `export` line into something that is not a shell would launch the agent
//! under the wrong account without saying so, which is the one outcome this
//! epic must never produce.

// This module is the syntax half of the two-step launch. Its first production
// callers are `herdr account login` (PR 3) and `herdr agent start --account`
// (PR 4); it ships here, fully tested, because both must type the *same* line
// and the quoting rules are the part that must not be reinvented per caller.
#![allow(dead_code)]

/// The assignment syntax families herdr's accepted pane shells fall into.
///
/// Built on `crate::platform::is_pane_shell_process_name`, which is the list
/// the server itself accepts as "this pane is at a prompt"; a test keeps the
/// two in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellFamily {
    /// `sh bash dash zsh ksh mksh`
    Posix,
    /// `fish`
    Fish,
    /// `csh tcsh`
    Csh,
    /// `pwsh powershell`
    PowerShell,
    /// `nu`
    Nu,
    /// `elvish`
    Elvish,
    /// `xonsh`
    Xonsh,
    /// `cmd`
    Cmd,
}

/// Why an environment line could not be produced.
///
/// `UnknownShell` is raised by the runtime driver that reads the pane's shell
/// (`herdr account login`, PR 3; `herdr agent start --account`, PR 4); it is
/// defined here with the rest of the vocabulary so those PRs add callers, not
/// error types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// The pane's foreground process is not a shell herdr knows how to write
    /// an assignment for.
    UnknownShell { name: String },
    /// The variable name is not a portable environment variable name.
    InvalidVariable { name: String },
    /// The value cannot be written safely in this shell's syntax.
    UnquotableValue { family: ShellFamily, reason: String },
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShell { name } => write!(
                formatter,
                "pane shell {name:?} is not a shell herdr can set an environment variable in; \
                 start the agent from a supported shell (sh, bash, zsh, fish, pwsh, …) or use \
                 --account none"
            ),
            Self::InvalidVariable { name } => {
                write!(formatter, "invalid environment variable name {name:?}")
            }
            Self::UnquotableValue { family, reason } => write!(
                formatter,
                "cannot set this value in a {} shell: {reason}",
                family.as_str()
            ),
        }
    }
}

impl ShellFamily {
    /// Classify a pane shell's process name (`bash`, `-zsh`, `/bin/fish`,
    /// `pwsh.exe`).
    ///
    /// `None` for anything herdr does not accept as a pane shell.
    pub fn from_process_name(name: &str) -> Option<Self> {
        if !crate::platform::is_pane_shell_process_name(name) {
            return None;
        }
        match normalized(name).as_str() {
            "sh" | "bash" | "dash" | "zsh" | "ksh" | "mksh" => Some(Self::Posix),
            "fish" => Some(Self::Fish),
            "csh" | "tcsh" => Some(Self::Csh),
            "pwsh" | "powershell" => Some(Self::PowerShell),
            "nu" => Some(Self::Nu),
            "elvish" => Some(Self::Elvish),
            "xonsh" => Some(Self::Xonsh),
            "cmd" => Some(Self::Cmd),
            // Unreachable while this match covers `is_pane_shell_process_name`
            // (a test proves it does), but a new upstream shell must be
            // refused, never guessed.
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::Fish => "fish",
            Self::Csh => "csh",
            Self::PowerShell => "powershell",
            Self::Nu => "nu",
            Self::Elvish => "elvish",
            Self::Xonsh => "xonsh",
            Self::Cmd => "cmd",
        }
    }
}

/// Normalize a process name the way `crate::platform` does: basename, no
/// leading `-` (a login shell), no `.exe`, lowercase.
fn normalized(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

/// The one line to type at `family`'s prompt to export `var = value`.
///
/// No production caller until `herdr account login` (PR 3) and
/// `herdr agent start --account` (PR 4) type it; the syntax and its refusals
/// ship here, with their golden tests, because both PRs must use the same one.
///
/// The returned line starts with a space so shells configured with
/// `HISTCONTROL=ignorespace` / `setopt HIST_IGNORE_SPACE` keep it out of the
/// user's history, and carries no trailing newline — the caller decides when
/// to submit it.
pub fn env_assignment_line(
    family: ShellFamily,
    var: &str,
    value: &str,
) -> Result<String, LaunchError> {
    if !is_valid_env_var(var) {
        return Err(LaunchError::InvalidVariable {
            name: var.to_string(),
        });
    }
    check_value(family, value)?;

    let line = match family {
        ShellFamily::Posix => format!(" export {var}={}", posix_quote(value)),
        ShellFamily::Fish => format!(" set -gx {var} {}", backslash_quote(value)),
        ShellFamily::Csh => format!(" setenv {var} {}", plain_single_quote(value)),
        ShellFamily::PowerShell => format!(" $env:{var} = {}", doubled_single_quote(value)),
        ShellFamily::Nu => format!(" $env.{var} = {}", plain_single_quote(value)),
        ShellFamily::Elvish => format!(" set-env {var} {}", doubled_single_quote(value)),
        ShellFamily::Xonsh => format!(" ${var} = {}", plain_single_quote(value)),
        ShellFamily::Cmd => format!(" set \"{var}={value}\""),
    };
    Ok(line)
}

fn is_valid_env_var(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Refuse anything that cannot be represented, per family.
///
/// The rule is the same everywhere: if quoting the value would need an escape
/// this family's quoting does not have, refuse rather than emit a line that
/// means something else.
fn check_value(family: ShellFamily, value: &str) -> Result<(), LaunchError> {
    let refuse = |reason: &str| {
        Err(LaunchError::UnquotableValue {
            family,
            reason: reason.to_string(),
        })
    };

    if value.is_empty() {
        return refuse("the value is empty");
    }
    if value.chars().any(char::is_control) {
        return refuse("the value contains a control character");
    }

    match family {
        ShellFamily::Posix | ShellFamily::PowerShell | ShellFamily::Elvish => Ok(()),
        // fish escapes inside single quotes with a backslash, so both a quote
        // and a backslash are representable.
        ShellFamily::Fish => Ok(()),
        // csh and tcsh run history substitution *before* quote processing, so
        // single quotes do not protect `!`: ` setenv X '/p/a!b'` fails at an
        // interactive prompt with "Event not found", the assignment never
        // happens, and the agent would then launch under the ambient account.
        // Only a backslash escapes it there, and mixing that with the quoting
        // is not worth the risk — refuse instead.
        ShellFamily::Csh => {
            if value.contains('\'') {
                return refuse("a single quote cannot be escaped in this shell's quoting");
            }
            if value.contains('!') {
                return refuse("'!' is history-expanded even inside single quotes in csh and tcsh");
            }
            Ok(())
        }
        // nu's single quotes have no escape at all.
        ShellFamily::Nu => {
            if value.contains('\'') {
                return refuse("a single quote cannot be escaped in this shell's quoting");
            }
            Ok(())
        }
        ShellFamily::Xonsh => {
            if value.contains('\'') || value.contains('\\') {
                return refuse(
                    "a single quote or backslash cannot be escaped in this shell's quoting",
                );
            }
            Ok(())
        }
        // cmd has no quoting for these: they are expanded or parsed before the
        // quotes are considered.
        ShellFamily::Cmd => {
            if let Some(character) = value.chars().find(|character| {
                matches!(character, '"' | '%' | '!' | '^' | '&' | '<' | '>' | '|')
            }) {
                return refuse(&format!(
                    "the character {character:?} cannot be written safely in cmd"
                ));
            }
            Ok(())
        }
    }
}

/// POSIX single quoting: `'` ends the string, so it is written `'\''`.
fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// fish single quoting: `\` and `'` are backslash-escaped.
fn backslash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Quoting for shells whose single quotes have no escape. Only reachable for
/// values [`check_value`] already accepted.
fn plain_single_quote(value: &str) -> String {
    format!("'{value}'")
}

/// PowerShell / elvish single quoting: `'` is doubled.
fn doubled_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAR: &str = "CLAUDE_CONFIG_DIR";

    #[test]
    fn every_shell_herdr_accepts_at_a_prompt_has_a_family() {
        for name in [
            "sh",
            "bash",
            "dash",
            "zsh",
            "fish",
            "ksh",
            "mksh",
            "csh",
            "tcsh",
            "elvish",
            "xonsh",
            "nu",
            "pwsh",
            "powershell",
            "cmd",
        ] {
            assert!(
                ShellFamily::from_process_name(name).is_some(),
                "{name} is accepted as a pane shell but has no assignment syntax"
            );
        }
    }

    #[test]
    fn process_names_are_normalized_the_way_the_server_normalizes_them() {
        assert_eq!(
            ShellFamily::from_process_name("-zsh"),
            Some(ShellFamily::Posix)
        );
        assert_eq!(
            ShellFamily::from_process_name("/usr/bin/fish"),
            Some(ShellFamily::Fish)
        );
        assert_eq!(
            ShellFamily::from_process_name("C:\\Program Files\\PowerShell\\pwsh.exe"),
            Some(ShellFamily::PowerShell)
        );
        assert_eq!(
            ShellFamily::from_process_name("BASH"),
            Some(ShellFamily::Posix)
        );
    }

    #[test]
    fn anything_that_is_not_a_pane_shell_is_refused() {
        for name in [
            "",
            "claude",
            "node",
            "weirdsh",
            "sh-but-not",
            "vim",
            "python3",
        ] {
            assert_eq!(ShellFamily::from_process_name(name), None, "{name}");
        }
    }

    #[test]
    fn each_family_has_its_golden_line() {
        let cases = [
            (ShellFamily::Posix, " export CLAUDE_CONFIG_DIR='/p/work'"),
            (ShellFamily::Fish, " set -gx CLAUDE_CONFIG_DIR '/p/work'"),
            (ShellFamily::Csh, " setenv CLAUDE_CONFIG_DIR '/p/work'"),
            (
                ShellFamily::PowerShell,
                " $env:CLAUDE_CONFIG_DIR = '/p/work'",
            ),
            (ShellFamily::Nu, " $env.CLAUDE_CONFIG_DIR = '/p/work'"),
            (ShellFamily::Elvish, " set-env CLAUDE_CONFIG_DIR '/p/work'"),
            (ShellFamily::Xonsh, " $CLAUDE_CONFIG_DIR = '/p/work'"),
            (ShellFamily::Cmd, " set \"CLAUDE_CONFIG_DIR=/p/work\""),
        ];
        for (family, expected) in cases {
            assert_eq!(
                env_assignment_line(family, VAR, "/p/work").as_deref(),
                Ok(expected),
                "{family:?}"
            );
        }
    }

    #[test]
    fn every_line_starts_with_a_space_so_shell_history_can_skip_it() {
        for family in [
            ShellFamily::Posix,
            ShellFamily::Fish,
            ShellFamily::Csh,
            ShellFamily::PowerShell,
            ShellFamily::Nu,
            ShellFamily::Elvish,
            ShellFamily::Xonsh,
            ShellFamily::Cmd,
        ] {
            let line = env_assignment_line(family, VAR, "/p/work").expect("line");
            assert!(line.starts_with(' '), "{family:?}: {line}");
            assert!(!line.ends_with('\n'), "{family:?}: {line}");
            assert!(line.contains("/p/work"), "{family:?}: {line}");
        }
    }

    #[test]
    fn quoting_survives_spaces_and_quotes_where_the_shell_can_express_them() {
        assert_eq!(
            env_assignment_line(ShellFamily::Posix, VAR, "/p/it's here").as_deref(),
            Ok(" export CLAUDE_CONFIG_DIR='/p/it'\\''s here'")
        );
        assert_eq!(
            env_assignment_line(ShellFamily::Fish, VAR, "/p/it's\\here").as_deref(),
            Ok(" set -gx CLAUDE_CONFIG_DIR '/p/it\\'s\\\\here'")
        );
        assert_eq!(
            env_assignment_line(ShellFamily::PowerShell, VAR, "/p/it's here").as_deref(),
            Ok(" $env:CLAUDE_CONFIG_DIR = '/p/it''s here'")
        );
        assert_eq!(
            env_assignment_line(ShellFamily::Elvish, VAR, "/p/it's here").as_deref(),
            Ok(" set-env CLAUDE_CONFIG_DIR '/p/it''s here'")
        );
    }

    #[test]
    fn a_value_a_shell_cannot_express_is_refused_not_mangled() {
        for family in [ShellFamily::Csh, ShellFamily::Nu, ShellFamily::Xonsh] {
            let error = env_assignment_line(family, VAR, "/p/it's here").expect_err("refused");
            assert!(
                matches!(error, LaunchError::UnquotableValue { .. }),
                "{family:?}"
            );
        }
        assert!(matches!(
            env_assignment_line(ShellFamily::Xonsh, VAR, "C:\\claude").expect_err("refused"),
            LaunchError::UnquotableValue { .. }
        ));
        for value in ["a\"b", "a%PATH%b", "a!b", "a^b", "a&b", "a<b", "a>b", "a|b"] {
            assert!(
                env_assignment_line(ShellFamily::Cmd, VAR, value).is_err(),
                "cmd must refuse {value:?}"
            );
        }
    }

    #[test]
    fn control_characters_and_empty_values_are_always_refused() {
        for family in [
            ShellFamily::Posix,
            ShellFamily::Fish,
            ShellFamily::Csh,
            ShellFamily::PowerShell,
            ShellFamily::Nu,
            ShellFamily::Elvish,
            ShellFamily::Xonsh,
            ShellFamily::Cmd,
        ] {
            for value in ["", "/p/a\nrm -rf /", "/p/a\rb", "/p/a\tb", "/p/a\u{7f}b"] {
                assert!(
                    env_assignment_line(family, VAR, value).is_err(),
                    "{family:?} must refuse {value:?}"
                );
            }
        }
    }

    #[test]
    fn a_line_never_carries_a_second_command() {
        // Everything a shell could read as a separator must end up inside the
        // quotes (or be refused outright).
        for value in ["/p/a; rm -rf /", "/p/a && echo x", "/p/a | tee", "/p/a`id`"] {
            for family in [
                ShellFamily::Posix,
                ShellFamily::Fish,
                ShellFamily::PowerShell,
                ShellFamily::Elvish,
            ] {
                let line = env_assignment_line(family, VAR, value).expect("line");
                let quoted = line
                    .split_once('\'')
                    .and_then(|(_, rest)| rest.rsplit_once('\''))
                    .map(|(inside, _)| inside.to_string())
                    .expect("a quoted body");
                assert!(
                    line.ends_with('\''),
                    "{family:?}: the line must end inside the quotes: {line}"
                );
                assert!(!quoted.is_empty(), "{family:?}: {line}");
            }
        }
    }

    /// csh and tcsh expand `!` before they look at quotes, so a value carrying
    /// one has to be refused: emitting it would make `setenv` fail and leave
    /// the agent running under whatever account the pane already had.
    #[test]
    fn csh_refuses_a_history_expansion_character() {
        let error =
            env_assignment_line(ShellFamily::Csh, VAR, "/p/a!b").expect_err("csh must refuse '!'");
        assert!(
            matches!(&error, LaunchError::UnquotableValue { family, reason }
                if *family == ShellFamily::Csh && reason.contains("history-expanded")),
            "{error:?}"
        );

        // Only csh: POSIX, fish, PowerShell, nu, elvish and xonsh quoting all
        // hold a literal `!`, and cmd refuses it for delayed expansion.
        for family in [
            ShellFamily::Posix,
            ShellFamily::Fish,
            ShellFamily::PowerShell,
            ShellFamily::Nu,
            ShellFamily::Elvish,
            ShellFamily::Xonsh,
        ] {
            let line = env_assignment_line(family, VAR, "/p/a!b").expect("line");
            assert!(line.contains("/p/a!b"), "{family:?}: {line}");
        }
    }

    /// Every family, not only the ones with a quoted body: a value that a shell
    /// would read as a second command must either be inside the quotes or
    /// refused outright.
    #[test]
    fn no_family_can_be_talked_into_a_second_command() {
        for value in [
            "/p/a; rm -rf /",
            "/p/a && echo x",
            "/p/a | tee",
            "/p/a`id`",
            "/p/a$(id)",
            "/p/a\nrm -rf /",
            "/p/a!b",
            "/p/a'; rm -rf /; '",
            "/p/a\"; rm -rf /",
            "/p/a%PATH%",
            "/p/a\\",
        ] {
            for family in [
                ShellFamily::Posix,
                ShellFamily::Fish,
                ShellFamily::Csh,
                ShellFamily::PowerShell,
                ShellFamily::Nu,
                ShellFamily::Elvish,
                ShellFamily::Xonsh,
                ShellFamily::Cmd,
            ] {
                let Ok(line) = env_assignment_line(family, VAR, value) else {
                    continue;
                };
                let body = match family {
                    // ` set "VAR=value"`: the value sits between the only two
                    // double quotes on the line.
                    ShellFamily::Cmd => {
                        let inner = line
                            .split_once('"')
                            .and_then(|(_, rest)| rest.rsplit_once('"'))
                            .map(|(inside, _)| inside)
                            .expect("a quoted body");
                        assert_eq!(inner.matches('"').count(), 0, "{family:?}: {line}");
                        inner
                            .split_once('=')
                            .map(|(_, value)| value)
                            .expect("an assignment")
                            .to_string()
                    }
                    _ => line
                        .split_once('\'')
                        .and_then(|(_, rest)| rest.rsplit_once('\''))
                        .map(|(inside, _)| inside.to_string())
                        .expect("a quoted body"),
                };
                assert!(
                    line.ends_with(match family {
                        ShellFamily::Cmd => '"',
                        _ => '\'',
                    }),
                    "{family:?}: the line must end where the quoting does: {line}"
                );
                assert!(!body.is_empty(), "{family:?}: {line}");
                assert!(
                    !line.contains('\n') && !line.contains('\r'),
                    "{family:?}: a line must stay one line: {line:?}"
                );
            }
        }
    }

    #[test]
    fn an_invalid_variable_name_is_refused() {
        for name in ["", "1VAR", "VAR-NAME", "VAR NAME", "VAR;rm"] {
            assert_eq!(
                env_assignment_line(ShellFamily::Posix, name, "/p/work"),
                Err(LaunchError::InvalidVariable {
                    name: name.to_string()
                }),
                "{name}"
            );
        }
    }

    #[test]
    fn errors_explain_what_to_do() {
        let unknown = LaunchError::UnknownShell {
            name: "weirdsh".to_string(),
        };
        assert!(unknown.to_string().contains("weirdsh"));
        assert!(unknown.to_string().contains("--account none"));
        assert!(LaunchError::UnquotableValue {
            family: ShellFamily::Cmd,
            reason: "nope".to_string(),
        }
        .to_string()
        .contains("cmd"));
    }
}
