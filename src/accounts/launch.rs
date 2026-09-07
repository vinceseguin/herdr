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

use std::path::PathBuf;

use crate::accounts::layout::ProfileInspection;
use crate::accounts::profile::AccountProfile;
use crate::api::schema::PaneProcessInfo;

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
    /// The pane is not sitting at its shell prompt, so typing an assignment
    /// would send it to whatever *is* running there.
    PaneNotAtPrompt { pane_id: String, detail: String },
    /// The profile's directory is not there. Launching anyway would make
    /// Claude create and use an empty, logged-out directory.
    ProfileDirectoryMissing { name: String, dir: PathBuf },
    /// The directory cannot be written as text, so it cannot be typed.
    UnrepresentableDirectory { name: String, dir: PathBuf },
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
            Self::PaneNotAtPrompt { pane_id, detail } => write!(
                formatter,
                "pane {pane_id} is not at its shell prompt ({detail}); nothing was typed. \
                 Wait for the pane to return to its prompt, or use --account none"
            ),
            Self::ProfileDirectoryMissing { name, dir } => write!(
                formatter,
                "account profile {name:?} points at {}, which does not exist; \
                 run `herdr account add {name}` or fix its config_dir",
                dir.display()
            ),
            Self::UnrepresentableDirectory { name, dir } => write!(
                formatter,
                "account profile {name:?} has a config_dir that is not valid UTF-8 ({:?}) \
                 and cannot be typed at a shell prompt",
                dir
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

/// The interactive shell a pane is sitting at, once the pane is known to be
/// idle at its prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneShell {
    pub pid: u32,
    pub name: String,
    pub family: ShellFamily,
}

/// Identify the pane's shell, and refuse unless the pane is idle at its prompt.
///
/// This is the gate in front of every `pane.send_text` the account tooling
/// does. The server decides the same thing the same way before it types an
/// agent's command line (`available_pane_shell_from_job`): the foreground
/// process group has to *be* the shell, and nothing else may be in it. Typing
/// an `export` line into a pane where something else is running would feed it
/// to that program instead — and then the agent would start under whatever
/// account the shell already had, silently.
pub fn pane_shell_at_prompt(info: &PaneProcessInfo) -> Result<PaneShell, LaunchError> {
    let not_at_prompt = |detail: &str| LaunchError::PaneNotAtPrompt {
        pane_id: info.pane_id.clone(),
        detail: detail.to_string(),
    };

    let Some(shell_pid) = info.shell_pid else {
        return Err(not_at_prompt(
            "herdr does not know the pane's shell process",
        ));
    };
    if info.foreground_process_group_id != Some(shell_pid) {
        return Err(not_at_prompt("another program holds the foreground"));
    }
    if info
        .foreground_processes
        .iter()
        .any(|process| process.pid != shell_pid)
    {
        return Err(not_at_prompt("another program is running in the pane"));
    }
    let Some(process) = info
        .foreground_processes
        .iter()
        .find(|process| process.pid == shell_pid)
    else {
        return Err(not_at_prompt("the pane reported no foreground process"));
    };

    // `name` is what the OS calls the process; `argv[0]` is what it was
    // launched as. The server accepts either, so both are tried here, and the
    // reported name is whichever one answered.
    let candidates = std::iter::once(process.name.as_str()).chain(
        process
            .argv
            .as_deref()
            .and_then(|argv| argv.first())
            .map(String::as_str),
    );
    for candidate in candidates {
        if let Some(family) = ShellFamily::from_process_name(candidate) {
            return Ok(PaneShell {
                pid: shell_pid,
                name: candidate.to_string(),
                family,
            });
        }
    }
    Err(LaunchError::UnknownShell {
        name: process.name.clone(),
    })
}

/// Everything the two-step launch needs, decided before anything is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub pane_id: String,
    /// The managed agent name `agent.start` will register.
    pub name: String,
    /// The agent kind, always `claude` while decision (c) holds.
    pub kind: &'static str,
    pub args: Vec<String>,
    /// The profile this launch applies.
    pub profile: AccountProfile,
    pub shell: PaneShell,
    /// The exact line to type at the prompt, with no trailing newline.
    pub line: String,
    /// The directory the launched process must report back.
    pub expected_config_dir: String,
    /// Non-fatal problems worth telling the user about. A missing hook is the
    /// one that matters: without it Claude never reports its session id, so
    /// `herdr agent switch-account` cannot keep the conversation.
    pub warnings: Vec<String>,
}

/// Decide the whole launch from data: no I/O, no server, no shell.
///
/// Every refusal here happens *before* a byte is typed into the pane, which is
/// the property that keeps a bad plan from becoming a launch under the wrong
/// account.
pub fn plan_launch(
    profile: &AccountProfile,
    inspection: &ProfileInspection,
    pane_id: &str,
    name: &str,
    args: &[String],
    shell: PaneShell,
) -> Result<LaunchPlan, LaunchError> {
    if !inspection.dir_exists {
        return Err(LaunchError::ProfileDirectoryMissing {
            name: profile.name.clone(),
            dir: profile.config_dir.clone(),
        });
    }
    let Some(config_dir) = profile.config_dir.to_str() else {
        return Err(LaunchError::UnrepresentableDirectory {
            name: profile.name.clone(),
            dir: profile.config_dir.clone(),
        });
    };
    let line = env_assignment_line(shell.family, profile.agent.config_dir_env_var(), config_dir)?;

    let mut warnings = Vec::new();
    if !inspection.hook_installed {
        warnings.push(format!(
            "account profile {:?} has no herdr session hook installed, so Claude will not report \
             its session id; `herdr agent switch-account` cannot keep the conversation until you \
             run `herdr account add`/`herdr integration install claude` for it",
            profile.name
        ));
    }
    if !inspection.logged_in {
        warnings.push(format!(
            "account profile {:?} has no credentials file; Claude will ask you to log in \
             (`herdr account login {}`)",
            profile.name, profile.name
        ));
    }

    Ok(LaunchPlan {
        pane_id: pane_id.to_string(),
        name: name.to_string(),
        kind: profile.agent.as_str(),
        args: args.to_vec(),
        profile: profile.clone(),
        shell,
        line,
        expected_config_dir: config_dir.to_string(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::PaneProcessInfoProcess;

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

    fn process(pid: u32, name: &str, argv: Option<&[&str]>) -> PaneProcessInfoProcess {
        PaneProcessInfoProcess {
            pid,
            name: name.to_string(),
            argv0: None,
            argv: argv.map(|argv| argv.iter().map(|arg| (*arg).to_string()).collect()),
            cmdline: None,
            cwd: None,
        }
    }

    fn pane_at_prompt(shell: &str) -> PaneProcessInfo {
        PaneProcessInfo {
            pane_id: "w1:p1".to_string(),
            shell_pid: Some(42),
            foreground_process_group_id: Some(42),
            tty: None,
            foreground_processes: vec![process(42, shell, Some(&["/usr/bin/bash"]))],
        }
    }

    fn profile(dir: &str) -> AccountProfile {
        AccountProfile {
            name: "work".to_string(),
            agent: crate::accounts::profile::AccountAgent::Claude,
            config_dir: PathBuf::from(dir),
            default: false,
            origin: crate::accounts::profile::ProfileOrigin::Store,
        }
    }

    fn healthy() -> ProfileInspection {
        ProfileInspection {
            dir_exists: true,
            logged_in: true,
            credentials_mode_ok: Some(true),
            identity: None,
            hook_installed: true,
            broken_links: Vec::new(),
        }
    }

    #[test]
    fn a_pane_idle_at_its_shell_prompt_is_identified() {
        let shell = pane_shell_at_prompt(&pane_at_prompt("bash")).expect("a shell");
        assert_eq!(
            shell,
            PaneShell {
                pid: 42,
                name: "bash".to_string(),
                family: ShellFamily::Posix,
            }
        );
    }

    /// The name the OS reports can be anything; `argv[0]` is the other thing
    /// the server accepts, so a pane whose comm is truncated still resolves.
    #[test]
    fn argv0_answers_when_the_process_name_does_not() {
        let mut info = pane_at_prompt("bash");
        info.foreground_processes = vec![process(42, "some-wrapper", Some(&["/usr/bin/zsh"]))];
        let shell = pane_shell_at_prompt(&info).expect("a shell");
        assert_eq!(shell.family, ShellFamily::Posix);
        assert_eq!(shell.name, "/usr/bin/zsh");
    }

    /// The refusal that matters most: an assignment typed while something else
    /// holds the foreground goes to *that* program, and the agent would then
    /// start under whatever account the shell already had.
    #[test]
    fn a_busy_pane_is_refused_before_anything_is_typed() {
        let mut info = pane_at_prompt("bash");
        info.foreground_process_group_id = Some(99);
        info.foreground_processes = vec![process(99, "claude", None)];
        assert!(matches!(
            pane_shell_at_prompt(&info),
            Err(LaunchError::PaneNotAtPrompt { .. })
        ));

        // Foreground group is the shell's, but a child is still in it.
        let mut info = pane_at_prompt("bash");
        info.foreground_processes.push(process(43, "sleep", None));
        assert!(matches!(
            pane_shell_at_prompt(&info),
            Err(LaunchError::PaneNotAtPrompt { .. })
        ));
    }

    #[test]
    fn a_pane_that_reports_no_shell_is_refused() {
        for info in [
            PaneProcessInfo {
                shell_pid: None,
                ..pane_at_prompt("bash")
            },
            PaneProcessInfo {
                foreground_processes: Vec::new(),
                ..pane_at_prompt("bash")
            },
        ] {
            assert!(matches!(
                pane_shell_at_prompt(&info),
                Err(LaunchError::PaneNotAtPrompt { .. })
            ));
        }
    }

    #[test]
    fn a_shell_herdr_cannot_write_an_assignment_for_is_refused_by_name() {
        let mut info = pane_at_prompt("bash");
        info.foreground_processes = vec![process(42, "weirdsh", Some(&["/usr/local/bin/weirdsh"]))];
        assert_eq!(
            pane_shell_at_prompt(&info),
            Err(LaunchError::UnknownShell {
                name: "weirdsh".to_string()
            })
        );
    }

    #[test]
    fn a_plan_carries_the_exact_line_and_the_directory_to_verify() {
        let profile = profile("/p/work");
        let shell = pane_shell_at_prompt(&pane_at_prompt("bash")).expect("a shell");
        let plan = plan_launch(
            &profile,
            &healthy(),
            "w1:p1",
            "a1",
            &["--resume".to_string(), "abc".to_string()],
            shell,
        )
        .expect("a plan");
        assert_eq!(plan.line, " export CLAUDE_CONFIG_DIR='/p/work'");
        assert_eq!(plan.expected_config_dir, "/p/work");
        assert_eq!(plan.kind, "claude");
        assert_eq!(plan.args, vec!["--resume".to_string(), "abc".to_string()]);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    /// A directory that is not there would make Claude create an empty,
    /// logged-out one and quietly use it.
    #[test]
    fn a_missing_profile_directory_is_refused() {
        let profile = profile("/p/work");
        let shell = pane_shell_at_prompt(&pane_at_prompt("bash")).expect("a shell");
        let inspection = ProfileInspection {
            dir_exists: false,
            ..healthy()
        };
        assert_eq!(
            plan_launch(&profile, &inspection, "w1:p1", "a1", &[], shell),
            Err(LaunchError::ProfileDirectoryMissing {
                name: "work".to_string(),
                dir: PathBuf::from("/p/work"),
            })
        );
    }

    /// Missing hook and missing credentials are warnings: the launch is still
    /// correct, the user just loses switching or has to log in.
    #[test]
    fn a_profile_without_a_hook_or_credentials_still_launches_with_warnings() {
        let profile = profile("/p/work");
        let shell = pane_shell_at_prompt(&pane_at_prompt("bash")).expect("a shell");
        let inspection = ProfileInspection {
            hook_installed: false,
            logged_in: false,
            ..healthy()
        };
        let plan = plan_launch(&profile, &inspection, "w1:p1", "a1", &[], shell).expect("a plan");
        assert_eq!(plan.warnings.len(), 2, "{:?}", plan.warnings);
        assert!(
            plan.warnings[0].contains("session hook"),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.warnings[1].contains("account login"),
            "{:?}",
            plan.warnings
        );
    }

    /// PR 1 refused `!` for csh and tcsh because history expansion runs before
    /// quoting, so the assignment would silently never happen. A launch must
    /// degrade to a refusal, never to a launch under the ambient account.
    #[test]
    fn a_csh_pane_refuses_a_directory_it_cannot_quote_rather_than_launching() {
        let profile = profile("/p/work!1");
        let mut info = pane_at_prompt("tcsh");
        info.foreground_processes = vec![process(42, "tcsh", Some(&["/usr/bin/tcsh"]))];
        let shell = pane_shell_at_prompt(&info).expect("a shell");
        assert!(matches!(
            plan_launch(&profile, &healthy(), "w1:p1", "a1", &[], shell),
            Err(LaunchError::UnquotableValue {
                family: ShellFamily::Csh,
                ..
            })
        ));
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

        let busy = LaunchError::PaneNotAtPrompt {
            pane_id: "w1:p1".to_string(),
            detail: "another program is running in the pane".to_string(),
        }
        .to_string();
        assert!(busy.contains("w1:p1"), "{busy}");
        assert!(busy.contains("nothing was typed"), "{busy}");

        let missing = LaunchError::ProfileDirectoryMissing {
            name: "work".to_string(),
            dir: PathBuf::from("/p/work"),
        }
        .to_string();
        assert!(missing.contains("/p/work"), "{missing}");
        assert!(missing.contains("herdr account add work"), "{missing}");
    }
}
