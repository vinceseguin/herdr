# Fork rules — `vinceseguin/herdr`

These rules bind every agent working in this repository. They are read together
with the root `AGENTS.md` (which `CLAUDE.md` symlinks to). Where the two
disagree, **this file wins**, because it describes the fork and `AGENTS.md`
describes upstream.

## What still applies from `AGENTS.md`

- **Universal Project Rules** — Principles, Multiplicative performance paths,
  Runtime/client boundary guardrail, Stable client endpoint contract. Verbatim.
- **Testing** — `just` recipes, `#[cfg(test)]` unit tests next to the code,
  `AppState::test_new()`-style pure-state testing, invariants for identity/state
  refactors, and the `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH cargo run -- …`
  recipe when testing a debug build from inside a running herdr.
- **Code Conventions** — no `unwrap()` in production code, `tracing` for logs,
  `#[allow]` only with a reason, compile-gated platform code, no dependency
  without a reason, protocol-version discipline.
- **Docs** — `docs/next/` is upstream's draft doc tree; do not edit it or
  `skills/herdr/SKILL.md`, `CHANGELOG.md`, root `README.md`, `docs/preview/`,
  `docs/versions/`, or `distribution/latest.json` for fork work. Fork docs go in
  `docs/fork/`.
- **Agent Detection Updates** and **Vendored libghostty-vt** — unchanged, if
  you ever touch them (you normally will not).

## What does NOT apply

- **Maintainer Workflow**, **Local Can Machine Workflow**, **Release Channels**
  (maintainer actions), and the **External contributor guardrail**. This is a
  private-purpose fork owned by `vinceseguin`; there is no maintainer approval
  step, no bot review to wait for, no `.github/APPROVED_CONTRIBUTORS` check, and
  PRs are merged by the agent that opened them once fork CI is green. Never
  open issues or PRs against `herdrdev/herdr` from this fork unless the user
  explicitly asks.
- The "no AI co-author lines" clause of **Commit Style**. See below.

## Fork conventions

### Branches and remotes

- `origin` = `git@github.com:vinceseguin/herdr.git` (the fork). `upstream` =
  `herdrdev/herdr`. The default branch is **`master`**, matching upstream.
- Task branches: `feat/<epic>-pr<N>-<slug>` (or `fix/…`, `docs/…`, `ci/…`),
  always created from the latest `origin/master`. Never commit to `master`
  directly; every change lands through a squash-merged PR on the fork.
- Upstream sync is a **merge**, never a rebase of `master`:
  `git fetch upstream && git merge upstream/master`, resolve, run the gate,
  push. Do it at epic boundaries at least.
- Parallel agents work in **isolated worktrees** under
  `.claude/worktrees/<branch-slug>` (gitignored). Never run bare `git` from an
  unknown cwd: always `git -C <worktree> …`. Never touch another agent's
  worktree or the root checkout's working tree.

### Commits and PRs

- Lowercase conventional commits, types from `scripts/conventional_commits.py`
  (`feat fix perf docs ci test refactor chore release`), no emojis, descriptive
  subject. The repo hook `.githooks/commit-msg` enforces the subject; fork CI
  enforces the PR title the same way.
- End every commit message with the trailer the environment specifies
  (currently `Co-Authored-By: Claude …` plus the `Claude-Session:` line). This
  overrides upstream's "no AI co-author lines" — upstream's reason (release
  notes fed from commit subjects) does not apply to the fork.
- Do not use `refs #<n>` unless the fork actually has that issue.
- PR body ends with the `🤖 Generated with [Claude Code]` line the environment
  specifies. Squash-merge with `--delete-branch`.

### Where fork code lives (additive, mergeable)

- New modules: `src/fleet/`, `src/gateway/` (feature `gateway`), `web/`,
  `scripts/fork/`, `docs/fork/`, `.claude/`, `.github/workflows/fork-*.yml`.
- Edits to upstream files are the minimum wiring needed (a `mod fleet;`, a CLI
  subcommand arm, a config struct field with a default). A change that reshapes
  an upstream file needs a written reason in the plan's PR section.
- **Never** change `src/protocol/wire.rs` or the endpoint contract
  (`src/protocol/endpoint.rs`, `tests/fixtures/endpoint-*.json`). If a server
  feature is genuinely required, it is a new advertised optional method that
  older servers may lack, and the client must degrade gracefully.
- `docs/*` is gitignored upstream; `.gitignore` carves out `docs/fork/`. Put
  every fork document, plan and ADR there.

### The gate (every PR)

```bash
bash scripts/fork/gate.sh <worktree>        # runs `just ci` under a machine-wide lock
echo "EXIT=$?"                              # the only thing that decides green/red
```

`just ci` = `cargo fmt --check` + `cargo clippy --all-targets -D warnings` +
`cargo nextest run` + the python maintenance tests + the bun test suites. Never
pipe the wrapper into `tail`/`head`/`grep` (the pipeline would report the
pager's exit code). While iterating use `just test-one <filter>` or
`cargo clippy`, but the gate before commit is the full wrapper. Only **one**
Rust build/test run at a time on this machine — the wrapper's lock enforces it;
do not bypass it with bare `cargo` in parallel with another agent.

Toolchain: Rust per `rust-toolchain.toml`, `just`, `cargo-nextest`, `bun`,
Zig 0.15.2 (libghostty-vt), `python3`. `scripts/fork/dev-setup.sh` (E0)
installs them through `mise`.

### Real-server validation (every PR, not optional)

The unit gate proves the code compiles and tests pass. Before a PR, exercise
the change against **real running herdr servers**:

- Build the binary (`cargo build`), then run it with an isolated config dir so
  you never touch the user's live herdr: `XDG_CONFIG_HOME=/tmp/herdr-<task>`
  plus `env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH`.
- Use `scripts/fork/fleet-lab.sh up <n>` (E0) to get N independent servers as
  named sessions, or start them by hand with `herdr --session <name> server`.
- Drive the real path: `herdr fleet status --json`, `herdr api snapshot`,
  `herdr terminal session observe …`, the gateway's HTTP/WS endpoints with
  `curl`, the TUI via a PTY harness like `tests/multi_client.rs`.
- Capture evidence (JSON output, status codes, frame bytes, screenshots) for the
  report, and tear everything down (`fleet-lab.sh down`, `herdr --session <n>
  server stop`).

### Safety

- Never commit tokens, keys, `hosts.toml`-style personal SSH targets, or
  anything from `~/.config/herdr`. Test fixtures use `localhost`, named
  sessions, and throwaway `XDG_CONFIG_HOME` dirs.
- Never run `herdr server stop` or `herdr update` against the user's real
  session while validating. Isolate with `XDG_CONFIG_HOME`.
- Never disable, delete, or weaken tests, lints, or the endpoint fixtures to get
  green.
