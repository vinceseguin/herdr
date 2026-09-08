<p align="center"><img src="../../assets/fork/logo-192.png" width="96" height="96" alt="Herdr Fleet logo"></p>

# Herdr Fleet — the `vinceseguin/herdr` fork

**Herdr Fleet** (short name *Fleet*) is the product name of this fork. The
binary, crate, config directories and protocol stay `herdr` so stock and fork
servers remain interchangeable; the name and the mark in
[`assets/fork/`](../../assets/fork/) appear only on fork-owned surfaces.

A fork of [herdrdev/herdr](https://github.com/herdrdev/herdr) that adds a
multi-machine console, a phone app, and private remote access. The plan is
[`ROADMAP.md`](./ROADMAP.md); PR-by-PR plans land in [`plans/`](./plans/);
architecture decisions in [`decisions/`](./decisions/).

## What the fork adds

| | Upstream herdr | This fork |
| --- | --- | --- |
| Console | upstream's multi-machine client (`herdr machine add`, `list`, …; machine sidebar, background connects, reconnects — upstream #3670) | **the same** — the fork's own console (E2) was retired in favour of it, see [ADR 0002](./decisions/0002-adopt-upstream-multi-machine-client.md) |
| Phone | — (third-party bridges) | `herdr gateway` + installable web app: agents grouped blocked-first, live terminals, answer prompts |
| Away from home | SSH | same thing over Tailscale; gateway gets HTTPS from `tailscale serve` |
| Claude accounts | one `~/.claude` per machine; log out to change licence | `[[accounts]]` profiles per agent: pick one at start, see it everywhere, move a running agent to another account keeping its conversation |
| Servers | stock | **stock** — LAN hosts run upstream or the fork interchangeably |

Design in one line: servers are untouched, the console is upstream's, SSH is
the only transport, the gateway is loopback-first and token-gated, and every
new line of code lives in an additive module so upstream merges stay cheap.

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

Since E3 PR 1 the crate has a cargo feature, `gateway`, on by default, so
green means **two** feature sets. Run the second one through the same wrapper
before opening a pull request:

```bash
bash scripts/fork/gate.sh .claude/worktrees/<branch-slug> ci-no-default
```

`just ci-no-default` is `cargo clippy --all-targets --locked
--no-default-features -- -D warnings` plus `cargo nextest run --locked
--no-default-features` — the upstream-shaped build, with no `herdr gateway`,
no axum and no other optional dependency linked. It needs no bun (it skips the
python and bun suites `just ci` already covers) but still needs Zig, because
`build.rs` always builds the vendored libghostty-vt. Anything gateway-only
must therefore carry `#[cfg(feature = "gateway")]`, and a `tests/*.rs` file
that uses it needs a crate-level `#![cfg(feature = "gateway")]`.

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
herdr 0.9.0-fork
```

The suffix is the upstream `x.y.z-<channel>` shape (`0.9.0-fork.<id>` when
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

- `0.9.0-fork` is not a parseable `x.y.z`: `Version::parse` returns `None` for
  it, so a numeric comparison silently degrades to a `false` branch instead of
  failing loudly. Compare with `Version::current()` (the base version), or drop
  the `-<channel>` suffix first the way `src/release_notes.rs`'s own
  (module-private) `comparable_version` does — never
  `Version::parse(build_info::version())`.
- Against a stock server, `herdr status --json` reports
  `server_binary_stale: true` — a plain string comparison of `0.9.0` against
  `0.9.0-fork`. The server really is a different binary; nothing is wrong.
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

## SSH lab

`scripts/fork/ssh-lab.sh` turns the fleet lab into an **SSH-reachable host**, so
the fork's ssh transport (and `herdr --remote`) can be exercised end to end on
one machine. It starts a **user-space** sshd on `127.0.0.1:2299` — no root, no
system service, and no contact with your `~/.ssh`, `~/.config` or installed
herdr.

```bash
bash scripts/fork/fleet-lab.sh up 2         # the ssh lab layers on the fleet lab
bash scripts/fork/ssh-lab.sh up             # start the sshd; prints the status
bash scripts/fork/ssh-lab.sh status --json
eval "$(bash scripts/fork/ssh-lab.sh env)"
bash scripts/fork/ssh-lab.sh down           # stop the sshd, delete <lab root>/ssh
```

It lives in `$HERDR_FLEET_LAB_ROOT/ssh` and refuses to run without the fleet
lab's `.herdr-fleet-lab` marker next to it. `status --json` is the
machine-readable contract (fields are added, never renamed):

```json
{"root": "/tmp/herdr-fleet-lab/ssh", "port": 2299, "running": true, "pid": 1615485,
 "target": "herdr-ssh-lab", "home": "/tmp/herdr-fleet-lab/ssh/home",
 "ssh_config": "/tmp/herdr-fleet-lab/ssh/home/.ssh/config"}
```

`env` prints exactly five exports:

```console
$ bash scripts/fork/ssh-lab.sh env
export HERDR_SSH_LAB_ROOT="/tmp/herdr-fleet-lab/ssh"
export HERDR_SSH_LAB_HOME="/tmp/herdr-fleet-lab/ssh/home"
export HERDR_SSH_LAB_TARGET="herdr-ssh-lab"
export HERDR_SSH_LAB_PORT="2299"
export HERDR_SSH_LAB_SSH_CONFIG="/tmp/herdr-fleet-lab/ssh/home/.ssh/config"
```

It deliberately does **not** export `HOME`. Instead, **run the herdr client with
`HOME=$HERDR_SSH_LAB_HOME`**: herdr's managed ssh config
(`[remote].manage_ssh_config`) `Include`s `$HOME/.ssh/config`, so that one
variable is what makes the alias `herdr-ssh-lab` resolvable without herdr ever
reading your real `~/.ssh`.

```console
$ ssh -F "$HERDR_SSH_LAB_SSH_CONFIG" -o BatchMode=yes herdr-ssh-lab \
    'echo $HOME; command -v herdr; herdr --session lab-1 status server --json'
/tmp/herdr-fleet-lab/ssh/home
/tmp/herdr-fleet-lab/ssh/home/.local/bin/herdr
{"status":"running","running":true,"version":"0.9.0-fork","protocol":22,…,"session":"lab-1",…}
```

A fleet host pointing at it is then just:

```toml
[[fleet.hosts]]
name = "lab-ssh"
kind = "ssh"
target = "herdr-ssh-lab"
session = "lab-1"
```

Isolation, by construction: everything lives under `<lab root>/ssh` behind its
own `.herdr-ssh-lab` marker; the sshd listens only on loopback, accepts only the
invoking user with a throwaway key under the lab, forwards nothing, and is told
not to run `~/.ssh/rc` or read `~/.ssh/authorized_keys`/`~/.ssh/environment`;
the remote side runs a wrapper that `exec`s the **fleet lab's own** herdr binary
(the one in the fleet lab marker's `bin=`) under the lab's `XDG_*` dirs, so
`ssh herdr-ssh-lab herdr …` can only reach lab sessions. `down` signals only the
pid this lab's pid file holds — after re-checking its argv and, on Linux, its
executable — then the per-connection children that listener still had, and
deletes only the marked `ssh` directory.

Knobs:

| Variable | Default | Notes |
| --- | --- | --- |
| `HERDR_FLEET_LAB_ROOT` | `/tmp/herdr-fleet-lab` | the ssh lab is `<root>/ssh` |
| `HERDR_SSH_LAB_PORT` | `2299` | unprivileged loopback port; `up` refuses a port already bound |
| `HERDR_FLEET_LAB_TIMEOUT_MS` | `15000` | per-step timeout |
| `HERDR_SSH_LAB_SSHD` | `/usr/sbin/sshd`, `/usr/bin/sshd`, then `sshd` on `PATH` | absolute path; a bad override is a hard error (exit 1), never "sshd not found" |

Exit codes: `0` ok, `1` error, `2` usage, **`3` `sshd not found`**. Exit 3 is the
one that means "this machine cannot run the ssh lab" — CI and validation scripts
treat it as a skip and fall back to `kind = "local"` hosts, so a typo in
`HERDR_SSH_LAB_SSHD` must never produce it.

Two ordering rules:

- **`ssh-lab.sh down` before `fleet-lab.sh down`.** The fleet lab's `down`
  deletes the whole root, including `<root>/ssh` and the sshd pid file, after
  which the sshd can no longer be identified.
- `ssh-lab.sh down` also sweeps the listener's per-connection processes, so the
  `ControlMaster`/`ControlPersist` master herdr opens does not keep the lab
  alive. You never need `ssh -O exit` by hand.

`tests/fork_ssh_lab.rs` exercises the lifecycle inside `just ci`, asserting one
of two explicit outcomes: the lab works end to end, or `up` exits 3 with
`sshd not found`.

## Fleet core

`herdr fleet status` merges the herdr servers listed in the `[fleet]` section of
`config.toml` into one host-qualified view — this machine's default session,
other named sessions on it, and machines reached over SSH.

```toml
[fleet]
include_local = true

[[fleet.hosts]]
name = "workbox"
kind = "ssh"          # "ssh" | "local"
target = "workbox"    # ssh destination; required for kind = "ssh"
session = "agents"    # named session on that host; required for kind = "local"
```

```console
$ herdr fleet status
client 0.9.0-fork  active host: none

HOST     KIND   STATE        VERSION     BLOCKED  WORKING  DONE  IDLE  UNKNOWN
local    local  unavailable  -           0        0        0     0     0
lab-ssh  ssh    connected    0.9.0-fork  0        0        0     0     0
lab-2    local  connected    0.9.0-fork  0        0        0     0     0
  ! local: no herdr server for session default at /tmp/herdr-fleet-lab/xdg/herdr-dev/herdr-client.sock

no agents
```

The multi-machine **console** is upstream's: `herdr machine add <target>`
(`list`, `rename`, `enable`, `disable`, `remove`) saves an SSH machine, and the client's machine sidebar shows every saved
machine's workspaces and agents (upstream #3670). `herdr fleet status` is the
fork's headless view of the same hosts, and the shape the gateway (E3) serves.

`--json` prints the `herdr.fleet.status.v1` report the gateway will serve, and
`--watch` streams one `FleetChange` per line. Full reference — every `[fleet]`
key, the `host/w1:p1` id form, connection states and reasons, ssh host setup and
reconnect behaviour, and the `src/fleet/` module map for E3 — is in
[`fleet-core.md`](./fleet-core.md).

## Gateway

`herdr gateway` serves the fleet over HTTP and WebSocket so a phone or a browser
can watch it. It is loopback-first (`127.0.0.1:7788`), token-gated, and a
passive reader — watching a fleet never resizes anybody's panes (taking control
of a pane, which needs the `control` scope, does).

```bash
herdr gateway                     # run it (foreground; SIGTERM stops it cleanly)
herdr gateway pair [--control]    # one-time pairing URL + QR code for a device
herdr gateway status [--json]     # is one running, and what has it paired
herdr gateway rotate-token read   # replace a token and revoke what it granted
```

`GET /api/fleet` serves the `herdr.fleet.status.v1` report, `/api/events`
streams fleet deltas, and `/api/terminal/{host}/{pane}` streams rendered ANSI
frames (observe, or control with the `control` scope). Full reference — every
`[gateway]` key, the token and device stores, the pairing flow, both WebSocket
contracts including the binary frame header, the `systemd --user` unit and
troubleshooting — is in [`gateway.md`](./gateway.md).

The whole module is behind the `gateway` cargo feature, on by default in fork
builds; `cargo build --no-default-features` yields an upstream-shaped binary
with no `gateway` command, which is what fork CI's
`check-no-default-features` job protects.

## Claude accounts

Usage limits are per Claude account. `[[accounts]]` profiles let each Claude
Code agent run under its own `CLAUDE_CONFIG_DIR`, so a second licence is a flag
instead of a logout.

```toml
[[accounts]]
name = "perso"
config_dir = "~/.claude"
default = true

[[accounts]]
name = "work"
config_dir = "~/.claude-work"
```

```bash
herdr account add work --config-dir ~/.claude-work   # seeds the directory, shares transcripts
herdr account login work                             # types `claude auth login` into a pane
herdr account status                                 # health + oauthAccount identity, never secrets
herdr agent start a1 --kind claude --pane w1:p1 --account work
herdr agent switch-account a1 perso                  # same conversation, other account
herdr account watch                                  # opt-in: label agents that hit their limit
```

In the TUI, right-clicking a pane offers `Start Claude as account...` or
`Switch Claude account...` (local endpoint only). The account is an ordinary
server metadata token (`account`, `account_state`, source `fork:accounts`), so
it shows up in `herdr agent get`, in the sidebar as `$account`, as an `ACCOUNT`
column in `herdr fleet status`, and through the gateway on the phone with no
extra work.

Servers stay stock: the launch types the profile's `CLAUDE_CONFIG_DIR` export
into the pane's shell and then calls the ordinary `agent.start`. Full reference
— every command with its exit codes and JSON, the seed layout, the switch
protocol and its guarantees, usage-limit detection and the `account watch`
label leases, the limitations, and the checklist of what still has to be
verified against a real Claude Code installation — is in
[`accounts.md`](./accounts.md). The design decisions are
[ADR 0003](./decisions/0003-claude-accounts-as-profile-dirs.md).

## Continuous integration

Fork CI is [`.github/workflows/fork-ci.yml`](../../.github/workflows/fork-ci.yml)
— four jobs on `ubuntu-latest`, and their names are what
`gh pr checks <pr> -R vinceseguin/herdr --watch` reports:

| Job | What it runs |
| --- | --- |
| `conventional-commits` | `scripts/conventional_commits.py` on the PR title (PR) or on the pushed subjects (push to `master`) |
| `check (ubuntu-latest)` | `just ci` — the same gate you run locally |
| `check-no-default-features (ubuntu-latest)` | `just ci-no-default` — clippy and nextest for the upstream-shaped build (no bun; Zig still required) |
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
git push -u origin HEAD && gh pr create -R vinceseguin/herdr --base master --fill && gh pr merge -R vinceseguin/herdr --merge --delete-branch
```

A sync PR is merged with a **merge commit** (`--merge`, never `--squash`) so
upstream's history stays in the fork's `master`. Resolve conflicts by
ownership (ADR 0002): upstream's side for `src/client/**`,
`src/remote/attach.rs` and new `src/remote/*` files, `src/server/**`,
`src/api/**`, `src/platform/**`, `docs/next/**`; the fork's side for
`src/fleet/**`, `src/gateway/**`, `scripts/fork/**`, `docs/fork/**`,
`.claude/**`, `.github/workflows/fork-*.yml`, `assets/fork/**`; both sides in
the wiring files of the table below. Then re-apply E1's three
`src/remote/attach.rs` hooks onto upstream's version (or adapt
`src/fleet/transport/ssh.rs` to an equivalent upstream bridge, and say so in
the merge commit).

After every sync, re-run `gh workflow list --all -R vinceseguin/herdr` and
`gh workflow disable <file>` any **new** upstream workflow the merge added.

Fork-owned paths never conflict — whole directories (`docs/fork/`, `.claude/`,
`scripts/fork/`, including `fleet-lab.sh`, `ssh-lab.sh`, `gate.sh` and
`dev-setup.sh`; `src/fleet/` and `src/gateway/`, and `web/` — whose committed
`web/dist` E3 embeds and E4 fills in) plus fork-only files that live inside
upstream directories: `src/cli/fleet.rs`, `.github/workflows/fork-*.yml`, every `tests/fork_*.rs`
(today `tests/fork_channel.rs`, `tests/fork_fleet_lab.rs`,
`tests/fork_gateway.rs`, `tests/fork_ssh_lab.rs`), `tests/support/fleet_lab.rs`
and `tests/cli/fleet.rs`.

The upstream files that currently carry fork wiring, and may conflict:

| File | Fork wiring |
| --- | --- |
| `.cargo/config.toml` | the `[env]` build channel |
| `src/build_info.rs`, `src/update.rs`, `src/release_notes.rs` | fork build identity, self-update disabled |
| `Cargo.toml`, `Cargo.lock` | the `[features]` section (`default = ["gateway"]`), the four optional gateway dependencies (`axum`, `qrcode`, `subtle`, `getrandom`) and `tokio`'s `net`/`signal` features (E3 PR 1) |
| `justfile` | `lint-no-default` and `ci-no-default` (E3 PR 1) |
| `src/main.rs` | `mod fleet;`, gated `mod gateway;`, the `[fleet]` block of `DEFAULT_CONFIG`, two `--help` usage lines, `"fleet"` and gated `"gateway"` in the bare-command list |
| `src/remote/attach.rs` | `pub(crate)` visibility on the ssh stdio bridge and remote discovery, plus `start_with`/`local_forward_socket_path_scoped`/`BridgeErrorSink` (E1 PR 3) — upstream's side first, then re-apply |
| `src/cli.rs`, `src/cli/spec.rs` | one `mod fleet;` + match arm and a gated `"gateway"` arm, `fleet_command()` and a gated `gateway_command()` (with its `pair`/`status`/`rotate-token` subcommands, E3 PR 8) |
| `build.rs` | one `cargo:rerun-if-changed=web/dist`, so a rebuilt web app re-embeds (E3 PR 4) |
| `src/config/model.rs`, `src/config/io.rs`, `src/config.rs` | the `[fleet]` and `[gateway]` sections, their `KNOWN_TOP_LEVEL_CONFIG_KEYS`/live-reload entries, their diagnostics, and the feature-gated `parse_gateway_origin`/`GatewayConfig`/`GatewayOrigin` re-exports |
| `src/client/mod.rs`, `src/client/terminal_sessions.rs` | `terminal_control_command_from_json` widened to `pub(crate)` and re-exported under `#[cfg(any(test, feature = "gateway"))]`, so the gateway parses the CLI's own terminal-control vocabulary (E3 PR 6) |
| `scripts/config_reference_check.py` | `SKIPPED_SUBTREES` entries for `fleet` and `gateway` |
| `src/app/mod.rs` (tests only), `tests/support/mod.rs`, `tests/support/gateway.rs`, `tests/api_ping.rs`, `tests/cli/sessions.rs`, `tests/cli/mod.rs` | fork test wiring |
| `.gitignore`, the fork section at the tail of `AGENTS.md` | fork layout and rules |

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
