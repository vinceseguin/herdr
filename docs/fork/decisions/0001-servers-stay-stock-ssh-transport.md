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
