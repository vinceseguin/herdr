# ADR 0002 — Adopt upstream's multi-machine client; retire the fork's fleet TUI (E2)

**Status:** accepted (2026-09-06). **Supersedes:** the E2 half of
`../ROADMAP.md` and `../plans/e2-fleet-tui.md`. **Amends:** nothing in
[ADR 0001](./0001-servers-stay-stock-ssh-transport.md)'s *Decision*.

## Context

Epic E2 built a fork-owned console: `herdr fleet` opened the client-owned shell
over the E1 `FleetConnector`, drew one host group per `[[fleet.hosts]]` entry
in the sidebar, switched the *active host* by click or through a
`[fleet.keys] host_picker` overlay, prefixed notifications with `[host]`, and
showed a reconnect notice when the active host dropped. It shipped in fork
PRs #22–#31 and was marked done in #32.

One day later upstream merged
[herdrdev/herdr#3670](https://github.com/herdrdev/herdr/pull/3670)
(`9e9bc8a1`, "feat: manage multiple ssh machines from one client", ~15k
lines). It adds saved SSH machine profiles (`herdr machine add|list|…`,
`src/remote/saved.rs`), a machine sidebar that lists every machine's
workspaces and agents in one client, background connects, health probes and
automatic reconnects — the E2 feature set, in upstream's own UI language —
and rewrites `src/client/mod.rs`, `src/remote/attach.rs` and 23 other files
the E2 work had edited.

Keeping both would mean carrying two sidebars, two host models and two
switching UXs in one binary, and paying a large conflict on every upstream
client change — the exact cost principle 4 of the roadmap ("additive,
mergeable code") exists to avoid.

## Options considered

1. **Keep E2 and merge upstream around it.** Every E2 seam (`ServerLink`,
   `ClientLoopEvent::Fleet`, the sidebar row split, the overlay hooks) lands
   on lines upstream just rewrote. The first sync would be a rewrite of the
   console, and every later sync a fight.
2. **Keep E2, drop upstream's client.** Diverges the fork from the one part
   of upstream that moves fastest, and gives up upstream's future machine
   features (health probes, reconnect policy, per-machine settings).
3. **Adopt upstream's client, retire E2, keep E1 headless.** The console
   becomes upstream's machine sidebar. E1's `src/fleet/` — `FleetState`, the
   connector, the ssh transport, `herdr fleet status` — stays as the headless
   aggregator the gateway (E3) is built on; nothing in it touched
   `src/client/**`.

## Decision

Option 3.

- **Upstream's client is the console.** Multi-machine viewing and switching
  in the TUI is `herdr machine …` plus upstream's machine sidebar. The fork
  adds no client-side host model, sidebar group, picker or notification
  prefix of its own.
- **E2 is retired.** Fork PRs #22 and #24–#32 are reverted in one PR
  (`chore: retire the fork fleet tui superseded by upstream multi-machine
  client`, fork PR #33). PR #23 (connector client options, active-surface tracking,
  `take_events`) stays: it lives entirely in `src/fleet/connector.rs` and is
  what a long-lived consumer such as the gateway drives the connector with.
- **E1 stays, headless.** `src/fleet/**` (pure state, connector, transports,
  report, `herdr fleet status`) is the gateway's aggregator and the shape of
  `/api/fleet`. It may source hosts from upstream's saved machine profiles
  (`src/remote/saved.rs`) in addition to `[[fleet.hosts]]`; E3 decides.
- **Sync policy.** On every `git merge upstream/master`:
  - take **upstream's side** for `src/client/**`, `src/remote/attach.rs` and
    any new `src/remote/*` file, `src/server/**`, `src/api/**`,
    `src/platform/**`, `docs/next/**`;
  - keep the **fork's side** for `src/fleet/**`, `src/gateway/**`,
    `scripts/fork/**`, `docs/fork/**`, `.claude/**`,
    `.github/workflows/fork-*.yml`, `assets/fork/**`;
  - take **both** in `src/cli.rs`, `src/cli/spec.rs`, `src/main.rs` (upstream's
    wiring plus the fork's `fleet status` wiring and fork-channel bits), in
    `src/update.rs` and `src/app/mod.rs` (the fork-channel guard), and in
    `tests/api_ping.rs` / `tests/cli/**` (channel-aware version assertions via
    `support::build_version()`).
  - E1's three hooks in `src/remote/attach.rs` (`pub(crate)` visibility on the
    ssh stdio bridge and discovery, `SshStdioBridge::start_with(…,
    BridgeErrorSink)`, `local_forward_socket_path_scoped`,
    `discover_remote_herdr`) are re-applied onto upstream's version at merge
    time, unless upstream now provides an equivalent reusable bridge — in
    which case `src/fleet/transport/ssh.rs` adapts to it and the merge commit
    body records the choice.

## Consequences

- The fork's user-facing surfaces are now the **gateway** (E3), the **phone
  app** (E4), off-LAN access (E5), push (E6) and the release pipeline (E8).
  The console is upstream's.
- **E7's console half is upstream's.** "Prompt an agent on another host from
  the console" is whatever upstream's machine client offers; the fork's E7
  scope is the gateway/phone half. E7 and E8 now depend on **E3 and E4**, not
  on E2.
- `[fleet.keys]`, `herdr fleet` (bare), `herdr --fleet`, `docs/fork/fleet.md`,
  `scripts/fork/tui-drive.py`, `tests/fork_fleet_tui.rs`,
  `tests/support/fleet_tui.rs` and `just bench-fleet-scale` no longer exist.
  `herdr fleet status` remains.
- The E2 work is preserved in git history (fork PRs #22–#31, reverted by the
  PR named above) and in `../plans/e2-fleet-tui.md`, which keeps its PR log
  under a "superseded" banner. Two E2 findings still hold for the gateway: a
  connecting client shell becomes the host's foreground client and sets its
  effective pane geometry (hold fleet connections only while a consumer needs
  them), and the ssh child's stderr is inherited (a daemon must redirect it).
- Upstream syncs get cheaper: after this decision the only upstream files
  carrying fork wiring are the fork-channel guard (`src/update.rs`,
  `src/build_info.rs`, `src/release_notes.rs`), the `[fleet]` config plumbing
  (`src/config/*`), the `fleet status` CLI wiring (`src/cli.rs`,
  `src/cli/spec.rs`, `src/main.rs`), the E1 hooks in `src/remote/attach.rs`,
  and test support. `docs/fork/README.md` keeps the authoritative table.

### Upstream sync 2026-09-06 (`chore: sync upstream master`)

_Filled in by the sync PR that follows the revert: how the E1 hooks were
reconciled with upstream's rewritten `src/remote/attach.rs`, and which
upstream-provided bridge, if any, `src/fleet/transport/ssh.rs` now uses._
