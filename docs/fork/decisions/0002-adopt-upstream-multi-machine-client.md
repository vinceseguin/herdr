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

### Upstream sync 2026-09-06 (`chore: sync upstream master through e366a05f`)

The first sync after the revert merged upstream `35b0dff9..e366a05f`
(including #3670). After PR #33 only two files conflicted: `src/main.rs` (the
bare-command list — both `"fleet"` and `"machine"` kept) and
`src/remote/attach.rs`. Upstream's `attach.rs` was taken whole, then E1's
hooks were re-applied on top of it as the smallest diff that keeps
`src/fleet/transport/ssh.rs` compiling:

- **No upstream bridge was adopted.** Upstream's new `src/client/transport.rs`
  is the endpoint reader/writer for the client loop, not an ssh bridge, and its
  saved-machine path (`prepare_saved_ssh`, `find_installed_remote_herdr`,
  `RemoteSsh::new_noninteractive`, `probe_remote_endpoint`) is built for the
  client's own endpoint registry: it *requires* surface-interest support on
  the remote and installs/updates herdr interactively. The fleet keeps its own
  discovery-only, install-never contract, so the E1 hooks stay.
- `pub(crate)` on `RemoteHerdr`, `ManagedSshOptions`, `RemoteSsh` (and its
  `new`/`options`), `SshStdioBridge`, `remote_bridge_command` — upstream
  narrowed them to `pub(super)`/private, which `pub(crate) use attach::*` does
  not re-export.
- `discover_remote_herdr(ssh) -> io::Result<Option<RemoteHerdr>>` is now a
  standalone helper next to upstream's `find_installed_remote_herdr`, probing
  with `remote_binary_supports_endpoint_requirement(…, false)` — generation
  match only, as before. Upstream's `prepare_remote_herdr` is untouched.
- `SshStdioBridge::start_with(…, noninteractive, BridgeErrorSink)`: upstream's
  `start` gained a `noninteractive` flag and logs through `tracing` in that
  mode; the sink gained a `Log` variant so `start` reproduces upstream's stderr
  and tracing output byte-for-byte in both modes, while the fleet's `Report`
  sink keeps its messages. `RemoteSsh::new` now takes the session name;
  `SshTransport` passes its own and stays interactive (managed ssh config,
  control master), exactly as `herdr --remote` does.
- `local_forward_socket_path_scoped` / `short_socket_hash_scoped` merged
  cleanly and were kept.

Two fleet-side adaptations outside `attach.rs`: upstream deleted
`ipc::shutdown_local_stream_write` (the fleet connector was its last caller),
so the half-close now lives in `src/fleet/connector.rs`; and the generation-1
hello gained `surface_active` (default `true`), which the fleet sends as
`true` — the pre-sync behaviour. Sending `false` for hosts with no active
surface is upstream's new passive-reader hook, exactly what the E1 review
asked for; E3 may adopt it.

Upstream syncs remain a merge commit (`gh pr merge --merge`), so upstream's
history stays in the fork's `master`.
