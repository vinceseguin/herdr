#!/usr/bin/env bash
# Fork developer setup: brings a machine from a fresh clone to a green
# `just ci`. Pins the toolchain through mise, installs cargo-nextest, checks
# Zig, installs the repo git hooks, and finally runs the fork gate.
#
# Usage: bash scripts/fork/dev-setup.sh [--check] [--skip-ci]
#   --check     verify only: install nothing, exit non-zero if anything is
#               missing (implies --skip-ci)
#   --skip-ci   run steps 1-5 but not the final gate
#
# Every step prints `ok` (already satisfied), `installed` (this run changed
# something) or `missing` (not satisfied). The script is idempotent: a second
# run reports everything `ok` and touches nothing, including the global mise
# config. The last line of a full run is the gate's own `EXIT=<code>`.
set -euo pipefail

# Zig is pinned by the vendored libghostty-vt (see build.rs); build.rs has no
# version check of its own, so this script is the guard.
ZIG_VERSION="0.15.2"
# Fallback only; the real pin is read from rust-toolchain.toml below.
RUST_VERSION_FALLBACK="1.96.1"
MISE_INSTALL_HINT="install mise first: https://mise.jdx.dev/getting-started.html (e.g. your distro package, or 'curl https://mise.run | sh')"

script_file="${BASH_SOURCE[0]}"
script_dir="$(cd "$(dirname "$script_file")" && pwd)"
script_file="$script_dir/$(basename "$script_file")"
repo_root="$(cd "$script_dir/../.." && pwd)"

check_only=0
skip_ci=0

usage() {
  # The header comment block above is the help text; stop at the first line
  # that is not a comment so this never drifts from the script.
  if [ -r "$script_file" ]; then
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$script_file"
  else
    echo "Usage: bash scripts/fork/dev-setup.sh [--check] [--skip-ci]"
  fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --check)
      check_only=1
      skip_ci=1
      ;;
    --skip-ci)
      skip_ci=1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "dev-setup: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

# Everything below assumes a real herdr checkout: `rustc` is the rustup shim,
# so `rust-toolchain.toml` only selects the pinned toolchain when the working
# directory is inside the checkout. Running from repo_root also makes the run
# identical from the root checkout, from a worktree, and from anywhere else.
if [ ! -f "$repo_root/justfile" ] || [ ! -f "$repo_root/rust-toolchain.toml" ]; then
  echo "dev-setup: $repo_root does not look like a herdr checkout (no justfile / rust-toolchain.toml)" >&2
  echo "dev-setup: run it by path, e.g. 'bash scripts/fork/dev-setup.sh' from the checkout" >&2
  exit 2
fi
cd "$repo_root"

missing=0

step() {
  printf '\n[%s/6] %s\n' "$1" "$2"
}

report() {
  # report <label> <state> [detail]
  printf '  %-16s %-10s %s\n' "$1" "$2" "${3:-}"
  if [ "$2" = "missing" ]; then
    missing=$((missing + 1))
  fi
  return 0
}

note() {
  printf '  %-16s %-10s %s\n' "" "" "$1"
}

first_line() {
  # first_line <text> -- no pipe, so no SIGPIPE/pipefail surprises
  printf '%s' "${1%%$'\n'*}"
}

# Emits one `spec<TAB>ok|missing<TAB>resolved-version` line per requested
# `tool@version` spec, from the JSON `mise ls` prints. `@latest` is satisfied by
# any installed, active version so a deliberate local pin is never clobbered.
mise_tool_status() {
  local ls_json="$1"
  shift
  MISE_LS_JSON="$ls_json" python3 - "$@" <<'PY'
import json
import os
import sys

try:
    data = json.loads(os.environ.get("MISE_LS_JSON") or "{}")
except ValueError:
    data = {}
if not isinstance(data, dict):
    data = {}


def resolve(tool, want):
    """Return the installed, active version satisfying `want`, else None."""
    for entry in data.get(tool) or []:
        if not isinstance(entry, dict) or not entry.get("installed"):
            continue
        if entry.get("active") is False:
            continue
        version = entry.get("version")
        if want == "latest":
            return version or "latest"
        if want in (version, entry.get("requested_version")):
            return version or want
    return None


for spec in sys.argv[1:]:
    tool, _, want = spec.partition("@")
    found = resolve(tool, want or "latest")
    print("%s\t%s\t%s" % (spec, "ok" if found else "missing", found or ""))
PY
}

# Prints the `ok`/`installed`/`missing` line for every spec in a status block.
# $1 = status text (mise_tool_status output), $2 = newline-separated specs this
# run just asked mise to install.
report_mise_status() {
  local status_text="$1"
  # Newline-delimited on both sides so a spec matches only as a whole line.
  local installed_specs=$'\n'"$2"$'\n'
  local spec state version tool
  while IFS=$'\t' read -r spec state version; do
    [ -n "$spec" ] || continue
    tool="${spec%@*}"
    if [ "$state" != "ok" ]; then
      report "$tool" missing "want ${spec#*@}"
    elif [[ "$installed_specs" == *$'\n'"$spec"$'\n'* ]]; then
      report "$tool" installed "${version:-${spec#*@}}"
    else
      report "$tool" ok "${version:-${spec#*@}}"
    fi
  done <<< "$status_text"
}

if [ "$check_only" -eq 1 ]; then
  mode="check (no installs)"
elif [ "$skip_ci" -eq 1 ]; then
  mode="install (gate skipped)"
else
  mode="install"
fi

echo "herdr fork dev setup"
echo "  repo: $repo_root"
echo "  mode: $mode"

# ---------------------------------------------------------------- 1. prereqs
step 1 "prerequisites (system packages; verified, never installed)"

if command -v mise > /dev/null 2>&1; then
  report mise ok "$(first_line "$(mise --version 2> /dev/null || true)")"
else
  report mise missing "not on PATH"
  note "$MISE_INSTALL_HINT"
fi

if ! command -v python3 > /dev/null 2>&1; then
  report python3 missing "not on PATH (needed by just ci and this script)"
elif ! python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)' > /dev/null 2>&1; then
  report python3 missing "$(first_line "$(python3 --version 2>&1 || true)") (need >= 3.10)"
else
  report python3 ok "$(first_line "$(python3 --version 2>&1 || true)")"
fi

if command -v flock > /dev/null 2>&1; then
  report flock ok "$(command -v flock)"
else
  report flock missing "not on PATH (scripts/fork/gate.sh needs it; install util-linux)"
fi

if [ "$missing" -gt 0 ]; then
  echo
  echo "dev-setup: $missing prerequisite(s) missing; install them and re-run" >&2
  exit 1
fi

# ------------------------------------------------------------ 2. mise pins
# The Rust version comes from rust-toolchain.toml so this script cannot drift
# from the toolchain the repo actually builds with.
rust_version="$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$repo_root/rust-toolchain.toml" 2> /dev/null | head -n 1 || true)"
if [ -z "$rust_version" ]; then
  rust_version="$RUST_VERSION_FALLBACK"
fi
# Only an exact version pin can be matched against `rustc --version`; a named
# channel ("stable", "nightly") just has to resolve to something.
case "$rust_version" in
  [0-9]*) rust_pin_is_version=1 ;;
  *) rust_pin_is_version=0 ;;
esac

step 2 "toolchain pins via mise (rust $rust_version, zig $ZIG_VERSION, just, bun, shellcheck)"

mise_specs=("rust@$rust_version" "zig@$ZIG_VERSION" "just@latest" "bun@latest" "shellcheck@latest")
mise_ls_json="$(mise ls --json 2> /dev/null || true)"
if ! mise_status="$(mise_tool_status "$mise_ls_json" "${mise_specs[@]}")"; then
  echo "dev-setup: could not evaluate mise tool state (python3 failed)" >&2
  exit 1
fi
pending="$(printf '%s\n' "$mise_status" | awk -F '\t' '$2 != "ok" { print $1 }')"

if [ -z "$pending" ]; then
  report_mise_status "$mise_status" ""
elif [ "$check_only" -eq 1 ]; then
  report_mise_status "$mise_status" ""
  note "run without --check to pin them: mise use -g $(printf '%s' "$pending" | tr '\n' ' ')"
else
  # Only the pending specs are passed to `mise use -g`, so an already-set-up
  # machine never has its global mise config rewritten.
  echo "  running: mise use -g $(printf '%s' "$pending" | tr '\n' ' ')"
  # shellcheck disable=SC2086 # deliberate word splitting: one spec per argument
  if ! mise use -g $pending; then
    echo "dev-setup: 'mise use -g' failed for: $(printf '%s' "$pending" | tr '\n' ' ')" >&2
    exit 1
  fi
  mise_ls_json="$(mise ls --json 2> /dev/null || true)"
  if ! mise_status="$(mise_tool_status "$mise_ls_json" "${mise_specs[@]}")"; then
    echo "dev-setup: could not evaluate mise tool state (python3 failed)" >&2
    exit 1
  fi
  report_mise_status "$mise_status" "$pending"
fi

# rust-toolchain.toml drives rustup/mise to the pinned toolchain plus its
# components; verify what the build will actually use (this runs in repo_root,
# so the rustup shim resolves the checkout's own pin).
rustc_bin="$(command -v rustc 2> /dev/null || true)"
rustc_version=""
if [ -n "$rustc_bin" ]; then
  rustc_version="$(first_line "$(rustc --version 2>&1 || true)")"
fi

if [ -z "$rustc_bin" ]; then
  report rustc missing "want $rust_version, not on PATH"
  note "if mise just installed it, restart the shell (or run 'mise activate') and re-run"
elif [ "$rust_pin_is_version" -eq 1 ] && [[ "$rustc_version" != *"$rust_version"* ]]; then
  report rustc missing "want $rust_version, found: ${rustc_version:-no output}"
  note "if mise just installed it, restart the shell (or run 'mise activate') and re-run"
elif [ -z "$rustc_version" ]; then
  report rustc missing "want $rust_version, 'rustc --version' printed nothing"
else
  report rustc ok "$rustc_version"
fi

if command -v cargo > /dev/null 2>&1 && cargo fmt --version > /dev/null 2>&1; then
  report "cargo fmt" ok "$(first_line "$(cargo fmt --version 2>&1 || true)")"
else
  report "cargo fmt" missing "rustfmt component unavailable (see rust-toolchain.toml)"
fi

if command -v cargo > /dev/null 2>&1 && cargo clippy --version > /dev/null 2>&1; then
  report "cargo clippy" ok "$(first_line "$(cargo clippy --version 2>&1 || true)")"
else
  report "cargo clippy" missing "clippy component unavailable (see rust-toolchain.toml)"
fi

# ------------------------------------------------------------ 3. nextest
step 3 "cargo-nextest (the gate's test runner)"

if ! command -v cargo > /dev/null 2>&1; then
  report cargo-nextest missing "cargo is not on PATH"
elif cargo nextest --version > /dev/null 2>&1; then
  report cargo-nextest ok "$(first_line "$(cargo nextest --version 2>&1 || true)")"
elif [ "$check_only" -eq 1 ]; then
  report cargo-nextest missing "install with: cargo install cargo-nextest --locked"
else
  echo "  running: cargo install cargo-nextest --locked"
  if cargo install cargo-nextest --locked && cargo nextest --version > /dev/null 2>&1; then
    report cargo-nextest installed "$(first_line "$(cargo nextest --version 2>&1 || true)")"
  else
    report cargo-nextest missing "cargo install cargo-nextest --locked failed"
  fi
fi

# ------------------------------------------------------------ 4. zig
step 4 "zig $ZIG_VERSION (vendored libghostty-vt)"

zig_bin="$(command -v zig 2> /dev/null || true)"
zig_found=""
if [ -n "$zig_bin" ]; then
  zig_found="$(first_line "$(zig version 2>&1 || true)")"
fi

if [ -n "$zig_bin" ] && [ "$zig_found" = "$ZIG_VERSION" ]; then
  report zig ok "$zig_found ($zig_bin)"
else
  report zig missing "want $ZIG_VERSION, found: ${zig_found:-not on PATH}"
  note "build.rs honours \$ZIG: export ZIG=/path/to/zig-$ZIG_VERSION/zig"
fi

# ------------------------------------------------------------ 5. git hooks
step 5 "git hooks (.githooks: conventional commit subjects + pre-commit lint)"

hooks_path="$(git -C "$repo_root" config core.hooksPath 2> /dev/null || true)"
if [ "$hooks_path" = ".githooks" ]; then
  report core.hooksPath ok ".githooks"
elif [ "$check_only" -eq 1 ]; then
  report core.hooksPath missing "${hooks_path:-unset} (install with: just install-hooks)"
elif ! command -v just > /dev/null 2>&1; then
  report core.hooksPath missing "just is not on PATH"
else
  echo "  running: just install-hooks"
  if just install-hooks; then
    hooks_path="$(git -C "$repo_root" config core.hooksPath 2> /dev/null || true)"
    if [ "$hooks_path" = ".githooks" ]; then
      report core.hooksPath installed ".githooks"
    else
      report core.hooksPath missing "${hooks_path:-unset} after just install-hooks"
    fi
  else
    report core.hooksPath missing "just install-hooks failed"
  fi
fi

if [ "$missing" -gt 0 ]; then
  echo
  echo "dev-setup: $missing item(s) still missing; fix them and re-run" >&2
  exit 1
fi

echo
echo "dev-setup: steps 1-5 ok"

# ------------------------------------------------------------ 6. the gate
if [ "$skip_ci" -eq 1 ]; then
  if [ "$check_only" -eq 1 ]; then
    echo "dev-setup: --check passed (no gate run)"
  else
    echo "dev-setup: --skip-ci given; run the gate with: bash scripts/fork/gate.sh"
  fi
  exit 0
fi

step 6 "the fork gate (just ci under the machine-wide lock)"
gate="$script_dir/gate.sh"
if [ ! -f "$gate" ]; then
  echo "dev-setup: $gate not found; cannot run the gate" >&2
  exit 1
fi
gate_code=0
bash "$gate" "$repo_root" || gate_code=$?
exit "$gate_code"
