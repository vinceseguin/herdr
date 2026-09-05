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

The repo needs: Rust per `rust-toolchain.toml`, `just`, `cargo-nextest`,
`bun`, Zig 0.15.2 (for the vendored libghostty-vt), `python3`. E0 delivers
`scripts/fork/dev-setup.sh` to install them through `mise`. Until then:

```bash
mise use -g rust@1.96.1 just bun zig@0.15.2
cargo install cargo-nextest --locked
just ci                      # the fork gate: fmt, clippy -D warnings, nextest, maintenance tests
```

Run the gate through the wrapper when agents may be building in parallel:

```bash
bash scripts/fork/gate.sh            # runs `just ci` under a machine-wide lock; read EXIT=
```

Test a debug build against isolated servers, never your live herdr:

```bash
export XDG_CONFIG_HOME=/tmp/herdr-dev
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- --session lab-a server &
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- --session lab-a status server
```

## Keeping up with upstream

```bash
git fetch upstream
git switch -c chore/sync-upstream-$(date +%Y%m%d) origin/master
git merge upstream/master          # merge, never rebase master
bash scripts/fork/gate.sh          # EXIT=0
git push -u origin HEAD && gh pr create --base master --fill && gh pr merge --squash --delete-branch
```

Fork-owned paths (`docs/fork/`, `.claude/`, `scripts/fork/`, `src/fleet/`,
`src/gateway/`, `web/`, `.github/workflows/fork-*.yml`) never conflict; the
few upstream files with fork wiring (`src/main.rs`, `src/cli/spec.rs`,
`src/config/model.rs`, `Cargo.toml`, `.gitignore`, `AGENTS.md` tail) may.

Upstream's own CI/release workflows are disabled on this repository with
`gh workflow disable` (E0); the fork's CI is `.github/workflows/fork-ci.yml`.

## Repository notes

- The fork is **public** — GitHub forks of public repositories cannot be
  private. Never commit tokens, SSH targets, or anything from `~/.config/herdr`.
- Upstream ignores `docs/*` except `docs/next|preview|versions`; `.gitignore`
  carves out `docs/fork/` so these files are tracked.
- Commits: lowercase conventional subjects (`feat fix perf docs ci test
  refactor chore release`), plus the `Co-Authored-By` / `Claude-Session`
  trailers this environment specifies.
