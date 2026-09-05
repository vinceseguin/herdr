#!/usr/bin/env bash
# Fork verification gate: runs `just ci` inside a worktree under a machine-wide
# lock so parallel agents never run two Rust builds/test suites at once.
#
# Usage: bash scripts/fork/gate.sh [<worktree-dir>] [<just-recipe>]
#   <worktree-dir>  defaults to the repo root containing this script
#   <just-recipe>   defaults to `ci`; e.g. `test-one fleet_state` while iterating
#
# Prints the last 40 lines of the log, the log path, and finally `EXIT=<code>`.
# Read the EXIT line — never pipe this script into tail/head/grep, the pipeline
# would report the pager's exit status instead of the gate's.
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
worktree="${1:-$(cd "$script_dir/../.." && pwd)}"
recipe="${2:-ci}"
shift $(( $# > 2 ? 2 : $# )) || true

if [ ! -f "$worktree/justfile" ]; then
  echo "gate: $worktree does not look like a herdr checkout (no justfile)" >&2
  echo "EXIT=2"
  exit 2
fi

log_dir="${TMPDIR:-/tmp}/herdr-fork-gates"
mkdir -p "$log_dir"
stamp="$(date +%Y%m%d-%H%M%S)"
slug="$(basename "$worktree" | tr -c 'A-Za-z0-9_.-' '_')"
log="$log_dir/$stamp-$slug-${recipe%% *}.log"
lock="$log_dir/gate.lock"

echo "gate: worktree=$worktree recipe='$recipe' log=$log"
exec 9>"$lock"
if ! flock -n 9; then
  echo "gate: waiting for gate lock (another gate is running)…"
  flock 9
fi

start=$(date +%s)
(
  cd "$worktree" || exit 2
  # Do not set HERDR_BUILD_CHANNEL here: upstream tests assert the plain
  # version string, and the fork channel (E0) ships with its own test updates.
  export CARGO_TERM_COLOR=never
  # shellcheck disable=SC2086
  just $recipe "$@"
) >"$log" 2>&1
code=$?
end=$(date +%s)

echo "----- last 40 lines of $log -----"
tail -n 40 "$log"
echo "-----"
echo "gate: duration=$((end - start))s log=$log"
echo "EXIT=$code"
exit "$code"
