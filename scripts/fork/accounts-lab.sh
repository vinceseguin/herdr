#!/usr/bin/env bash
#
# accounts-lab.sh — one isolated herdr server with two seeded Claude account
# profiles and a fake `claude` first on PATH, so E9 can be validated against a
# real running server without touching the user's Claude installation.
#
#   accounts-lab.sh up          start session `accounts-lab` with profiles
#                               perso (default) and work, and one pane sitting
#                               at a shell prompt
#   accounts-lab.sh status [--json]
#   accounts-lab.sh env         export-able lines for `eval "$(… env)"`
#   accounts-lab.sh down        stop the session and delete the lab root
#
# Knobs:
#   HERDR_BIN                     herdr binary (default: <repo>/target/debug/herdr,
#                                 else `herdr` on PATH)
#   HERDR_ACCOUNTS_LAB_ROOT       lab root (default: /tmp/herdr-accounts-lab)
#   HERDR_ACCOUNTS_LAB_TIMEOUT_MS per-step timeout in ms (default: 15000)
#
# Isolation, by construction (same rules as fleet-lab.sh):
#   * every herdr call carries `--session accounts-lab`, the lab's own
#     XDG_CONFIG_HOME/XDG_RUNTIME_DIR/XDG_STATE_HOME/XDG_DATA_HOME/
#     XDG_CACHE_HOME, and drops HERDR_SOCKET_PATH, HERDR_CLIENT_SOCKET_PATH,
#     HERDR_ENV, HERDR_SESSION and HERDR_CONFIG_PATH, so nothing here can reach
#     ~/.config/herdr, ~/.config/herdr-dev or the caller's default session;
#   * the profile directories live under the lab root, and CLAUDE_CONFIG_DIR is
#     pinned to $ROOT/profiles/ambient for everything this script starts, so an
#     agent launched without a profile still lands inside the lab; the fake
#     `claude` additionally refuses ~/.claude outright, so neither ~/.claude nor
#     ~/.claude.json is ever read or written;
#   * `down` only signals a pid this script wrote itself, and only after
#     re-checking that the pid is still this lab's herdr server (pid reuse);
#   * `down` deletes only a plain directory carrying this lab's marker file,
#     and refuses `/`, $HOME, any ancestor of $HOME, the caller's
#     XDG_CONFIG_HOME (and anything inside it) and reserved system directories.

set -euo pipefail

SCRIPT_NAME=$(basename "$0")
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd -P)

MARKER_NAME=".herdr-accounts-lab"
MARKER_HEADER="herdr-accounts-lab"
SESSION_NAME="accounts-lab"
DEFAULT_PROFILE="perso"
SECOND_PROFILE="work"
AMBIENT_PROFILE="ambient"
MAX_PID_DIGITS=10

ROOT=""
BIN=""
TIMEOUT_MS="${HERDR_ACCOUNTS_LAB_TIMEOUT_MS:-15000}"

PY_PANE_ID=$(
    cat <<'PY'
import json
import sys

try:
    snapshot = json.load(sys.stdin)["result"]["snapshot"]
except Exception:
    sys.exit(0)

for pane in snapshot.get("panes", []):
    print(pane.get("pane_id", ""))
    break
PY
)

PY_SESSION=$(
    cat <<'PY'
import json
import sys

name = sys.argv[1]
try:
    sessions = json.load(sys.stdin).get("sessions", [])
except Exception:
    sys.exit(0)

for session in sessions:
    if session.get("name") != name:
        continue
    print(
        "\x1f".join(
            (
                "true" if session.get("running") else "false",
                session.get("socket_path", "") or "",
                session.get("session_dir", "") or "",
            )
        )
    )
    break
PY
)

PY_STATUS_JSON=$(
    cat <<'PY'
import json
import sys

root, binary, session = sys.argv[1], sys.argv[2], sys.argv[3]
running, pid, pane_id, client_socket, api_socket = (sys.stdin.read().split("\x1f") + [""] * 5)[:5]
print(
    json.dumps(
        {
            "root": root,
            "bin": binary,
            "session": session,
            "running": running.strip() == "true",
            "pid": int(pid.strip()) if pid.strip().isdigit() else None,
            "pane_id": pane_id.strip() or None,
            "client_socket": client_socket.strip() or None,
            "api_socket": api_socket.strip() or None,
            "profiles": {
                "perso": root + "/profiles/perso",
                "work": root + "/profiles/work",
                "ambient": root + "/profiles/ambient",
            },
            "claude_stub": root + "/bin/claude",
        }
    )
)
PY
)

die() {
    printf '%s: %s\n' "$SCRIPT_NAME" "$*" >&2
    exit 1
}

log() {
    printf '%s: %s\n' "$SCRIPT_NAME" "$*" >&2
}

usage() {
    cat >&2 <<USAGE
usage: $SCRIPT_NAME up | down | status [--json] | env

  up                start the accounts lab (session $SESSION_NAME)
  down              stop it and remove its root
  status [--json]   report the lab
  env               print export lines for eval "\$($SCRIPT_NAME env)"

environment:
  HERDR_BIN                        herdr binary to run
  HERDR_ACCOUNTS_LAB_ROOT          lab root (default /tmp/herdr-accounts-lab)
  HERDR_ACCOUNTS_LAB_TIMEOUT_MS    per-step timeout in ms (default 15000)
USAGE
}

# ---------------------------------------------------------------------------
# resolution and safety (mirrors scripts/fork/fleet-lab.sh)
# ---------------------------------------------------------------------------

resolve_bin() {
    local candidate="${HERDR_BIN:-}"
    if [ -z "$candidate" ]; then
        if [ -x "$REPO_ROOT/target/debug/herdr" ]; then
            candidate="$REPO_ROOT/target/debug/herdr"
        else
            candidate="herdr"
        fi
    fi

    local resolved
    if ! resolved=$(command -v -- "$candidate" 2>/dev/null); then
        die "herdr binary not found: $candidate (set HERDR_BIN)"
    fi
    case "$resolved" in
        /*) ;;
        *) resolved="$(cd "$(dirname "$resolved")" && pwd -P)/$(basename "$resolved")" ;;
    esac
    [ -f "$resolved" ] && [ -x "$resolved" ] ||
        die "herdr binary is not an executable file: $resolved"
    printf '%s\n' "$resolved"
}

resolve_timeout() {
    case "$TIMEOUT_MS" in
        "" | *[!0-9]*) die "HERDR_ACCOUNTS_LAB_TIMEOUT_MS must be a positive integer: $TIMEOUT_MS" ;;
    esac
    TIMEOUT_MS=$((10#$TIMEOUT_MS))
    [ "$TIMEOUT_MS" -gt 0 ] || die "HERDR_ACCOUNTS_LAB_TIMEOUT_MS must be a positive integer"
}

assert_safe_root() {
    local root="$1"

    [ -n "$root" ] || die "lab root is empty"
    case "$root" in
        /*) ;;
        *) die "lab root must be an absolute path: $root" ;;
    esac
    case "$root" in
        *$'\n'* | *$'\r'*) die "refusing a lab root containing a newline" ;;
    esac
    if [ "$root" = "/" ]; then
        die "refusing / as the lab root"
    fi

    local trimmed="${root#/}"
    case "$trimmed" in
        */*) ;;
        *) die "refusing a top-level directory as the lab root: $root" ;;
    esac

    case "$root" in
        /home/*/* | /Users/*/*) ;;
        /home/* | /Users/*) die "refusing a home directory as the lab root: $root" ;;
    esac

    local reserved
    for reserved in /tmp /var/tmp /var/run /home /root /usr /etc /opt /var /srv /mnt /media /Users /System /Library; do
        if [ "$root" = "$reserved" ]; then
            die "refusing a reserved system directory as the lab root: $root"
        fi
    done

    if [ -n "${HOME:-}" ] && [ -d "$HOME" ]; then
        local home_real
        home_real=$(cd "$HOME" && pwd -P)
        if [ "$root" = "$home_real" ]; then
            die "refusing \$HOME as the lab root: $root"
        fi
        case "$home_real/" in
            "$root"/*) die "refusing an ancestor of \$HOME as the lab root: $root" ;;
        esac
    fi

    local caller_config="${XDG_CONFIG_HOME:-}"
    if [ -z "$caller_config" ] && [ -n "${HOME:-}" ]; then
        caller_config="$HOME/.config"
    fi
    if [ -n "$caller_config" ] && [ -d "$caller_config" ]; then
        local config_real
        config_real=$(cd "$caller_config" && pwd -P)
        if [ "$config_real" != "$root/xdg" ]; then
            if [ "$root" = "$config_real" ]; then
                die "refusing the caller's XDG_CONFIG_HOME as the lab root: $root"
            fi
            case "$config_real/" in
                "$root"/*) die "refusing an ancestor of the caller's XDG_CONFIG_HOME as the lab root: $root" ;;
            esac
            case "$root/" in
                "$config_real"/*) die "refusing a directory inside the caller's XDG_CONFIG_HOME as the lab root: $root" ;;
            esac
        fi
    fi
}

resolve_root() {
    local raw="${HERDR_ACCOUNTS_LAB_ROOT:-/tmp/herdr-accounts-lab}"
    case "$raw" in
        /*) ;;
        *) die "HERDR_ACCOUNTS_LAB_ROOT must be an absolute path: $raw" ;;
    esac
    case "$raw" in
        *$'\n'* | *$'\r'*) die "refusing a lab root containing a newline" ;;
    esac

    local real
    if [ -L "$raw" ] || [ -L "${raw%/}" ]; then
        die "HERDR_ACCOUNTS_LAB_ROOT must not be a symlink: $raw"
    elif [ -d "$raw" ]; then
        real=$(cd "$raw" && pwd -P)
    elif [ -e "$raw" ]; then
        die "HERDR_ACCOUNTS_LAB_ROOT exists and is not a plain directory: $raw"
    else
        local parent base
        parent=$(dirname "$raw")
        base=$(basename "$raw")
        case "$base" in
            . | ..) die "HERDR_ACCOUNTS_LAB_ROOT must name a directory: $raw" ;;
        esac
        [ -d "$parent" ] || die "parent of HERDR_ACCOUNTS_LAB_ROOT does not exist: $parent"
        parent=$(cd "$parent" && pwd -P)
        real="${parent%/}/$base"
    fi

    assert_safe_root "$real"
    printf '%s\n' "$real"
}

marker_path() {
    printf '%s\n' "$ROOT/$MARKER_NAME"
}

has_marker() {
    local marker
    marker=$(marker_path)
    [ ! -L "$marker" ] && [ -f "$marker" ] || return 1
    [ "$(head -n 1 "$marker" 2>/dev/null)" = "$MARKER_HEADER" ]
}

remove_root() {
    assert_safe_root "$ROOT"
    has_marker || die "refusing to delete $ROOT: no valid $MARKER_NAME marker"
    [ ! -L "$ROOT" ] && [ -d "$ROOT" ] || die "refusing to delete $ROOT: not a plain directory"
    rm -rf -- "$ROOT"
}

# ---------------------------------------------------------------------------
# herdr plumbing
# ---------------------------------------------------------------------------

LAB_ENV=()

set_lab_env() {
    LAB_ENV=(
        -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV
        -u HERDR_SESSION -u HERDR_CONFIG_PATH
        XDG_CONFIG_HOME="$ROOT/xdg" XDG_RUNTIME_DIR="$ROOT/runtime"
        XDG_STATE_HOME="$ROOT/state" XDG_DATA_HOME="$ROOT/data"
        XDG_CACHE_HOME="$ROOT/cache"
        # The fake `claude` wins over anything installed, and the stub reports
        # its session id through this exact binary.
        PATH="$ROOT/bin:${PATH:-/usr/bin:/bin}"
        HERDR_BIN="$BIN"
        # Anything started without a profile (a hand-typed `claude`, or
        # `--account none`) still writes inside the lab, never into ~/.claude.
        CLAUDE_CONFIG_DIR="$ROOT/profiles/$AMBIENT_PROFILE"
    )
}

lab_herdr() {
    env "${LAB_ENV[@]}" "$BIN" --session "$SESSION_NAME" "$@"
}

pid_file() {
    printf '%s\n' "$ROOT/pids/$SESSION_NAME.pid"
}

read_pid_file() {
    local file="$1" pid
    [ ! -L "$file" ] && [ -f "$file" ] || return 1
    pid=$(head -n 1 "$file" 2>/dev/null | tr -d '[:space:]' || true)
    case "$pid" in
        "" | *[!0-9]* | 0*) return 1 ;;
    esac
    [ "${#pid}" -le "$MAX_PID_DIGITS" ] || return 1
    [ "$pid" -gt 1 ] || return 1
    [ "$pid" != "$$" ] && [ "$pid" != "$PPID" ] || return 1
    printf '%s\n' "$pid"
}

process_argv() {
    local pid="$1" argv
    if [ -d /proc/self ]; then
        argv=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
    else
        argv=$(ps -o args= -p "$pid" 2>/dev/null || true)
    fi
    printf '%s\n' "${argv% }"
}

process_runtime_dir_matches() {
    local pid="$1"
    [ -d /proc/self ] || return 0
    [ -r "/proc/$pid/environ" ] || return 1
    tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null |
        grep -Fxq "XDG_RUNTIME_DIR=$ROOT/runtime"
}

marker_bin() {
    has_marker || return 0
    sed -n 's/^bin=//p' "$(marker_path)" | head -n 1
}

expected_bin() {
    local from_marker
    from_marker=$(marker_bin)
    printf '%s\n' "${from_marker:-$BIN}"
}

pid_is_lab_server() {
    local pid="$1" expected_binary="$2" argv

    kill -0 "$pid" 2>/dev/null || return 1
    process_runtime_dir_matches "$pid" || return 1

    argv=$(process_argv "$pid")
    [ -n "$argv" ] || return 1
    [ -n "$expected_binary" ] || return 1

    local expected="$expected_binary --session $SESSION_NAME server"
    case "$argv" in
        "$expected" | "$expected "* | *" $expected" | *" $expected "*) ;;
        *) return 1 ;;
    esac
}

terminate_lab_pid() {
    local pid="$1" expected_binary="$2" waited=0

    pid_is_lab_server "$pid" "$expected_binary" || return 0

    kill -TERM "$pid" 2>/dev/null || return 0
    while [ "$waited" -lt 40 ]; do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.05
        waited=$((waited + 1))
    done

    if pid_is_lab_server "$pid" "$expected_binary"; then
        kill -KILL "$pid" 2>/dev/null || true
    fi
}

# running|api_socket|session_dir for this lab's session, or nothing.
lab_session_record() {
    lab_herdr session list --json 2>/dev/null | python3 -c "$PY_SESSION" "$SESSION_NAME" || true
}

lab_is_running() {
    local record
    record=$(lab_session_record)
    case "$record" in
        true$'\x1f'*) return 0 ;;
        *) return 1 ;;
    esac
}

lab_pid_alive() {
    local pid
    pid=$(read_pid_file "$(pid_file)") || return 1
    pid_is_lab_server "$pid" "$(expected_bin)"
}

stop_started_session() {
    local pid
    lab_herdr session stop "$SESSION_NAME" >/dev/null 2>&1 || true
    if pid=$(read_pid_file "$(pid_file)"); then
        terminate_lab_pid "$pid" "$(expected_bin)"
    fi
}

api_is_ready() {
    # Captured first, then matched: piping straight into `grep -q` lets grep
    # exit on the first match, and `set -o pipefail` would then report the
    # server's SIGPIPE as a failure and wedge the wait. Same shape as
    # fleet-lab.sh.
    local snapshot
    snapshot=$(lab_herdr api snapshot 2>/dev/null || true)
    case "$snapshot" in
        *'"snapshot"'*) return 0 ;;
        *) return 1 ;;
    esac
}

snapshot_pane_id() {
    lab_herdr api snapshot 2>/dev/null | python3 -c "$PY_PANE_ID"
}

# ---------------------------------------------------------------------------
# seeding
# ---------------------------------------------------------------------------

# A profile directory that looks like a logged-in Claude config dir, without a
# single real secret in it.
seed_profile() {
    local name="$1" email="$2" plan="$3" profile
    profile="$ROOT/profiles/$name"

    mkdir -p "$profile"
    chmod 700 "$profile"
    printf '{"hasCompletedOnboarding":true,"oauthAccount":{"emailAddress":"%s","organizationName":"Example Org","subscriptionType":"%s"}}\n' \
        "$email" "$plan" >"$profile/.claude.json"
    printf '{"claudeAiOauth":{"fake":true}}\n' >"$profile/.credentials.json"
    chmod 600 "$profile/.credentials.json"
}

seed_profiles() {
    mkdir -p "$ROOT/profiles"
    seed_profile "$DEFAULT_PROFILE" "perso@example.test" "max"
    seed_profile "$SECOND_PROFILE" "work@example.test" "max"

    # Transcripts are shared between profiles by symlink — decision (a) — so
    # `claude --resume` finds a conversation started under the other account.
    mkdir -p "$ROOT/profiles/$DEFAULT_PROFILE/projects"
    ln -s "../$DEFAULT_PROFILE/projects" "$ROOT/profiles/$SECOND_PROFILE/projects"

    # Where a launch that applied no profile ends up. Deliberately logged out,
    # so evidence from it can never be mistaken for perso or work.
    mkdir -p "$ROOT/profiles/$AMBIENT_PROFILE"
    chmod 700 "$ROOT/profiles/$AMBIENT_PROFILE"
}

write_config() {
    local app
    for app in herdr herdr-dev; do
        mkdir -p "$ROOT/xdg/$app"
        cat >"$ROOT/xdg/$app/config.toml" <<CONFIG
onboarding = false

[[accounts]]
name = "$DEFAULT_PROFILE"
agent = "claude"
config_dir = "$ROOT/profiles/$DEFAULT_PROFILE"
default = true

[[accounts]]
name = "$SECOND_PROFILE"
agent = "claude"
config_dir = "$ROOT/profiles/$SECOND_PROFILE"
CONFIG
    done
}

install_stub() {
    mkdir -p "$ROOT/bin"
    [ -f "$SCRIPT_DIR/fake-claude.sh" ] ||
        die "missing $SCRIPT_DIR/fake-claude.sh"
    cp "$SCRIPT_DIR/fake-claude.sh" "$ROOT/bin/claude"
    chmod 755 "$ROOT/bin/claude"
}

# ---------------------------------------------------------------------------
# up
# ---------------------------------------------------------------------------

UP_STARTED=0
UP_CREATED_ROOT=0

up_cleanup() {
    local status="$1"
    trap - EXIT INT TERM
    if [ "$UP_STARTED" = "1" ]; then
        exit "$status"
    fi

    log "up failed; cleaning up what it started"
    stop_started_session
    if [ "$UP_CREATED_ROOT" = "1" ] && has_marker; then
        remove_root || true
    fi
    exit "$status"
}

up_cleanup_on_exit() {
    up_cleanup "$?"
}

arm_up_cleanup() {
    trap up_cleanup_on_exit EXIT
    trap 'up_cleanup 130' INT
    trap 'up_cleanup 143' TERM
}

wait_for_session() {
    local pid="$1" deadline
    deadline=$((SECONDS + (TIMEOUT_MS + 999) / 1000))
    while :; do
        if ! kill -0 "$pid" 2>/dev/null; then
            log "server exited early; last log lines:"
            tail -n 20 "$ROOT/logs/$SESSION_NAME.log" >&2 2>/dev/null || true
            return 1
        fi
        if lab_is_running && api_is_ready; then
            return 0
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            break
        fi
        sleep 0.05
    done
    log "timed out waiting for $SESSION_NAME after ${TIMEOUT_MS}ms"
    tail -n 20 "$ROOT/logs/$SESSION_NAME.log" >&2 2>/dev/null || true
    return 1
}

cmd_up() {
    [ "$#" -eq 0 ] || die "usage: $SCRIPT_NAME up"

    local sample_socket="$ROOT/xdg/herdr-dev/sessions/$SESSION_NAME/herdr-client.sock"
    if [ "${#sample_socket}" -ge 104 ]; then
        die "lab root is too long for a unix socket path (${#sample_socket} bytes): $ROOT"
    fi

    if [ -e "$ROOT" ]; then
        has_marker ||
            die "refusing to use $ROOT: it exists but has no valid $MARKER_NAME marker"
        if lab_is_running || lab_pid_alive; then
            die "accounts lab is already up at $ROOT (run: $SCRIPT_NAME down)"
        fi
        log "removing stale lab at $ROOT"
        stop_started_session
        remove_root
    fi

    UP_CREATED_ROOT=1
    arm_up_cleanup

    mkdir -p "$ROOT/runtime" "$ROOT/logs" "$ROOT/pids" "$ROOT/work" \
        "$ROOT/state" "$ROOT/data" "$ROOT/cache"
    install_stub
    seed_profiles
    write_config

    {
        printf '%s\n' "$MARKER_HEADER"
        printf 'bin=%s\n' "$BIN"
        printf 'session=%s\n' "$SESSION_NAME"
        printf 'created=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    } >"$(marker_path)"

    nohup env "${LAB_ENV[@]}" "$BIN" --session "$SESSION_NAME" server \
        </dev/null >"$ROOT/logs/$SESSION_NAME.log" 2>&1 &
    local pid=$!
    printf '%s\n' "$pid" >"$(pid_file)"

    wait_for_session "$pid" || return 1

    # A workspace whose root pane is left at its shell prompt: every account
    # command needs a pane that is *not* running an agent.
    lab_herdr workspace create --label "$SESSION_NAME" --cwd "$ROOT/work" --no-focus >/dev/null

    UP_STARTED=1
    trap - EXIT INT TERM
    cmd_status
}

# ---------------------------------------------------------------------------
# status / env
# ---------------------------------------------------------------------------

# running|pid|pane_id|client_socket|api_socket
lab_status_record() {
    has_marker || return 0

    local running="false" pid="" pane_id="" client_socket="" api_socket=""
    local record session_dir=""
    record=$(lab_session_record)
    if [ -n "$record" ]; then
        IFS=$'\x1f' read -r running api_socket session_dir <<<"$record"
    fi
    if [ "$running" = "true" ]; then
        pane_id=$(snapshot_pane_id || true)
    fi
    if pid=$(read_pid_file "$(pid_file)"); then
        pid_is_lab_server "$pid" "$(expected_bin)" || pid=""
    else
        pid=""
    fi
    if [ -n "${session_dir:-}" ]; then
        client_socket="$session_dir/herdr-client.sock"
    fi

    printf '%s\x1f%s\x1f%s\x1f%s\x1f%s' \
        "$running" "$pid" "$pane_id" "$client_socket" "$api_socket"
}

cmd_status() {
    local json=0
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --json) json=1 ;;
            *) die "usage: $SCRIPT_NAME status [--json]" ;;
        esac
        shift
    done

    local record
    record=$(lab_status_record)

    if [ "$json" = "1" ]; then
        printf '%s' "$record" | python3 -c "$PY_STATUS_JSON" "$ROOT" "$BIN" "$SESSION_NAME"
        return 0
    fi

    if [ -z "$record" ]; then
        printf 'no accounts lab at %s\n' "$ROOT"
        return 0
    fi

    local running pid pane_id client_socket api_socket
    IFS=$'\x1f' read -r running pid pane_id client_socket api_socket <<<"$record"
    printf '%-10s %-8s %-8s %-8s %s\n' "session" "running" "pid" "pane" "client socket"
    printf '%-10s %-8s %-8s %-8s %s\n' \
        "$SESSION_NAME" "$running" "${pid:--}" "${pane_id:--}" "${client_socket:--}"
    printf 'root:     %s\nbin:      %s\nclaude:   %s\nprofiles: %s, %s\n' \
        "$ROOT" "$BIN" "$ROOT/bin/claude" \
        "$ROOT/profiles/$DEFAULT_PROFILE" "$ROOT/profiles/$SECOND_PROFILE"
}

quoted() {
    printf '"%s"' "$(printf '%s' "$1" | sed -e 's/[\\"$`]/\\&/g')"
}

cmd_env() {
    [ "$#" -eq 0 ] || die "usage: $SCRIPT_NAME env"

    local record running pid pane_id client_socket api_socket
    record=$(lab_status_record)
    IFS=$'\x1f' read -r running pid pane_id client_socket api_socket <<<"${record:-}"

    printf 'export HERDR_ACCOUNTS_LAB_ROOT=%s\n' "$(quoted "$ROOT")"
    printf 'export XDG_CONFIG_HOME=%s\n' "$(quoted "$ROOT/xdg")"
    printf 'export HERDR_ACCOUNTS_LAB_SESSION=%s\n' "$(quoted "$SESSION_NAME")"
    printf 'export HERDR_ACCOUNTS_LAB_PROFILE_PERSO=%s\n' \
        "$(quoted "$ROOT/profiles/$DEFAULT_PROFILE")"
    printf 'export HERDR_ACCOUNTS_LAB_PROFILE_WORK=%s\n' \
        "$(quoted "$ROOT/profiles/$SECOND_PROFILE")"
    printf 'export HERDR_ACCOUNTS_LAB_PROFILE_AMBIENT=%s\n' \
        "$(quoted "$ROOT/profiles/$AMBIENT_PROFILE")"
    printf 'export HERDR_BIN=%s\n' "$(quoted "$BIN")"
    if [ -n "${pane_id:-}" ]; then
        printf 'export HERDR_ACCOUNTS_LAB_PANE=%s\n' "$(quoted "$pane_id")"
    fi
    if [ -n "${client_socket:-}" ]; then
        printf 'export HERDR_ACCOUNTS_LAB_CLIENT_SOCKET=%s\n' "$(quoted "$client_socket")"
    fi
    if [ -n "${api_socket:-}" ]; then
        printf 'export HERDR_ACCOUNTS_LAB_API_SOCKET=%s\n' "$(quoted "$api_socket")"
    fi
    # Deliberately not XDG_RUNTIME_DIR: eval-ing that into an interactive shell
    # would hijack the caller's Wayland/D-Bus/PipeWire sockets.
    printf 'export PATH=%s:"$PATH"\n' "$(quoted "$ROOT/bin")"
    return 0
}

# ---------------------------------------------------------------------------
# down
# ---------------------------------------------------------------------------

cmd_down() {
    [ "$#" -eq 0 ] || die "usage: $SCRIPT_NAME down"

    if [ ! -e "$ROOT" ]; then
        log "nothing to tear down at $ROOT"
        return 0
    fi
    has_marker || die "refusing to tear down $ROOT: no valid $MARKER_NAME marker"

    stop_started_session
    remove_root
    log "removed $ROOT"
}

# ---------------------------------------------------------------------------

main() {
    local command="${1:-}"
    [ "$#" -gt 0 ] && shift || true

    case "$command" in
        "" | help | -h | --help)
            usage
            [ -z "$command" ] && return 2 || return 0
            ;;
        up | down | status | env) ;;
        *)
            usage
            return 2
            ;;
    esac

    resolve_timeout
    BIN=$(resolve_bin)
    ROOT=$(resolve_root)
    set_lab_env

    case "$command" in
        up) cmd_up "$@" ;;
        down) cmd_down "$@" ;;
        status) cmd_status "$@" ;;
        env) cmd_env "$@" ;;
    esac
}

main "$@"
