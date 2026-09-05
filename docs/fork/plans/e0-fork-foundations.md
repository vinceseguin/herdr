# Epic E0 — Fork foundations, CI, fleet lab

## Context

**Goal (roadmap):** a fork that builds and tests on this machine and in its own
CI, never fights upstream's release machinery, and has the local "fake LAN"
every later epic validates against. **Why:** nothing can be gated until
`just ci` runs here, and upstream's release/preview/website workflows must not
fire on the fork. The exit state of E0 is a green `just ci` on `master`.

Scope contract = the roadmap's E0 deliverables: `scripts/fork/dev-setup.sh`,
`.github/workflows/fork-ci.yml` + upstream workflows disabled through `gh`,
fork build identity (`HERDR_BUILD_CHANNEL=fork`, self-update disabled),
`scripts/fork/fleet-lab.sh`, `scripts/fork/gate.sh` verified against a real
`just ci`, `docs/fork/README.md`, and a review of ADR 0001. E0 has no upstream
epic dependency.

### Real current state (verified on `master` @ `93558f08`, herdr 0.8.2)

- **Toolchain is already installed** and matches the pins: Rust 1.96.1 with
  clippy + rustfmt (`rust-toolchain.toml`), just 1.58.0, cargo-nextest 0.9.143
  (`~/.cargo/bin`), Zig 0.15.2, bun 1.4.1, python3 3.14.7, gh 2.99.0, all via
  mise 2026.8.15 (`~/.config/mise/config.toml` pins `rust = "1.96.1"`,
  `zig = "0.15.2"`, `just`/`bun`/`gh` = latest). `flock`, `setsid`, `nohup` are
  present (util-linux/coreutils). `git config core.hooksPath` is **unset**, so
  `.githooks/commit-msg` (conventional-subject check) and `.githooks/pre-commit`
  (`just lint`) are not active yet; `just install-hooks` installs them.
- **Machine:** 8 cores, 15 GiB RAM. Waves are capped at 2 agents; each
  worktree has its own `target/`, so two cold builds at once is the memory
  hazard the gate lock exists for.
- **`justfile`:** `just ci` (unix-only) = `lint` (`cargo fmt --check`,
  `cargo clippy --all-targets --locked -- -D warnings`) → `cargo nextest run
  --locked -E "all()"` → `maintenance-test` (10 python unittest modules) →
  `ui-hot-path-architecture-test` → `integration-assets-test` (3 bun suites) →
  `plugin-marketplace-test` (`bun install --frozen-lockfile` — needs network).
  `just ci` does **not** run `docs-contract-test`. No recipe sets
  `HERDR_BUILD_CHANNEL`.
- **`scripts/fork/gate.sh` exists and is final**: `flock` on
  `${TMPDIR:-/tmp}/herdr-fork-gates/gate.lock`, runs `just <recipe>` (default
  `ci`) in the given worktree, logs to that directory, prints `EXIT=<code>`
  last, and deliberately does not set `HERDR_BUILD_CHANNEL` (comment at
  lines 42–43). E0 verifies it; it does not rewrite it.
- **Workflows:** all 12 entries in `gh workflow list --all` are `active`
  (10 file-backed: Release, Build artifacts (manual), PR Gate, CI, Close
  pending-release issues, Nix, Preview, Windows ARM64 installer, Trigger
  Website Deploy, Distribution contract; 2 GitHub-managed dynamic entries:
  Dependabot Updates, Copilot). Only `pr-gate.yml`, `release.yml`, and
  `preview.yml` carry a `github.repository == 'herdrdev/herdr'` guard.
  `ci.yml` (3-OS matrix + Windows ConPTY package, `just ci` on ubuntu/macOS),
  `distribution.yml`, `nix.yml`, `website-deploy.yml` (hard-fails without the
  `HERDR_WEBSITE_DEPLOY_HOOK` secret), `windows-arm64.yml`,
  `label-next-release-issues.yml`, `build-artifacts-manual.yml` run unguarded
  on every PR / push to `master` until disabled. `ci.yml`'s
  `conventional-commits` job is the reference for the PR-title check:
  `python3 scripts/conventional_commits.py "$PR_TITLE"` (PR) and
  `--range "${before}..${after}"` (push). No `.github/actions/` exist.
- **Build identity:** `src/build_info.rs` reads `option_env!("HERDR_BUILD_CHANNEL")`
  (default `"stable"`); `version()` already renders `0.8.2-<channel>` or
  `0.8.2-<channel>.<HERDR_BUILD_ID>` for any non-stable channel — no code
  change needed for the string. `is_preview()` is the only predicate; there is
  no `BuildChannel` enum. `build.rs` only emits `rerun-if-env-changed` for
  `HERDR_BUILD_CHANNEL`/`HERDR_BUILD_ID`/`HERDR_BUILD_COMMIT`.
  `.cargo/config.toml` has only a Windows `rustflags` table — no `[env]`.
  `herdr --version` is hand-rolled at `src/main.rs:694-698`
  (`println!("herdr {}", crate::build_info::version())`).
- **Updater:** `src/update.rs` — `pub fn self_update(SelfUpdateOptions)`
  (`:2113`) refuses Homebrew/mise/Nix installs with `Err("self-update is
  disabled for … installs; run …")`; `src/main.rs:568` prints any error that
  starts with `"self-update is disabled"` verbatim (prefix is load-bearing).
  `pub fn auto_update(events)` (`:2245`) is the background check, spawned from
  `App::new` (`src/app/mod.rs:543`) and `run_auto_update_check`
  (`src/app/runtime.rs:97`), both gated by the pure helpers
  `auto_updates_enabled` / `background_update_check_enabled`
  (`src/app/mod.rs:163-169`; debug builds never run it). `herdr channel set`
  (`src/cli.rs:151`) prints package-manager guidance via
  `channel_set_install_action(package_manager_channel_update_guidance_for_current_install())`
  instead of calling `self_update`. `Version::parse` (`src/update.rs:73`)
  accepts bare `x.y.z` only; `Version::current()` uses `BASE_VERSION`.
- **Tests that assert the plain version string** (break under a non-stable
  channel; confirmed by commit `93558f08`): `tests/api_ping.rs:303`
  (`result.version == env!("CARGO_PKG_VERSION")`), `tests/cli/sessions.rs:388`
  (`"  version: {CARGO_PKG_VERSION}"` in `herdr status`), and the in-crate
  release-notes/announcement tests in `src/app/mod.rs` (`:1561`, `:1599`,
  `:1631-1634`) that save notes under `CARGO_PKG_VERSION` and expect
  `release_notes::mark_current_version_seen_at` (string equality against
  `build_info::version()`) to match. `remote/attach.rs` tests compare
  `current_channel()` with itself and are unaffected.
- **`--remote` compatibility is keyed on endpoint generation, not the version
  string** (`src/remote/attach.rs:1056`, `remote_server_restart_reason`), so a
  `0.8.2-fork` client attaches to a stock `0.8.2` server. A fork client can
  only seed a *foreign-platform* remote from the stable manifest by version
  lookup (`remote_release_asset`, `:1521`), which will not contain
  `0.8.2-fork`; same-platform seeding copies the local binary. Expected, and
  consistent with "servers stay stock".
- **Sessions and sockets:** `herdr --session <name> server` runs the server in
  the **foreground** (no daemonize flag; the bare `herdr` launch is what
  `setsid`s a daemon in `src/server/autodetect.rs`). A named session lives at
  `<config_dir>/sessions/<name>/` with `herdr.sock` (API), `herdr-client.sock`,
  `herdr-server.log`, `session.json`. `config_dir()` = `$XDG_CONFIG_HOME/<app>`
  where `<app>` is **`herdr-dev` for debug builds and `herdr` for release**
  (`src/config/io.rs:22`). Explicit `--session` beats `HERDR_SOCKET_PATH` /
  `HERDR_CLIENT_SOCKET_PATH`; `HERDR_ENV=1` is the nested-herdr guard. There is
  **no pid file** and no `herdr server status`; use `herdr session list --json`
  (`{"sessions":[{name,default,running,socket_path,session_dir}]}`) and
  `herdr status server --json`. `herdr session stop <name>` / `herdr server
  stop` send `server.stop` and poll both sockets for up to 15 s.
- **CLI for the lab:** `herdr workspace create [--cwd P] [--label T]` prints
  the JSON response (`result.root_pane.pane_id`); `herdr api snapshot` prints
  `{"id":…,"result":{version,protocol,workspaces,tabs,panes[{pane_id,…}],
  layouts,agents}}`; `herdr pane run <pane_id> <cmd…>` types the command +
  Enter (`pane.send_input`); `herdr pane wait-output <pane_id> --match T
  --timeout MS`; `herdr pane read <pane_id> --source recent`; `herdr agent
  read`, `herdr terminal session observe <target>`. There is no generic
  `herdr api <method>`; raw calls are newline-delimited JSON on `herdr.sock`.
- **Test harness:** `tests/support/mod.rs` is pid/runtime-dir hygiene only
  (`register_spawned_herdr_pid`, `register_runtime_dir`, `cleanup_test_base`,
  `wait_for_socket`, `terminate_pid` = SIGTERM → 400 ms → SIGKILL, a watchdog
  that attributes servers through `XDG_RUNTIME_DIR=` in `/proc/<pid>/environ`
  and requires the exe to be this checkout's `target/debug/herdr`). The named
  -session spawner is `tests/cli/harness.rs::spawn_named_server`
  (`herdr --session <n> server` with `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`,
  `HERDR_SOCKET_PATH`/`HERDR_CLIENT_SOCKET_PATH`/`HERDR_ENV` removed, stdio to
  `/dev/null`). `tests/multi_client.rs` uses `portable_pty` only to hold a
  client process; assertions go through the JSON API.
- **Repo hygiene already in place:** `.gitignore` ignores `/.claude/worktrees/`
  and un-ignores `/docs/fork/**` (verified; no change needed). `origin` =
  `vinceseguin/herdr`, `upstream` = `herdrdev/herdr`, upstream drift 0, no
  branch protection on `master`, `gh` logged in as `vinceseguin`.
  `docs/fork/README.md`, `ROADMAP.md`, `decisions/0001-…md` exist;
  `docs/fork/plans/` is empty.

### Locked decisions

- **(a) CI shape — `fork-ci.yml`, ubuntu-only, upstream workflows disabled
  through `gh workflow disable`** (roadmap default). Repository state, not
  file edits, so upstream merges never re-enable or conflict. Fork CI runs
  exactly the gate (`just ci`) plus the PR-title / push-range conventional
  commit check copied from `ci.yml`, and `shellcheck scripts/fork/*.sh`.
- **(b) Update guard — self-update disabled on channel `fork` now** (roadmap
  default); E8 repoints it to a fork manifest. Both `herdr update` and the
  background check refuse with a message pointing at `docs/fork/README.md`;
  the agent-detection manifest auto-update stays enabled *(auto default —
  detection manifests are stock data the fork benefits from)*.
- **(c) Toolchain manager — mise** (roadmap default; it is what already
  provides every tool here). `dev-setup.sh` pins through `mise use -g` exactly
  as `docs/fork/README.md` documents today; no root `mise.toml` is added
  *(auto default — avoids a new root file and matches the machine)*.
- **(d) Where the channel is set — `.cargo/config.toml` `[env]`
  `HERDR_BUILD_CHANNEL = "fork"`** *(auto default)*: applies uniformly to
  `cargo build/run/test/nextest` and CI without editing the upstream `justfile`;
  a real environment variable still overrides it (no `force`), so E8's release
  workflow can add `HERDR_BUILD_ID` and CI can pin explicitly.
- **(e) Version string — keep upstream's shape `0.8.2-fork`
  (`0.8.2-fork.<build_id>` when `HERDR_BUILD_ID` is set)** *(auto default)*.
  The roadmap's E8 mention of `+fork.<n>` is E8's call; changing
  `build_info::version()` now would touch handoff/status comparisons for no E0
  benefit.
- **(f) Version-asserting tests become channel-aware** *(auto default)*: one
  additive helper `support::build_version()` in `tests/support/mod.rs` that
  mirrors `build_info::version()` with `option_env!`, used by
  `tests/api_ping.rs` and `tests/cli/sessions.rs`; in-crate tests use
  `crate::build_info::version()` only where the code under test compares
  against it. No test is weakened or skipped.
- **(g) Fleet-lab test lives in `tests/fork_fleet_lab.rs`** *(auto default)*:
  nextest discovers it, so it runs inside `just ci` with no `justfile` edit.
  Unix-only, uses `env!("CARGO_BIN_EXE_herdr")` through the script's
  `HERDR_BIN` knob.
- **(h) Fleet-lab layout** *(auto default)*: sessions `lab-1 … lab-N`; root
  `${HERDR_FLEET_LAB_ROOT:-/tmp/herdr-fleet-lab}` containing `xdg/`
  (`XDG_CONFIG_HOME`), `runtime/` (`XDG_RUNTIME_DIR`, for `tests/support`
  attribution), `logs/lab-N.log`, `pids/lab-N.pid`, `work/lab-N/` (workspace
  cwd), and a `.herdr-fleet-lab` marker file that `down` requires before it
  deletes anything.

### Sequencing hazards

- **PR 1 must land alone and first.** Until the upstream workflows are
  disabled, every PR triggers `ci.yml`'s 3-OS matrix and the Windows package
  job (15–25 min, may fail for fork-unrelated reasons). PR 1 disables them
  *before* pushing its branch, so its own PR already sees fork CI only.
- **PR 3 changes the compile environment of every binary** (`.cargo/config.toml`
  `[env]` + `build.rs` rerun trigger). Any worktree rebased over it rebuilds the
  herdr crate and libghostty-vt once. It edits `tests/support/mod.rs`
  (additive helper only); PR 4 reads that module but does not edit it.
- **Upstream files touched, and by which PR only:** `.cargo/config.toml`,
  `src/build_info.rs`, `src/update.rs`, `src/main.rs` (help text only),
  `src/cli.rs`, `src/release_notes.rs`, `src/app/mod.rs` (tests),
  `tests/api_ping.rs`, `tests/cli/sessions.rs`, `tests/support/mod.rs` — PR 3.
  (`src/release_notes.rs` was not foreseen; the channel suffix exposed a real
  bug there — see PR 3's *As built*.) `.github/workflows/`
  gains one new file (PR 1). Nothing else upstream is edited; `justfile`,
  `Cargo.toml`, `Cargo.lock`, `src/protocol/**` are untouched by E0.
- **Fork docs collide by design:** `docs/fork/README.md` is edited only by
  PR 5; PRs 1–4 must not touch it (they may leave TODO notes in their PR
  body for PR 5).
- **Git hooks:** `dev-setup.sh` (PR 2) installs `.githooks`, whose `pre-commit`
  runs `just lint` (bare `cargo clippy`, outside the gate lock). Commit only
  after a gate on the same tree so clippy is a cache hit; never commit while
  another agent's gate is mid-build.

## Status legend

✅ merged · 🔨 in progress · ⬜ not started · ⛔ blocked

## PR map

| # | Title | Group | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | ci: add fork ci workflow and disable upstream workflows | A · Repository | — | ✅ |
| 2 | chore: add mise-based fork dev setup script and verify the gate | A · Repository | 1 | ✅ |
| 3 | feat: fork build channel disables self-update and shows in version | B · Identity | 1 | ✅ |
| 4 | feat: fleet lab script boots isolated named herdr sessions | C · Fleet lab | 1 | ✅ |
| 5 | docs: fork readme for dev setup, ci, fleet lab; review adr 0001 | D · Docs | 2, 3, 4 | ⬜ |

**Wave preview (2-agent cap):** W1 `[1]` → W2 `[2, 3]` → W3 `[4]` → W4 `[5]`.
PR 4 only needs PR 1, so a wider cap could run it in W2; under the cap it
follows numeric order. No PR touches `Cargo.toml`/`Cargo.lock`.

**Model assignment:** tasks run on `opus`. Review agent must be **`fable`** for
**PR 3** (a wrong guard lets upstream's `latest.json` replace a fork binary)
and **PR 4** (the script kills processes and `rm -rf`s a root; it must never be
able to reach the user's real herdr config or session).

## Verification (the gate — every PR)

```bash
bash scripts/fork/gate.sh <worktree>        # runs `just ci` under the machine-wide lock
echo "EXIT=$?"                              # read the EXIT= line; never pipe the wrapper
```

`just ci` = `cargo fmt --check` + `cargo clippy --all-targets --locked -D
warnings` + `cargo nextest run --locked` + python maintenance tests + bun
suites (`integration-assets-test`, `plugin-marketplace-test`). While iterating
use `bash scripts/fork/gate.sh <worktree> "test-one <filter>"`.

Real-server validation is mandatory for PRs 3 and 4 and is written per PR
below; PRs 1, 2 and 5 have no runtime code and substitute the evidence named
in their sections (CI run URLs, `gh workflow list --all` output, a scripted
walk-through of the documented commands). Always isolate:
`XDG_CONFIG_HOME=/tmp/herdr-e0-<task>` and
`env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV`.

CI gotchas to expect:

- libghostty-vt needs **Zig 0.15.2** on the runner (`mlugg/setup-zig`), plus
  `just`, `cargo-nextest` (`taiki-e/install-action`), bun, python3.
- `plugin-marketplace-test` runs `bun install --frozen-lockfile` (network).
- nextest boots real servers (upstream already does this on `ubuntu-latest`);
  PR 4's test adds two more servers per run. Give the job `timeout-minutes: 30`.
- A new workflow file on a PR branch **does** run for that PR's
  `pull_request` event; `push` runs only on `master`.
- After PR 3, every worktree's first build reruns `build.rs` (Zig) once.
- Scripts must be `shellcheck`-clean (`shellcheck scripts/fork/*.sh` is a
  fork-CI step; `dev-setup.sh` installs shellcheck locally through mise).
- Do not depend on `setsid` in scripts (absent on macOS); use
  `nohup … </dev/null >log 2>&1 &`.

## Cross-cutting constraints (all PRs)

- **Additive, minimal wiring.** New files under `scripts/fork/`,
  `.github/workflows/fork-*.yml`, `docs/fork/`, `tests/fork_*.rs`. Upstream
  edits are limited to the list in *Sequencing hazards*; no reshaping.
- **No wire or endpoint-contract change.** `src/protocol/wire.rs`,
  `src/protocol/endpoint.rs`, `tests/fixtures/endpoint-*.json` stay untouched.
  `PROTOCOL_VERSION` (22) is not bumped.
- **Servers stay stock.** Nothing in E0 changes server behaviour; the fleet lab
  runs the unmodified `herdr … server` path.
- **Code conventions.** No `unwrap()` in production code (tests may);
  `tracing` for logs (use `crate::logging::update_check_failed`-style helpers
  that already exist); `#[allow]` only with a reason; `#[cfg(unix)]` on the
  fleet-lab test and any unix-only helper; no new crate dependency (E0 needs
  none).
- **Pure, testable guards.** Channel checks are pure functions taking the
  channel string (precedent: `default_update_channel_for_build(is_windows,
  is_preview)` in `src/config/model.rs:53`) so they get unit tests without
  network, config or `current_exe()`.
- **Scripts:** `#!/usr/bin/env bash`, `set -euo pipefail`, shellcheck-clean,
  idempotent, never touch `~/.config/herdr`, refuse to delete anything outside
  their own root, print machine-readable output where a later epic will parse
  it (`fleet-lab.sh status --json`, `env`).
- **Never** run `herdr server stop`, `herdr update`, or `herdr channel set`
  against the user's live session; every validation uses a throwaway
  `XDG_CONFIG_HOME` and named sessions.
- **Docs discipline.** Do not edit `docs/next/**`, root `README.md`,
  `CHANGELOG.md`, `skills/herdr/SKILL.md`, `distribution/**`. Fork docs go in
  `docs/fork/`. No `refs #<n>` lines. Commit trailers as the environment
  specifies.
- **Branches:** `ci/e0-pr1-fork-ci`, `chore/e0-pr2-dev-setup`,
  `feat/e0-pr3-fork-channel`, `feat/e0-pr4-fleet-lab`, `docs/e0-pr5-readme`,
  each from the latest `origin/master`, in `.claude/worktrees/<slug>`, all git
  through `git -C`.

## Per-PR detail

### PR 1 — ci: add fork ci workflow and disable upstream workflows · deps: —

**Goal:** the fork has its own CI that runs the gate, and upstream's workflows
never fire on this repository again — by repository state, so upstream merges
cannot re-enable them.

**Files**

- `.github/workflows/fork-ci.yml` (new).
- No other file. (`.gitignore` already ignores `/.claude/worktrees/` — verify
  with `git check-ignore -v .claude/worktrees/x`; do not add a duplicate line.)

**Shapes/approach**

- `name: Fork CI`; `on: push: branches: [master]` and `pull_request: types:
  [opened, synchronize, reopened]`; `permissions: contents: read`;
  `concurrency: group: fork-ci-${{ github.event.pull_request.number ||
  github.ref }}`, `cancel-in-progress: true`; `env: RUST_TOOLCHAIN_VERSION:
  1.96.1`.
- Job `conventional-commits` (ubuntu-latest, 5 min): copy the two steps from
  `ci.yml` verbatim — `python3 scripts/conventional_commits.py --range
  "${{ github.event.before }}..${{ github.event.after }}"` on push,
  `python3 scripts/conventional_commits.py "$PR_TITLE"` (title via `env:`) on
  PR; `fetch-depth: 0`, `persist-credentials: false`.
- Job `check` (ubuntu-latest, `timeout-minutes: 30`): `actions/checkout`,
  `dtolnay/rust-toolchain` @ `${{ env.RUST_TOOLCHAIN_VERSION }}` with
  `components: rustfmt,clippy`, `taiki-e/install-action` `tool:
  just,cargo-nextest`, `oven-sh/setup-bun` `bun-version: 1.3.14` (upstream's
  pin), `mlugg/setup-zig` `version: 0.15.2`, `Swatinem/rust-cache`
  (`cache-bin: false`, `key: fork-ubuntu-latest`), then `run: just ci`. Pin
  the same action SHAs `ci.yml` uses so Dependabot-style bumps upstream stay
  mergeable.
- Job `shellcheck` (ubuntu-latest, 5 min): `shellcheck -S warning
  scripts/fork/*.sh` (shellcheck is preinstalled on `ubuntu-latest`).
- Disable upstream workflows **before pushing** (repository state):
  `for w in ci.yml pr-gate.yml release.yml preview.yml distribution.yml
  nix.yml website-deploy.yml windows-arm64.yml label-next-release-issues.yml
  build-artifacts-manual.yml; do gh workflow disable "$w" --repo
  vinceseguin/herdr; done`. Attempt the two dynamic entries by name
  (`"Dependabot Updates"`, `"Copilot"`); if `gh` refuses because they are not
  file-backed, record that in the PR body — they are not triggered by pushes
  (Dependabot version updates are off on forks unless enabled in settings) and
  `.github/dependabot.yml` stays as upstream ships it.
- Verify: `gh workflow list --all --repo vinceseguin/herdr` shows
  `disabled_manually` for all ten file-backed workflows and `active` only for
  `Fork CI` (after merge) and any dynamic entry that could not be disabled.
  Paste that output into the PR body.
- **Confirm Actions actually run on the fork.** As of the plan PR
  (`vinceseguin/herdr#1`) the fork has never executed a workflow:
  `gh run list -R vinceseguin/herdr` is empty and the PR's
  `statusCheckRollup` was `[]`, although `actions/permissions` reports
  `enabled: true`. GitHub forks need a one-time "enable workflows" in the
  repository's Actions tab. After disabling the upstream workflows and pushing
  this PR, check `gh run list -R vinceseguin/herdr --branch <branch>`; if no
  `Fork CI` run appears within two minutes, enable Actions in the fork's
  Actions tab (or via `gh api -X PUT repos/vinceseguin/herdr/actions/permissions
  -f enabled=true -f allowed_actions=all` and re-push), then re-verify. PR 1 is
  not done until a `Fork CI` run exists for its head and is green. Always pass
  `-R vinceseguin/herdr` to `gh`.

**Tests**

- The workflow's own first run on the PR is the test: `conventional-commits`,
  `check`, `shellcheck` all green. Locally: `bash scripts/fork/gate.sh
  <worktree>` → `EXIT=0` (no Rust change; proves the tree is green at the
  start of E0).
- Negative check for the title job, locally (no throwaway PRs): `python3
  scripts/conventional_commits.py "bad title"` exits 1; `python3
  scripts/conventional_commits.py "ci: add fork ci workflow and disable
  upstream workflows"` exits 0.

**Real-server validation:** none (no runtime code). Evidence: the fork-CI run
URL for the PR head with all three jobs green; `gh workflow list --all`
output; `gh run list --repo vinceseguin/herdr --workflow=ci.yml --limit 3`
showing no run for the PR head (upstream CI did not fire).

**Outcome (as merged)**

- `.github/workflows/fork-ci.yml` landed with three jobs whose check names are
  stable for `gh pr checks`: `conventional-commits`, `check (ubuntu-latest)`,
  `shellcheck`. Action SHAs are identical to `ci.yml`'s ubuntu path
  (checkout v6, `dtolnay/rust-toolchain` v1, `taiki-e/install-action` v2.81.9,
  `oven-sh/setup-bun` v2 @ bun 1.3.14, `mlugg/setup-zig` v2.2.1 @ 0.15.2,
  `Swatinem/rust-cache` v2 with `cache-bin: false`, `key: fork-ubuntu-latest`).
  macOS/Windows-only steps (Homebrew Zig, `.zig-cache` restore, cmake/ninja,
  ConPTY) are intentionally absent.
- **Two deliberate divergences from `ci.yml`**, both commented in the file:
  1. The push-event commit check does **not** use
     `conventional_commits.py --range before..after`. `.claude/rules/fork.md`
     mandates `git merge upstream/master` pushed to fork `master`, and
     `origin/master` already carries non-conventional upstream subjects
     (`Merge pull request #25 from …`, `Update rose pine surface_dim colour …`),
     so a plain range would leave `master` permanently red on every sync. The
     step walks `git log --no-merges --first-parent "$BEFORE..$AFTER"` and
     passes those subjects positionally, guarded by
     `git rev-parse --verify --quiet` for a zero/unreachable `before`
     (branch creation, force push). Trade-off: a non-squash merge of a *fork*
     feature branch would skip its individual commits — fork policy is
     squash-merge only, and the PR-title check covers those.
  2. `cancel-in-progress: ${{ github.event_name == 'pull_request' }}` — PR runs
     supersede, but back-to-back squash merges must not cancel a `master` run,
     because the epic validation reads that green record.
- **Upstream workflows are disabled by repository state**, done *before* the
  branch was pushed. All ten file-backed workflows report `disabled_manually`
  in `gh workflow list --all -R vinceseguin/herdr`; `gh run list
  --workflow=ci.yml` shows no run for this PR's head. The two dynamic entries
  named in the plan (`Dependabot Updates`, `Copilot`) are **not present at all**
  on this fork — `gh workflow disable "Dependabot Updates"` and `… "Copilot"`
  both answer `could not find any workflows named …`. Nothing to do; they are
  not push-triggered and `.github/dependabot.yml` stays as upstream ships it.
- `shellcheck` was **not** on this machine's PATH; it was installed with
  `mise use -g shellcheck@latest` (0.11.0) to run the job's command locally.
  PR 2's `dev-setup.sh` must keep pinning it (already in decision (c)).

**Downstream**

- Every later PR gets only `Fork CI`; `gh pr checks --watch --fail-fast` must
  see `conventional-commits`, `check (ubuntu-latest)`… names from this file.
- `just ci` remains the single definition of "green"; fork CI never adds a
  check that the local gate cannot run (shellcheck being the one extra, which
  PR 2 installs locally).
- Upstream syncs must never re-enable workflows: after `git merge
  upstream/master`, re-run `gh workflow list --all` and disable any *new*
  upstream workflow file.

### PR 2 — chore: add mise-based fork dev setup script and verify the gate · deps: 1

**Goal:** a new machine (or this one, idempotently) reaches a green `just ci`
with one command, git hooks are installed, and `scripts/fork/gate.sh` is
proven against a real `just ci`, including its lock.

**Files**

- `scripts/fork/dev-setup.sh` (new, executable).
- `scripts/fork/gate.sh` — **no change** (verified only).

**Shapes/approach**

- `dev-setup.sh [--check] [--skip-ci]`. Steps, each idempotent and printed as
  `ok`/`installed`/`missing`:
  1. Require `mise` on `PATH` (print the install one-liner and exit 1 if
     absent; never curl-pipe automatically). Require `python3 >= 3.10` and
     `flock` (verify only; system packages).
  2. Pin tools globally exactly as `docs/fork/README.md` documents:
     `mise use -g rust@1.96.1 zig@0.15.2 just@latest bun@latest
     shellcheck@latest` (skip any tool already at the pinned version — read
     `mise ls --json`). `rust-toolchain.toml` then drives rustup to
     1.96.1 + clippy + rustfmt; verify `cargo fmt --version`, `cargo clippy
     --version`, `rustc --version | grep 1.96.1`.
  3. `cargo nextest --version` or `cargo install cargo-nextest --locked`.
  4. `zig version` must print `0.15.2` (build.rs has no version check, so this
     script is the guard); print the `ZIG=` hint from `build.rs` if wrong.
  5. `just install-hooks` (sets `core.hooksPath .githooks` for this repo and
     all its worktrees).
  6. Unless `--skip-ci`: `bash scripts/fork/gate.sh "$repo_root"` and exit with
     its code; the last line of the script's output is the gate's `EXIT=`.
  `--check` runs steps 1–5 in verify-only mode (no installs) and exits non-zero
  on any `missing`.
- Gate verification (recorded in the PR body, not code): run
  `bash scripts/fork/gate.sh <worktree>` → `EXIT=0` with the log path and
  duration; then run two gates concurrently
  (`bash scripts/fork/gate.sh <worktree> lint & bash scripts/fork/gate.sh
  <worktree> lint; wait`) and show the second printing
  `gate: waiting for gate lock (another gate is running)…` before its own
  `EXIT=0`; then `bash scripts/fork/gate.sh /nonexistent` → `EXIT=2`.

**Tests**

- `shellcheck -S warning scripts/fork/dev-setup.sh` clean.
- `bash scripts/fork/dev-setup.sh --check` → exit 0 on this machine.
- Idempotence: run `bash scripts/fork/dev-setup.sh --skip-ci` twice; second
  run reports every tool `ok` and makes no change to
  `~/.config/mise/config.toml` (diff before/after).
- No Rust tests (no Rust change); the gate still runs in full.

**Real-server validation:** after `dev-setup.sh` ends in `EXIT=0`, prove the
built binary runs as a server under isolation:
`XDG_CONFIG_HOME=/tmp/herdr-e0-setup env -u HERDR_SOCKET_PATH -u
HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV nohup target/debug/herdr --session
setup server </dev/null >/tmp/herdr-e0-setup.log 2>&1 &`, then
`… herdr --session setup status server --json` shows `running: true` and
`… herdr session list --json` lists `setup`; `… herdr session stop setup`;
`rm -rf /tmp/herdr-e0-setup`.

**Outcome (as merged)**

- `scripts/fork/dev-setup.sh` (new, executable, `shellcheck -S warning` clean)
  implements the six steps in order. Each line prints `ok` (already satisfied),
  `installed` (this run changed something) or `missing`; the script exits 1 if
  anything is still `missing` after step 5, so the gate never runs on a
  half-set-up machine. `--check` implies `--skip-ci` (verify only, installs
  nothing), `--skip-ci` runs steps 1–5, `-h/--help` prints the header comment
  block, an unknown flag exits 2.
- **Two deliberate refinements over the plan text:**
  1. The Rust pin is read from `rust-toolchain.toml` (`channel = "1.96.1"`,
     falling back to `1.96.1`) instead of being hardcoded, so the script cannot
     drift from the toolchain the build actually uses. Zig stays a script
     constant (`ZIG_VERSION="0.15.2"`): `build.rs` has no version check, so this
     script is the guard, and it prints the `export ZIG=…` hint on mismatch.
  2. `mise use -g` is called **only** with the specs `mise ls --json` does not
     already satisfy (parsed by an inline `python3` snippet — no `jq`
     dependency). An `@latest` spec counts as satisfied by any installed, active
     version, so a deliberate local pin is never clobbered, and an already
     set-up machine never has `~/.config/mise/config.toml` rewritten. That is
     what makes the idempotence requirement byte-exact rather than
     best-effort.
- Step 2 also verifies what the build will really use (`rustc --version`
  contains the pinned version, `cargo fmt --version`, `cargo clippy --version`),
  because `rust-toolchain.toml` — not mise — is what selects the toolchain and
  its components on this machine (`rustc` resolves to the rustup shim in
  `~/.cargo/bin`). The script therefore resolves its own directory, refuses to
  run when that does not sit in a herdr checkout (`justfile` +
  `rust-toolchain.toml`, exit 2, the same guard shape `gate.sh` uses) and
  `cd`s to the repo root first, so the rustup shim resolves the checkout's pin
  no matter where the script was invoked from. `-h/--help` still works there.
- Hardened for `set -euo pipefail`: every probe of an external tool
  (`zig version`, `rustc --version`, `sed` over `rust-toolchain.toml`,
  `mise ls --json`, the `python3` helper) is captured with an explicit failure
  branch, so a tool that is present but broken is reported `missing` with its
  error text instead of aborting the run with no diagnostic, and no probe is
  piped into `head` where `pipefail` could turn a SIGPIPE into a false
  `missing`. Step 1 reports all three prerequisites before exiting rather than
  stopping at the first one. The mise helper returns each spec's *resolved*
  version, so `just`/`bun`/`shellcheck` report `ok 1.58.0` rather than
  `ok latest`.
- Step 5 runs `just install-hooks`, which writes `core.hooksPath .githooks` into
  the **shared** repo config: hooks become active in the root checkout and in
  every worktree at once. `.githooks/pre-commit` then runs `just lint` (bare
  `cargo fmt --check` + `cargo clippy --all-targets --locked`) **outside** the
  gate lock, so from here on commit only right after a green gate on the same
  tree, and never while another agent's gate is mid-build.
- `scripts/fork/gate.sh` is unchanged and was verified as the plan asks:
  `EXIT=0` for a full `just ci`; two concurrent `lint` gates where the second
  prints `gate: waiting for gate lock (another gate is running)…` before its own
  `EXIT=0`; `bash scripts/fork/gate.sh /nonexistent` → `EXIT=2`. Evidence is in
  the PR body.

**Downstream**

- `dev-setup.sh` is the documented entry point (PR 5) and the first step of
  the epic's end-to-end validation.
- Hooks are active from here on: commit after gating so `pre-commit`'s
  `just lint` is a cache hit.
- **For PR 5's README rewrite** (PR 2 must not edit `docs/fork/README.md`):
  replace the `mise use -g …` block under *Development setup* with
  `bash scripts/fork/dev-setup.sh`, and document the three modes — plain (steps
  1–6, last line is the gate's `EXIT=`), `--skip-ci` (steps 1–5), `--check`
  (verify only, exit non-zero on any `missing`). Mention that the script pins
  `rust` (from `rust-toolchain.toml`), `zig@0.15.2`, `just`, `bun` and
  `shellcheck` through `mise use -g`, only touching the global mise config when
  a pin is actually missing, and that it installs the git hooks, after which
  every commit runs `just lint`.

### PR 3 — feat: fork build channel disables self-update and shows in version · deps: 1

**Goal:** every binary built from this checkout identifies as channel `fork`
(`herdr --version` → `herdr 0.8.2-fork`), `herdr update`, `herdr channel set`
and the background update check refuse to touch a fork binary and point at
`docs/fork/README.md`, and upstream's `distribution/latest.json` can never
replace it. Tests stay green under the fork channel.

**Files**

- `.cargo/config.toml` *(upstream file — minimal wiring)*: add
  `[env]\nHERDR_BUILD_CHANNEL = "fork"` (no `force`).
- `src/build_info.rs` *(upstream file — minimal wiring)*: add
  `pub fn is_fork() -> bool { channel() == "fork" }` and a pure
  `pub fn is_fork_channel(channel: &str) -> bool` with unit tests; add a test
  that `version()` ends with `-fork` when `channel() == "fork"`.
- `src/update.rs` *(upstream file — minimal wiring)*: a pure
  `fn fork_channel_refusal(channel: &str) -> Option<&'static str>` returning
  `Some("self-update is disabled for fork builds; see docs/fork/README.md")`
  for `"fork"`; call it first in `self_update` (before the Homebrew branch) and
  first in `auto_update` after the fake-version block, logging through
  `crate::logging::update_check_failed("fork build: self-update disabled; see
  docs/fork/README.md")` and returning. Add
  `pub(crate) fn fork_channel_update_guidance() -> Option<&'static str>`
  (same message, for the CLI).
- `src/cli.rs` *(upstream file — minimal wiring)*: in `channel_set`, feed
  `fork_channel_update_guidance().or(package_manager_channel_update_guidance_for_current_install())`
  into `channel_set_install_action` so `herdr channel set <stable|preview>`
  writes the config but prints guidance instead of running `self_update`.
- `src/main.rs` *(upstream file — minimal wiring)*: none required for the
  version string (already `herdr 0.8.2-fork`); optionally append
  ` (fork build; self-update disabled)` to the `herdr update` help line — keep
  it to one line if done.
- `src/app/mod.rs` tests *(upstream file — test-only edits)*: replace
  `env!("CARGO_PKG_VERSION")` with `crate::build_info::version()` in the
  release-notes/announcement tests that expect equality with the running
  version (`:1561`, `:1599`, `:1631-1634`); leave the `"99.99.99"` /
  `"0.4.9"` cases as-is.
- `tests/support/mod.rs` *(upstream file — additive helper)*:
  `pub fn build_version() -> String` mirroring `build_info::version()`
  (`option_env!("HERDR_BUILD_CHANNEL")`, `option_env!("HERDR_BUILD_ID")`,
  `env!("CARGO_PKG_VERSION")`).
- `tests/api_ping.rs:303`, `tests/cli/sessions.rs:388` *(upstream files —
  one-line edits)*: assert against `support::build_version()`.
- Any further failure surfaced by the full gate that stems from the channel
  (e.g. `src/product_announcements.rs` tests) is fixed the same way — make the
  expectation channel-aware, never skip the test.

**Shapes/approach**

- Channel semantics: `channel()` stays a `&'static str` (no enum — smallest
  diff); predicates `is_preview()` / `is_fork()`; `default_update_channel()`
  keeps returning `Stable` for fork (config `[update] channel` is untouched;
  E8 adds `fork` there).
- The refusal message keeps the `self-update is disabled` prefix so
  `src/main.rs:568` prints it verbatim and exits 1.
- `auto_update` early-return happens before any manifest fetch, so a fork
  binary never contacts `https://herdr.dev/latest.json` for itself. The
  manifest *detection* updater (`src/detect/manifest_update`) is unchanged.
- `herdr --version` output: `herdr 0.8.2-fork` (from `version()`); `herdr
  status` shows `version: 0.8.2-fork` for client and server via the existing
  code paths — no edits there.

**Tests**

- Unit: `fork_channel_refusal("fork")` is `Some(..)` with the prefix,
  `("stable")`/`("preview")` are `None`; `is_fork_channel`; `version()` suffix
  test (asserts on `channel()` so it is meaningful under both stable and fork
  compiles); `channel_set_install_action` prefers fork guidance over
  package-manager guidance (extend the existing
  `channel_set_skips_self_update_for_package_manager_guidance` pattern in
  `src/cli.rs`).
- Integration: `tests/api_ping.rs::ping_over_socket_returns_version` and
  `tests/cli/sessions.rs` pass with `build_version()`; add
  `tests/cli/sessions.rs`-style assertion that `herdr update` exits 1 and
  stderr starts with `self-update is disabled for fork builds` (new test in a
  new file `tests/fork_channel.rs` using `env!("CARGO_BIN_EXE_herdr")`, no
  server needed).
- Full gate green under the fork channel — the whole point of this PR.

**Real-server validation**

```bash
export XDG_CONFIG_HOME=/tmp/herdr-e0-chan; H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
$H --version                                   # herdr 0.8.2-fork
$H update; echo "exit=$?"                      # self-update is disabled for fork builds; see docs/fork/README.md · exit=1
nohup $H --session chan server </dev/null >/tmp/herdr-e0-chan.log 2>&1 &
$H --session chan status --json | jq '.client.version, .server.version'   # "0.8.2-fork" twice
$H --session chan api snapshot | jq -r .result.version                     # 0.8.2-fork
$H --session chan channel set preview                                      # writes config, prints fork guidance, exit 0, no download
$H --session chan session stop chan
```

Release-build background check: `cargo build --release`, start
`target/release/herdr --session chan-rel server` with `HERDR_LOG=herdr=debug`
under the same `XDG_CONFIG_HOME`, wait 5 s, and grep
`/tmp/herdr-e0-chan/herdr/sessions/chan-rel/herdr-server.log` for the fork
skip line; assert no request to `herdr.dev` appears in the log. Stock-server
compatibility: start the installed stock binary as
`XDG_CONFIG_HOME=/tmp/herdr-e0-stock /usr/bin/herdr --session stock server`
and point the fork client at it with
`HERDR_SOCKET_PATH=/tmp/herdr-e0-stock/herdr/sessions/stock/herdr.sock
target/debug/herdr status server --json` → `running: true`, `version:
"0.8.2"`; stop both sessions and remove both dirs.

**As built** (merged; deviations and facts later PRs need)

- Exact strings, now fixtures: `herdr --version` → `herdr 0.8.2-fork`;
  `herdr update` exits 1 printing
  `self-update is disabled for fork builds; see docs/fork/README.md`
  (`update::FORK_UPDATE_REFUSAL`); the background check logs
  `err="fork build: self-update disabled; see docs/fork/README.md"`
  (`update::FORK_UPDATE_CHECK_SKIP`) through
  `logging::update_check_failed`, at WARN, event `update.check.complete`.
- Symbols added: `build_info::{FORK_CHANNEL, is_fork_channel, is_fork}`,
  `update::{fork_channel_refusal, fork_channel_update_guidance}` (both
  `pub(crate)`), `cli::channel_set_guidance(fork, package_manager)` —
  extracted rather than inlining `.or()` at the call site so the precedence is
  unit-testable — and `tests/support::build_version()`.
- **Extra upstream file: `src/release_notes.rs`.** The channel suffix exposed a
  real bug, not just a test failure: `release_notes_from_stored` decided
  "these notes are newer than me" with `Version::parse(build_info::version())`,
  which returns `None` for `0.8.2-fork` (and for upstream's
  `0.8.2-preview.<id>`), so every pending release note silently read as
  non-preview and `update_available` stayed `None`. Fixed with a pure
  `comparable_version(&str)` that falls back to the pre-`-` base version.
  Version *identity* checks (`mark_current_version_seen_at`,
  `product_announcements`) still compare the full string, which is correct.
- `tests/cli/sessions.rs` needed six edits, not the one the plan listed
  (`:388, :429, :441, :458, :476, :482`); all now use
  `crate::support::build_version()`.
- New `tests/fork_channel.rs` drives the real binary under a throwaway
  `XDG_CONFIG_HOME`: `--version`, `--help`, `herdr update`, and
  `herdr channel set stable|preview`. Every channel-conditional test asserts on
  both branches (no silent `return`), so the suite stays meaningful if someone
  compiles this checkout with an explicit non-fork channel. The `herdr update`
  and `channel set` tests set `HERDR_ENV=1` purely as a backstop: `self_update`
  also refuses inside a herdr session, so a regressed fork guard can never make
  the test suite download or install anything — it just fails on the message.
- `update::auto_update` has a call-site test, not only a pure-guard test: the
  manifest fetch is a PATH-resolved `curl`
  (`noninteractive_process::curl_command`), so the test shims `curl` first on
  `PATH` and asserts a fork build never invokes it. Moving the guard after
  `check_latest()` turns it red.
- `src/main.rs` help: the usage line reads
  `herdr update [--handoff]         (fork build; self-update disabled)` on fork
  builds only. **Deferred:** the command-table description at
  `src/main.rs:616` still reads "Download and install the latest version";
  making it channel-aware means turning a `&'static str` table into owned
  strings, which is more reshaping of an upstream file than this PR is allowed.
- Verified against real servers: client and server both report `0.8.2-fork` in
  `herdr status --json`; `api snapshot` reports it at
  `result.snapshot.version`; a release build's background check logs the fork
  skip and never touches `herdr.dev`; a fork client attaches to a stock
  `0.8.2` server (`running: true`, `version: "0.8.2"`).
  Two fields differ against a stock server and both are correct:
  `server_binary_stale: true` (string compare `0.8.2` vs `0.8.2-fork` — the
  server really is a different binary) and, on this machine only,
  `compatible: false` because the installed `/usr/bin/herdr` predates master's
  `PROTOCOL_VERSION` bump (20 vs 22) — unrelated to the channel.

**Downstream**

- E8 replaces `fork_channel_refusal` with a fork manifest lookup; keep the
  helper pure and the message in one place so that is a one-site change.
- Every later PR's tests and validation see `0.8.2-fork`; never assert a bare
  `CARGO_PKG_VERSION` again — use `support::build_version()` /
  `build_info::version()`.
- `Version::parse("0.8.2-fork")` is `None` by design; code that needs a
  comparable version uses `Version::current()` (`BASE_VERSION`) or
  `release_notes::comparable_version`. Any *new* code that compares
  `build_info::version()` numerically must do the same — a bare
  `Version::parse` on it silently yields `None` and the comparison degrades to
  a `false` branch instead of failing loudly.

### PR 4 — feat: fleet lab script boots isolated named herdr sessions · deps: 1

**Goal:** `scripts/fork/fleet-lab.sh up <n> | down | status [--json] | env`
gives every later epic a "LAN of N hosts" on this machine: N independent
herdr servers as named sessions under a throwaway `XDG_CONFIG_HOME`, each
with one labelled workspace whose pane runs a visible marker process, with
client socket paths printed, and a teardown that can only ever touch its own
root.

**Files**

- `scripts/fork/fleet-lab.sh` (new, executable).
- `tests/fork_fleet_lab.rs` (new; `#![cfg(unix)]`, `mod support;`).

**Shapes/approach**

- Knobs: `HERDR_BIN` (default: `<repo>/target/debug/herdr` if present, else
  `herdr` on `PATH`), `HERDR_FLEET_LAB_ROOT` (default `/tmp/herdr-fleet-lab`),
  `HERDR_FLEET_LAB_TIMEOUT_MS` (default 15000).
- Layout (decision (h)): `$ROOT/.herdr-fleet-lab` marker (contains the
  `HERDR_BIN` used and the session count), `$ROOT/xdg` → `XDG_CONFIG_HOME`,
  `$ROOT/runtime` → `XDG_RUNTIME_DIR`, `$ROOT/logs/lab-N.log`,
  `$ROOT/pids/lab-N.pid`, `$ROOT/work/lab-N/`.
- Every herdr invocation is
  `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV -u
  HERDR_SESSION XDG_CONFIG_HOME=$ROOT/xdg XDG_RUNTIME_DIR=$ROOT/runtime
  "$HERDR_BIN" --session lab-N …` (argv order from
  `tests/cli/harness.rs::spawn_named_server`).
- `up <n>` (default 2): refuse if the marker exists and any session is running
  (`herdr session list --json` → `.sessions[].running`), else create dirs,
  write `$ROOT/xdg/<app>/config.toml` with `onboarding = false` where `<app>`
  is discovered from the binary (run `herdr --session lab-probe session list
  --json` once and read `session_dir`, or accept both `herdr-dev` and `herdr`
  by writing the file after the first server creates the dir); for each
  `lab-N`: `nohup … --session lab-N server </dev/null >$ROOT/logs/lab-N.log
  2>&1 &`, write `$!` to the pid file, poll `session list --json` until
  `running == true` (25 ms interval, timeout knob), then `workspace create
  --label lab-N --cwd $ROOT/work/lab-N --no-focus` → parse
  `.result.root_pane.pane_id` (python3 `json` — no jq dependency), then
  `pane run <pane_id> "printf 'herdr-fleet-lab:%s\n' lab-N; exec sh -c 'while
  :; do sleep 60; done'"` and `pane wait-output <pane_id> --match
  herdr-fleet-lab:lab-N --timeout $TIMEOUT`. Finally print the `status` table.
- `status [--json]`: from `session list --json`: `name`, `running`, `pid`,
  api socket (`socket_path`), client socket (`session_dir/herdr-client.sock`),
  workspace label / pane id from `api snapshot`. `--json` emits one object
  `{"root":…,"bin":…,"sessions":[{name,running,pid,api_socket,client_socket,
  pane_id}]}` — E1's integration test consumes this.
- `env`: prints `export`-able lines for `eval "$(fleet-lab.sh env)"`:
  `HERDR_FLEET_LAB_ROOT`, `XDG_CONFIG_HOME`, `HERDR_FLEET_LAB_SESSIONS="lab-1
  lab-2"`, and `HERDR_FLEET_LAB_CLIENT_SOCKET_<N>`.
- `down`: refuse unless `$ROOT/.herdr-fleet-lab` exists and `$ROOT` is not
  `/`, `$HOME`, or a parent of `$HOME`; for each pid file: `herdr --session
  lab-N session stop lab-N` (bounded by herdr's 15 s), then if the pid is
  still alive `kill -TERM`, wait ≤ 2 s, `kill -KILL` (mirrors
  `tests/support::terminate_pid`); then `rm -rf "$ROOT"`. Exit 0 when nothing
  is up.
- Never reads or writes `~/.config/herdr`; never uses the default session.

**Tests**

- `shellcheck -S warning scripts/fork/fleet-lab.sh` clean.
- `tests/fork_fleet_lab.rs` (runs in `just ci`): with
  `HERDR_BIN=env!("CARGO_BIN_EXE_herdr")` and `HERDR_FLEET_LAB_ROOT=<unique
  temp dir>`, `support::register_runtime_dir(root/runtime)` so the harness
  watchdog reaps leaks; run `up 2` → exit 0; `status --json` → two sessions
  `running: true` whose `client_socket` paths exist and connect
  (`support::wait_for_socket`); `herdr --session lab-2 pane read <pane_id>
  --source recent` contains `herdr-fleet-lab:lab-2`; `up 2` again → non-zero
  with "already up"; `down` → exit 0, root removed, no `herdr … server`
  process with `XDG_RUNTIME_DIR=<root>/runtime` remains
  (`support::herdr_server_pids_for_runtime_dir` on Linux); `down` again →
  exit 0. Failure path: `HERDR_BIN=/nonexistent up 1` → non-zero, no root
  left behind.
- Keep the test's wall time under ~15 s; it boots two servers.

**Real-server validation**

```bash
cargo build
bash scripts/fork/fleet-lab.sh up 3            # table with lab-1..lab-3, sockets, pids
bash scripts/fork/fleet-lab.sh status --json | python3 -m json.tool
eval "$(bash scripts/fork/fleet-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
$H --session lab-2 api snapshot | python3 -c 'import json,sys; s=json.load(sys.stdin)["result"]["snapshot"]; print(s["version"], [w["label"] for w in s["workspaces"]], len(s["panes"]))'
#   0.8.2-fork ['lab-2'] 1
#   NB (verified in PR 3): the snapshot lives under `result.snapshot`, not
#   `result`. PR 3 is merged, so the version is `0.8.2-fork`.
$H --session lab-2 pane read "$(…pane_id…)" --source recent | grep herdr-fleet-lab:lab-2
$H --session lab-2 terminal session observe <pane_id> --cols 80 --rows 24 | head -c 400 | cat -v   # ANSI frame bytes
ls -l /tmp/herdr-fleet-lab/xdg/*/sessions/*/herdr-client.sock                 # three sockets, mode srw-------
bash scripts/fork/fleet-lab.sh down && test ! -e /tmp/herdr-fleet-lab && pgrep -af 'herdr --session lab-' ; echo "leftover=$?"   # leftover=1
```

Also prove isolation: with the lab up, `herdr session list --json` **without**
the lab's `XDG_CONFIG_HOME` (i.e. the user's real config) must not list any
`lab-*` session.

**As built** (merged; deviations and facts later PRs need)

- Subcommands, knobs and layout are as planned. `status --json` emits exactly
  the contracted object; `pid` comes from `$ROOT/pids/lab-N.pid` (herdr's
  `SessionInfo` carries no pid) and is reported only while that pid still
  validates as this lab's server.
- **Readiness is a served snapshot, not a listening socket.** The API socket
  accepts connections before the app loop consumes requests, so a
  `workspace create` issued as soon as `session list --json` reports
  `running: true` fails with `server_unavailable: request handling failed:
  receiving on a closed channel`. `up` therefore polls *both* `session list`
  and `api snapshot` (looking for `"snapshot"`) before it touches a session.
  Any later epic that boots a herdr server and immediately drives it needs the
  same gate.
- **Unix socket paths cap the lab root.**
  `$ROOT/xdg/herdr-dev/sessions/lab-N/herdr-client.sock` must stay under
  ~104 bytes or the server dies inside its own bind with `local socket name
  length exceeds capacity of sun_path of sockaddr_un`. `up` refuses a too-long
  root up front, and `tests/fork_fleet_lab.rs` uses short
  `/tmp/herdr-lab-<label>-<pid>-<n>` roots. E1's fixtures must keep lab roots
  short too.
- **Isolation is wider than the plan's list.** Beyond `--session lab-N`,
  `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR` and dropping
  `HERDR_SOCKET_PATH`/`HERDR_CLIENT_SOCKET_PATH`/`HERDR_ENV`/`HERDR_SESSION`,
  every lab process also drops `HERDR_CONFIG_PATH` (`config::io::config_path`
  honours it *above* `XDG_CONFIG_HOME`, so a caller with it set would have had
  the lab read and write the real `config.toml`) and redirects
  `XDG_STATE_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` into the root
  (`state_dir()` otherwise writes announcement/plugin state to
  `~/.local/state/herdr-dev`).
- **`env` deliberately does not export `XDG_RUNTIME_DIR`**: `eval`-ing it into
  an interactive shell would hijack the caller's Wayland/D-Bus/PipeWire socket
  lookup, and herdr clients do not need it (sockets live under
  `XDG_CONFIG_HOME`). It exports the additive `HERDR_FLEET_LAB_RUNTIME_DIR`
  instead, plus `HERDR_FLEET_LAB_API_SOCKET_<N>` and
  `HERDR_FLEET_LAB_PANE_<N>`. The contracted lines (`HERDR_FLEET_LAB_ROOT`,
  `XDG_CONFIG_HOME`, `HERDR_FLEET_LAB_SESSIONS`,
  `HERDR_FLEET_LAB_CLIENT_SOCKET_<N>`) are unchanged.
- **Deletion safety** (`down`, and the stale-lab path of `up`): the marker must
  be a regular non-symlink file whose first line is `herdr-fleet-lab`; the root
  must be a plain non-symlink directory, absolute, at least two path
  components, and is refused when it is `/`, `$HOME`, an ancestor of `$HOME`,
  `/home/<user>` or `/Users/<user>` (even with `HOME` unset or wrong), a
  reserved system directory, at/above/inside the caller's `XDG_CONFIG_HOME`
  (the lab's own `$ROOT/xdg` is exempt so `eval "$(fleet-lab.sh env)"` still
  allows `down`), or contains a newline.
- **Signal safety:** only pids from this lab's own pid files are ever
  signalled. Parsing is strict (first line, decimal digits, no sign, no leading
  zero, at most 10 digits, never `0`/`1`/`$$`/`$PPID`, symlinked pid files
  ignored) and each pid is re-validated immediately before TERM/KILL against
  `/proc/<pid>/environ` (`XDG_RUNTIME_DIR=$ROOT/runtime`) and its argv
  (`<bin> --session lab-N server`), so pid reuse cannot reach a bystander. On
  non-Linux hosts argv alone identifies the process (documented in the script).
- **A failed `up` undoes itself** at every stage (missing binary, server exits
  early, API never ready, workspace/pane failure) and exits with the failing
  status — 130 on SIGINT, 143 on SIGTERM, since `$?` inside a signal trap is
  meaningless.
- `terminal session observe` emits newline-delimited JSON records with
  base64-encoded `bytes`, not raw ANSI; the plan's `cat -v` line shows that
  JSON envelope.
- `tests/fork_fleet_lab.rs` has four tests (lifecycle, missing binary, server
  exits early, unmarked/decoy marker) and runs in ~1.4 s. It registers the lab
  runtime dir with `support::register_runtime_dir` *after* a successful `up`,
  because registering first creates `$ROOT/runtime` and `up` refuses a root
  that carries no marker of its own. `tests/support/mod.rs` is untouched.

**Downstream**

- E1's `tests/` integration test and E2/E3 validations call
  `fleet-lab.sh up <n>` and read `status --json` / `env`; keep those two
  output shapes stable (add fields, never rename).
- Session names `lab-N`, the `herdr-fleet-lab:lab-N` marker text, and the
  workspace label `lab-N` are the fixtures later plans assert on.
- The lab uses `--session` explicitly on every call; E1's `kind = "local",
  session = "lab-N"` host config maps 1:1 onto these sessions.

### PR 5 — docs: fork readme for dev setup, ci, fleet lab; review adr 0001 · deps: 2, 3, 4

**Goal:** `docs/fork/README.md` describes the fork as it now is (one-command
setup, gate, fork CI, fork channel, fleet lab, upstream sync, skills), and ADR
0001 records what E0's exploration confirmed so later epics do not
re-derive it.

**Files**

- `docs/fork/README.md` (rewrite the *Development setup*, *Keeping up with
  upstream*, and *Repository notes* sections; add *Fork build identity* and
  *Fleet lab* sections).
- `docs/fork/decisions/0001-servers-stay-stock-ssh-transport.md` (append an
  "E0 review" subsection under *Consequences*; do not rewrite the decision).

**Shapes/approach**

- README: replace the "Until then: `mise use -g …`" block with
  `bash scripts/fork/dev-setup.sh` (`--check`, `--skip-ci`), keep the gate
  wrapper paragraph, document `herdr --version` → `0.8.2-fork` and that
  `herdr update` / `herdr channel set` / background checks are disabled (E8
  will add a fork channel) — quote the exact strings PR 3 ships:
  `herdr update` exits 1 with
  `self-update is disabled for fork builds; see docs/fork/README.md`, and the
  background check logs
  `err="fork build: self-update disabled; see docs/fork/README.md"`. This
  README path is the one both messages point at, so it must exist and explain
  the guard. Document `fleet-lab.sh up|status|env|down` with the isolation
  rules and the `HERDR_BIN` / `HERDR_FLEET_LAB_ROOT` /
  `HERDR_FLEET_LAB_TIMEOUT_MS` knobs — including that the lab root must be
  short enough for a unix socket path and that `env` exports
  `HERDR_FLEET_LAB_RUNTIME_DIR`, not `XDG_RUNTIME_DIR` (see PR 4's
  *As built*) — state that fork CI
  is `.github/workflows/fork-ci.yml` (= `just ci` + PR-title check +
  shellcheck) and that upstream workflows are disabled by repository state
  (with the re-check step after every upstream sync), and keep the skills
  table.
- ADR amendment (facts from *Real current state*): `--remote` compatibility
  is generation-based, so fork clients attach to stock servers; a fork client
  cannot seed a foreign-platform remote from the stable manifest (must
  pre-install herdr there or set `HERDR_REMOTE_BINARY`) — consistent with
  "servers stay stock", and E1 must surface it as a host-local
  `Unavailable { reason }`; named local sessions are a real second host kind
  (`lab-N`); debug builds use the `herdr-dev` config dir, so the fleet lab and
  E1 tests must never hardcode `herdr`. No contradiction with the decision was
  found; nothing is amended in the *Decision* section.

**Tests**

- Docs only. `just ci` still runs (the gate is mandatory for every PR).
- Every command block in the README is executed verbatim once as the PR's
  validation (see below) and fixed if it does not work as written.

**Real-server validation:** walk the README top to bottom on a clean shell:
`bash scripts/fork/dev-setup.sh --check` → 0; `bash scripts/fork/gate.sh` →
`EXIT=0`; `target/debug/herdr --version` → `herdr 0.8.2-fork`;
`bash scripts/fork/fleet-lab.sh up 2` … `down` exactly as documented; the
"Keeping up with upstream" block is dry-run up to `git merge upstream/master`
(drift is 0, so the merge is a no-op) on a throwaway branch that is deleted
afterwards without pushing.

**Downstream**

- `docs/fork/README.md` is the entry point every later plan links to for
  setup and fleet lab usage; later epics add sections (`fleet.md`,
  `gateway.md`, `remote-access.md`) as separate files rather than growing the
  README.

## Critical files referenced (reuse, don't reinvent)

- `scripts/fork/gate.sh` — the gate wrapper; `EXIT=` contract; do not set
  `HERDR_BUILD_CHANNEL` there (PR 3 sets it in `.cargo/config.toml`).
- `justfile` — `ci`, `lint`, `test-one`, `install-hooks`; **not edited** by E0.
- `.github/workflows/ci.yml` — action SHAs, toolchain/Zig/bun/nextest install
  steps and the `conventional-commits` job to copy into `fork-ci.yml`.
- `scripts/conventional_commits.py` — allowed types `feat fix perf docs ci
  test refactor chore release`; CLI `[subjects…] [--range] [--message-file]`.
- `.githooks/commit-msg`, `.githooks/pre-commit` — installed by
  `just install-hooks` (PR 2).
- `src/build_info.rs` — `channel()`, `build_id()`, `version()`, `is_preview()`.
- `src/update.rs` — `self_update` (`:2113`), `auto_update` (`:2245`),
  `is_*_managed_install`, `package_manager_channel_update_guidance_for_current_install`,
  `Version::parse`; test template
  `preview_channel_is_rejected_for_package_manager_paths` (`:2670`).
- `src/main.rs:553-577` — `herdr update` dispatch and the
  `"self-update is disabled"` prefix rule; `:694-698` `--version`.
- `src/cli.rs:151-254` — `channel_set`, `channel_set_install_action`,
  `ChannelSetInstallAction`.
- `src/app/mod.rs:163-169` — pure update-check gating helpers (precedent for
  testable guards); `:543` and `src/app/runtime.rs:97` spawn sites.
- `src/config/model.rs:49-59` — `default_update_channel_for_build` (pure
  helper precedent).
- `src/config/io.rs:22-35` — `app_dir_name()` (`herdr-dev` vs `herdr`),
  `config_dir()` (`XDG_CONFIG_HOME`).
- `src/session.rs` — `data_dir_for`, `api_socket_path_for`,
  `client_socket_path_for`, `validate_name` (≤ 64 chars, `[A-Za-z0-9._-]`),
  `list_sessions`, `stop_session` (`server.stop` + 15 s socket poll).
- `src/server/socket_paths.rs` — `client_socket_path()` precedence (explicit
  `--session` > `HERDR_SOCKET_PATH` > `HERDR_CLIENT_SOCKET_PATH`).
- `src/server/autodetect.rs:208-240` and `src/platform/mod.rs:99-120` — how
  the bare launch daemonizes (`setsid`); `herdr server` itself does not.
- `tests/cli/harness.rs::spawn_named_server` (`:163`), `run_named_cli`,
  `send_request` — the named-session spawn recipe the lab script mirrors.
- `tests/support/mod.rs` — `register_runtime_dir`, `register_spawned_herdr_pid`,
  `wait_for_socket`, `herdr_server_pids_for_runtime_dir`, `terminate_pid`
  semantics; new `build_version()` helper (PR 3).
- `tests/api_ping.rs:288-309`, `tests/cli/sessions.rs:375-395` — the two
  version assertions PR 3 makes channel-aware.
- `src/remote/attach.rs:1053-1060`, `:1500-1540` — generation-based remote
  compatibility and manifest-based seeding (ADR review facts).
- Binding rules: `AGENTS.md` → Universal Project Rules, Testing, Code
  Conventions; `.claude/rules/fork.md` → branches, commits, gate, real-server
  validation, safety; `docs/fork/ROADMAP.md` → principles 1–8 and E0.

## End-to-end epic validation

Run by `implement-epic` after PRs 1–5 are ✅ and merged, from the root checkout
on `master` (`git -C <root> pull --ff-only`):

1. **Green gate on master (the E0 exit state):** `bash scripts/fork/dev-setup.sh`
   → every tool `ok`, hooks installed, final line `EXIT=0`. Then
   `bash scripts/fork/gate.sh <root>` → `EXIT=0` a second time (idempotent,
   cached).
2. **Fork CI is the only CI:** `gh run list --repo vinceseguin/herdr
   --workflow=fork-ci.yml --branch master --limit 1` → `completed success` for
   the merge commit of PR 5; `gh workflow list --all --repo vinceseguin/herdr`
   → the ten upstream file-backed workflows `disabled_manually`, `Fork CI`
   `active`; `gh run list --workflow=ci.yml --limit 1` shows no run newer than
   PR 1's disable.
3. **Fork identity:** `target/debug/herdr --version` → `herdr 0.8.2-fork`;
   `XDG_CONFIG_HOME=/tmp/herdr-e0-final env -u HERDR_SOCKET_PATH -u
   HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr update` → exit 1,
   stderr `self-update is disabled for fork builds; see docs/fork/README.md`.
4. **Fleet lab is the E2E fixture:** `bash scripts/fork/fleet-lab.sh up 3`;
   `status --json` → 3 sessions `running: true` with existing client sockets;
   for each `lab-N`, `herdr --session lab-N api snapshot` → one workspace
   labelled `lab-N`, `version` `0.8.2-fork`, and `pane read` of its pane
   contains `herdr-fleet-lab:lab-N`; `herdr --session lab-1 terminal session
   observe <pane_id> --cols 80 --rows 24 | head -c 200` yields bytes; the
   user's own `herdr session list` shows no `lab-*`; `bash
   scripts/fork/fleet-lab.sh down` → root gone, `pgrep -af 'herdr --session
   lab-'` empty.
5. **Docs are executable:** every command block in `docs/fork/README.md` was
   run verbatim during PR 5's validation; re-run `bash
   scripts/fork/dev-setup.sh --check` as the final assertion.
6. Flip E0 to ✅ in `docs/fork/ROADMAP.md` only when 1–5 all hold; leave no
   `/tmp/herdr-e0-*` or `/tmp/herdr-fleet-lab` directories and no lab
   processes behind.
