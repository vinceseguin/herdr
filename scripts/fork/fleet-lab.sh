#!/usr/bin/env bash
#
# fleet-lab.sh — boot N isolated herdr servers on this machine so fork work has
# a "LAN of N hosts" to validate against.
#
#   fleet-lab.sh up [n]        start n (default 2) named sessions lab-1..lab-n,
#                              each with one workspace labelled lab-N whose pane
#                              prints the marker `herdr-fleet-lab:lab-N`
#   fleet-lab.sh status [--json]
#   fleet-lab.sh env           export-able lines for `eval "$(fleet-lab.sh env)"`
#   fleet-lab.sh down          stop every session this lab started and delete
#                              the lab root
#
# Knobs:
#   HERDR_BIN                  herdr binary (default: <repo>/target/debug/herdr,
#                              else `herdr` on PATH)
#   HERDR_FLEET_LAB_ROOT       lab root (default: /tmp/herdr-fleet-lab)
#   HERDR_FLEET_LAB_TIMEOUT_MS per-step timeout in ms (default: 15000)
#
# Isolation, by construction:
#   * every herdr call carries an explicit `--session lab-N`, the lab's own
#     XDG_CONFIG_HOME/XDG_RUNTIME_DIR/XDG_STATE_HOME/XDG_DATA_HOME/
#     XDG_CACHE_HOME, and drops HERDR_SOCKET_PATH, HERDR_CLIENT_SOCKET_PATH,
#     HERDR_ENV, HERDR_SESSION and HERDR_CONFIG_PATH, so nothing here can reach
#     ~/.config/herdr, ~/.config/herdr-dev or the caller's default session;
#   * `down` only signals pids this script wrote itself, and only after
#     re-checking that the pid is still that lab's herdr server (pid reuse);
#   * `down` deletes only a plain directory carrying this lab's marker file, and
#     refuses `/`, $HOME, any ancestor of $HOME, the caller's XDG_CONFIG_HOME
#     (and anything inside it) and a handful of reserved system directories.

set -euo pipefail

SCRIPT_NAME=$(basename "$0")
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd -P)

MARKER_NAME=".herdr-fleet-lab"
MARKER_HEADER="herdr-fleet-lab"
SESSION_PREFIX="lab-"
MAX_SESSIONS=32
# Longest pid we accept from a pid file (Linux pid_max fits in 7 digits).
MAX_PID_DIGITS=10
# Internal record separator. Not a tab: bash `read` collapses runs of IFS
# whitespace, so an empty field between tabs would shift every later column.
SEP=$'\x1f'

ROOT=""
BIN=""
TIMEOUT_MS="${HERDR_FLEET_LAB_TIMEOUT_MS:-15000}"

PY_LAB_SESSIONS=$(
    cat <<'PY'
import json
import sys

prefix = sys.argv[1]
try:
    sessions = json.load(sys.stdin).get("sessions", [])
except Exception:
    sys.exit(0)

rows = []
for session in sessions:
    name = session.get("name", "")
    suffix = name[len(prefix):]
    if not name.startswith(prefix) or not suffix.isdigit():
        continue
    rows.append(
        (
            int(suffix),
            name,
            "true" if session.get("running") else "false",
            session.get("socket_path", ""),
            session.get("session_dir", ""),
        )
    )

for row in sorted(rows):
    print("\x1f".join(row[1:]))
PY
)

PY_PANE_ID=$(
    cat <<'PY'
import json
import sys

name = sys.argv[1]
try:
    snapshot = json.load(sys.stdin)["result"]["snapshot"]
except Exception:
    sys.exit(0)

labelled = {
    workspace.get("workspace_id")
    for workspace in snapshot.get("workspaces", [])
    if workspace.get("label") == name
}
panes = snapshot.get("panes", [])
for pane in panes:
    if pane.get("workspace_id") in labelled:
        print(pane.get("pane_id", ""))
        break
else:
    if panes:
        print(panes[0].get("pane_id", ""))
PY
)

PY_STATUS_JSON=$(
    cat <<'PY'
import json
import sys

root, binary = sys.argv[1], sys.argv[2]
sessions = []
for line in sys.stdin.read().splitlines():
    if not line.strip():
        continue
    name, running, pid, api_socket, client_socket, pane_id = (line.split("\x1f") + [""] * 6)[:6]
    sessions.append(
        {
            "name": name,
            "running": running == "true",
            "pid": int(pid) if pid.isdigit() else None,
            "api_socket": api_socket or None,
            "client_socket": client_socket or None,
            "pane_id": pane_id or None,
        }
    )

print(json.dumps({"root": root, "bin": binary, "sessions": sessions}))
PY
)

PY_ENV=$(
    cat <<'PY'
import re
import sys

root, prefix = sys.argv[1], sys.argv[2]


def quoted(value):
    return '"%s"' % re.sub(r'([\\"$`])', r"\\\1", value)


names = []
lines = []
for line in sys.stdin.read().splitlines():
    if not line.strip():
        continue
    name, running, pid, api_socket, client_socket, pane_id = (line.split("\x1f") + [""] * 6)[:6]
    names.append(name)
    index = name[len(prefix):]
    if client_socket:
        lines.append("export HERDR_FLEET_LAB_CLIENT_SOCKET_%s=%s" % (index, quoted(client_socket)))
    if api_socket:
        lines.append("export HERDR_FLEET_LAB_API_SOCKET_%s=%s" % (index, quoted(api_socket)))
    if pane_id:
        lines.append("export HERDR_FLEET_LAB_PANE_%s=%s" % (index, quoted(pane_id)))

print("export HERDR_FLEET_LAB_ROOT=%s" % quoted(root))
print("export XDG_CONFIG_HOME=%s" % quoted(root + "/xdg"))
# Deliberately not XDG_RUNTIME_DIR: eval-ing that into an interactive shell
# would hijack the caller's Wayland/D-Bus/PipeWire sockets. Herdr clients do
# not need it to reach the lab; the test watchdog reads it from the servers.
print("export HERDR_FLEET_LAB_RUNTIME_DIR=%s" % quoted(root + "/runtime"))
print("export HERDR_FLEET_LAB_SESSIONS=%s" % quoted(" ".join(names)))
for line in lines:
    print(line)
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
usage: $SCRIPT_NAME up [n] | down | status [--json] | env

  up [n]            start n isolated herdr sessions (default 2, max $MAX_SESSIONS)
  down              stop this lab's sessions and remove its root
  status [--json]   report the lab's sessions
  env               print export lines for eval "\$($SCRIPT_NAME env)"

environment:
  HERDR_BIN                    herdr binary to run
  HERDR_FLEET_LAB_ROOT         lab root (default /tmp/herdr-fleet-lab)
  HERDR_FLEET_LAB_TIMEOUT_MS   per-step timeout in ms (default 15000)
USAGE
}

# ---------------------------------------------------------------------------
# resolution and safety
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
        "" | *[!0-9]*) die "HERDR_FLEET_LAB_TIMEOUT_MS must be a positive integer: $TIMEOUT_MS" ;;
    esac
    TIMEOUT_MS=$((10#$TIMEOUT_MS))
    [ "$TIMEOUT_MS" -gt 0 ] || die "HERDR_FLEET_LAB_TIMEOUT_MS must be a positive integer"
}

# Refuse any root that could make `rm -rf` destructive outside the lab.
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

    # Require at least two path components, so /tmp-like roots are impossible.
    local trimmed="${root#/}"
    case "$trimmed" in
        */*) ;;
        *) die "refusing a top-level directory as the lab root: $root" ;;
    esac

    # A user's home directory itself, even when $HOME is unset or points
    # somewhere else.
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

    # The caller's own config home must never be reachable — neither as the
    # root, nor as a descendant of it, nor as an ancestor of it — except when
    # it is this lab's own xdg dir, which is what `eval "$(fleet-lab.sh env)"`
    # sets.
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
    local raw="${HERDR_FLEET_LAB_ROOT:-/tmp/herdr-fleet-lab}"
    case "$raw" in
        /*) ;;
        *) die "HERDR_FLEET_LAB_ROOT must be an absolute path: $raw" ;;
    esac
    case "$raw" in
        *$'\n'* | *$'\r'*) die "refusing a lab root containing a newline" ;;
    esac

    local real
    if [ -L "$raw" ] || [ -L "${raw%/}" ]; then
        die "HERDR_FLEET_LAB_ROOT must not be a symlink: $raw"
    elif [ -d "$raw" ]; then
        real=$(cd "$raw" && pwd -P)
    elif [ -e "$raw" ]; then
        die "HERDR_FLEET_LAB_ROOT exists and is not a plain directory: $raw"
    else
        local parent base
        parent=$(dirname "$raw")
        base=$(basename "$raw")
        case "$base" in
            . | ..) die "HERDR_FLEET_LAB_ROOT must name a directory: $raw" ;;
        esac
        [ -d "$parent" ] || die "parent of HERDR_FLEET_LAB_ROOT does not exist: $parent"
        parent=$(cd "$parent" && pwd -P)
        real="${parent%/}/$base"
    fi

    assert_safe_root "$real"
    printf '%s\n' "$real"
}

marker_path() {
    printf '%s\n' "$ROOT/$MARKER_NAME"
}

# True only for a regular (non-symlink) marker file written by `up`.
has_marker() {
    local marker
    marker=$(marker_path)
    [ ! -L "$marker" ] && [ -f "$marker" ] || return 1
    [ "$(head -n 1 "$marker" 2>/dev/null)" = "$MARKER_HEADER" ]
}

# rm -rf, but only ever on a validated, marked, plain-directory lab root.
remove_root() {
    assert_safe_root "$ROOT"
    has_marker || die "refusing to delete $ROOT: no valid $MARKER_NAME marker"
    [ ! -L "$ROOT" ] && [ -d "$ROOT" ] || die "refusing to delete $ROOT: not a plain directory"
    rm -rf -- "$ROOT"
}

# ---------------------------------------------------------------------------
# herdr plumbing
# ---------------------------------------------------------------------------

# The single environment every herdr process of this lab runs under, as
# arguments to env(1). Kept as an array (not a wrapper function) so the server
# can be started as one simple background command: `$!` must be the pid that
# execs into herdr, not a bash subshell wrapping a function call.
LAB_ENV=()

set_lab_env() {
    LAB_ENV=(
        -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV
        -u HERDR_SESSION -u HERDR_CONFIG_PATH
        XDG_CONFIG_HOME="$ROOT/xdg" XDG_RUNTIME_DIR="$ROOT/runtime"
        XDG_STATE_HOME="$ROOT/state" XDG_DATA_HOME="$ROOT/data"
        XDG_CACHE_HOME="$ROOT/cache"
    )
}

lab_herdr() {
    local session="$1"
    shift
    assert_session_name "$session"
    env "${LAB_ENV[@]}" "$BIN" --session "$session" "$@"
}

assert_session_name() {
    case "$1" in
        "$SESSION_PREFIX"[0-9]*) ;;
        *) die "refusing a session name outside the lab namespace: $1" ;;
    esac
    case "${1#"$SESSION_PREFIX"}" in
        *[!0-9]*) die "refusing a session name outside the lab namespace: $1" ;;
        "") die "refusing a session name outside the lab namespace: $1" ;;
    esac
}

session_name() {
    printf '%s%s\n' "$SESSION_PREFIX" "$1"
}

# name|running|api_socket|session_dir ($SEP-separated) for lab sessions, ordered by index.
lab_sessions_tsv() {
    local listing
    if ! listing=$(lab_herdr "$(session_name 1)" session list --json 2>/dev/null); then
        return 0
    fi
    printf '%s' "$listing" | python3 -c "$PY_LAB_SESSIONS" "$SESSION_PREFIX"
}

lab_any_running() {
    local tsv
    tsv=$(lab_sessions_tsv)
    printf '%s\n' "$tsv" | grep -q "${SEP}true${SEP}"
}

snapshot_pane_id() {
    local name="$1" snapshot
    if ! snapshot=$(lab_herdr "$name" api snapshot 2>/dev/null); then
        return 0
    fi
    printf '%s' "$snapshot" | python3 -c "$PY_PANE_ID" "$name"
}

# ---------------------------------------------------------------------------
# pid handling — only ever pids this lab wrote, re-validated before signalling
# ---------------------------------------------------------------------------

pid_file_for() {
    printf '%s\n' "$ROOT/pids/$1.pid"
}

# Prints the pid from a pid file this script wrote, or fails. Strict on
# purpose: one line, decimal digits only, no sign, no leading zero (Linux
# /proc rejects those and the identity checks below would be skipped), bounded
# length (so `[ -gt ]` cannot overflow), never 0/1, never this script or its
# parent.
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

# argv of $1 as one space-joined line (the lab's servers carry no spaces
# except inside the binary path, which is compared as a whole).
process_argv() {
    local pid="$1" argv
    if [ -d /proc/self ]; then
        argv=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
    else
        argv=$(ps -o args= -p "$pid" 2>/dev/null || true)
    fi
    printf '%s\n' "${argv% }"
}

# Ties the pid to *this* lab root, not merely to a session called lab-N:
# another lab under a different root uses the same session names.
process_runtime_dir_matches() {
    local pid="$1"
    # Linux: the environment is authoritative. Elsewhere there is no /proc, so
    # the argv check carries the identification alone.
    [ -d /proc/self ] || return 0
    [ -r "/proc/$pid/environ" ] || return 1
    tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null |
        grep -Fxq "XDG_RUNTIME_DIR=$ROOT/runtime"
}

# True only when $1 is still a live herdr server started by this lab for $2.
pid_is_lab_server() {
    local pid="$1" name="$2" expected_bin="$3" argv

    kill -0 "$pid" 2>/dev/null || return 1
    process_runtime_dir_matches "$pid" || return 1

    argv=$(process_argv "$pid")
    [ -n "$argv" ] || return 1
    [ -n "$expected_bin" ] || return 1

    # argv[0] is the binary itself for a real herdr; an interpreter may precede
    # it when HERDR_BIN is a script (tests). Either way the absolute binary
    # path must be immediately followed by exactly this session's server args.
    local expected="$expected_bin --session $name server"
    case "$argv" in
        "$expected" | "$expected "* | *" $expected" | *" $expected "*) ;;
        *) return 1 ;;
    esac
}

terminate_lab_pid() {
    local pid="$1" name="$2" expected_bin="$3" waited=0

    pid_is_lab_server "$pid" "$name" "$expected_bin" || return 0

    kill -TERM "$pid" 2>/dev/null || return 0
    while [ "$waited" -lt 40 ]; do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.05
        waited=$((waited + 1))
    done

    if pid_is_lab_server "$pid" "$name" "$expected_bin"; then
        kill -KILL "$pid" 2>/dev/null || true
    fi
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

# Iterates this lab's pid files: prints `name|pid` ($SEP-separated) for every well-formed one.
lab_pid_files() {
    local file name pid
    [ -d "$ROOT/pids" ] || return 0
    for file in "$ROOT"/pids/*.pid; do
        [ -e "$file" ] || continue
        name=$(basename "$file" .pid)
        case "$name" in
            "$SESSION_PREFIX"[0-9]*) ;;
            *) continue ;;
        esac
        pid=$(read_pid_file "$file") || continue
        printf '%s%s%s\n' "$name" "$SEP" "$pid"
    done
}

lab_any_pid_alive() {
    local expected name pid
    expected=$(expected_bin)
    while IFS="$SEP" read -r name pid; do
        [ -n "$name" ] || continue
        if pid_is_lab_server "$pid" "$name" "$expected"; then
            return 0
        fi
    done < <(lab_pid_files)
    return 1
}

# Ask each server this lab started to stop, then escalate to TERM/KILL — but
# only for pids that are still verifiably this lab's servers.
stop_started_sessions() {
    local expected name pid
    expected=$(expected_bin)
    while IFS="$SEP" read -r name pid; do
        [ -n "$name" ] || continue
        pid_is_lab_server "$pid" "$name" "$expected" || continue
        lab_herdr "$name" session stop "$name" >/dev/null 2>&1 || true
        terminate_lab_pid "$pid" "$name" "$expected"
    done < <(lab_pid_files)
}

# ---------------------------------------------------------------------------
# up
# ---------------------------------------------------------------------------

UP_STARTED=0
UP_CREATED_ROOT=0

# $1 is the exit status to finish with: the failing command's status on a
# normal error, or 128+signal when interrupted (bash's `$?` inside a signal
# trap is whatever ran last and is often 0, which would report success).
up_cleanup() {
    local status="$1"
    trap - EXIT INT TERM
    if [ "$UP_STARTED" = "1" ]; then
        exit "$status"
    fi

    log "up failed; cleaning up what it started"
    stop_started_sessions
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
    local name="$1" pid="$2" deadline
    # Wall-clock bound: every poll execs the herdr CLI, so counting iterations
    # would drift far past the configured timeout on a slow machine.
    deadline=$((SECONDS + (TIMEOUT_MS + 999) / 1000))
    while :; do
        if ! kill -0 "$pid" 2>/dev/null; then
            log "server for $name exited early; last log lines:"
            tail -n 20 "$ROOT/logs/$name.log" >&2 2>/dev/null || true
            return 1
        fi
        if lab_sessions_tsv | grep -q "^$name${SEP}true${SEP}" && api_is_ready "$name"; then
            return 0
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            break
        fi
        sleep 0.05
    done
    log "timed out waiting for $name to come up after ${TIMEOUT_MS}ms"
    tail -n 20 "$ROOT/logs/$name.log" >&2 2>/dev/null || true
    return 1
}

# The API socket starts listening before the app loop consumes requests, so a
# listening socket is not readiness: only a served snapshot is.
api_is_ready() {
    local name="$1" snapshot
    snapshot=$(lab_herdr "$name" api snapshot 2>/dev/null) || return 1
    printf '%s' "$snapshot" | grep -q '"snapshot"'
}

start_session() {
    local index="$1" name pane_id
    name=$(session_name "$index")

    mkdir -p "$ROOT/work/$name"

    # One simple command, so bash forks straight into nohup -> env -> herdr and
    # `$!` is the server's own pid.
    nohup env "${LAB_ENV[@]}" "$BIN" --session "$name" server \
        </dev/null >"$ROOT/logs/$name.log" 2>&1 &
    local pid=$!
    printf '%s\n' "$pid" >"$(pid_file_for "$name")"

    wait_for_session "$name" "$pid" || return 1

    pane_id=$(lab_herdr "$name" workspace create \
        --label "$name" --cwd "$ROOT/work/$name" --no-focus |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["root_pane"]["pane_id"])')
    [ -n "$pane_id" ] || return 1

    lab_herdr "$name" pane run "$pane_id" \
        "printf 'herdr-fleet-lab:%s\\n' $name; exec sh -c 'while :; do sleep 60; done'" >/dev/null
    lab_herdr "$name" pane wait-output "$pane_id" \
        --match "herdr-fleet-lab:$name" --timeout "$TIMEOUT_MS" >/dev/null

    log "$name up (pid $pid, pane $pane_id)"
}

cmd_up() {
    local count="${1:-2}"
    [ "$#" -le 1 ] || die "usage: $SCRIPT_NAME up [n]"
    case "$count" in
        "" | *[!0-9]*) die "usage: $SCRIPT_NAME up [n]" ;;
    esac
    count=$((10#$count))
    if [ "$count" -lt 1 ] || [ "$count" -gt "$MAX_SESSIONS" ]; then
        die "session count must be between 1 and $MAX_SESSIONS: $count"
    fi

    # Unix socket paths are capped at ~108 bytes (104 on macOS); fail with a
    # clear message instead of letting the first server die inside its own
    # socket bind.
    local sample_socket="$ROOT/xdg/herdr-dev/sessions/$SESSION_PREFIX$count/herdr-client.sock"
    if [ "${#sample_socket}" -ge 104 ]; then
        die "lab root is too long for a unix socket path (${#sample_socket} bytes): $ROOT"
    fi

    if [ -e "$ROOT" ]; then
        has_marker ||
            die "refusing to use $ROOT: it exists but has no valid $MARKER_NAME marker"
        if lab_any_running || lab_any_pid_alive; then
            die "fleet lab is already up at $ROOT (run: $SCRIPT_NAME down)"
        fi
        log "removing stale lab at $ROOT"
        stop_started_sessions
        remove_root
    fi

    UP_CREATED_ROOT=1
    arm_up_cleanup

    mkdir -p "$ROOT/runtime" "$ROOT/logs" "$ROOT/pids" "$ROOT/work" \
        "$ROOT/state" "$ROOT/data" "$ROOT/cache"
    # The config dir name depends on the build (`herdr-dev` for debug builds,
    # `herdr` for release), so seed both inside this throwaway XDG_CONFIG_HOME.
    local app
    for app in herdr herdr-dev; do
        mkdir -p "$ROOT/xdg/$app"
        printf 'onboarding = false\n' >"$ROOT/xdg/$app/config.toml"
    done

    {
        printf '%s\n' "$MARKER_HEADER"
        printf 'bin=%s\n' "$BIN"
        printf 'sessions=%s\n' "$count"
        printf 'created=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    } >"$(marker_path)"

    local index=1
    while [ "$index" -le "$count" ]; do
        start_session "$index"
        index=$((index + 1))
    done

    UP_STARTED=1
    trap - EXIT INT TERM
    cmd_status
}

# ---------------------------------------------------------------------------
# status / env
# ---------------------------------------------------------------------------

# name|running|pid|api_socket|client_socket|pane_id ($SEP-separated)
lab_status_tsv() {
    has_marker || return 0

    local expected
    expected=$(expected_bin)

    local name running api_socket session_dir pid pid_candidate pane_id client_socket
    while IFS="$SEP" read -r name running api_socket session_dir; do
        [ -n "$name" ] || continue
        pid=""
        if pid_candidate=$(read_pid_file "$(pid_file_for "$name")"); then
            if pid_is_lab_server "$pid_candidate" "$name" "$expected"; then
                pid="$pid_candidate"
            fi
        fi
        client_socket=""
        [ -n "$session_dir" ] && client_socket="$session_dir/herdr-client.sock"
        pane_id=""
        if [ "$running" = "true" ]; then
            pane_id=$(snapshot_pane_id "$name")
        fi
        printf '%s%s%s%s%s%s%s%s%s%s%s\n' \
            "$name" "$SEP" "$running" "$SEP" "$pid" "$SEP" "$api_socket" "$SEP" \
            "$client_socket" "$SEP" "$pane_id"
    done < <(lab_sessions_tsv)
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

    local tsv
    tsv=$(lab_status_tsv)

    if [ "$json" = "1" ]; then
        printf '%s' "$tsv" | python3 -c "$PY_STATUS_JSON" "$ROOT" "$BIN"
        return 0
    fi

    printf '%-10s %-8s %-8s %-8s %s\n' "session" "running" "pid" "pane" "client socket"
    if [ -z "$tsv" ]; then
        printf 'no fleet lab at %s\n' "$ROOT"
        return 0
    fi
    local name running pid api_socket client_socket pane_id
    while IFS="$SEP" read -r name running pid api_socket client_socket pane_id; do
        [ -n "$name" ] || continue
        printf '%-10s %-8s %-8s %-8s %s\n' \
            "$name" "$running" "${pid:--}" "${pane_id:--}" "${client_socket:--}"
    done <<<"$tsv"
    printf 'root: %s\nbin:  %s\n' "$ROOT" "$BIN"
}

cmd_env() {
    [ "$#" -eq 0 ] || die "usage: $SCRIPT_NAME env"

    lab_status_tsv | python3 -c "$PY_ENV" "$ROOT" "$SESSION_PREFIX"
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
    has_marker ||
        die "refusing to tear down $ROOT: no valid $MARKER_NAME marker"

    # Politely stop anything the lab's own session listing still reports as
    # running (covers a server started by hand with the lab's env), then deal
    # with the pids this script wrote.
    local name running api_socket session_dir
    while IFS="$SEP" read -r name running api_socket session_dir; do
        [ -n "$name" ] || continue
        [ "$running" = "true" ] || continue
        lab_herdr "$name" session stop "$name" >/dev/null 2>&1 || true
    done < <(lab_sessions_tsv)

    stop_started_sessions
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
