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
