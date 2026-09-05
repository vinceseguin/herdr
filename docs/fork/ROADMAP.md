# herdr fork — Roadmap (epics)

This is `vinceseguin/herdr`, a fork of [`herdrdev/herdr`](https://github.com/herdrdev/herdr)
(Apache-2.0). Upstream herdr is a background server that owns your coding
agents' terminals plus a TUI client attached to **one** server on **one**
machine. This fork adds three things upstream does not have:

1. **One console for every machine on the LAN.** Several computers each run a
   herdr server; a single `herdr fleet` TUI on any of them shows every host's
   workspaces and agents at once, with live status, and lets you work in any of
   them.
2. **A phone app.** An installable web app (PWA) served by a small gateway on
   one LAN machine shows every agent on every machine, streams any terminal
   live, and lets you answer a blocked agent from the phone.
3. **Access away from home** through a private network (Tailscale), never
   through an internet-exposed port.

This document is the epic-level plan. Each epic is broken into PRs when it is
implemented (skill `plan-epic` → `docs/fork/plans/`), then built incrementally
(`implement-epic` → `implement-task`). `implement-roadmap` drives all of it in
dependency order and owns the **Epic status** table below.

---

## Guiding principles (non-negotiable)

They constrain every epic. They exist because this fork must stay mergeable
with a fast-moving upstream and because it puts agent terminals — which contain
source code and secrets — on a network.

1. **Servers stay stock.** Everything new lives in the client, in a new `fleet`
   module, and in a new `gateway`. A LAN host may run upstream herdr or this
   fork interchangeably. No wire-protocol change (`src/protocol/wire.rs`) and no
   server behaviour change unless an epic proves it unavoidable — and then it
   is an *advertised optional* endpoint method that degrades gracefully, per
   upstream's "Stable client endpoint contract" in `AGENTS.md`.
2. **SSH is the transport. Never a new listening port on the server.** Hosts are
   reached the way `herdr --remote` already reaches them: an SSH stdio bridge
   to the remote client socket (`src/remote/attach.rs`, `src/remote/host_unix.rs`).
   Authentication and encryption are OpenSSH's. Off-LAN, the same SSH runs over
   a VPN. herdr never authenticates network peers itself.
3. **The gateway is loopback-first and token-gated.** `herdr gateway` binds
   `127.0.0.1` by default. Binding anything else requires a token and an origin
   allowlist, and the documented deployment puts it on a Tailscale address, not
   the internet. Tokens live in `0600` files under the config dir; nothing
   secret is ever committed.
4. **Additive, mergeable code.** New code goes in new modules (`src/fleet/`,
   `src/gateway/`, `web/`, `docs/fork/`, `scripts/fork/`, `.claude/`). Edits to
   upstream files are the minimum needed to wire a module in. `master` is
   synced by `git merge upstream/master` (never rebase), ideally weekly. A
   change that would rewrite the shape of an upstream file needs a written
   reason in the plan.
5. **Upstream's universal rules still bind.** `AGENTS.md` → "Universal Project
   Rules" and "Code Conventions" apply verbatim: state separated from runtime
   (pure `FleetState`, testable without sockets), pure render, no god objects,
   platform code isolated, multiplicative-perf discipline in render/fanout
   loops, no `unwrap()` in production code, `tracing` for logs, no dependency
   without a reason. The maintainer-only, Can-machine-only and
   external-contributor sections of `AGENTS.md` do **not** apply here — see
   `.claude/rules/fork.md`.
6. **Locally buildable and validatable.** Every PR is proven against **real
   running herdr servers on this machine**: named sessions
   (`herdr --session <name>`) under an isolated `XDG_CONFIG_HOME` are
   independent servers, so a "LAN of N hosts" is `scripts/fork/fleet-lab.sh up N`
   with zero extra hardware. A green unit gate is not "done".
7. **Read is safe, control is explicit.** Viewing a terminal from the console or
   the phone can never change it. Sending input, approving, starting or
   stopping anything is a distinct, visibly-toggled capability with its own
   token scope. Nothing destructive happens from the phone without a confirm.
8. **Host failure is local.** One host unreachable, incompatible, or slow never
   disconnects the others, never exits the console, and never blocks the
   gateway. This is upstream's rule for client-owned shells, applied across
   hosts.

---

## Architecture map

Stack: **Rust** (edition 2021, toolchain pinned in `rust-toolchain.toml`),
`tokio`, `ratatui`, `interprocess` local sockets, vendored `libghostty-vt`
(built with **Zig 0.15.2** by `build.rs`), `just` recipes, `cargo nextest`,
`bun` for the few JS test suites, `python3` maintenance scripts. The web app
adds **TypeScript** (Vite) with **xterm.js**.

| Concern | Where it lives |
| --- | --- |
| Server: panes, PTYs, agents, sessions, socket API | `src/server/`, `src/app/`, `src/api/`, `src/pane/` — **untouched** |
| Wire protocol (private) and stable endpoint contract | `src/protocol/wire.rs`, `src/protocol/endpoint.rs` — **untouched** |
| TUI client, client-owned shell (snapshot, sidebar, input) | `src/client/`, `src/client/shell/` |
| SSH stdio bridge used by `herdr --remote` | `src/remote/attach.rs` (`SshStdioBridge`, `prepare_remote_herdr`, `ensure_remote_server_ready`), `src/remote/host_unix.rs` |
| **Fleet core** — hosts config, connector, merged pure state | `src/fleet/` (new) |
| **Fleet TUI** — host groups in sidebar, host switching | `src/client/shell/` additions + `src/fleet/` |
| **Gateway** — HTTP/WebSocket for phones and browsers | `src/gateway/` (new), behind a cargo feature |
| **Phone app** — installable PWA | `web/` (new), built assets embedded in the gateway |
| Fork docs, plans, ADRs | `docs/fork/` (`docs/*` is gitignored upstream; `.gitignore` carves this out) |
| Fork scripts: dev setup, gate wrapper, fleet lab, install | `scripts/fork/` |
| Agent skills that build this roadmap | `.claude/skills/`, `.claude/rules/fork.md` |
| Fork CI | `.github/workflows/fork-ci.yml` (upstream workflows disabled on the fork) |

Key upstream facts every plan builds on (verified on `master` @ 0.8.2):

- A client connects to `<config>/herdr-client.sock` (or
  `<config>/sessions/<name>/herdr-client.sock`), sends `ClientShellHello`, and
  receives `ClientShellSnapshot` JSON (`boot_id`, `revision`, `workspaces[]`,
  `tabs[]`, `panes[]`, `agents[]` with `agent_status`) plus pane surface
  frames/patches for the tab it views. Commands go through
  `ClientShellEndpointRequest` carrying JSON-API requests; available methods are
  advertised in `EndpointServerWelcome.methods`.
- `herdr --remote <ssh-target>` spawns `ssh <target> herdr remote-client-bridge`,
  which pipes the remote client socket over SSH stdio; locally it exposes a
  forward socket and runs the normal client against it. Generation-1 endpoint
  compatibility means local and remote versions need not match.
- `ObserveTerminal` / `ControlTerminal` on the client socket stream one
  terminal as diffed ANSI (`RenderEncoding::TerminalAnsi`); the CLI exposes them
  as `herdr terminal session observe|control`. This is what a browser terminal
  renders.
- The JSON socket API (`herdr api snapshot`, `events.subscribe`, `agent.*`,
  `pane.*`) is per-server and reachable through the same SSH bridge idea.
- `XDG_CONFIG_HOME` relocates the whole config/socket dir; `--session <name>`
  creates an independent server. `HERDR_BUILD_CHANNEL` (read in
  `src/build_info.rs`) tags a build; the updater already refuses to self-update
  Homebrew/mise/Nix installs, which is the pattern for a `fork` channel.

---

## Sequencing

**Dependency order: E0 → E1 → E2, then E3 → E4 → E5, then E6 / E7 / E8.**

- **E0** first, because `just ci` has to run on this machine before a single
  task can be gated, and because fork CI and the fleet lab are what every later
  epic validates with.
- **E1** is the shared core. Both the TUI (E2) and the gateway (E3) are thin
  consumers of it; getting the pure `FleetState` right is where correctness
  lives.
- **E2** and **E3** are independent once E1 exists and may run in parallel if
  the machine can afford two Rust builds at once (see
  `implement-roadmap`'s concurrency rule).
- **E5** is mostly documentation plus a small gateway convenience; it is listed
  as its own epic because it answers the user's actual question ("do I just
  need a VPN?" — yes, Tailscale) and because HTTPS from `tailscale serve` is
  what makes E4's installability and E6's push notifications work on iOS.
- **E8** last: only the console machine and the gateway host need fork
  binaries; LAN servers can stay stock.

## Milestones

- **MVP 1 — LAN console:** E0 → E1 → E2. Sit at one computer, see and drive
  every agent on every LAN machine from one `herdr fleet`.
- **MVP 2 — Phone, anywhere:** E3 → E4 → E5. Install the PWA, see every agent
  grouped blocked-first, watch a terminal, answer a prompt, from the couch or
  from a café over Tailscale.
- **Polish:** E6 (push), E7 (rich control), E8 (release/install).

---

## Epic status

Owned by `implement-roadmap`. Legend: ✅ done · 🔨 in progress · ⬜ not started · ⛔ blocked.

| Epic | Title | Depends on | Status | Plan |
| --- | --- | --- | --- | --- |
| E0 | Fork foundations, CI, fleet lab | — | ✅ | `docs/fork/plans/e0-fork-foundations.md` |
| E1 | Fleet core (multi-host runtime model) | E0 | 🔨 | `docs/fork/plans/e1-fleet-core.md` |
| E2 | Fleet TUI (one console, every machine) | E1 | ⬜ | — |
| E3 | Fleet gateway (HTTP + WebSocket) | E1 | ⬜ | — |
| E4 | Phone app (installable PWA) | E3 | ⬜ | — |
| E5 | Off-LAN access via Tailscale | E3 | ⬜ | — |
| E6 | Push notifications to the phone | E4, E5 | ⬜ | — |
| E7 | Control from phone and console (approvals, prompts, start) | E2, E4 | ⬜ | — |
| E8 | Fork release and install pipeline | E2, E4 | ⬜ | — |

---

## Epics

### E0 — Fork foundations, CI, fleet lab

**Goal:** a fork that builds and tests on this machine and in its own CI, never
fights upstream's release machinery, and has the local "fake LAN" every later
epic validates against.
**Why:** nothing can be gated until `just ci` runs here (no Rust toolchain is
installed yet), and upstream's release/preview/website workflows must not fire
on the fork.
**Deliverables:**

- `scripts/fork/dev-setup.sh` — idempotent, `mise`-based: Rust toolchain per
  `rust-toolchain.toml` (1.96.1 with clippy + rustfmt), `just`,
  `cargo-nextest`, `bun`, **Zig 0.15.2** (required by `build.rs` for
  libghostty-vt), verifies `python3`. Ends by running `just ci` and reporting
  the result. The exit state of E0 is a green `just ci` on `master`.
- Fork CI: `.github/workflows/fork-ci.yml` running `just ci` on `ubuntu-latest`
  on push to `master` and on pull requests, plus the existing
  `scripts/conventional_commits.py` PR-title check. Upstream workflows
  (`ci.yml`, `preview.yml`, `release.yml`, `distribution.yml`, `nix.yml`,
  `website-deploy.yml`, `windows-arm64.yml`, `label-next-release-issues.yml`,
  `build-artifacts-manual.yml`, `pr-gate.yml`) are **disabled via
  `gh workflow disable`** — repository state, not file edits, so upstream
  merges never re-enable them and never conflict.
- Fork identity: fork builds set `HERDR_BUILD_CHANNEL=fork` (via
  `.cargo/config.toml` `[env]` or the just recipes). With channel `fork`,
  `herdr update` and the background update check are disabled with a message
  pointing at `docs/fork/README.md` — same pattern the updater already uses for
  Homebrew/mise/Nix installs in `src/update.rs`. `herdr --version` shows the
  channel. Upstream's `distribution/latest.json` must never replace a fork
  binary.
- `scripts/fork/fleet-lab.sh up <n> | down | status | env` — boots `n` isolated
  herdr servers as named sessions under a throwaway `XDG_CONFIG_HOME`
  (`/tmp/herdr-fleet-lab/`), each with one workspace and a pane running a
  visible marker process, prints their client socket paths, and tears them
  down cleanly (reuse the pid/runtime-dir hygiene of `tests/support/mod.rs`).
  This is the E2E fixture for E1–E7.
- `scripts/fork/gate.sh` — machine-wide `flock` around `just ci` so parallel
  agents never run two Rust builds/test suites at once, logs to
  `${TMPDIR:-/tmp}/herdr-fork-gates/`, prints `EXIT=<code>` last (already
  written; E0 verifies it against a real `just ci`).
- `docs/fork/README.md`: what the fork is, dev setup, upstream sync procedure
  (`git fetch upstream && git merge upstream/master`, resolve, `just ci`),
  how to use the skills.
- Review `docs/fork/decisions/0001-servers-stay-stock-ssh-transport.md` (the
  ADR behind principles 1–3, already written) against what E0's exploration
  finds; amend it rather than contradicting it silently.

**Depends on:** —
**Open decisions (default in bold):** (a) CI shape — **fork-ci.yml ubuntu-only
+ disable upstream workflows through `gh`** vs trimming `ci.yml` in place
(conflicts on every upstream CI change); (b) update guard — **disable
self-update on channel `fork`** now, repoint to a fork manifest in E8;
(c) toolchain manager — **mise** (already on this machine) vs rustup direct.

---

### E1 — Fleet core (multi-host runtime model)

**Goal:** one client process holds connections to N herdr servers — local named
sessions and SSH hosts — and exposes a single merged, pure, testable view of
their hosts, workspaces, tabs, panes and agents.
**Why:** the TUI (E2) and the gateway (E3) need exactly the same aggregation.
Built once as pure state plus a connector, it follows upstream's "state is
separated from runtime" principle and is testable without sockets.
**Deliverables:**

- Config: a `[fleet]` section in `config.toml` (`src/config/model.rs`, with
  defaults and the `herdr --default-config` reference updated):

  ```toml
  [fleet]
  # The local default session is always host "local" unless disabled.
  include_local = true

  [[fleet.hosts]]
  name = "workbox"          # display name and id prefix
  kind = "ssh"              # "ssh" | "local"
  target = "workbox"        # ssh target (alias, user@host, ssh://host:2222) — kind = "ssh"
  # session = "agents"      # optional named session on that host
  # enabled = true
  ```

  `kind = "local"` with `session = "<name>"` targets a named session on this
  machine — this is how the fleet lab and the tests build a fleet without
  hardware.
- `src/fleet/` module:
  - `FleetState` — pure data: `HostId`, per-host `HostConnection`
    (`Connecting | Connected { server_version } | Unavailable { reason } |
    Incompatible { generation }`), per-host latest `ClientShellSnapshot` with
    `boot_id`/`revision`, `active_host`, merged agent list ordered
    blocked → working → done → idle → unknown then by fleet-wide recency
    (a `FleetState`-owned `fleet_change_seq`; `state_change_seq` is per server
    boot and not comparable across hosts),
    per-host roll-ups (counts by status). Host-qualified ids through a typed
    `FleetPaneRef { host: HostId, pane_id: String }` (and tab/workspace
    equivalents) with a stable string form `host/w1:p1` for CLI/JSON.
    `FleetState::test_new()` and `assert_invariants_for_test()`, mirroring
    `AppState`'s testing conventions.
  - `FleetConnector` — runtime: per host, open the stream (local:
    `crate::ipc::connect_local_stream` on the session's client socket; ssh:
    the SSH stdio bridge refactored out of `src/remote/attach.rs` into a
    reusable transport so it can run without owning the process), perform the
    generation-1 `ClientShellHello` handshake, run a reader thread, and
    reconnect with backoff (1 s → 30 s). Every failure becomes host-local
    state; the connector never panics or exits the process because of one host.
  - Snapshot ingestion: replace a host's snapshot on new `boot_id`/`revision`;
    forward pane surface frames only for the active host (inactive hosts must
    not cost render work — multiplicative-perf rule).
- CLI: `herdr fleet status [--json] [--timeout-ms <n>] [--watch]` — hosts,
  connection state, server version, agent counts by status, and the merged
  agent list; `--watch` streams one change per line. First user-visible
  deliverable and the shape the gateway later serves. Documented in
  `docs/fork/fleet-core.md`.
- Tests: pure-state unit tests (merge ordering, id round-trips, reconnect state
  machine, unknown-status fallback); an integration test under `tests/` that
  boots two named sessions with the `tests/support` harness and asserts
  `herdr fleet status --json` shows both with the right counts.

**Depends on:** E0.
**Open decisions (default in bold):** (a) SSH reuse — **refactor
`SshStdioBridge` + `prepare_remote_herdr` into a `fleet::transport` the
connector calls directly** vs spawning `herdr --remote`-style subprocesses per
host; (b) id form — **`host/w1:p1`** vs `w1:p1@host`; (c) where `[fleet]`
lives — **main `config.toml`** vs a separate `fleet.toml`.
**Constraint downstream epics must honor:** no server or wire-protocol change;
`FleetState` stays free of ratatui, sockets, and async.

---

### E2 — Fleet TUI (one console, every machine)

**Goal:** `herdr fleet` on the console machine shows every host's workspaces and
agents in the sidebar with live status, and lets you pick any host and work in
it at full fidelity.
**Why:** this is MVP 1 — "see all the sessions at once" from one computer.
**Deliverables:**

- Launch: `herdr fleet [--session <name>]` (and `herdr --fleet` alias). Reuses
  `run_client_with_mode` / the client-owned shell; the difference is N streams
  behind a `FleetConnector` instead of one socket.
- Sidebar: a host group per configured host (`▸ workbox · 2 blocked · 3
  working`), then that host's workspaces and agents using the existing agent
  status glyphs and ordering; unreachable hosts shown dimmed with the reason;
  the local host first. Click a host header or use the host picker
  (`prefix+shift+h` default, configurable under `[keys]`) to switch the
  **active host**.
- Pane area renders the active host only — its focused tab and panes exactly as
  today. Switching hosts swaps which stream's pane surfaces are installed.
  Mixed-host layouts (panes from two hosts in one tab) are **out of scope**.
- Routing: pane input, resize, focus, scroll, endpoint commands go to the
  active host; selecting an agent on another host switches the active host,
  then focuses it there.
- Notifications from any host surface with the host name prefixed
  (`[workbox] reviewer needs input`), including sound per existing config.
- Local keybindings apply to every host (same default as `--remote`);
  `--remote-keybindings server` semantics are not offered in fleet mode v1.
- Failure UX: a host dropping shows "reconnecting…" in its group and keeps the
  rest usable; if the *active* host drops, the pane area shows a reconnect
  notice, not an exit.
- Docs: `docs/fork/fleet.md` (config, keys, limits).
- Tests: shell-state unit tests for host grouping/switching/routing without
  PTYs; a `tests/` integration run against the fleet lab asserting the
  snapshot-driven sidebar lists both hosts' agents and that input reaches the
  right server.

**Depends on:** E1.
**Open decisions (default in bold):** (a) host switch UX — **sidebar host
groups + a picker overlay reusing the existing overlay pattern** vs tabs-per-host;
(b) whether inactive hosts' focused-tab surfaces are prefetched — **no**
(subscribe only on switch; accept a brief redraw).
**Performance constraint:** sidebar work is × hosts × agents; keep it
O(visible rows), no per-frame allocation for inactive hosts, and profile 1 vs 5
hosts × 15 agents before merging (`AGENTS.md` → multiplicative paths).

---

### E3 — Fleet gateway (HTTP + WebSocket)

**Goal:** `herdr gateway` — a headless daemon on one LAN machine that uses the
fleet core to aggregate every host and serves a JSON/WebSocket API plus the
embedded web app to phones and browsers.
**Why:** phones cannot speak the herdr socket protocol or SSH; one gateway on
the LAN is the only network surface, and it is what E5 puts on the tailnet.
**Deliverables:**

- `herdr gateway [--bind 127.0.0.1:7788] [--config <path>]`, cargo feature
  `gateway` (on by default in fork builds, so `--no-default-features` yields an
  upstream-shaped binary). Loopback by default. Any non-loopback bind requires
  a token and an origin allowlist; refused otherwise with a clear message.
- Auth: two random 32-byte tokens generated on first run and stored `0600`
  under `<config>/gateway/` — `read` (snapshot, events, observe terminals) and
  `control` (input, resize, and E7's actions). Bearer header or a one-time
  pairing URL that exchanges into a per-device cookie; constant-time compare;
  failure rate limiting.
- HTTP: `GET /api/fleet` (the `herdr fleet status --json` shape), `GET /health`,
  static assets for the web app served from bytes embedded at build time
  (`include_bytes!` over `web/dist`).
- WebSocket: `/api/events` streams fleet deltas (host state, agent status
  changes, workspace/tab/pane changes) as newline-free JSON messages;
  `/api/terminal/{host}/{pane}` streams rendered ANSI frames through the
  observe/control path (`ObserveTerminal` / `ControlTerminal`,
  `RenderEncoding::TerminalAnsi`) and accepts `terminal.input`,
  `terminal.resize`, `terminal.scroll`, `terminal.release` — control only with
  the `control` scope. Frame size limits mirror `MAX_FRAME_SIZE`.
- `herdr gateway pair [--control]` prints the pairing URL and a terminal QR
  code; `herdr gateway status`, `herdr gateway rotate-token`.
- Ops: example `systemd --user` unit and `docs/fork/gateway.md`.
- Tests: handler-level tests with a fake `FleetState`; an integration test that
  starts the gateway on loopback against the fleet lab and asserts `/api/fleet`,
  an events stream delta, and a terminal frame.

**Depends on:** E1.
**Open decisions (default in bold):** (a) HTTP/WS stack — **`axum` (with its
`ws` feature) on the existing `tokio`** vs `hyper` + `tokio-tungstenite` vs
hand-rolled (the QR code uses the small `qrcode` crate); (b) token model —
**two scopes** vs one token + per-device role; (c) lives in the main binary
behind a feature — **yes** vs a separate crate (would require turning the repo
into a cargo workspace, a heavy upstream-file change).

---

### E4 — Phone app (installable PWA)

**Goal:** open the gateway URL on the phone, add it to the home screen, and see
every agent on every machine grouped blocked-first; tap one to watch its
terminal live; send text or keys when you choose to.
**Why:** this is MVP 2's user surface; a PWA ships to iOS and Android with no
store, no signing, and works on the LAN and over Tailscale unchanged.
**Deliverables:**

- `web/` — TypeScript + Vite, **Preact** for the small UI, **xterm.js** (with
  the fit addon) for the terminal, a service worker for installability and
  offline app shell, `manifest.webmanifest`, dark/light following the OS.
- Screens: **Fleet** (hosts as sections; agents sorted blocked → working → done
  → idle with host, workspace, title, and "since" time; unreachable hosts
  shown), **Terminal** (read-only by default, "Control" toggle when the token
  has the scope, virtual key bar: Esc, Tab, Ctrl-C, arrows, Enter, `y`, `n`, and
  a text field with send), **Settings** (gateway URL, pairing, read-only lock,
  font size). Wake Lock while a terminal is open; safe-area insets; large tap
  targets.
- Data: bootstrap from `GET /api/fleet`, then apply `/api/events` deltas;
  reconnect with backoff; stale badge when disconnected.
- Build: `just web-build` (bun) writes `web/dist`; `web/dist` is **committed**
  so `cargo build` needs no Node at all; CI verifies `web/dist` matches the
  sources (build and diff).
- Tests: unit tests for the fleet-model reducer and ordering (bun test);
  a Playwright smoke run against the gateway + fleet lab is optional and
  headless.

**Depends on:** E3.
**Open decisions (default in bold):** (a) app shape — **PWA** vs Capacitor
wrapper vs native; (b) UI library — **Preact + TS** vs vanilla vs React;
(c) committing `web/dist` — **yes, with a CI freshness check** vs building in
`build.rs`.

---

### E5 — Off-LAN access via Tailscale

**Goal:** the same console and the same phone app work away from home, without
opening any port to the internet.
**Why:** because principles 2–3 make the whole system private-network-native,
the only missing piece off-LAN is the private network. Tailscale (WireGuard)
provides it in minutes, gives stable MagicDNS names for the SSH targets in
`[fleet]`, and `tailscale serve` provides **valid HTTPS** for the gateway —
which is what makes the PWA installable on iOS and what E6's push requires.
**Deliverables:**

- `docs/fork/remote-access.md`: install Tailscale on every LAN host and the
  phone; use MagicDNS names as `[fleet]` ssh targets; run the gateway on the
  tailnet; `tailscale serve --bg 7788` for HTTPS; an ACL snippet limiting the
  gateway port to the owner's devices; when to use Tailscale SSH; alternatives
  compared honestly (plain WireGuard, Cloudflare Tunnel; Tailscale Funnel
  discouraged because it is internet-exposed).
- `herdr gateway --tailscale`: detects the tailnet address (`tailscale ip -4`,
  `tailscale status --json`), binds to it (token required, as always
  off-loopback), and prints the `https://<host>.<tailnet>.ts.net` URL when
  `tailscale serve` is active; refuses a public/non-tailnet bind without
  `--allow-insecure-bind`.
- Fleet lab documentation for testing "remote" behaviour locally (SSH to
  `localhost` as a fake remote host).

**Depends on:** E3.
**Open decisions (default in bold):** (a) VPN — **Tailscale** vs hand-managed
WireGuard; (b) should the gateway run `tailscale serve` itself — **no, print
the exact command** (it needs its own permissions and is a one-time step).

---

### E6 — Push notifications to the phone

**Goal:** be pinged when an agent turns blocked (or done) even with the app
closed.
**Why:** "never hunt for the stuck one" is herdr's promise; off the console it
needs a push.
**Deliverables:**

- Web Push from the gateway: VAPID keys generated once (stored `0600`),
  subscriptions per paired device, RFC 8291 encryption (`web-push` crate or
  equivalent — a dependency with a reason), delivery on blocked/done
  transitions with per-device rules (blocked only / blocked + done), debounce
  against flapping agents, quiet hours, and a tap that deep-links to that
  terminal. iOS requires the installed PWA (16.4+) and HTTPS (E5).
- In-app fallback: toasts and an unread badge while the app is open.
- `herdr gateway notify-test` to verify a device receives a push.

**Depends on:** E4, E5.
**Open decisions (default in bold):** **Web Push** vs an ntfy.sh bridge vs
Telegram bot.

---

### E7 — Control from phone and console (approvals, prompts, start)

**Goal:** answer an agent's question in one tap from the phone; prompt an agent
on another host from the console; start, rename, or stop agents and create
workspaces on any host.
**Why:** viewing is MVP; acting closes the loop.
**Deliverables:**

- Fleet-routed endpoint requests: the fleet core exposes
  `request(host, Request)` over each host's `ClientShellEndpointRequest`
  channel, honoring the methods advertised in that host's
  `EndpointServerWelcome.methods`; missing methods disable just that action
  (contract).
- Gateway (control scope): `POST /api/hosts/{host}/agents/{pane}/prompt`,
  `/keys`, `/start`, `/rename`, `/close`, `POST /api/hosts/{host}/workspaces`
  — thin wrappers over `agent.prompt`, `agent.send_keys`, `agent.start`,
  `agent.rename`, `pane.close`, `workspace.create`.
- PWA: quick actions on a blocked agent (`y`, `n`, Enter, Esc, "approve" for
  known approval prompts, free-text prompt), confirm dialogs for close/stop.
- Fleet TUI: prompt/send-keys to an agent on another host from the picker
  without switching, plus "new workspace on host…".

**Depends on:** E2, E4.
**Open decisions (default in bold):** which destructive actions the phone may
do — **close pane / stop agent with confirm; never `server.stop`**.

---

### E8 — Fork release and install pipeline

**Goal:** build fork binaries and install them on the console machine and the
gateway host; keep LAN servers on stock or fork interchangeably.
**Why:** a fork nobody can install is a branch.
**Deliverables:**

- `.github/workflows/fork-release.yml` on tags `fork-v*`: linux x86_64 and
  aarch64 (macOS arm64 optional), attached to a GitHub Release, with
  `HERDR_BUILD_CHANNEL=fork` and the version string suffixed `+fork.<n>`.
- `distribution/fork/latest.json` in the fork's shape; `herdr channel set fork`
  and `herdr update` read it (raw GitHub URL), replacing E0's hard disable.
- `scripts/fork/install.sh` (curl-able), systemd user units for the gateway
  and for a headless `herdr server`, and `docs/fork/install.md` covering
  "console machine", "gateway host", and "LAN server (stock or fork)".

**Depends on:** E2, E4.
**Open decisions (default in bold):** **fork update channel** vs manual install
only.

---

## Explicitly out of scope

- **A TCP/TLS listener or any network authentication inside the herdr
  server.** SSH and the VPN do that; adding them would fork the protocol and
  the security surface.
- **A cloud relay or hosted service.** Nothing leaves the LAN/tailnet.
- **Windows as a fleet server host.** Upstream does not support Windows as a
  remote host; the fork inherits that. Windows as a *console* follows upstream.
- **Mixed-host pane layouts** (one tab showing panes from two machines) in v1.
- **Native app-store apps.** The PWA is the phone app; a wrapper can come later
  without changing the gateway.
- **Replacing upstream's plugin ecosystem.** `herdr-mirror`, `herdr-web`,
  `herdr-mobile-relay` and `herdrm` were studied as prior art; this fork builds
  the same capabilities in-tree on the stable endpoint contract rather than as
  external bridges.
