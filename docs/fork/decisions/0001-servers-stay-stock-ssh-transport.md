# ADR 0001 — Servers stay stock; SSH is the transport; one loopback-first gateway

**Status:** accepted (2026-09-05). **Applies to:** every epic in `../ROADMAP.md`.

## Context

The fork needs (1) one console showing several LAN machines' herdr sessions,
(2) a phone app, and (3) access away from home. Upstream herdr talks to a
server only through owner-only local sockets (`0600`), reaches a remote server
by piping that socket over SSH (`herdr --remote`, `src/remote/attach.rs`), and
publishes a frozen "generation 1" client endpoint contract
(`src/protocol/endpoint.rs`) that explicitly names Local, SSH and Cloud
connections and requires unavailable servers to be a client-local outcome.

## Options considered

1. **Add a TCP/TLS listener with its own auth to the herdr server.** Every LAN
   host would need the fork; the server grows a network attack surface and a
   credential store; the wire protocol (`PROTOCOL_VERSION`, bincode frames)
   becomes a public interface we would have to version against upstream.
2. **A cloud relay** (as `herdr-mobile-relay` / `herdr-remote` do). Works from
   anywhere, but sends agent terminals — code and secrets — through a third
   party or a self-hosted VPS, and adds accounts, pairing, and an always-on
   service to run.
3. **An external plugin that mirrors remote servers** (as `herdr-mirror` does)
   via `herdr terminal session observe/control`. Zero fork, but the mirrors are
   fake local panes, status is one-way, and the phone still needs a bridge.
4. **Aggregate in the client over SSH; add one gateway for phones; use a VPN
   off-LAN.** The TUI holds N client-socket streams (local sessions and SSH
   bridges), merges their generation-1 snapshots in a pure `FleetState`, and
   renders one active host. A `herdr gateway` reuses that core and serves
   HTTP/WebSocket on loopback (or a Tailscale address) with a token. Off-LAN,
   Tailscale carries both SSH and the gateway; `tailscale serve` gives HTTPS.

## Decision

Option 4.

- **Servers stay stock.** No server code or wire-protocol change. LAN hosts may
  run upstream herdr. Any future server-side need is an *advertised optional*
  endpoint method that degrades gracefully.
- **SSH is the only host-to-host transport.** Authentication, encryption, and
  key management are OpenSSH's; `[remote].manage_ssh_config` semantics carry
  over. Named local sessions are a second "host kind" so a fleet is testable on
  one machine.
- **The gateway is the one network surface**, loopback by default, token-gated
  and origin-checked off loopback, documented on a tailnet address, never on
  the internet.
- **Off-LAN = Tailscale**, not a relay. WireGuard mesh, MagicDNS names as SSH
  targets, `tailscale serve` for a valid HTTPS certificate (needed for PWA
  install and Web Push on iOS).

## Consequences

- Only the console machine and the gateway host need fork binaries.
- New code is additive (`src/fleet/`, `src/gateway/`, `web/`), so upstream
  merges stay cheap.
- Mixed-host pane layouts are out of scope for v1: the render path stays
  single-endpoint; the sidebar is what aggregates.
- The SSH stdio bridge in `src/remote/attach.rs` must be refactored into a
  reusable transport (E1) — the one non-trivial edit to an upstream module.
- Push notifications and installability depend on HTTPS, which the design gets
  from Tailscale rather than from certificates the gateway manages itself.

### E0 review (2026-09-05)

E0 built the fork's toolchain, CI, build identity and fleet lab against the real
codebase. Nothing contradicted the *Decision*; four facts are worth recording so
later epics do not re-derive them.

- **`--remote` compatibility is generation-based, not version-based.**
  `remote_server_restart_reason` (`src/remote/attach.rs`) compares the server's
  `endpoint_protocol_generation` against `ENDPOINT_PROTOCOL_GENERATION` and
  whether it detaches its daemon; no version string enters that decision. A
  `0.8.2-fork` client therefore attaches to a stock `0.8.2` server unchanged —
  which is exactly what "servers stay stock" needs. The client's own version
  reaches only the display (`herdr status` reports `server_binary_stale: true`
  from a plain string compare; cosmetic) and the seeding lookup below; live
  handoff's `--expected-version` carries the *prepared remote binary's*
  version, not the client's.
- **A fork client cannot seed a *foreign-platform* remote.**
  `resolve_install_source` uploads the local binary only when the remote
  platform equals the local one *and* the running exe is not a
  package-manager-managed path; otherwise it looks the client's own version up
  in upstream's release manifest, which will never contain `0.8.2-fork`. The
  failure is a clear error — `release manifest does not include herdr
  0.8.2-fork; build herdr for <asset> or install it there manually` — but note
  that this message does not name the escape hatch: pre-install herdr on that
  host, or point `HERDR_REMOTE_BINARY` at a binary built for it. Consistent
  with the decision — E1 must surface it as a host-local
  `Unavailable { reason }` and never as a fleet-wide failure. Same-platform
  seeding does upload *this* binary, so a host seeded by a fork client runs a
  fork build: stock server behaviour, but version `x.y.z-fork` with self-update
  disabled.
- **Named local sessions are a real second host kind.**
  `scripts/fork/fleet-lab.sh` boots N isolated `herdr --session lab-N server`
  processes on one machine, and E1's `kind = "local", session = "lab-N"` host
  config maps onto them 1:1. A fleet is fully testable without a second
  machine.
- **Config directory names differ by build profile.** `config::io::app_dir_name`
  is `herdr-dev` for debug builds and `herdr` for release, so sessions live
  under `$XDG_CONFIG_HOME/<app>/sessions/<name>/`. `HERDR_CONFIG_PATH` outranks
  `XDG_CONFIG_HOME` for the config *file* (`config_path()`), while the session
  directories keep following `XDG_CONFIG_HOME` (`config_dir()`). Fixtures, the
  fleet lab and every later test must derive the path instead of hardcoding
  `herdr`, and must drop `HERDR_CONFIG_PATH` when isolating.

Nothing in *Decision* is amended.

### E1 review (2026-09-05)

E1 built the fleet core against the real codebase: `[fleet]` configuration,
the pure `FleetState`/`FleetStatusReport`, the per-host connector, the ssh
transport, and `herdr fleet status`. Option 4 held — servers were not touched,
`src/protocol/**` and `tests/fixtures/endpoint-*.json` have an empty diff, and a
fleet host runs a stock server. Six facts are worth recording.

- **The SSH bridge was widened in place, not moved.** The decision's
  "must be refactored into a reusable transport" was implemented as the
  smallest behaviour-preserving diff to `src/remote/attach.rs`: the existing
  types stay there with `pub(crate)` visibility, plus three injection points —
  `SshStdioBridge::start_with(…, BridgeErrorSink)`, a `scope` argument on
  `local_forward_socket_path_scoped`, and `discover_remote_herdr` split out of
  `prepare_remote_herdr` — while the fleet-side adapter lives in
  `src/fleet/transport/ssh.rs`. Moving ~800 lines into `src/fleet/` would have
  turned every upstream edit to the bridge into a conflict against deleted code
  and made `herdr --remote` depend on the fleet module. The unscoped socket
  names, readable *and* hashed, are byte-identical to upstream's, so
  `--remote`'s behaviour is unchanged.
- **The fleet never installs, uploads, stops or hands off a herdr.** The E0
  review above noted that a fork client cannot seed a foreign-platform remote
  and that E1 must surface that as a host-local `Unavailable { reason }`. E1
  went further: the connector uses *discovery only*, ignores
  `HERDR_REMOTE_BINARY`, and never prompts. A host with no generation-1 herdr
  reports ``no herdr with endpoint generation 1 on host; run `herdr --remote
  <target>` once to install it``. `herdr --remote` — interactive, allowed to
  install and hand off — stays the one place that changes a host, run once by
  the operator. This keeps "servers stay stock" true of the *fleet* as well as
  of the protocol.
- **`state_change_seq` is not comparable across hosts.** It is a per-server-boot
  counter, so a merged, blocked-first list ordered by it would be meaningless
  across a fleet. `FleetState` therefore owns a monotonic `fleet_change_seq`
  assigned when an agent appears or its status advances, and the fleet order is
  `(status_rank, Reverse(fleet_change_seq), host_index, pane_id)`. Every later
  epic that ranks agents across hosts must use `merged_agents()`; upstream's
  `status_priority` stays correct only *within* one host.
- **A read-only fleet client is not passive today.** A connecting client shell
  becomes that host's foreground client, and the foreground client's surface is
  the host's effective pane geometry — so a fleet client with a small surface
  reflows every pane on every configured host. E1 handshakes inactive hosts at
  herdr's own default headless geometry, which makes the common case (headless
  servers running agents) a no-op, but a host with an attached client or a
  `[server]` `headless_cols`/`headless_rows` of its own is still resized while
  the fleet is connected. A
  genuinely passive reader would need an *advertised optional* endpoint
  observer method — exactly the escape hatch the decision reserves — and is out
  of scope until an epic needs it. Until then a fleet consumer (the gateway,
  E3 — the fork's console was retired by ADR 0002) must hold fleet
  connections only while something is reading them.
- **A user-space sshd is a sufficient SSH stand-in.** `scripts/fork/ssh-lab.sh`
  runs sshd as the invoking user on `127.0.0.1:2299` with a throwaway key, an
  in-lab `HOME`, and `SetEnv HOME/XDG_CONFIG_HOME/PATH`, so the whole ssh path —
  managed ssh config, control master, discovery, the stdio bridge, reconnect —
  is exercised without root, without a second machine, and without touching the
  caller's `~/.ssh` or installed herdr. Combined with `kind = "local"` hosts
  from the fleet lab, a full mixed-transport fleet is testable on one laptop.
  Machines that have no `sshd` are handled explicitly (exit 3), not silently.
- **Host failure is local, and provably so.** Each host has its own supervisor
  thread, backoff (1 s → 30 s) and reason string; a live run that cut the ssh
  lab out from under a connected host produced connection changes for that host
  only, with `lab-2` and `local` emitting none. That is the "unavailable servers
  are a client-local outcome" clause of the endpoint contract, honoured by the
  fleet as well as by the single-host client.

Nothing in *Decision* is amended.
