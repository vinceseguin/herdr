#!/usr/bin/env bash
#
# ssh-lab.sh — make the fleet lab reachable over SSH, with zero root and zero
# contact with the caller's ~/.ssh, ~/.config or installed herdr.
#
#   ssh-lab.sh up              start a user-space sshd on 127.0.0.1:<port> that
#                              serves the fleet lab's herdr as the alias
#                              `herdr-ssh-lab`
#   ssh-lab.sh status [--json]
#   ssh-lab.sh env             export-able lines for `eval "$(ssh-lab.sh env)"`
#   ssh-lab.sh down            stop that sshd and delete the ssh lab root
#
# Knobs:
#   HERDR_FLEET_LAB_ROOT       fleet lab root (default: /tmp/herdr-fleet-lab);
#                              the ssh lab lives in <root>/ssh
#   HERDR_SSH_LAB_PORT         listen port on 127.0.0.1 (default: 2299)
#   HERDR_FLEET_LAB_TIMEOUT_MS per-step timeout in ms (default: 15000)
#   HERDR_SSH_LAB_SSHD         sshd binary, absolute (default: /usr/sbin/sshd,
#                              /usr/bin/sshd, then `sshd` on PATH)
#   HERDR_BIN                  herdr binary; only consulted when the fleet lab
#                              marker records none (passed to fleet-lab.sh)
#
# Isolation, by construction:
#   * everything lives under <fleet lab root>/ssh, which must sit next to a
#     valid `.herdr-fleet-lab` marker owned by the invoking user and carries
#     its own `.herdr-ssh-lab` marker; the caller's ~/.ssh, ~/.local/bin and
#     ~/.config are never read or written — every ssh client this script runs
#     carries HOME=<root>/home, an explicit `-F <root>/home/.ssh/config`, no
#     agent and no default identity, and the sshd is told not to run
#     ~/.ssh/rc or read ~/.ssh/authorized_keys and ~/.ssh/environment;
#   * the sshd runs as the invoking user, listens only on 127.0.0.1, accepts
#     only that user with the throwaway key this script generated, forwards
#     nothing, and is never installed, enabled or started as a system service;
#   * the remote side runs a wrapper that execs the fleet lab's own herdr
#     binary under the fleet lab's XDG_* dirs, so an `ssh herdr-ssh-lab herdr
#     …` can only ever reach the lab's sessions;
#   * `down` signals only the pid sshd wrote into this lab's pid file, and
#     only after re-checking that it is still the listener `up` started (its
#     exact argv and, on Linux, its executable: pid reuse), then the
#     per-connection children that listener still had, and deletes only a
#     plain, marked directory named `ssh` inside the fleet lab root.
#
# Tear down in order: `ssh-lab.sh down` before `fleet-lab.sh down`. The fleet
# lab's `down` deletes the whole root including <root>/ssh and its pid file,
# after which the sshd can no longer be identified and is left for the
# operator (see the hint `down` prints when the port is still in use).

set -euo pipefail

SCRIPT_NAME=$(basename "$0")
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)

FLEET_MARKER_NAME=".herdr-fleet-lab"
FLEET_MARKER_HEADER="herdr-fleet-lab"
MARKER_NAME=".herdr-ssh-lab"
MARKER_HEADER="herdr-ssh-lab"
TARGET_ALIAS="herdr-ssh-lab"
SSH_DIR_NAME="ssh"
# Longest pid we accept from a pid file (Linux pid_max fits in 7 digits).
MAX_PID_DIGITS=10
# Exit code reserved for "this machine has no sshd": callers degrade to the
# local-kind path instead of failing.
EXIT_NO_SSHD=3

LAB_ROOT=""
ROOT=""
BIN=""
SSHD=""
PORT="${HERDR_SSH_LAB_PORT:-2299}"
TIMEOUT_MS="${HERDR_FLEET_LAB_TIMEOUT_MS:-15000}"

PY_FLEET_BIN=$(
    cat <<'PY'
import json
import sys

try:
    print(json.load(sys.stdin).get("bin") or "")
except Exception:
    sys.exit(1)
PY
)

PY_STATUS_JSON=$(
    cat <<'PY'
import json
import sys

root, port, running, pid, target, home, ssh_config = (sys.argv[1:8] + [""] * 7)[:7]
print(
    json.dumps(
        {
            "root": root,
            "port": int(port),
            "running": running == "true",
            "pid": int(pid) if pid.isdigit() else None,
            "target": target,
            "home": home,
            "ssh_config": ssh_config,
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

  up                start a user-space sshd serving the fleet lab over ssh
  down              stop that sshd and remove the ssh lab root
  status [--json]   report the ssh lab
  env               print export lines for eval "\$($SCRIPT_NAME env)"

environment:
  HERDR_FLEET_LAB_ROOT         fleet lab root (default /tmp/herdr-fleet-lab)
  HERDR_SSH_LAB_PORT           listen port on 127.0.0.1 (default 2299)
  HERDR_FLEET_LAB_TIMEOUT_MS   per-step timeout in ms (default 15000)
  HERDR_SSH_LAB_SSHD           sshd binary, absolute path
  HERDR_BIN                    herdr binary, when the fleet lab marker has none

exit codes:
  0 ok   1 error   2 usage   $EXIT_NO_SSHD sshd not found
USAGE
}

# ---------------------------------------------------------------------------
# resolution and safety
# ---------------------------------------------------------------------------

# Refuse any root that could make `rm -rf` destructive outside the lab.
# Same rules as fleet-lab.sh's assert_safe_root; kept in sync by hand because
# these two scripts are deliberately standalone.
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
    # it is the fleet lab's own xdg dir, which is what
    # `eval "$(fleet-lab.sh env)"` sets.
    local caller_config="${XDG_CONFIG_HOME:-}"
    if [ -z "$caller_config" ] && [ -n "${HOME:-}" ]; then
        caller_config="$HOME/.config"
    fi
    if [ -n "$caller_config" ] && [ -d "$caller_config" ]; then
        local config_real
        config_real=$(cd "$caller_config" && pwd -P)
        if [ "$config_real" != "$LAB_ROOT/xdg" ]; then
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

# Every path this script embeds ends up unquoted in `sshd_config` (whitespace
# separated, `:` separates PATH entries), inside the remote wrapper (a /bin/sh
# script) and in `env` output the caller evals, so only a conservative
# character set is accepted: no whitespace, quotes, `$`, backticks, `\`, `:`
# or shell metacharacters can ever reach those files.
assert_safe_path_chars() {
    local what="$1" path="$2"
    case "$path" in
        *[![:alnum:]._/+@-]*)
            die "refusing $what with characters outside [A-Za-z0-9._/+@-]: $path"
            ;;
    esac
}

# $1 is `required` (the fleet lab must exist: up) or `optional` (status, env
# and down report a missing fleet lab instead of failing on it).
resolve_lab_root() {
    local mode="$1" raw="${HERDR_FLEET_LAB_ROOT:-/tmp/herdr-fleet-lab}"
    case "$raw" in
        /*) ;;
        *) die "HERDR_FLEET_LAB_ROOT must be an absolute path: $raw" ;;
    esac
    case "$raw" in
        *$'\n'* | *$'\r'*) die "refusing a lab root containing a newline" ;;
    esac
    if [ -L "$raw" ] || [ -L "${raw%/}" ]; then
        die "HERDR_FLEET_LAB_ROOT must not be a symlink: $raw"
    fi

    local real
    if [ -d "$raw" ]; then
        real=$(cd "$raw" && pwd -P)
    elif [ -e "$raw" ]; then
        die "HERDR_FLEET_LAB_ROOT exists and is not a plain directory: $raw"
    elif [ "$mode" = "required" ]; then
        die "no fleet lab at $raw (run: fleet-lab.sh up)"
    else
        real="${raw%/}"
    fi
    assert_safe_path_chars "a lab root" "$real"
    printf '%s\n' "$real"
}

# The fleet lab must exist, be the real thing and be ours: this script writes
# inside it and its `down` deletes a directory under it.
assert_fleet_lab() {
    local marker="$LAB_ROOT/$FLEET_MARKER_NAME"
    [ -d "$LAB_ROOT" ] && [ -O "$LAB_ROOT" ] ||
        die "refusing to use $LAB_ROOT: not a directory owned by $(id -un)"
    [ ! -L "$marker" ] && [ -f "$marker" ] && [ -O "$marker" ] ||
        die "refusing to use $LAB_ROOT: no valid $FLEET_MARKER_NAME marker (run: fleet-lab.sh up)"
    [ "$(head -n 1 "$marker" 2>/dev/null)" = "$FLEET_MARKER_HEADER" ] ||
        die "refusing to use $LAB_ROOT: no valid $FLEET_MARKER_NAME marker (run: fleet-lab.sh up)"
}

resolve_root() {
    local root="$LAB_ROOT/$SSH_DIR_NAME"
    if [ -L "$root" ]; then
        die "ssh lab root must not be a symlink: $root"
    fi
    if [ -e "$root" ] && [ ! -d "$root" ]; then
        die "ssh lab root exists and is not a plain directory: $root"
    fi
    assert_safe_root "$root"
    printf '%s\n' "$root"
}

resolve_port() {
    case "$PORT" in
        "" | *[!0-9]*) die "HERDR_SSH_LAB_PORT must be a port number: $PORT" ;;
    esac
    PORT=$((10#$PORT))
    if [ "$PORT" -lt 1024 ] || [ "$PORT" -gt 65535 ]; then
        die "HERDR_SSH_LAB_PORT must be an unprivileged port (1024-65535): $PORT"
    fi
}

resolve_timeout() {
    case "$TIMEOUT_MS" in
        "" | *[!0-9]*) die "HERDR_FLEET_LAB_TIMEOUT_MS must be a positive integer: $TIMEOUT_MS" ;;
    esac
    TIMEOUT_MS=$((10#$TIMEOUT_MS))
    [ "$TIMEOUT_MS" -gt 0 ] || die "HERDR_FLEET_LAB_TIMEOUT_MS must be a positive integer"
}

# sshd re-execs itself and refuses to start unless it was invoked through an
# absolute path, so resolve one here rather than relying on PATH.
#
# A misconfigured HERDR_SSH_LAB_SSHD is a hard error, never "sshd not found":
# exit 3 makes callers skip SSH validation, and a typo must not do that
# quietly. Checked here in the main shell — a `die` inside the `$(…)` that
# calls resolve_sshd would only end the subshell and read as exit 3.
assert_sshd_override() {
    local override="${HERDR_SSH_LAB_SSHD:-}"
    [ -n "$override" ] || return 0
    case "$override" in
        /*) ;;
        *) die "HERDR_SSH_LAB_SSHD must be an absolute path: $override" ;;
    esac
    [ -f "$override" ] && [ -x "$override" ] ||
        die "HERDR_SSH_LAB_SSHD is not an executable file: $override"
}

resolve_sshd() {
    local candidate resolved
    if [ -n "${HERDR_SSH_LAB_SSHD:-}" ]; then
        printf '%s\n' "$HERDR_SSH_LAB_SSHD"
        return 0
    fi
    for candidate in /usr/sbin/sshd /usr/bin/sshd; do
        if [ -f "$candidate" ] && [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    if resolved=$(command -v sshd 2>/dev/null); then
        case "$resolved" in
            /*) ;;
            *) resolved="$(cd "$(dirname "$resolved")" && pwd -P)/$(basename "$resolved")" ;;
        esac
        if [ -f "$resolved" ] && [ -x "$resolved" ]; then
            printf '%s\n' "$resolved"
            return 0
        fi
    fi
    return 1
}

require_tool() {
    command -v "$1" >/dev/null 2>&1 || die "$1 not found"
}

# The herdr binary the fleet lab's servers actually run: the one its marker
# recorded at `fleet-lab.sh up`. `status --json` reports the binary resolved
# from the *current* HERDR_BIN, which could differ; it is the fallback for a
# marker without a `bin=` line.
resolve_bin() {
    local status resolved
    resolved=$(sed -n 's/^bin=//p' "$LAB_ROOT/$FLEET_MARKER_NAME" 2>/dev/null | head -n 1 || true)
    if [ -z "$resolved" ]; then
        if ! status=$(bash "$SCRIPT_DIR/fleet-lab.sh" status --json 2>/dev/null); then
            die "fleet-lab.sh status --json failed (run: fleet-lab.sh up)"
        fi
        resolved=$(printf '%s' "$status" | python3 -c "$PY_FLEET_BIN") ||
            die "could not read the fleet lab's herdr binary from status --json"
    fi
    [ -n "$resolved" ] || die "fleet lab reported no herdr binary"
    case "$resolved" in
        /*) ;;
        *) die "fleet lab reported a non-absolute herdr binary: $resolved" ;;
    esac
    assert_safe_path_chars "a herdr binary path" "$resolved"
    [ -f "$resolved" ] && [ -x "$resolved" ] ||
        die "fleet lab's herdr binary is not an executable file: $resolved"
    printf '%s\n' "$resolved"
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

# rm -rf, but only ever on a validated, marked, plain directory named `ssh`
# directly inside the validated fleet lab root.
remove_root() {
    assert_safe_root "$ROOT"
    [ "$ROOT" = "$LAB_ROOT/$SSH_DIR_NAME" ] ||
        die "refusing to delete $ROOT: not the ssh lab of $LAB_ROOT"
    has_marker || die "refusing to delete $ROOT: no valid $MARKER_NAME marker"
    [ ! -L "$ROOT" ] && [ -d "$ROOT" ] || die "refusing to delete $ROOT: not a plain directory"
    rm -rf -- "$ROOT"
}

# ---------------------------------------------------------------------------
# pid handling — only ever the pid this script wrote, re-validated before
# signalling
# ---------------------------------------------------------------------------

pid_file() {
    printf '%s\n' "$ROOT/sshd.pid"
}

# Prints the pid from the pid file, or fails. Strict on purpose: one line,
# decimal digits only, no sign, no leading zero, bounded length, never 0/1,
# never this script or its parent.
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

# argv of $1 as one space-joined line.
process_argv() {
    local pid="$1" argv
    if [ -d /proc/self ]; then
        argv=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
    else
        argv=$(ps -o args= -p "$pid" 2>/dev/null || true)
    fi
    printf '%s\n' "${argv% }"
}

# Linux: /proc/<pid>/exe names the binary a process is actually running,
# which it cannot fake the way it can its argv; a package upgrade under a
# running sshd only appends " (deleted)". Elsewhere there is no /proc, so the
# argv check carries the identification alone. (Not /proc/<pid>/environ:
# sshd's process-title emulation overwrites that area, so it never matches.)
process_exe_is_sshd() {
    local pid="$1" exe
    [ -d /proc/self ] || return 0
    exe=$(readlink "/proc/$pid/exe" 2>/dev/null) || return 1
    exe="${exe% (deleted)}"
    case "${exe##*/}" in
        sshd | sshd-session | sshd-auth) ;;
        *) return 1 ;;
    esac
}

# True only when $1 is still the live sshd listener this lab started: alive,
# an sshd binary (above), and carrying exactly the argv `up` used — sshd
# retitles its listener to `sshd: <path> -f … -E … [listener] …` on some
# platforms, so the argv is matched as a contiguous run, anchored on the
# binary name, not as loose substrings (another lab under a different root
# uses the same alias and port).
pid_is_lab_sshd() {
    local pid="$1" argv
    kill -0 "$pid" 2>/dev/null || return 1
    process_exe_is_sshd "$pid" || return 1
    argv=$(process_argv "$pid")
    [ -n "$argv" ] || return 1
    case "$argv" in
        *sshd" -f $ROOT/sshd_config -E $ROOT/sshd.log"*) ;;
        *) return 1 ;;
    esac
}

# True only for a per-connection process of this lab's sshd: alive, an sshd
# binary (above), and carrying sshd's session process title (`sshd-session:
# user …`, or `sshd: user [priv]` on older OpenSSH). Callers only pass pids
# that were children of the verified listener a moment earlier; that parent
# link is what ties them to this root.
pid_is_lab_sshd_session() {
    local pid="$1" argv
    kill -0 "$pid" 2>/dev/null || return 1
    process_exe_is_sshd "$pid" || return 1
    argv=$(process_argv "$pid")
    case "$argv" in
        sshd* | */sshd*) ;;
        *) return 1 ;;
    esac
}

lab_sshd_pid() {
    local pid
    pid=$(read_pid_file "$(pid_file)") || return 1
    pid_is_lab_sshd "$pid" || return 1
    printf '%s\n' "$pid"
}

# Direct children of $1, one pid per line.
child_pids() {
    local parent="$1"
    ps -eo pid=,ppid= 2>/dev/null | awk -v parent="$parent" '$2 == parent { print $1 }'
}

# TERM, then KILL after $3 × 50 ms, but only while $2 (a pid_is_* predicate)
# still vouches for the pid.
signal_lab_pid() {
    local pid="$1" predicate="$2" budget="$3" waited=0

    "$predicate" "$pid" || return 0
    kill -TERM "$pid" 2>/dev/null || return 0
    while [ "$waited" -lt "$budget" ]; do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.05
        waited=$((waited + 1))
    done
    if "$predicate" "$pid"; then
        kill -KILL "$pid" 2>/dev/null || true
    fi
}

# Stops the listener, then the per-connection processes it still had. The
# listener's own SIGTERM never reaches those: sshd forks one per open ssh
# session and they outlive it, so a caller's ControlPersist master or an
# interactive session would otherwise keep the lab alive after `down`. They
# are collected while the listener is alive and verified — once it is gone
# they are reparented and can no longer be traced to it.
terminate_lab_sshd() {
    local pid="$1" child children

    pid_is_lab_sshd "$pid" || return 0
    children=$(child_pids "$pid")

    signal_lab_pid "$pid" pid_is_lab_sshd 60

    for child in $children; do
        pid_is_lab_sshd_session "$child" || continue
        log "stopping ssh session process $child still open on the lab"
        signal_lab_pid "$child" pid_is_lab_sshd_session 40
    done
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
    local status="$1" pid
    trap - EXIT INT TERM
    if [ "$UP_STARTED" = "1" ]; then
        exit "$status"
    fi

    log "up failed; cleaning up what it started"
    if pid=$(lab_sshd_pid); then
        terminate_lab_sshd "$pid"
    fi
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

# A connect attempt in a subshell: its exit status is the answer, and the
# descriptor dies with the subshell.
port_is_busy() {
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null
}

# `IdentityAgent none` and `IdentitiesOnly yes` keep ssh off the caller's
# agent and default ~/.ssh/id_* keys; `-F` (always passed) keeps it off
# ~/.ssh/config, and UserKnownHostsFile off ~/.ssh/known_hosts.
write_ssh_client_config() {
    cat >"$ROOT/home/.ssh/config" <<CONFIG
Host $TARGET_ALIAS
  HostName 127.0.0.1
  Port $PORT
  User $(id -un)
  IdentityFile $ROOT/home/.ssh/id_ed25519
  IdentitiesOnly yes
  IdentityAgent none
  UserKnownHostsFile $ROOT/home/.ssh/known_hosts
  StrictHostKeyChecking no
  LogLevel ERROR
CONFIG
    chmod 600 "$ROOT/home/.ssh/config"
}

write_herdr_wrapper() {
    cat >"$ROOT/home/.local/bin/herdr" <<WRAPPER
#!/bin/sh
# The fleet lab's own herdr, under the fleet lab's XDG dirs. Nothing here can
# reach the caller's ~/.config/herdr or default session.
exec env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \\
  -u HERDR_SESSION -u HERDR_CONFIG_PATH \\
  XDG_CONFIG_HOME="$LAB_ROOT/xdg" XDG_RUNTIME_DIR="$LAB_ROOT/runtime" \\
  XDG_STATE_HOME="$LAB_ROOT/state" XDG_DATA_HOME="$LAB_ROOT/data" \\
  XDG_CACHE_HOME="$LAB_ROOT/cache" \\
  "$BIN" "\$@"
WRAPPER
    chmod 755 "$ROOT/home/.local/bin/herdr"
}

# Only this user, only the lab key, no forwarding of any kind, and none of
# sshd's per-user files: with AuthorizedKeysFile pointed here and
# PermitUserRC/PermitUserEnvironment off, sshd never opens
# ~/.ssh/authorized_keys, ~/.ssh/rc or ~/.ssh/environment (which it would
# resolve through the passwd home, not $HOME). StrictModes must be off: it
# would demand that every ancestor of the key file — /tmp included — be
# non-world-writable. LogLevel INFO so the log tailed on a failed `up` shows
# the accepted/refused authentication, not just fatal errors.
write_sshd_config() {
    cat >"$ROOT/sshd_config" <<CONFIG
Port $PORT
ListenAddress 127.0.0.1
HostKey $ROOT/hostkey
PidFile $ROOT/sshd.pid
AuthorizedKeysFile $ROOT/authorized_keys
AllowUsers $(id -un)
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
StrictModes no
PermitUserEnvironment no
PermitUserRC no
X11Forwarding no
AllowAgentForwarding no
AllowTcpForwarding no
AllowStreamLocalForwarding no
PermitTunnel no
PrintMotd no
LogLevel INFO
SetEnv HOME=$ROOT/home XDG_CONFIG_HOME=$LAB_ROOT/xdg PATH=$ROOT/home/.local/bin:/usr/local/bin:/usr/bin:/bin
CONFIG
    chmod 600 "$ROOT/sshd_config"
}

# Every ssh client this script runs is pinned to the lab's fake HOME and its
# own config file, so the caller's ~/.ssh is never opened.
lab_ssh() {
    env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
        -u HERDR_SESSION -u HERDR_CONFIG_PATH \
        HOME="$ROOT/home" \
        ssh -F "$ROOT/home/.ssh/config" -o BatchMode=yes -o ConnectTimeout=5 \
        "$TARGET_ALIAS" "$@"
}

wait_for_sshd() {
    local deadline pid probe
    deadline=$((SECONDS + (TIMEOUT_MS + 999) / 1000))
    while :; do
        if pid=$(lab_sshd_pid); then
            if probe=$(lab_ssh 'command -v herdr' 2>"$ROOT/probe.log"); then
                if [ "$probe" = "$ROOT/home/.local/bin/herdr" ]; then
                    log "sshd up (pid $pid, port $PORT, target $TARGET_ALIAS)"
                    return 0
                fi
                log "remote herdr resolved to '$probe', expected $ROOT/home/.local/bin/herdr"
                return 1
            fi
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            break
        fi
        sleep 0.1
    done
    log "timed out waiting for the ssh lab after ${TIMEOUT_MS}ms"
    log "last ssh client error ($ROOT/probe.log):"
    tail -n 5 "$ROOT/probe.log" >&2 2>/dev/null || true
    log "sshd log ($ROOT/sshd.log):"
    tail -n 20 "$ROOT/sshd.log" >&2 2>/dev/null || true
    return 1
}

cmd_up() {
    [ "$#" -eq 0 ] || die "usage: $SCRIPT_NAME up"

    require_tool ssh
    require_tool ssh-keygen
    require_tool python3

    if [ -e "$ROOT" ]; then
        has_marker ||
            die "refusing to use $ROOT: it exists but has no valid $MARKER_NAME marker"
        if lab_sshd_pid >/dev/null; then
            die "ssh lab is already up at $ROOT (run: $SCRIPT_NAME down)"
        fi
        log "removing stale ssh lab at $ROOT"
        remove_root
    fi

    if port_is_busy; then
        die "127.0.0.1:$PORT is already in use (set HERDR_SSH_LAB_PORT; a lab sshd left behind shows up in: ps -eo pid,args | grep '[s]shd.*-f .*/ssh/sshd_config')"
    fi

    UP_CREATED_ROOT=1
    arm_up_cleanup

    mkdir -p "$ROOT"
    chmod 700 "$ROOT"
    {
        printf '%s\n' "$MARKER_HEADER"
        printf 'bin=%s\n' "$BIN"
        printf 'port=%s\n' "$PORT"
        printf 'created=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    } >"$(marker_path)"

    mkdir -p "$ROOT/home/.ssh" "$ROOT/home/.local/bin"
    chmod 700 "$ROOT/home" "$ROOT/home/.ssh"

    ssh-keygen -q -t ed25519 -N '' -C "$TARGET_ALIAS-client" \
        -f "$ROOT/home/.ssh/id_ed25519" </dev/null
    ssh-keygen -q -t ed25519 -N '' -C "$TARGET_ALIAS-host" \
        -f "$ROOT/hostkey" </dev/null
    chmod 600 "$ROOT/hostkey"

    : >"$ROOT/home/.ssh/known_hosts"
    chmod 600 "$ROOT/home/.ssh/known_hosts"
    cp "$ROOT/home/.ssh/id_ed25519.pub" "$ROOT/authorized_keys"
    chmod 600 "$ROOT/authorized_keys"

    write_ssh_client_config
    write_herdr_wrapper
    write_sshd_config

    # Pre-flight the config with sshd's own parser, so a typo or an option this
    # sshd does not know fails here with sshd's message instead of as a
    # timeout below.
    "$SSHD" -t -f "$ROOT/sshd_config" || die "sshd rejected $ROOT/sshd_config"

    # Not a service: a plain user process that daemonizes itself and writes
    # $ROOT/sshd.pid. `sshd -f` never touches /etc/ssh/sshd_config. This exact
    # argv is what pid_is_lab_sshd re-checks before anything is signalled.
    "$SSHD" -f "$ROOT/sshd_config" -E "$ROOT/sshd.log"

    wait_for_sshd

    UP_STARTED=1
    trap - EXIT INT TERM
    cmd_status
}

# ---------------------------------------------------------------------------
# status / env
# ---------------------------------------------------------------------------

cmd_status() {
    local json=0
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --json) json=1 ;;
            *) die "usage: $SCRIPT_NAME status [--json]" ;;
        esac
        shift
    done

    local pid running
    pid=""
    running="false"
    if has_marker && pid=$(lab_sshd_pid); then
        running="true"
    fi

    if [ "$json" = "1" ]; then
        python3 -c "$PY_STATUS_JSON" "$ROOT" "$PORT" "$running" "$pid" \
            "$TARGET_ALIAS" "$ROOT/home" "$ROOT/home/.ssh/config"
        return 0
    fi

    printf 'root:       %s\n' "$ROOT"
    printf 'port:       %s\n' "$PORT"
    printf 'running:    %s\n' "$running"
    printf 'pid:        %s\n' "${pid:--}"
    printf 'target:     %s\n' "$TARGET_ALIAS"
    printf 'home:       %s\n' "$ROOT/home"
    printf 'ssh_config: %s\n' "$ROOT/home/.ssh/config"
}

cmd_env() {
    [ "$#" -eq 0 ] || die "usage: $SCRIPT_NAME env"

    printf 'export HERDR_SSH_LAB_ROOT="%s"\n' "$ROOT"
    printf 'export HERDR_SSH_LAB_HOME="%s"\n' "$ROOT/home"
    printf 'export HERDR_SSH_LAB_TARGET="%s"\n' "$TARGET_ALIAS"
    printf 'export HERDR_SSH_LAB_PORT="%s"\n' "$PORT"
    printf 'export HERDR_SSH_LAB_SSH_CONFIG="%s"\n' "$ROOT/home/.ssh/config"
}

# ---------------------------------------------------------------------------
# down
# ---------------------------------------------------------------------------

cmd_down() {
    [ "$#" -eq 0 ] || die "usage: $SCRIPT_NAME down"

    if [ ! -e "$ROOT" ]; then
        log "nothing to tear down at $ROOT"
        # Without the root there is no pid file, so an sshd cannot be tied to
        # this lab any more; say so instead of guessing at a pid.
        if port_is_busy; then
            log "note: 127.0.0.1:$PORT is still in use; if fleet-lab.sh down ran first, its lab sshd is orphaned: ps -eo pid,args | grep '[s]shd.*-f $ROOT/sshd_config'"
        fi
        return 0
    fi
    has_marker ||
        die "refusing to tear down $ROOT: no valid $MARKER_NAME marker"

    local pid
    if pid=$(lab_sshd_pid); then
        terminate_lab_sshd "$pid"
    fi
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
    resolve_port
    if [ "$command" = "up" ]; then
        LAB_ROOT=$(resolve_lab_root required)
    else
        LAB_ROOT=$(resolve_lab_root optional)
    fi
    # fleet-lab.sh applied the same rules when it created the root; re-check
    # here rather than trust a marker alone.
    assert_safe_root "$LAB_ROOT"
    # status, env and down on a fleet lab that is already gone report that
    # (nothing is running, nothing to delete) instead of failing.
    if [ "$command" = "up" ] || [ -e "$LAB_ROOT" ]; then
        assert_fleet_lab
    fi
    ROOT=$(resolve_root)

    if [ "$command" = "up" ]; then
        assert_sshd_override
        if ! SSHD=$(resolve_sshd); then
            printf '%s: sshd not found\n' "$SCRIPT_NAME" >&2
            return "$EXIT_NO_SSHD"
        fi
        BIN=$(resolve_bin)
    fi

    case "$command" in
        up) cmd_up "$@" ;;
        down) cmd_down "$@" ;;
        status) cmd_status "$@" ;;
        env) cmd_env "$@" ;;
    esac
}

main "$@"
