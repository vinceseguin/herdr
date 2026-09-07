#!/bin/sh
#
# fake-claude.sh — a stand-in for Claude Code, for herdr fork tests.
#
# Every E9 validation drives this instead of the real `claude`: the epic is
# about *which config directory* an agent runs against, and that is observable
# without a real account, a real login or a real usage limit. Nothing here ever
# reads or writes the user's ~/.claude.
#
# What it imitates, and why each part matters:
#   * prints `CLAUDE_CONFIG_DIR=<dir>` and its argv, so a pane read proves which
#     profile the two-step launch applied;
#   * writes `<dir>/last-launch.json`, so a test can assert the same off-screen;
#   * reports its session id through `herdr pane report-agent-session` exactly
#     like the hook herdr installs, so `--resume` and switching have an id;
#   * honours `--resume <id>` (keeps the id, reports `resume`);
#   * `auth login` writes a 0600 `.credentials.json` and an `oauthAccount` into
#     `.claude.json`, so `herdr account status` has an identity to show;
#   * with FAKE_CLAUDE_LIMIT=1 prints a usage-limit screen (from
#     $FAKE_CLAUDE_LIMIT_FILE when set) instead of going idle;
#   * shows a `❯ ` prompt and exits on `/exit`.
#
# Safety: the stub writes into CLAUDE_CONFIG_DIR, so it never guesses one. With
# the variable unset it runs with no directory at all (printing an empty
# CLAUDE_CONFIG_DIR= line, which is exactly what `--account none` should look
# like) and writes nothing, and it refuses the real ~/.claude outright. A
# `$HOME/.claude` fallback would let one forgotten variable overwrite the user's
# own credentials.
#
# Environment:
#   CLAUDE_CONFIG_DIR      the profile directory (this is the whole point)
#   HERDR_BIN              herdr binary used to report the session id
#                          (falls back to HERDR_BIN_PATH, which herdr itself
#                          puts in every pane's environment, then to PATH)
#   HERDR_PANE_ID          set by herdr in the pane; no report without it
#   FAKE_CLAUDE_LIMIT      1 to print the usage-limit screen
#   FAKE_CLAUDE_LIMIT_FILE file to print instead of the built-in limit text

set -u

dir="${CLAUDE_CONFIG_DIR:-}"
herdr_bin="${HERDR_BIN:-${HERDR_BIN_PATH:-herdr}}"
prompt='❯ '

# Never the caller's own Claude installation, whatever the environment says.
if [ -n "${HOME:-}" ] && [ -n "$dir" ]; then
    case "${dir%/}" in
        "${HOME%/}/.claude" | "${HOME%/}")
            printf 'fake-claude: refusing to run against the real %s\n' "$dir" >&2
            exit 3
            ;;
    esac
fi

require_dir() {
    if [ -z "$dir" ]; then
        printf 'fake-claude: CLAUDE_CONFIG_DIR is not set; refusing to guess one\n' >&2
        exit 3
    fi
}

json_string() {
    printf '"%s"' "$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
}

json_argv() {
    argv_json=""
    for argument in "$@"; do
        if [ -z "$argv_json" ]; then
            argv_json="$(json_string "$argument")"
        else
            argv_json="$argv_json,$(json_string "$argument")"
        fi
    done
    printf '[%s]' "$argv_json"
}

cmd_auth_login() {
    require_dir
    mkdir -p "$dir" || exit 1
    email="${FAKE_CLAUDE_EMAIL:-$(basename "$dir")@example.test}"
    plan="${FAKE_CLAUDE_PLAN:-max}"
    organization="${FAKE_CLAUDE_ORG:-Example Org}"

    (
        umask 077
        printf '{"claudeAiOauth":{"fake":true}}\n' >"$dir/.credentials.json"
    )
    chmod 600 "$dir/.credentials.json" 2>/dev/null || true

    printf '{"hasCompletedOnboarding":true,"oauthAccount":{"emailAddress":%s,"organizationName":%s,"subscriptionType":%s}}\n' \
        "$(json_string "$email")" "$(json_string "$organization")" "$(json_string "$plan")" \
        >"$dir/.claude.json"

    printf 'fake-claude: auth login\n'
    printf 'CLAUDE_CONFIG_DIR=%s\n' "$dir"
    printf 'logged in as %s (%s)\n' "$email" "$plan"
}

case "${1:-}" in
    --version | -v)
        printf 'fake-claude 0.0.0 (herdr fork test stub)\n'
        exit 0
        ;;
    auth)
        case "${2:-}" in
            login)
                cmd_auth_login
                exit 0
                ;;
            logout)
                require_dir
                rm -f "$dir/.credentials.json"
                printf 'fake-claude: auth logout\n'
                exit 0
                ;;
            *)
                printf 'fake-claude: unknown auth subcommand: %s\n' "${2:-}" >&2
                exit 2
                ;;
        esac
        ;;
esac

argv_json="$(json_argv "fake-claude" "$@")"

session_id=""
start_source="startup"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --resume)
            shift
            session_id="${1:-}"
            start_source="resume"
            ;;
        --resume=*)
            session_id="${1#--resume=}"
            start_source="resume"
            ;;
    esac
    [ "$#" -gt 0 ] && shift
done

if [ -z "$session_id" ]; then
    session_id="fake-$$-$(date +%s 2>/dev/null || printf '0')"
    start_source="startup"
fi

printf 'CLAUDE_CONFIG_DIR=%s\n' "$dir"
printf 'fake-claude argv: %s\n' "$argv_json"

# No directory means no profile was applied: say so on screen and write
# nothing, rather than inventing somewhere to write.
if [ -n "$dir" ]; then
    mkdir -p "$dir" 2>/dev/null || true
    printf '{"config_dir":%s,"argv":%s,"session_id":%s,"session_start_source":%s,"pane_id":%s}\n' \
        "$(json_string "$dir")" \
        "$argv_json" \
        "$(json_string "$session_id")" \
        "$(json_string "$start_source")" \
        "$(json_string "${HERDR_PANE_ID:-}")" \
        >"$dir/last-launch.json" 2>/dev/null || true
else
    printf 'fake-claude: no CLAUDE_CONFIG_DIR; no profile state written\n'
fi

if [ "$start_source" = "resume" ]; then
    printf 'resumed %s\n' "$session_id"
fi

# The same report the real hook makes from <CLAUDE_CONFIG_DIR>/hooks.
if [ -n "${HERDR_PANE_ID:-}" ]; then
    "$herdr_bin" pane report-agent-session "$HERDR_PANE_ID" \
        --source herdr:claude --agent claude \
        --agent-session-id "$session_id" \
        --session-start-source "$start_source" >/dev/null 2>&1 || true
fi

if [ "${FAKE_CLAUDE_LIMIT:-0}" = "1" ]; then
    if [ -n "${FAKE_CLAUDE_LIMIT_FILE:-}" ] && [ -r "${FAKE_CLAUDE_LIMIT_FILE}" ]; then
        cat "${FAKE_CLAUDE_LIMIT_FILE}"
    else
        printf '\n'
        printf '5-hour limit reached ∙ resets 3pm\n'
        printf '/upgrade to increase your usage limit.\n'
    fi
fi

while :; do
    printf '%s' "$prompt"
    if ! IFS= read -r line; then
        printf '\n'
        exit 0
    fi
    case "$line" in
        /exit | /quit)
            printf 'fake-claude: exiting\n'
            exit 0
            ;;
        "")
            ;;
        *)
            printf 'fake-claude: %s\n' "$line"
            ;;
    esac
done
