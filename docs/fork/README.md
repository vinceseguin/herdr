# herdr fork — `vinceseguin/herdr`

A fork of [herdrdev/herdr](https://github.com/herdrdev/herdr) that adds a
multi-machine console, a phone app, and private remote access. The plan is
[`ROADMAP.md`](./ROADMAP.md); PR-by-PR plans land in [`plans/`](./plans/);
architecture decisions in [`decisions/`](./decisions/).

## What the fork adds

| | Upstream herdr | This fork |
| --- | --- | --- |
| Console | one TUI ↔ one server (local or one `--remote`) | `herdr fleet`: every LAN host's workspaces and agents in one sidebar, work in any of them |
| Phone | — (third-party bridges) | `herdr gateway` + installable web app: agents grouped blocked-first, live terminals, answer prompts |
| Away from home | SSH | same thing over Tailscale; gateway gets HTTPS from `tailscale serve` |
| Servers | stock | **stock** — LAN hosts run upstream or the fork interchangeably |

Design in one line: servers are untouched, SSH is the only transport, the
gateway is loopback-first and token-gated, and every new line of code lives in
an additive module so upstream merges stay cheap.

## Working in the fork with Claude Code

Four skills under `.claude/skills/` drive the roadmap:

| Skill | Model | What it does |
| --- | --- | --- |
| `/plan-epic E1` | fable | Turns one roadmap epic into `docs/fork/plans/e1-….md` (PR map + per-PR specs) |
| `/implement-task 3 e1-fleet-core` | opus | Ships one PR: implement, gate, ultrathink review, real-server validation, PR, CI, merge |
| `/implement-epic e1-fleet-core --auto` | fable | Runs `implement-task` agents in dependency waves until the plan is ✅, then validates the epic |
| `/implement-roadmap --auto` | fable | Plans and implements every epic in dependency order, owning the roadmap's Epic status table |

Rules that bind every agent: `.claude/rules/fork.md` (read together with the
root `AGENTS.md`, whose maintainer-only sections do not apply here).

## Development setup

The repo needs: Rust per `rust-toolchain.toml`, `just`, `cargo-nextest`, `bun`,
Zig 0.15.2 (for the vendored libghostty-vt), `shellcheck`, `python3` ≥ 3.10,
`flock`. One command installs and verifies all of it:

```bash
bash scripts/fork/dev-setup.sh
```

It pins `rust` (the version read from `rust-toolchain.toml`), `zig@0.15.2`,
`just`, `bun` and `shellcheck` through `mise use -g`, installs `cargo-nextest`
if missing, installs the repo git hooks (`just install-hooks`), and finishes by
running the gate — so the last line of a full run is the gate's own `EXIT=`.
Every step prints `ok`, `installed` or `missing`, and the script is idempotent:
it only calls `mise use -g` for pins that are actually absent, so a second run
changes nothing (including `~/.config/mise/config.toml`).

`mise`, `python3` and `flock` are the only prerequisites it will not install —
it reports them as `missing` (with the mise install hint) and exits 1. Three
modes:

```bash
bash scripts/fork/dev-setup.sh --check      # verify only, install nothing; non-zero if anything is missing
bash scripts/fork/dev-setup.sh --skip-ci    # install/verify (steps 1-5), do not run the gate
bash scripts/fork/dev-setup.sh --help       # usage
```

Installing the hooks makes every commit run `just lint` (`cargo fmt --check`
plus `cargo clippy --all-targets --locked`) and validates the subject as a
conventional commit. That lint runs **outside** the gate lock, so commit right
after a green gate on the same tree — never while another agent's gate is
building.

### The gate

`just ci` is the fork's single definition of green: `cargo fmt --check`,
`cargo clippy --all-targets --locked -D warnings`, `cargo nextest run --locked`,
the python maintenance tests and the bun suites. Always run it through the
wrapper, which serializes builds across agents:

```bash
bash scripts/fork/gate.sh                   # runs `just ci` in this checkout under a machine-wide lock
```

The wrapper prints the last 40 log lines, the log path
(`${TMPDIR:-/tmp}/herdr-fork-gates/`), and `EXIT=<code>` last. Read that line —
never pipe the wrapper into `tail`/`head`/`grep`, or the pipeline reports the
pager's exit status instead of the gate's. With no arguments it gates the
checkout the script itself lives in; it takes an optional worktree and recipe:

```bash
bash scripts/fork/gate.sh .claude/worktrees/<branch-slug>     # gate another worktree
bash scripts/fork/gate.sh . "test-one fleet_lab"               # one nextest filter, same lock
```

A `test-one` filter that matches nothing makes nextest exit 4, so the gate
prints `EXIT=4` — name a test that exists.

A path that is not a herdr checkout exits 2. The gate's last suite runs
`bun install --frozen-lockfile`, so a full `just ci` needs network.

### Testing a debug build by hand

Never point a debug build at your live herdr. Use a throwaway
`XDG_CONFIG_HOME`, an explicit `--session`, and clear the inherited socket
overrides:

```bash
export XDG_CONFIG_HOME=/tmp/herdr-scratch
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
  target/debug/herdr --session scratch server &
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
  target/debug/herdr --session scratch status server --json
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
  target/debug/herdr --session scratch session stop scratch
rm -rf /tmp/herdr-scratch
```

Debug builds use the **`herdr-dev`** config directory name and release builds
use `herdr`, so a debug session's files live under
`$XDG_CONFIG_HOME/herdr-dev/sessions/<name>/`. Never hardcode `herdr` in a
fixture. For more than one server at a time, use the fleet lab below.

## Fork build identity

Everything built from this checkout compiles with `HERDR_BUILD_CHANNEL=fork`
(set in `.cargo/config.toml`, without `force`, so a real environment variable
still wins):

```console
$ target/debug/herdr --version
herdr 0.8.2-fork
```

The suffix is the upstream `x.y.z-<channel>` shape (`0.8.2-fork.<id>` when
`HERDR_BUILD_ID` is set). It is what keeps upstream's release machinery away
from a fork binary:

- `herdr update` exits 1 with
  `self-update is disabled for fork builds; see docs/fork/README.md` — this
  page is what that message points at.
- `herdr channel set stable|preview` still writes the config, but prints the
  same guidance instead of downloading anything.
- The background update check returns before any network call and logs
  `err="fork build: self-update disabled; see docs/fork/README.md"`, so a fork
  binary never fetches `https://herdr.dev/latest.json` for itself. The
  agent-detection manifest update is unaffected — that is stock data the fork
  wants.

Epic E8 will repoint these at a fork release manifest; until then a fork binary
is only ever replaced by rebuilding it.

Two consequences worth knowing:

- `0.8.2-fork` is not a parseable `x.y.z`: `Version::parse` returns `None` for
  it, so a numeric comparison silently degrades to a `false` branch instead of
  failing loudly. Compare with `Version::current()` (the base version), or drop
  the `-<channel>` suffix first the way `src/release_notes.rs`'s own
  (module-private) `comparable_version` does — never
  `Version::parse(build_info::version())`.
- Against a stock server, `herdr status --json` reports
  `server_binary_stale: true` — a plain string comparison of `0.8.2` against
  `0.8.2-fork`. The server really is a different binary; nothing is wrong.
  Actual `--remote` compatibility is decided by the endpoint generation, not by
  the version string (see
  [ADR 0001](./decisions/0001-servers-stay-stock-ssh-transport.md)).

Tests must assert `support::build_version()` (integration) or
`build_info::version()` (in-crate), never a bare `CARGO_PKG_VERSION`.

## Fleet lab

`scripts/fork/fleet-lab.sh` boots N independent herdr servers on this machine —
the "LAN of N hosts" every later epic validates against. Sessions are named
`lab-1 … lab-N`; each gets one workspace labelled `lab-N` whose pane prints the
marker `herdr-fleet-lab:lab-N` and then idles.

```bash
cargo build                                 # the lab defaults to target/debug/herdr
bash scripts/fork/fleet-lab.sh up 2         # start lab-1, lab-2; prints the status table
bash scripts/fork/fleet-lab.sh status       # the same table
bash scripts/fork/fleet-lab.sh status --json
eval "$(bash scripts/fork/fleet-lab.sh env)"
bash scripts/fork/fleet-lab.sh down         # stop the sessions and delete the lab root
```

`up` with no count starts two sessions and accepts at most 32.

`status --json` is the machine-readable contract later epics consume (fields
are added, never renamed):

```json
{"root": "/tmp/herdr-fleet-lab", "bin": "…/target/debug/herdr",
 "sessions": [{"name": "lab-1", "running": true, "pid": 1234,
               "api_socket": "…/herdr.sock", "client_socket": "…/herdr-client.sock",
               "pane_id": "…"}]}
```

`env` prints `export` lines for `eval`: `HERDR_FLEET_LAB_ROOT`,
`XDG_CONFIG_HOME`, `HERDR_FLEET_LAB_SESSIONS`,
`HERDR_FLEET_LAB_CLIENT_SOCKET_<N>`, plus `HERDR_FLEET_LAB_API_SOCKET_<N>` and
`HERDR_FLEET_LAB_PANE_<N>`. It deliberately does **not** export
`XDG_RUNTIME_DIR` — that would hijack your shell's Wayland/D-Bus/PipeWire
sockets — and exports `HERDR_FLEET_LAB_RUNTIME_DIR` instead.

Knobs:

| Variable | Default | Notes |
| --- | --- | --- |
| `HERDR_BIN` | `<repo>/target/debug/herdr`, else `herdr` on `PATH` | the binary every lab session runs |
| `HERDR_FLEET_LAB_ROOT` | `/tmp/herdr-fleet-lab` | must stay **short**: `$ROOT/xdg/herdr-dev/sessions/lab-N/herdr-client.sock` has to fit in a `sun_path` (~104 bytes) or `up` refuses it |
| `HERDR_FLEET_LAB_TIMEOUT_MS` | `15000` | per-step timeout (server readiness, pane marker) |

Isolation is the point, and it is structural rather than by convention: every
lab process carries an explicit `--session lab-N`, the lab's own
`XDG_CONFIG_HOME`/`XDG_RUNTIME_DIR`/`XDG_STATE_HOME`/`XDG_DATA_HOME`/
`XDG_CACHE_HOME` under the root, and has `HERDR_SOCKET_PATH`,
`HERDR_CLIENT_SOCKET_PATH`, `HERDR_ENV`, `HERDR_SESSION` and
`HERDR_CONFIG_PATH` removed. Nothing the lab does can reach `~/.config/herdr`,
`~/.config/herdr-dev` or your default session — with the lab up, `herdr session
list` run from a shell that has **not** eval'd `fleet-lab.sh env` (which points
`XDG_CONFIG_HOME` at the lab) shows no `lab-*`.

Teardown is equally deliberate: `down` refuses a root that does not carry the
lab's own `.herdr-fleet-lab` marker file, and refuses `/`, `$HOME`, any
ancestor of `$HOME`, your `XDG_CONFIG_HOME` and a list of reserved system
directories. It only ever signals pids the script itself wrote, re-validating
each one against its argv — and, on Linux, against `/proc/<pid>/environ` — so
pid reuse cannot reach a bystander. A failed `up` undoes itself; `down` on a lab
that is not up exits 0.

Talk to a lab session like any other named session:

```bash
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
eval "$(bash scripts/fork/fleet-lab.sh env)"
$H --session lab-2 api snapshot | python3 -c 'import json,sys; s=json.load(sys.stdin)["result"]["snapshot"]; print(s["version"], [w["label"] for w in s["workspaces"]])'
$H --session lab-2 pane read "$HERDR_FLEET_LAB_PANE_2" --source recent | grep herdr-fleet-lab:lab-2
```

(The snapshot lives under `result.snapshot`, not `result`.
`terminal session observe` emits newline-delimited JSON records with
base64-encoded `bytes`, not raw ANSI.)

`tests/fork_fleet_lab.rs` exercises the whole lifecycle inside `just ci`, so
the script is covered by the gate.

## Continuous integration

Fork CI is [`.github/workflows/fork-ci.yml`](../../.github/workflows/fork-ci.yml)
— three jobs on `ubuntu-latest`, and their names are what
`gh pr checks <pr> -R vinceseguin/herdr --watch` reports:

| Job | What it runs |
| --- | --- |
| `conventional-commits` | `scripts/conventional_commits.py` on the PR title (PR) or on the pushed subjects (push to `master`) |
| `check (ubuntu-latest)` | `just ci` — the same gate you run locally |
| `shellcheck` | `shellcheck -S warning scripts/fork/*.sh` |

Two deliberate divergences from upstream's `ci.yml`: the push check walks
`git log --no-merges --first-parent` instead of validating a raw commit range
(an upstream sync merge carries subjects this fork neither authors nor can fix),
and `cancel-in-progress` applies to pull-request runs only, so back-to-back
merges never cancel a `master` run.

**Upstream's ten workflows are disabled by repository state**
(`gh workflow disable`), not by editing their files — so an upstream merge can
never re-enable them and never conflicts. Verify with:

```bash
gh workflow list --all -R vinceseguin/herdr
```

Everything except `Fork CI` must read `disabled_manually`.

## Keeping up with upstream

```bash
git fetch upstream
git switch -c chore/sync-upstream-$(date +%Y%m%d) origin/master
git merge upstream/master          # merge, never rebase master
bash scripts/fork/gate.sh          # EXIT=0
git push -u origin HEAD && gh pr create -R vinceseguin/herdr --base master --fill && gh pr merge -R vinceseguin/herdr --squash --delete-branch
```

After every sync, re-run `gh workflow list --all -R vinceseguin/herdr` and
`gh workflow disable <file>` any **new** upstream workflow the merge added.

Fork-owned paths (`docs/fork/`, `.claude/`, `scripts/fork/`, `src/fleet/`,
`src/gateway/`, `web/`, `.github/workflows/fork-*.yml`, `tests/fork_*.rs`) never
conflict. The upstream files that currently carry fork wiring, and may:
`.cargo/config.toml` (the `[env]` channel), `src/build_info.rs`,
`src/update.rs`, `src/cli.rs`, `src/main.rs`, `src/release_notes.rs`,
`src/app/mod.rs` (tests only), `tests/support/mod.rs`, `tests/api_ping.rs`,
`tests/cli/sessions.rs`, `.gitignore`, and the fork section at the tail of
`AGENTS.md`. Later epics add `src/main.rs` / `src/cli.rs` subcommand wiring and
a `src/config/model.rs` field.

`src/protocol/wire.rs`, `src/protocol/endpoint.rs` and
`tests/fixtures/endpoint-*.json` are never touched by the fork; take upstream's
side of any conflict there.

## Repository notes

- The fork is **public** — GitHub forks of public repositories cannot be
  private. Never commit tokens, SSH targets, or anything from `~/.config/herdr`.
- Upstream ignores `docs/*` except `docs/next|preview|versions`; `.gitignore`
  carves out `docs/fork/` so these files are tracked. `.claude/worktrees/` is
  ignored — parallel agents work there.
- Do not edit `docs/next/**`, root `README.md`, `CHANGELOG.md`,
  `skills/herdr/SKILL.md`, `docs/preview/**`, `docs/versions/**` or
  `distribution/**` for fork work; fork docs go in `docs/fork/`.
- Branches: `feat|fix|docs|ci|chore/<epic>-pr<N>-<slug>` off the latest
  `origin/master`, squash-merged into `master`. Never commit to `master`
  directly.
- Commits: lowercase conventional subjects (`feat fix perf docs ci test
  refactor chore release`), plus the `Co-Authored-By` / `Claude-Session`
  trailers this environment specifies. No `refs #<n>` unless the fork really
  has that issue.
