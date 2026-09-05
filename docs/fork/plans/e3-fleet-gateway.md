# Epic E3 — Fleet gateway (HTTP + WebSocket)

## Context

**Goal (roadmap):** `herdr gateway` — a headless daemon on one LAN machine
that uses the fleet core to aggregate every host and serves a JSON/WebSocket
API plus the embedded web app to phones and browsers. **Why:** phones cannot
speak the herdr socket protocol or SSH; one gateway on the LAN is the only
network surface, and it is what E5 puts on the tailnet.

Scope contract = the roadmap's E3 deliverables: `herdr gateway [--bind
127.0.0.1:7788] [--config <path>]` behind cargo feature `gateway` (on by
default in fork builds; `--no-default-features` yields an upstream-shaped
binary), loopback by default, any non-loopback bind refused without a token
and an origin allowlist; two random 32-byte tokens (`read`, `control`)
generated on first run and stored `0600` under `<config>/gateway/`, presented
as a bearer header or exchanged from a one-time pairing URL into a per-device
cookie, constant-time compare, failure rate limiting; `GET /api/fleet` (the
`herdr fleet status --json` shape), `GET /health`, static assets embedded at
build time from `web/dist`; WebSocket `/api/events` streaming fleet deltas as
newline-free JSON and `/api/terminal/{host}/{pane}` streaming rendered ANSI
frames through `ObserveTerminal`/`ControlTerminal` (`RenderEncoding::
TerminalAnsi`) accepting `terminal.input`, `terminal.resize`,
`terminal.scroll`, `terminal.release` — input only with the `control` scope,
frame limits mirroring `MAX_FRAME_SIZE`; `herdr gateway pair [--control]` with
a terminal QR code, `herdr gateway status`, `herdr gateway rotate-token`; an
example `systemd --user` unit and `docs/fork/gateway.md`; handler-level tests
with a fake `FleetState` and an integration test against the fleet lab
asserting `/api/fleet`, an events delta and a terminal frame.

The product name for fork-owned surfaces is **Herdr Fleet** (short "Fleet");
binary, commands, config directories and protocol stay `herdr`. E3 ships only
a tiny committed placeholder `web/dist/index.html` so the embedding path is
exercised; the Vite/Preact app is E4's.

**Dependency chain:** E1 is ✅ (`origin/master` @ `09919e4a`, herdr
`0.8.2-fork`). **E2 is 🔨 and being implemented concurrently** — none of its
code is on `master` yet, and E3 must never depend on it: both are consumers
of the E1 fleet core. E3 assumes exactly these E1 contracts (verified in the
code):

- `src/fleet/connector.rs`: `FleetConnector::start(specs, options) -> Self`,
  `events(&mut self) -> &mut tokio::sync::mpsc::Receiver<FleetEvent>`,
  `shutdown(self)` (bounded 2 s join; unlinks ssh forward sockets),
  `FleetConnectorOptions::for_config(&Config)` (reads
  `[remote].manage_ssh_config`; `handshake = HandshakeParams::read_only(
  INACTIVE_SURFACE)`, `max_frame_size = MAX_FRAME_SIZE`), `FleetEvent::Host
  { host, event: HostEvent }` (the only variant a non-active consumer
  receives besides `Notification`/`EndpointResponse`; surfaces are dropped in
  the reader thread for inactive hosts). The gateway never calls
  `set_active`, so no host is ever asked for a bigger surface.
- `src/fleet/state.rs`: `FleetState::{new, hosts, host, active_host,
  set_active_host, apply, merged_agents, totals}`; `HostConnection::{
  Connecting, Connected{server_version, methods}, Unavailable{reason,
  retry_in}, Incompatible}` + `is_connected/state_name/reason`;
  `FleetChange::{HostConnection, Snapshot, AgentAdded, AgentRemoved,
  AgentStatus, ActiveHost}` tagged `kind` (`host_connection`, `snapshot`,
  `agent_added`, `agent_removed`, `agent_status`, `active_host`), `Serialize
  + Deserialize`, **no catch-all** — a reader skips unknown kinds; the
  `HostConnection` delta serializes as `ConnectionReport` without `methods`.
  `test_new()`, `test_with_adversarial_identity_state()`,
  `assert_invariants_for_test()` are `#[cfg(test)]` and reachable only from
  in-crate tests.
- `src/fleet/report.rs`: `FLEET_STATUS_SCHEMA = "herdr.fleet.status.v1"`,
  `FleetStatusReport::from_state(&mut FleetState, client_version)` (`&mut`
  for the merged cache), `HostReport`, `ConnectionReport` (`#[serde(other)]
  Unknown`), `WorkspaceReport`, `AgentReport`; every key always present.
- `src/fleet/hosts.rs`: `HostId` (`new`, `LOCAL`, `as_str`, serde as a
  string, never contains `/`), `HostKind::{Local{session}, Ssh{target,
  session}}`, `HostSpec { id, kind, enabled }`, `resolve_hosts(&FleetConfig)
  -> Result<Vec<HostSpec>, Vec<String>>` (all-or-nothing).
  `src/fleet/refs.rs`: `FleetPaneRef::new(host, pane_id)`, `Display`/`FromStr`
  as `host/w1:p1`, `is_valid_resource_id`.
- `src/fleet/transport/`: `trait HostTransport { connect(&mut self) ->
  io::Result<LocalStream>; read_timeout(); describe() }`,
  `transport_for(&HostSpec, &FleetConnectorOptions) -> Result<Box<dyn
  HostTransport>, String>`, `LocalTransport::new(session)`,
  `SshTransport::new(host, target, session, manage_ssh_config)` — which binds
  its forward socket at `local_forward_socket_path_scoped(host.as_str(),
  target, session)`. **A second `SshTransport` with the same scope in the
  same process fails with `AddrInUse`**, so the gateway's terminal streams
  need their own scope (PR 6).
- `src/fleet/oneshot.rs`: `FleetSession::start(&Config)`, `settle`,
  `next_changes` (`blocking_recv` — must not run inside a runtime),
  `report`, `shutdown`; the model for "resolve config → specs → connector →
  state", which the gateway re-does asynchronously.
- E1 constraints recorded in the roadmap and `docs/fork/fleet-core.md`:
  `agents[]` excludes plain panes (the fleet lab's marker panes are **not**
  agents — `/api/fleet` from the lab shows `workspaces[]` but empty
  `agents[]`); `fleet_change_seq` is per `FleetState` instance; an ssh host's
  forward socket is unlinked only by `shutdown`, so the gateway **must** call
  it on every exit path; on unix dropping an `SshTransport` while a bridged
  stream is still open blocks until the ssh child exits — release streams
  first; a connecting client shell becomes each host's *foreground* client
  and its surface (120×40 inactive) is the host's effective pane geometry —
  headless servers are unaffected, a host with a configured `headless_size`
  or an attached console is resized while the gateway holds it (documented
  limitation; a truly passive reader needs an advertised optional endpoint
  observer method, out of scope). The ssh child inherits stderr; the gateway
  is not full-screen, so it may stay.

### Real current state (verified on `master` @ `09919e4a`)

- **No gateway anywhere.** `src/gateway/`, `web/`, `[gateway]`,
  `tests/fork_gateway.rs`, `docs/fork/gateway.md` do not exist.
  `grep -rn 'cfg(feature' src/` is empty: **`Cargo.toml` has no `[features]`
  section at all**, so `gateway` is the crate's first feature and the
  `--no-default-features` build path is untested today. There is no
  `src/lib.rs` and no `[lib]` — the crate is a single bin (`herdr`), so
  `tests/*.rs` cannot `use herdr::…` and drive the binary through
  `env!("CARGO_BIN_EXE_herdr")`; handler-level tests must be in-crate
  `#[cfg(test)]` modules under `src/gateway/`.
- **Dependencies.** `Cargo.toml` `[dependencies]` (no `[dev-dependencies]`,
  no `[profile]`): `tokio = { version = "1", features = ["rt-multi-thread",
  "macros", "sync", "time", "process", "io-util"] }` — **no `net`, no
  `signal`**; `serde`/`serde_json`, `base64 = "0.22.1"`, `sha2 = "0.10"`,
  `bytes = "1"`, `time = "0.3.47"`, `tracing`/`tracing-subscriber`,
  `interprocess = "2.4.2"`, `bincode = "2"`, `clap 4.5` (`std,help,usage`),
  `ratatui`, `crossterm`, `portable-pty` (patched), `libc`, `regex`, `toml`,
  `schemars`, `png`, `ctrlc`. `Cargo.lock` has **no** `axum`, `hyper`,
  `http`, `tower`, `tungstenite`, `qrcode`, `subtle`, `constant_time_eq`,
  `hmac`, `url`, `mime_guess`, `include_dir`; it *does* carry `rand 0.8.5`,
  `getrandom 0.3.4` + `0.4.2`, `futures-util 0.3.33`, `uuid 1.22.0`
  transitively. `just ci` runs `--locked`, so `Cargo.lock` ships in the same
  PR as `Cargo.toml`. `rust-toolchain.toml` pins `1.96.1`; edition 2021.
  `.cargo/config.toml` sets `[env] HERDR_BUILD_CHANNEL = "fork"`.
  `build.rs` builds libghostty-vt with Zig and emits `rerun-if-env-changed`
  for the build-channel vars; it does not know about `web/`.
- **Runtime shape to copy.** No `#[tokio::main]`; the long-running daemon
  path is `src/server/headless/bootstrap.rs:40`
  `tokio::runtime::Builder::new_multi_thread().enable_all().build()` +
  `rt.block_on`. Every socket read/write on the herdr side is **blocking
  std I/O**: `protocol::write_message(&mut W, &M)` / `read_message(&mut R,
  max_frame_size)` (`src/protocol/wire.rs:1601,1624`, `[u32 LE len][bincode]`),
  `crate::ipc::connect_local_stream(&Path) -> io::Result<LocalStream>`
  (`LocalStream = interprocess::local_socket::Stream`, `try_clone` for a
  write half as `src/client/terminal_sessions.rs` does).
- **Observe/control wire path (frozen, reused as-is).** A client-socket
  connection has one mode for its lifetime, chosen by its first message
  (`src/server/client_transport.rs:669-786`): `ClientMessage::TerminalHello
  { version: PROTOCOL_VERSION (22), cols, rows, cell_width_px,
  cell_height_px, pixel_mouse }` → `ServerMessage::Welcome { version,
  encoding: RenderEncoding::TerminalAnsi, error: None }` and the connection
  is `TerminalPending`; then `ClientMessage::ObserveTerminal { target }`
  (tag 7) or `ControlTerminal { target, takeover }` (tag 8), where `target`
  resolves server-side to a raw terminal id, a public pane id `w1:p1`, or an
  agent target (`resolve_terminal_target_id_string`, `src/server/
  headless.rs:1160`; unknown → `ServerShutdown { reason: "terminal session
  observe failed: terminal target … not found" }` and disconnect). Output is
  **one message type**, `ServerMessage::Terminal(TerminalFrame { seq: u64,
  width: u16, height: u16, full: bool, bytes: Vec<u8> })` — already-diffed
  ANSI, `full: false` is the patch; `Graphics` and everything else is
  dropped by the CLI. Inputs on a control connection reuse the direct
  terminal vocabulary: `Input { data }` (tag 1), `Resize { cols, rows,
  cell_width_px, cell_height_px, pixel_mouse }` (tag 3), `AttachScroll {
  source: Wheel | PageKey{input}, direction: Up|Down, lines, column, row,
  modifiers }` (tag 6), `Detach` (tag 4 — "release"). Server facts that
  matter: observers are unlimited and never own the terminal
  (`terminal_observe_allows_multiple_clients_without_attach_ownership`,
  `src/server/headless/tests/mod.rs:3088`); **an observer's `Resize` changes
  only its own virtual viewport** (`ServerEvent::ClientResize`,
  `src/server/headless.rs:2185`, `render_terminal_virtual`), while a
  controller's `Resize` resizes the real PTY (`:2181`); control is
  single-owner (second `ControlTerminal` without `takeover` gets a
  `ServerShutdown` "already has an attached client; retry with --takeover");
  mode is one-way (observe → control on the same connection is refused); an
  observed hidden pane keeps rendering; the server's per-client render lane
  has capacity one, so a slow reader coalesces frames rather than desyncing
  (`src/server/headless/render.rs:620-639`). Caps: hello and non-graphics
  frames use `MAX_FRAME_SIZE = 2 MiB`; `MAX_GRAPHICS_FRAME_SIZE = 32 MiB`
  only for graphics, which the gateway never enables.
- **The CLI twin of that path**, `src/client/terminal_sessions.rs` (267
  lines): `run_terminal_session_observe(target, cols, rows)` /
  `run_terminal_session_control(target, takeover, cols, rows)`;
  `connect_terminal_session_stream` (hardcodes `client_socket_path()`, calls
  `std::process::exit` on failure — not reusable), `write_terminal_session_
  output` (the read loop, emits NDJSON `{"type":"terminal.frame","seq",
  "encoding":"ansi","width","height","full","bytes":<base64>}` /
  `{"type":"terminal.closed","reason"}`), and
  `pub(super) fn terminal_control_command_from_json(raw: &str) ->
  Result<ClientMessage, String>` mapping `terminal.input {text | bytes}`,
  `terminal.resize {cols, rows, cell_width_px, cell_height_px}`,
  `terminal.scroll {direction, lines, source, column, row, modifiers}`,
  `terminal.release` (tests at `src/client/tests/mod.rs:783-840`). Defaults
  120×40; there is no SIGWINCH handling and no `--encoding` flag.
  `do_handshake` (`src/client/handshake.rs:131`) is `pub(super)`; the
  terminal hello is two frames and is re-implemented in the gateway rather
  than widening `src/client/`.
- **CLI wiring pattern** (E1's `fleet`): `src/cli.rs:28` `mod fleet;` and
  `:116` `"fleet" => fleet::run_fleet_command(&args[2..])?` in `maybe_run`
  (`pub(super) fn run_fleet_command(args: &[String]) -> io::Result<i32>`,
  hand-parsed args, `help` → 0, usage error → 2); `src/cli/spec.rs:31`
  `.subcommand(fleet_command())` + `fn fleet_command()` at `:147` (help and
  completions only; invariants `spec_describes_all_completion_commands`,
  `every_spec_subcommand_renders_short_and_long_help`,
  `spec_passes_clap_invariants`); `src/main.rs:746-768` the bare-command
  allowlist (`"fleet"` at `:758` — a missing entry makes `herdr gateway`
  exit 2 "unknown command") and `:611` the `--help` usage lines.
  `build_info::is_fork()` already gates fork-only help text.
- **Config pattern** (E1's `[fleet]`): `src/config/model.rs:979` `FleetConfig`
  (`#[derive(Debug, Deserialize)] #[serde(default)]`, hand-written `Default`,
  pure `diagnostics()`), `Config.fleet` at `:323` (last field); `src/config/
  io.rs:7` `KNOWN_TOP_LEVEL_CONFIG_KEYS` allowlist (alphabetical; `"gateway"`
  goes between `"fleet"` and `"keys"`), `load_live_section(table, "fleet",
  …)` + `diagnostics.extend(config.fleet.diagnostics())` on the live-reload
  path, `Config::collect_diagnostics()` (`src/config.rs:114`);
  `src/main.rs:64` `DEFAULT_CONFIG` with the fully commented `[fleet]` block
  at `:404` and two contract tests (`default_config_fleet_block_parses_
  without_diagnostics`, `default_config_fleet_block_is_commented_out`);
  `scripts/config_reference_check.py` `SKIPPED_SUBTREES = ("keys.command",
  "fleet")` — `scripts/test_config_reference_check.py` runs inside `just ci`
  and fails on any new key outside a skipped subtree because fork rules
  forbid editing `docs/next/**`. Paths: `config::config_dir()`
  (`$XDG_CONFIG_HOME/<app>`; `app_dir_name()` = `herdr-dev` in debug,
  `herdr` in release), `config_path()` (`HERDR_CONFIG_PATH` override),
  `Config::load() -> LoadedConfig` (no path parameter — `--session` works by
  setting `HERDR_SESSION` before anything runs, `src/session.rs:62-70`).
- **Secrets/permissions precedent.** `0o600`/`0o700` via
  `std::os::unix::fs::{OpenOptionsExt, PermissionsExt}` in
  `src/pane_graphics_files.rs` (`DIRECTORY_MODE`/`FILE_MODE`, verifies mode of
  an existing dir before use) and `src/ipc.rs:335`
  `restrict_socket_permissions` with a `#[cfg(windows)]` no-op twin (`just
  check`'s `windows-lint` clippy-compiles the bin for
  `x86_64-pc-windows-msvc`). **No constant-time compare and no CSPRNG exist
  in `src/`** — unique ids are `AtomicU64`/pid+nanos.
- **Tests/tooling.** `tests/support/mod.rs` (`build_version()`,
  `wait_for_socket`, `client_handshake` = a hand-encoded `TerminalHello`,
  `read_server_message` → `(tag, payload)`, pid/runtime-dir hygiene),
  `tests/support/fleet_lab.rs` `Lab::{new, up, run, herdr, runtime_dir}` +
  `unique_root`/`stdout_of`/`stderr_of`/`STEP_TIMEOUT_MS`,
  `tests/fork_fleet_lab.rs`/`tests/fork_ssh_lab.rs` (the `fork_*` naming),
  `tests/cli/fleet.rs` (writes `[fleet]` into `<config_home>/<app>/
  config.toml`, `TWO_LOCAL_HOSTS`). `scripts/fork/fleet-lab.sh up N | status
  [--json] | env | down` writes only `onboarding = false` — tests append
  `[fleet]`/`[gateway]` themselves. `scripts/fork/ssh-lab.sh` gives one ssh
  host (`herdr-ssh-lab`, exit 3 = no sshd). `justfile`: `ci` = `lint`
  (`cargo fmt --check`, `cargo clippy --all-targets --locked -- -D
  warnings`) + `cargo nextest run --locked` + `maintenance-test` +
  `ui-hot-path-architecture-test` + `integration-assets-test` +
  `plugin-marketplace-test`; **no recipe passes `--features`**. Fork CI
  (`.github/workflows/fork-ci.yml`): `conventional-commits`, `check
  (ubuntu-latest)` (toolchain 1.96.1 + just/nextest + bun 1.3.14 + Zig
  0.15.2 + rust-cache key `fork-ubuntu-latest` → `just ci`), `shellcheck -S
  warning scripts/fork/*.sh`. No `websocat`; `python3` ≥ 3.10 stdlib and
  `curl` are available. `assets/fork/{logo.svg, logo-192.png, logo-512.png}`
  exist for E4.
- **Registry versions (2026-09-05):** `axum 0.8.9`, `qrcode 0.14.1`,
  `subtle 2.6.1`, `getrandom 0.3.4` (already in the lock), `tower-http
  0.7.1`, `tokio-tungstenite 0.30.0`, `hyper 1.11.1`, `http 1.5.0`.

### Locked decisions

- **(a) HTTP/WS stack — `axum` with its `ws` feature on the existing
  `tokio`** (roadmap default). `axum = { version = "0.8", default-features =
  false, features = ["http1", "tokio", "ws", "json", "query"] }` — no
  `tower-http`, no `hyper`/`http` as direct deps (reached through axum's
  re-exports `axum::http`, `axum::body`), no `tokio-tungstenite` directly
  (axum's `ws` pulls it and exposes `axum::extract::ws`). `tokio` gains
  `net` (TCP listener) and `signal` (graceful stop). The QR code is
  `qrcode = { version = "0.14", default-features = false }` rendered with its
  built-in `unicode::Dense1x2` renderer (no image feature). Constant-time
  compare is `subtle = "2.6"` (`ConstantTimeEq`, no deps); randomness is
  `getrandom = "0.3"` (`getrandom::fill`, already in the tree so no new lock
  subtree — `rand` is not needed). All five are `optional = true` and
  enabled only by the `gateway` feature.
- **(b) Token model — two scopes, `read` and `control`** (roadmap default).
  Files `<config>/gateway/read.token` and `control.token`, 64 lowercase hex
  chars (32 random bytes) + newline, mode `0600` in a `0700` directory,
  generated on first `herdr gateway` or `herdr gateway pair`. `control`
  implies `read`. Tokens reach the gateway as `Authorization: Bearer <hex>`
  or as the per-device cookie `herdr_gateway_device=<id>.<secret>` minted by
  a one-time pairing URL; **never as a query parameter** (proxy logs). The
  gateway compares SHA-256 digests with `subtle::ConstantTimeEq`, never the
  raw strings, so lengths do not leak. Device records (`devices.json`,
  `0600`) store only the digest of the secret. Pairing codes are one-time
  `0600` files under `<config>/gateway/pairings/`, default TTL 10 minutes,
  deleted on first use. `rotate-token <read|control>` rewrites that file and
  revokes every device of that scope.
- **(c) Lives in the main binary behind cargo feature `gateway`, on by
  default** (roadmap default). `[features] default = ["gateway"]`; every
  `src/gateway/**` line, the `mod gateway;`, the CLI arms, the spec
  subcommand and the bare-command entry are `#[cfg(feature = "gateway")]`.
  The `[gateway]` **config section is unconditional** (pure data with no
  optional deps): an upstream-shaped `--no-default-features` binary still
  parses and validates it instead of reporting an unknown key, and
  `KNOWN_TOP_LEVEL_CONFIG_KEYS` stays a plain array. No cargo workspace.
- **(d) Tokens are required on every bind, loopback included** *(auto
  default)*. Loopback relaxes only the origin allowlist: the default
  allowlist for a loopback bind is the bind's own origins
  (`http://127.0.0.1:<port>`, `http://localhost:<port>`,
  `http://[::1]:<port>`); a non-loopback bind is refused at startup (exit 1,
  message names the key) unless `[gateway] allowed_origins` is non-empty.
  Reason: herdr's sockets are owner-only `0600` precisely so another local
  uid cannot read agent terminals; an unauthenticated loopback HTTP port
  would undo that. Unauthenticated routes are exactly `GET /health`
  (`{"ok":true}`, no version), the embedded static assets (the app shell
  carries no data), and `GET /pair` (rate-limited, consumes a code).
- **(e) Origin policy** *(auto default)*: a request carrying an `Origin`
  header (every browser request, every browser WebSocket upgrade) must match
  the allowlist exactly (`scheme://host[:port]`, compared case-insensitively
  on host) or gets `403 {"error":"origin_not_allowed"}`; a request without
  `Origin` (curl, the python stand-in, native clients) passes the origin
  check and relies on the token. `[gateway] public_url` (E5 sets it to the
  `https://<host>.<tailnet>.ts.net` origin) is implicitly allowed and is what
  pairing URLs advertise; its `https` scheme also switches the device cookie
  to `Secure`.
- **(f) Failure rate limiting** *(auto default)*: per peer IP, 5 failed
  authentications (bad bearer, bad cookie, bad or expired pairing code) in a
  60 s window → `429` with `Retry-After: 60` for 60 s; pure `AuthLimiter`
  with an injected clock, bounded to 4096 peers (oldest evicted). A
  successful auth does not reset the counter (no oracle).
- **(g) Runtime shape** *(auto default)*: one multi-thread tokio runtime
  (`Builder::new_multi_thread().enable_all()`, as the server does). The
  gateway owns the `FleetConnector` in one task that awaits
  `connector.events().recv()` and folds `FleetEvent::Host` into a
  `std::sync::Mutex<FleetState>` (never held across an `.await`),
  broadcasting every `FleetChange` through `tokio::sync::broadcast` (capacity
  1024, `Arc<str>` pre-serialized JSON). Handlers read the mutex briefly.
  Terminal sessions are blocking std threads per session (reader + writer)
  bridged to the WebSocket task with bounded channels (capacity 2) so a slow
  browser applies backpressure to the server's render lane instead of
  buffering; the gateway therefore never holds more than two frames per
  open terminal. Graceful stop on SIGINT/SIGTERM: stop accepting → close
  terminal sessions (send `Detach`, drop streams, join threads) → drop
  terminal transports → `connector.shutdown()` → remove the runtime marker.
- **(h) `/api/events` carries the full report first** *(auto default)*.
  Because deltas between `GET /api/fleet` and the WebSocket connect would be
  lost, the stream is self-contained: subscribe to the broadcast, then take
  the report under the state lock, send `hello` + `fleet`, then deltas. A
  lagged subscriber gets `{"kind":"resync"}` followed by a fresh `fleet`
  message. `GET /api/fleet` remains for one-shot readers and E4's bootstrap.
- **(i) Terminal frames are binary WebSocket messages** *(auto default)*:
  a 14-byte header (`u8` kind `0x01`, `u64 LE` seq, `u16 LE` width, `u16 LE`
  height, `u8` full) followed by the raw ANSI bytes — xterm.js writes bytes,
  and base64-in-JSON would cost 33 % on a 2 MiB full frame. Every other
  message on that socket is a text JSON object with a `type` field, using
  the CLI's exact command vocabulary (`terminal.input`, `terminal.resize`,
  `terminal.scroll`, `terminal.release`) so the JSON → `ClientMessage`
  mapping is shared. The browser's first message chooses the mode and the
  geometry (`terminal.open`), which is also how the server learns cols/rows
  before `ObserveTerminal`. `terminal.input` requires the `control` scope
  **and** `mode: "control"`; `terminal.resize`/`terminal.scroll`/
  `terminal.release` are allowed in observe mode (server-verified viewport-
  only semantics for observers; PR 6 records the live result for scroll).
- **(j) Terminal streams use their own per-host transports, scoped
  `gateway`** *(auto default)*. The connector's transports live inside its
  supervisor threads and cannot be borrowed, and a second `SshTransport`
  with E1's scope collides on the forward socket. PR 6 adds
  `SshTransport::new_scoped(…, scope)` and `transport_for_scoped(spec,
  options, scope)` (additive, in files E2 never touches) so the gateway holds
  one `Mutex<Box<dyn HostTransport>>` per host and calls `connect()` once per
  browser terminal (each ssh `connect` is one fresh `ssh` child through the
  bridge, exactly like `herdr --remote`).
- **(k) `web/dist` is committed and embedded with `include_bytes!`** *(auto
  default; matches E4's default)*. E3 commits `web/dist/index.html` (a tiny
  Herdr Fleet placeholder page that calls `/api/gateway`) and an
  `EmbeddedAsset { path, content_type, bytes }` table in
  `src/gateway/assets.rs`; `build.rs` gains `cargo:rerun-if-changed=web/dist`
  so an asset change rebuilds. E4 grows the table; the freshness check is
  E4's.
- **(l) WebSocket stand-in for validation is `scripts/fork/ws-client.py`**
  *(auto default)*: python3 stdlib only (raw socket, RFC 6455 handshake,
  client masking, ping/pong), prints one line per received message (`text
  <json>` / `binary <hex>`), sends `--send` messages and stdin lines, exits
  0 on a clean close. It is both the manual stand-in (no `websocat`) and the
  client `tests/fork_gateway.rs` shells out to, the way `tests/fork_fleet_
  lab.rs` shells out to the lab script.
- **(m) `--config <path>` is applied exactly like `--session`** *(auto
  default)*: set `HERDR_CONFIG_PATH` at the top of `run_gateway_command`
  before any thread exists, so `config_path()`, `Config::load()` and a later
  live reload all agree. `--bind` overrides `[gateway] bind`.
- **(n) `--bind 127.0.0.1:0` is supported and the bound address is printed**
  *(auto default)*: the first stdout line is `listening on
  http://127.0.0.1:<port>` and the same is written to
  `<config>/gateway/gateway.json` (`{pid, listen, started_unix}`, `0600`,
  removed on clean exit) — what `herdr gateway status` and the tests read.
- **(o) Scope information is advertised** *(auto default)*: `GET
  /api/gateway` returns `herdr.gateway.info.v1` (`client_version`, the
  caller's `scope`, `loopback`, `features`) so E4 can show the Control toggle
  only when the device has the scope, and E5/E7 can add features without a
  new endpoint.
- **(p) No E2 code is used** (orchestrator constraint). E3 does not touch
  `src/client/**` or `src/fleet/sidebar.rs`, uses `FleetConnector::events()`
  (not E2's `take_events`) and `FleetConnectorOptions::for_config` (not
  `for_client`), and edits no file E2's PRs own except the three upstream
  wiring lines every subcommand needs (see hazards).

### Sequencing hazards

- **E2 is landing concurrently.** Shared upstream lines: `src/cli.rs`
  (`mod` + match arm), `src/cli/spec.rs` (`.subcommand(…)`), `src/main.rs`
  (bare-command list, `--help` usage line) are edited by **E3 PR 1** and by
  E2 PR 4; `src/config/model.rs` (E3 PR 2 appends `GatewayConfig` after
  `FleetConfig`; E2 PR 6 adds `FleetConfig.keys`); `justfile` (E3 PR 1 adds
  `lint-no-default`/`ci-no-default`; E2 PR 5 adds `bench-fleet-scale`);
  `tests/support/mod.rs` (E3 PR 4 adds `pub mod gateway;`; E2 PR 4 adds
  `pub mod fleet_tui;`). All are adjacent-line textual conflicts: whichever
  lands second rebases onto `master` keeping both sides, re-runs the gate.
  E2 PR 1 changes `src/fleet/connector.rs` (`FleetConnectorOptions` grows
  `for_client`/`ActiveGeometry`, `take_events`, `shutdown` tolerant of a
  taken receiver) and `handshake.rs`: E3 PR 3 uses only `start`, `events`,
  `shutdown`, `for_config`, `FleetEvent::Host`, which E2 PR 1's Downstream
  promises to keep. **If E2 PR 1 lands first and removes `events()` in
  favour of `take_events()`, PR 3 switches to `take_events()` (an owned
  receiver suits the gateway's task better) — flag it in the PR body.** E3
  never edits `src/fleet/mod.rs`, `connector.rs`, `handshake.rs`,
  `state.rs`, `report.rs`, `hosts.rs`, `refs.rs`, `oneshot.rs` or anything
  under `src/client/`; its only fleet edit is PR 6's additive
  `transport/{mod,ssh}.rs`, which no E2 PR lists.
- **Upstream files touched, and by which PR only:** `Cargo.toml`,
  `Cargo.lock`, `justfile`, `.github/workflows/fork-ci.yml`, `src/main.rs`
  (`mod gateway;`, allowlist, usage), `src/cli.rs`, `src/cli/spec.rs` — PR 1.
  `src/config/model.rs`, `src/config/io.rs`, `src/config.rs` (only if
  `collect_diagnostics` needs a line), `src/main.rs` (`DEFAULT_CONFIG` block
  + two tests), `scripts/config_reference_check.py` — PR 2. `build.rs`
  (one `rerun-if-changed`), `.gitignore` (nothing expected — `node_modules/`
  is already ignored; `web/dist` is committed) — PR 4.
  `src/client/terminal_sessions.rs` (one visibility widening,
  `pub(super)` → `pub(crate)`, on `terminal_control_command_from_json`) —
  PR 6 (file not owned by any E2 PR). `tests/support/mod.rs` — PR 4. Never:
  `src/protocol/**`, `src/server/**`, `src/app/**`, `src/remote/attach.rs`,
  `tests/fixtures/endpoint-*.json`, `docs/next/**`.
- **PR 1 changes `Cargo.toml`/`Cargo.lock` and runs alone** in its wave; it
  is the only dependency change of the epic — every later PR is dep-free.
- **`src/gateway/server.rs` (router) and `tests/fork_gateway.rs` are edited
  by PRs 4, 5, 6, 7, 8.** Waves keep at most two of those in flight (W4
  `[5, 6]`, W5 `[7, 8]`); the later PR in a wave rebases onto `master`
  before its gate and re-runs the integration test. Each PR adds its routes
  as a separate `fn <area>_routes() -> Router<AppState>` merged in `router()`
  so the diffs are one line apart, not interleaved.
- **Two Rust builds at once** (W2, W4, W5) is the memory hazard the gate
  lock exists for; with E2 running in parallel the orchestrator's cap, not
  this plan, bounds the total. Never bypass `scripts/fork/gate.sh`.
- **The gateway holds every configured host as its foreground client at
  120×40** for as long as it runs (E1 caveat). Every live validation points
  `[fleet]` at lab sessions only (`include_local = false`), under the lab's
  `XDG_CONFIG_HOME`, never at the user's default session; `docs/fork/
  gateway.md` documents the headless-server assumption.
- **Secrets in evidence.** Validation transcripts paste status codes, JSON
  keys and frame headers — never a token, cookie or pairing URL. Test
  fixtures generate tokens under a throwaway `XDG_CONFIG_HOME`; nothing
  under `~/.config/herdr*` is read or written by any test.
- **CI now runs the crate twice** (`just ci` and `just ci-no-default`); the
  second job shares the toolchain/Zig steps but not the cargo cache key, so
  budget ~10 extra minutes per run. `tests/live_handoff.rs` `wait_for_file`
  remains a known upstream flake (`gh run rerun --failed -R
  vinceseguin/herdr`).

## Status legend

✅ merged · 🔨 in progress · ⬜ not started · ⛔ blocked

## PR map

| # | Title | Group | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | chore: add gateway cargo feature with axum, qrcode and token dependencies | A · Foundations | — | ⬜ |
| 2 | feat(gateway): gateway config section, token store, bind and origin policy | A · Foundations | 1 | ⬜ |
| 3 | feat(gateway): async fleet runtime folding connector events into shared state | A · Foundations | 1 | ⬜ |
| 4 | feat(gateway): herdr gateway serves health, fleet report and embedded assets over http | B · HTTP | 2, 3 | ⬜ |
| 5 | feat(gateway): websocket fleet event stream | C · Streams | 4 | ⬜ |
| 6 | feat(gateway): websocket terminal observe stream over per-host transports | C · Streams | 4 | ⬜ |
| 7 | feat(gateway): terminal control mode gated by the control scope | C · Streams | 6 | ⬜ |
| 8 | feat(gateway): pairing urls with qr codes, device cookies, status and token rotation | D · Ops | 4 | ⬜ |
| 9 | docs: gateway guide, systemd unit, adr e3 review, roadmap drift | E · Docs | 5, 7, 8 | ⬜ |

**Wave preview (2-agent cap, Cargo PR alone):** W1 `[1]` → W2 `[2, 3]` →
W3 `[4]` → W4 `[5, 6]` → W5 `[7, 8]` → W6 `[9]`. Critical path 1 → 2 → 4 →
6 → 7 → 9. Only PR 1 touches `Cargo.toml`/`Cargo.lock`.

**Model assignment:** tasks run on `opus`. Review agent must be **`fable`**
for **PR 2** (token files, constant-time compare, rate limiter, bind/origin
policy), **PR 4** (the auth middleware and route-level scope gates — the
first network surface), **PR 6** (a terminal stream attached to the wrong
host or pane is a silent mis-route; ssh transport scoping and drop order),
**PR 7** (input reaching a terminal: scope gate, takeover, release), and
**PR 8** (pairing exchange, cookie minting, rotation/revocation). PRs 1, 3,
5 and 9 review on `opus`.

## Verification (the gate — every PR)

```bash
bash scripts/fork/gate.sh <worktree>        # runs `just ci` under the machine-wide lock
echo "EXIT=$?"                              # read the EXIT= line; never pipe the wrapper
```

`just ci` = `cargo fmt --check` + `cargo clippy --all-targets --locked -D
warnings` + `cargo nextest run --locked` + python maintenance tests
(including `test_config_reference_check` and
`test_ui_hot_path_architecture`) + bun suites. While iterating use
`bash scripts/fork/gate.sh <worktree> "test-one <filter>"` (a filter that
matches nothing exits 4). **From PR 1 on, every PR also runs the second
feature set before opening:** `bash scripts/fork/gate.sh <worktree>
ci-no-default` (= `cargo clippy --all-targets --locked --no-default-features
-- -D warnings` + `cargo nextest run --locked --no-default-features`), and
fork CI runs both as separate jobs — `check (ubuntu-latest)` and
`check-no-default-features (ubuntu-latest)`. A PR is green only when both
jobs and `conventional-commits`/`shellcheck` pass.

Real-server validation is mandatory for every PR with runtime code (2–8):
`cargo build`, `bash scripts/fork/fleet-lab.sh up 2`, `eval "$(bash
scripts/fork/fleet-lab.sh env)"`, a `[fleet]` block with `include_local =
false` naming `lab-1`/`lab-2` as `kind = "local"` hosts (plus the PR's
`[gateway]` keys) appended to `$XDG_CONFIG_HOME/herdr-dev/config.toml`, and
`H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV
target/debug/herdr"`. The gateway runs in the foreground from that shell
(`$H gateway --bind 127.0.0.1:7788 & GW=$!`), tokens are read from
`$XDG_CONFIG_HOME/herdr-dev/gateway/*.token`, HTTP is exercised with `curl
-s -o /dev/null -w '%{http_code}'` (status) and `curl -s … | python3 -c`
(JSON keys), WebSockets with `python3 scripts/fork/ws-client.py` (PR 5+).
Each PR states the exact commands, the expected status codes, JSON keys and
frame bytes, and which lab pane changed (`$H --session lab-N pane read
"$HERDR_FLEET_LAB_PANE_N" --source recent`). Tear down with `kill -TERM $GW;
wait $GW` (expect exit 0 and no `gateway.json` left), then `fleet-lab.sh
down`, and confirm from a shell **without** the lab env that `herdr session
list` shows no `lab-*` and that `ls ~/.config/herdr*/gateway` does not exist.
SSH validations (PR 6) add `ssh-lab.sh up` + `eval "$(bash
scripts/fork/ssh-lab.sh env)"`, run the gateway with
`HOME=$HERDR_SSH_LAB_HOME`, and tear down `ssh-lab.sh down` **before**
`fleet-lab.sh down`; `sshd not found` (exit 3) degrades to local hosts and
is recorded in the PR body.

CI gotchas to expect:

- Zig 0.15.2 is needed for **both** jobs (`build.rs` always builds
  libghostty-vt); the `--no-default-features` job copies the `check` job's
  toolchain/Zig/just/nextest steps with cache key
  `fork-ubuntu-latest-nodefault`; it does not need bun.
- `cargo nextest run --locked --no-default-features` compiles every
  `tests/*.rs`; gateway tests must be `#![cfg(feature = "gateway")]` and
  in-crate gateway modules `#[cfg(feature = "gateway")]`, or the second job
  fails to build.
- `just check`'s `windows-lint` is not in the gate but the code must
  clippy-compile for `x86_64-pc-windows-msvc`: token-file modes,
  `tokio::signal::unix`, and `LocalStream` details go behind `#[cfg(unix)]`
  with a Windows arm (`0600` verification is a no-op there, like
  `restrict_socket_permissions`).
- `src/cli/spec.rs` invariants (`spec_describes_all_completion_commands`,
  `every_spec_subcommand_renders_short_and_long_help`,
  `spec_passes_clap_invariants`) must stay green with a feature-gated
  subcommand: the completion-command list and the spec must agree under
  both feature sets.
- `scripts/test_config_reference_check.py` runs in `just ci` against the
  real model: `[gateway]` must be in `SKIPPED_SUBTREES` (PR 2) or the gate
  fails; the section is documented in `docs/fork/gateway.md` only.
- `clippy::large_enum_variant`, `too_many_arguments` (threshold 11) and
  `-D warnings` apply; dead code introduced for a later PR carries a narrow
  `#[allow(dead_code)]` with a comment naming that PR, removed by it.
- `scripts/fork/ws-client.py` is python (not shellchecked) and must run on
  python ≥ 3.10 with the stdlib only; any new `scripts/fork/*.sh` must be
  `shellcheck -S warning` clean.
- Integration tests boot two lab servers each; keep them in one binary
  (`tests/fork_gateway.rs`) and well under the 30-minute job.

## Cross-cutting constraints (all PRs)

- **Servers stay stock; no wire or endpoint-contract change.**
  `src/protocol/**`, `src/server/**`, `src/app/**`,
  `tests/fixtures/endpoint-*.json` have an empty diff for the whole epic. The
  gateway speaks the frozen `TerminalHello`/`ObserveTerminal`/
  `ControlTerminal`/`Input`/`Resize`/`AttachScroll`/`Detach` messages and
  the generation-1 endpoint handshake E1 already speaks, nothing else.
- **Additive, mergeable code.** All new code is `src/gateway/**`, `web/**`,
  `scripts/fork/ws-client.py`, `scripts/fork/systemd/**`, `tests/
  fork_gateway.rs`, `tests/support/gateway.rs`, `docs/fork/**`. Upstream
  edits are limited to the list in *Sequencing hazards*; each is a handful
  of lines and lands in exactly one PR. No E2 file is edited.
- **Security (roadmap principles 3 and 7) is not negotiable:** loopback
  default; non-loopback bind refused without `allowed_origins`; tokens
  `0600` under `<config>/gateway/` (`0700`), verified before use and refused
  if group/world readable; constant-time digest compare; per-peer failure
  rate limiting; the `read` scope can **never** send input (enforced in the
  terminal session state machine, not only at the route); every terminal
  socket read uses `MAX_FRAME_SIZE`, inbound WebSocket messages are capped
  (4 KiB on `/api/events`, 64 KiB on `/api/terminal/*`); no secret is ever
  logged, printed in evidence, or committed. Nothing destructive is
  reachable from the gateway in E3 (no `pane.close`, no `server.stop`).
- **State is separated from runtime; render is pure.** `src/gateway/auth.rs`
  (token digests, device records, pairing codes, `AuthLimiter`, bind/origin
  policy) and `src/gateway/protocol.rs` (message shapes, the binary frame
  header) are pure, sync, testable without sockets or a runtime; only
  `fleet.rs`, `terminal.rs`, `server.rs` and the handlers touch tokio, axum
  or streams. `FleetState` stays the single source of truth; the gateway
  keeps no second copy of hosts or agents.
- **Host failure is local.** An unavailable host is data in `/api/fleet` and
  a `host_connection` delta, never a 5xx; a terminal open on a down host is
  a `terminal.error` on that socket only; a slow browser stalls its own
  terminal session only. Nothing in the gateway calls `std::process::exit`
  after startup or panics because of what a host or a client sent.
- **Multiplicative-perf discipline.** Broadcast fan-out is × subscribers
  per change: serialize each `FleetChange` **once** (`Arc<str>`) and clone
  the pointer. `/api/fleet` costs one `from_state` per request (O(hosts ×
  agents)); do not cache it (it is cheap and would race deltas). Terminal
  sessions are × open terminals, never × panes: nothing observes a pane
  nobody is watching, and frames are never re-encoded.
- **Runtime/client boundary.** Everything the gateway exposes is a shared
  runtime fact expressed in neutral names (`host`, `pane`, `agent`,
  `connection`, `scope`); no UI-surface names. Server-side ids stay the
  server's (`w1:p1`), host-qualified only through `FleetPaneRef`.
- **Code conventions.** No `unwrap()`/`expect()` in production code;
  `tracing` (`target: "gateway"`) for logs; `#[allow]` only with a reason;
  platform code compile-gated; no dependency beyond PR 1's five;
  protocol-version untouched.
- **Docs.** `docs/fork/gateway.md` (PR 9) is the only reference for
  `[gateway]` keys, the HTTP/WS contract and the token files; every earlier
  PR adds its keys/endpoints to a running *Reference notes* list in its PR
  body so PR 9 needs no re-derivation. Never edit `docs/next/**`, root
  `README.md`, `CHANGELOG.md`.
- **Commits.** Lowercase conventional subjects from the allowed types, the
  `Co-Authored-By`/`Claude-Session` trailers, no `refs #<n>`. Branches
  `feat/e3-pr<N>-<slug>` (`chore/…`, `docs/…`) from the latest
  `origin/master`; squash-merge with `--delete-branch` once both CI jobs
  are green.

## Per-PR detail

### PR 1 — chore: add gateway cargo feature with axum, qrcode and token dependencies · deps: —

**Goal:** the crate gains its first cargo feature, `gateway`, on by default,
with every dependency the epic needs declared once, minimal features, and a
written reason each; a feature-gated `src/gateway/` skeleton is wired into
the CLI so `herdr gateway` exists (and prints usage) in a default build and
is absent from a `--no-default-features` build; the gate and fork CI test
both feature sets from this PR on. Later PRs are dependency-free.

**Files**

- `Cargo.toml` *(upstream file — minimal wiring)*: a `[features]` section and
  five optional dependencies; `tokio` gains `net` and `signal`.
- `Cargo.lock`: regenerated (`cargo update -p herdr` is not enough; run
  `cargo build` and commit the lock).
- `src/gateway/mod.rs` (new): module doc, `pub(crate) fn
  run_gateway_command(args: &[String]) -> io::Result<i32>` (usage on
  stderr + exit 2 for everything except `help` → 0), `GATEWAY_USAGE`.
- `src/main.rs` *(upstream file — minimal wiring)*: `#[cfg(feature =
  "gateway")] mod gateway;` after `mod fleet;`; `#[cfg(feature = "gateway")]
  "gateway",` in the bare-command allowlist; a gated `println!("       herdr
  gateway [--bind ADDR] …")` usage line next to the `fleet` one.
- `src/cli.rs` *(upstream file — minimal wiring)*: `#[cfg(feature =
  "gateway")] "gateway" => crate::gateway::run_gateway_command(&args[2..])?,`.
- `src/cli/spec.rs` *(upstream file — minimal wiring)*: `fn
  gateway_command() -> Command` (gated) and a gated `.subcommand(…)`; keep
  the three spec invariants green under both feature sets.
- `justfile` *(upstream file — minimal wiring)*: `lint-no-default` and
  `ci-no-default` recipes (below).
- `.github/workflows/fork-ci.yml` (fork-owned): job
  `check-no-default-features` (`name: check-no-default-features
  (ubuntu-latest)`).
- `docs/fork/README.md`: one row per upstream file in the fork-wiring table
  (`Cargo.toml` feature + deps, `justfile`, `src/main.rs`/`src/cli.rs`/
  `src/cli/spec.rs` gateway arms) and the CI job in the CI table.

**Shapes/approach**

```toml
[features]
default = ["gateway"]
# The fork's HTTP/WebSocket gateway (`herdr gateway`). Off = upstream-shaped binary.
gateway = ["dep:axum", "dep:qrcode", "dep:subtle", "dep:getrandom", "tokio/net", "tokio/signal"]

[dependencies]
# gateway: HTTP router + WebSocket upgrade on the existing tokio; `ws` brings
# tokio-tungstenite transitively so no direct WebSocket dependency is needed.
axum = { version = "0.8", default-features = false, features = ["http1", "tokio", "ws", "json", "query"], optional = true }
# gateway: terminal QR code for `herdr gateway pair` (unicode renderer only).
qrcode = { version = "0.14", default-features = false, optional = true }
# gateway: constant-time token digest comparison (no dependencies).
subtle = { version = "2.6", optional = true }
# gateway: 32-byte token/pairing secrets from the OS CSPRNG (already in the tree).
getrandom = { version = "0.3", optional = true }
```

Reason per crate, recorded in `Cargo.toml` comments and the PR body: `axum`
(roadmap default; hand-rolling HTTP/1.1 + RFC 6455 is ~1.5 kLOC of security-
sensitive parsing); `qrcode` (roadmap default; the only QR encoder small
enough to vendor-free, `default-features = false` drops `image`); `subtle`
(no constant-time compare exists in the tree; `sha2` digests + `ct_eq` is
the standard pattern); `getrandom` (no CSPRNG exists in the tree; it is
already a transitive dependency at 0.3.x, so the lock gains no subtree —
`rand` would add one); `tokio` `net` + `signal` are features of an existing
dependency. Explicitly **not** added: `tower-http` (static files are ~40
lines over an embedded table), `hyper`/`http` direct (reached as
`axum::http`), `tokio-tungstenite` direct, `rand`, `base64`/`sha2` (already
direct), `include_dir`, `url` (origins are parsed by hand: `scheme://host[:
port]`, nothing else is accepted). Also decline the E3 deps that E6/E8 will
need (`web-push` etc.) — one PR, one reason set.

```just
[unix]
lint-no-default:
    cargo clippy --all-targets --locked --no-default-features -- -D warnings

ci-no-default: lint-no-default
    cargo nextest run --locked --no-default-features -E 'all()' --status-level fail --final-status-level slow --failure-output final --success-output never
```

The CI job duplicates `check`'s checkout/toolchain/install-action/setup-zig/
rust-cache steps (cache key `fork-ubuntu-latest-nodefault`, no bun), then
`run: just ci-no-default`. `run_gateway_command` is the whole runtime
surface of this PR: `herdr gateway help` → usage, exit 0; anything else →
usage on stderr, exit 2, so every wiring line is exercised. Under
`--no-default-features`, `herdr gateway` must print `unknown command:
gateway` and exit 2 (the existing path), which the second CI job proves by
building at all and a `#[cfg(not(feature = "gateway"))]` unit test in
`src/cli/spec.rs`'s test module asserting the spec has no `gateway`
subcommand.

**Tests**

- `src/cli/spec.rs`: the three invariants pass with and without the
  feature; a gated test asserts `gateway` renders `herdr gateway --help`.
- `src/gateway/mod.rs`: `run_gateway_command(["help"])` → `Ok(0)`;
  `([])`/`(["bogus"])` → `Ok(2)`.
- `tests/fork_gateway.rs` (new, `#![cfg(all(unix, not(target_os = "macos"),
  feature = "gateway"))]`): `gateway_help_exits_zero` running
  `CARGO_BIN_EXE_herdr gateway help` under a throwaway `XDG_CONFIG_HOME` —
  the file exists from PR 1 so later PRs append tests, never create it.
- `just ci` **and** `just ci-no-default` green locally (paste both `EXIT=`
  lines).

**Real-server validation**

```bash
cargo build && target/debug/herdr gateway help; echo "exit=$?"                      # usage, exit=0
target/debug/herdr gateway; echo "exit=$?"                                          # usage on stderr, exit=2
target/debug/herdr --help | grep -c 'herdr gateway'                                 # 1
cargo build --no-default-features && target/debug/herdr gateway; echo "exit=$?"     # unknown command: gateway, exit=2
target/debug/herdr --version                                                        # herdr 0.8.2-fork (both builds)
cargo tree -e features -i axum | head -3                                            # axum reached only via feature gateway
grep -c 'name = "axum"\|name = "qrcode"\|name = "subtle"' Cargo.lock                # 3
```

Evidence: the two `exit=` lines per build, the `--help` grep, the lock grep,
and both CI jobs green on the PR. No lab is needed.

**Downstream**

- The feature name `gateway` and the `#[cfg(feature = "gateway")]` idiom are
  fixed; every gateway item, test and `tests/fork_gateway.rs` uses exactly
  that gate. E4 (`web/dist` embedding) and E5 (`--tailscale`) add nothing to
  `Cargo.toml`; E6 (`web-push`) will need its own solo dependency PR.
- `run_gateway_command` is the single CLI entry; PRs 4 and 8 add arms
  (`status`, `pair`, `rotate-token`) and the bare run inside it, never a
  second dispatch in `src/cli.rs`.
- `just ci-no-default` and the `check-no-default-features (ubuntu-latest)`
  job are part of "green" for every later PR in the fork, not only E3.

### PR 2 — feat(gateway): gateway config section, token store, bind and origin policy · deps: 1

**Goal:** the pure half of the gateway's security model: a `[gateway]`
config section with defaults and diagnostics, a token store that creates
and verifies `0600` token files with constant-time digest compares, device
and pairing records, a failure rate limiter, and the bind/origin policy —
all testable without a socket or a runtime.

**Files**

- `src/config/model.rs` *(upstream file — minimal wiring)*: `GatewayConfig`
  appended after `FleetConfig`, `pub gateway: GatewayConfig` as the last
  field of `Config`, `impl Default`, `diagnostics()`.
- `src/config/io.rs` *(upstream file — minimal wiring)*: `"gateway"` in
  `KNOWN_TOP_LEVEL_CONFIG_KEYS`; `load_live_section(table, "gateway", …)` +
  `diagnostics.extend(config.gateway.diagnostics())` on the live path.
- `src/config.rs` *(upstream file — minimal wiring)*: chain
  `gateway.diagnostics()` in `collect_diagnostics` if the section list is
  explicit there.
- `src/main.rs` *(upstream file — minimal wiring)*: a fully commented
  `[gateway]` block in `DEFAULT_CONFIG` after `[fleet]`, plus
  `default_config_gateway_block_parses_without_diagnostics` and
  `default_config_gateway_block_is_commented_out` mirroring the fleet tests.
- `scripts/config_reference_check.py` *(upstream file — minimal wiring)*:
  `"gateway"` in `SKIPPED_SUBTREES` with the same fork comment as `fleet`.
- `src/gateway/auth.rs` (new, pure except the file I/O helpers):
  `TokenScope`, `TokenDigest`, `TokenStore`, `DeviceRecord`, `DeviceStore`,
  `PairingCode`, `PairingStore`, `AuthLimiter`, `Principal`.
- `src/gateway/policy.rs` (new, pure): `BindPolicy`, `OriginAllowlist`.
- `src/gateway/paths.rs` (new): `gateway_dir()`, file names, the
  `0700`/`0600` create-and-verify helpers (unix; no-op verification on
  windows).
- `src/gateway/mod.rs`: `mod auth; mod paths; mod policy;` (items carry a
  narrow `#[allow(dead_code)]` naming PR 4 until it consumes them).

**Shapes/approach**

```toml
[gateway]
# Herdr Fleet gateway (`herdr gateway`): HTTP + WebSocket for phones and browsers.
# Loopback by default. Any other bind address also needs `allowed_origins`.
# bind = "127.0.0.1:7788"
# Browser origins allowed to call the API, as scheme://host[:port]. Required for a
# non-loopback bind; a loopback bind allows its own origins when this is empty.
# allowed_origins = ["https://gateway.tailnet-name.ts.net"]
# The origin pairing URLs and QR codes advertise (E5: the `tailscale serve` HTTPS
# origin). Implicitly allowed; an https scheme marks device cookies Secure.
# public_url = ""
# Failed authentications per peer address before it is refused for the window.
# auth_failure_limit = 5
# auth_failure_window_secs = 60
# How long a pairing URL from `herdr gateway pair` stays valid.
# pairing_ttl_secs = 600
```

```rust
// src/config/model.rs
#[derive(Debug, Clone, Deserialize)] #[serde(default)]
pub struct GatewayConfig {
    pub bind: String,                    // "127.0.0.1:7788"
    pub allowed_origins: Vec<String>,    // []
    pub public_url: String,              // ""
    pub auth_failure_limit: u32,         // 5
    pub auth_failure_window_secs: u64,   // 60
    pub pairing_ttl_secs: u64,           // 600
}
impl GatewayConfig {
    pub fn diagnostics(&self) -> Vec<String>  // bind parses as SocketAddr; each origin and public_url
                                              // parse as scheme://host[:port] with no path/userinfo/query;
                                              // limits > 0; ttl 30..=86400
    pub fn bind_addr(&self) -> Option<SocketAddr>
}

// src/gateway/auth.rs
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "lowercase")]
pub enum TokenScope { Read, Control }
impl TokenScope { pub fn allows(self, needed: TokenScope) -> bool }    // Control allows Read
pub struct TokenDigest([u8; 32]);                                      // sha2::Sha256 of the presented/stored secret
impl TokenDigest { pub fn of(secret: &[u8]) -> Self; pub fn ct_eq(&self, other: &Self) -> bool /* subtle */ }
pub fn random_secret_hex() -> io::Result<String>                       // getrandom::fill 32 bytes → 64 hex
pub struct TokenStore { read: TokenDigest, control: TokenDigest, loaded_at: FileStamp }
impl TokenStore {
    pub fn load_or_create(dir: &Path) -> io::Result<Self>              // creates dir 0700 + both files 0600 if absent;
                                                                        // refuses an existing file with group/other bits
    pub fn verify_bearer(&self, presented: &str) -> Option<TokenScope>  // hex-decode, digest, ct_eq control then read
    pub fn rotate(dir: &Path, scope: TokenScope) -> io::Result<()>
}
#[derive(Serialize, Deserialize)] pub struct DeviceRecord { pub id: String, pub scope: TokenScope,
    pub secret_sha256: String, pub label: String, pub created_unix: u64, pub last_seen_unix: u64 }
pub struct DeviceStore;   // devices.json (0600): load, verify_cookie("<id>.<secret>") -> Option<(scope, id)>,
                          // insert, revoke_scope(scope), all constant-time on the secret
pub struct PairingCode { pub id: String, pub secret_sha256: String, pub scope: TokenScope, pub expires_unix: u64 }
pub struct PairingStore; // pairings/<id>.json (0600): create(scope, ttl, now) -> (code_text "<id>.<secret>", PairingCode),
                         // consume(code_text, now) -> Result<PairingCode, PairingError{NotFound|Expired|Invalid}>
                         // (deletes the file on any outcome that names a file), sweep_expired(now)
pub struct AuthLimiter { limit: u32, window: Duration, peers: HashMap<IpAddr, Vec<Instant>> }
impl AuthLimiter { pub fn new(limit, window) -> Self; pub fn check(&mut self, peer: IpAddr, now: Instant) -> Result<(), Duration>;
                   pub fn record_failure(&mut self, peer, now); /* evicts expired stamps; caps peers at 4096 */ }
pub struct Principal { pub scope: TokenScope, pub via: Credential /* Bearer | Device{id} */ }

// src/gateway/policy.rs
pub struct BindPolicy;
impl BindPolicy {
    pub fn check(bind: SocketAddr, config: &GatewayConfig) -> Result<(), String>
    // loopback → Ok; otherwise Err("refusing to bind <addr>: [gateway] allowed_origins is empty; …") unless non-empty
}
pub struct OriginAllowlist { origins: Vec<Origin> }   // Origin { scheme, host (lowercase), port: Option<u16> }
impl OriginAllowlist {
    pub fn for_bind(bind: SocketAddr, config: &GatewayConfig) -> Self  // config origins + public_url + (loopback only) self origins
    pub fn allows(&self, origin_header: &str) -> bool
}
```

`paths.rs`: `gateway_dir() = crate::config::config_dir().join("gateway")`,
`read.token`, `control.token`, `devices.json`, `pairings/`, `gateway.json`;
`create_private_dir(path)` (`0700`, verifies mode and ownership of an
existing dir on unix, mirrors `src/pane_graphics_files.rs`),
`write_private_file(path, bytes)` (create with `.mode(0o600)` then rename
into place), `verify_private_file(path)` (refuses `0o077` bits). Windows
arms compile and skip the mode checks with a `debug!`.

**Tests** (all in-crate, pure, no runtime)

- `GatewayConfig`: defaults; every diagnostic (bad bind, origin with path,
  `public_url` with userinfo, zero limits, ttl out of range); live-reload
  isolates a bad `[gateway]` to `invalid_sections == ["gateway"]`; the two
  `DEFAULT_CONFIG` tests.
- `TokenStore`: `load_or_create` creates both files with `0600` in a `0700`
  dir (unix); a `0644` token is refused with a message naming the file;
  `verify_bearer` accepts the exact token, rejects a one-char change, a
  truncated token, non-hex, empty, and the read token for control;
  `rotate(Control)` changes only `control.token`.
- `DeviceStore`: insert/verify/revoke_scope; a cookie with a valid id and
  wrong secret fails; revoke keeps the other scope's devices.
- `PairingStore`: consume once → `Ok`, twice → `NotFound`; expired →
  `Expired` and the file is gone; malformed → `Invalid`, nothing deleted.
- `AuthLimiter`: 5 failures within 60 s → blocked with the remaining
  duration; 61 s later → allowed; eviction at 4096 peers.
- `BindPolicy`/`OriginAllowlist`: `127.0.0.1`, `::1` and `localhost`-mapped
  binds are loopback; `0.0.0.0`/`100.x`/LAN binds require origins;
  `allows` is exact on scheme/host/port, case-insensitive on host, rejects
  `null`, subdomains, trailing slashes; self origins only on loopback.

**Real-server validation**

```bash
cargo build && export XDG_CONFIG_HOME=/tmp/herdr-e3-pr2 && mkdir -p $XDG_CONFIG_HOME/herdr-dev
printf 'onboarding = false\n[gateway]\nbind = "0.0.0.0:7788"\n' > $XDG_CONFIG_HOME/herdr-dev/config.toml
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
$H config check; echo "exit=$?"                                     # no diagnostics (a bad bind is refused at run time, not parse time)
printf '[gateway]\nbind = "not-an-addr"\nallowed_origins = ["http://x/path"]\n' >> $XDG_CONFIG_HOME/herdr-dev/config.toml
$H config check; echo "exit=$?"                                     # two [gateway] diagnostics naming bind and allowed_origins[0]
$H --default-config | sed -n '/^\[gateway\]/,/^$/p'                 # every line after the header starts with '#'
rm -rf /tmp/herdr-e3-pr2
```

Evidence: the diagnostic lines, the commented block, and the unit test
names green. No token file is created by this PR's CLI surface (the store is
first consumed by PR 4).

**Downstream**

- `TokenScope`, `Principal`, `TokenStore::verify_bearer`,
  `DeviceStore::verify_cookie`, `AuthLimiter`, `BindPolicy`,
  `OriginAllowlist` are what PR 4's middleware calls; PR 8 calls
  `PairingStore`, `DeviceStore::insert`, `TokenStore::rotate`. No later PR
  adds a second credential path.
- `[gateway]` key names are the user-facing contract (add, never rename).
  E5 adds nothing here (`public_url` and `bind` cover the tailnet case;
  `--tailscale` only computes them).
- File names under `<config>/gateway/` are fixed; E6 adds `vapid.json` and
  `subscriptions.json` beside them using the same `write_private_file`.

### PR 3 — feat(gateway): async fleet runtime folding connector events into shared state · deps: 1

**Goal:** the gateway's fleet half: start the E1 connector from config, fold
every `FleetEvent::Host` into a shared `FleetState` from a tokio task,
broadcast each `FleetChange` pre-serialized to any number of subscribers,
serve `FleetStatusReport` on demand, and shut down cleanly — with no HTTP
yet.

**Files**

- `src/gateway/fleet.rs` (new): `FleetRuntime`, `FleetHandle`,
  `ChangeStream`.
- `src/gateway/mod.rs`: `mod fleet;` (+ narrow allow naming PR 4).

**Shapes/approach**

```rust
pub struct FleetRuntime { task: JoinHandle<()>, stop: watch::Sender<bool>, handle: FleetHandle }
#[derive(Clone)]
pub struct FleetHandle {
    state: Arc<Mutex<FleetState>>,                 // std Mutex; never held across .await
    changes: broadcast::Sender<Arc<str>>,          // each FleetChange serialized once
    client_version: Arc<str>,
}
impl FleetRuntime {
    /// resolve_hosts(&config.fleet) → FleetState::new(specs) (active host cleared, as `herdr fleet status` reports)
    /// → FleetConnector::start(specs, FleetConnectorOptions::for_config(config)) → spawn the fold task.
    pub fn start(config: &Config) -> Result<Self, Vec<String>>      // Err = [fleet] diagnostics (exit 1 in PR 4)
    pub fn handle(&self) -> FleetHandle
    /// Signal stop, await the task, then `connector.shutdown()` (blocking, run in spawn_blocking).
    pub async fn shutdown(self)
}
impl FleetHandle {
    pub fn report(&self) -> FleetStatusReport                          // FleetStatusReport::from_state(&mut state, version)
    pub fn host_connection(&self, host: &HostId) -> Option<HostConnection>
    pub fn host_spec(&self, host: &HostId) -> Option<HostSpec>
    /// Subscribe first, then snapshot under the same lock: no delta can fall between them.
    pub fn subscribe_with_report(&self) -> (ChangeStream, FleetStatusReport)
}
pub struct ChangeStream(broadcast::Receiver<Arc<str>>);
pub enum ChangeItem { Change(Arc<str>), Lagged }
impl ChangeStream { pub async fn next(&mut self) -> Option<ChangeItem> }   // None when the runtime stopped
```

The fold task: `loop { select! { _ = stop.changed() => break, ev =
connector.events().recv() => match ev { Some(FleetEvent::Host{host, event})
=> { let changes = state.lock().apply(&host, event); for c in changes {
let _ = changes_tx.send(serde_json::to_string(&c)?.into()); } } Some(_) =>
{} /* notifications/endpoint responses: ignored in E3 */ None => break } }
}`. Because the connector is owned by the task, `shutdown` sends the stop
signal, awaits the task (which returns the connector), then runs
`connector.shutdown()` inside `spawn_blocking` so the forward sockets are
unlinked even when no HTTP request is in flight. `report()` calls
`set_active_host(None)` once at construction so `active_host` is `null`
exactly as `herdr fleet status --json` prints it. Nothing here imports
axum.

**Tests** (in-crate; `#[cfg(all(test, unix))]` for the socket ones)

- Pure fan-out: with a `FleetState::test_new()` behind a `FleetHandle`
  built by a test constructor, applying a synthetic `HostEvent::Connected`
  + `Snapshot` yields the serialized `host_connection`/`snapshot`/
  `agent_added` strings on two subscribers, identical `Arc` pointers.
- `subscribe_with_report` ordering: a change applied concurrently is either
  in the report or in the stream, never in neither (loop 200 iterations
  with a spawned applier).
- Lag: a subscriber that stops reading past the capacity gets `Lagged`
  then resumes.
- Against real sockets, the `src/fleet/oneshot.rs` idiom (a throwaway
  `XDG_CONFIG_HOME`, `bind_local_listener` on the session's client-socket
  path, a fake endpoint that answers the generation-1 hello and sends one
  snapshot): `FleetRuntime::start` reports the host `connected`, the
  subscriber receives `host_connection` + `snapshot`; killing the fake
  listener yields `unavailable`; `shutdown().await` completes within 3 s.
- `start` on a `[fleet]` naming `local` while `include_local = true` →
  `Err(diagnostics)`.

**Real-server validation**

No CLI surface yet; prove it with the socket-level test above plus one
throwaway binary-free check that the connector path is untouched:

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
cat >> "$XDG_CONFIG_HOME/herdr-dev/config.toml" <<'EOF'
[fleet]
include_local = false
[[fleet.hosts]]
name = "lab-1"
kind = "local"
session = "lab-1"
[[fleet.hosts]]
name = "lab-2"
kind = "local"
session = "lab-2"
EOF
$H fleet status --json | python3 -c 'import json,sys;r=json.load(sys.stdin);print(r["schema"],[(h["id"],h["connection"]["state"]) for h in r["hosts"]])'
# → herdr.fleet.status.v1 [('lab-1','connected'),('lab-2','connected')]  (unchanged by this PR)
bash scripts/fork/gate.sh <worktree> "test-one gateway::fleet"           # EXIT=0
bash scripts/fork/fleet-lab.sh down
```

Evidence: the unchanged status line and the `test-one` `EXIT=0`.

**Downstream**

- `FleetHandle` is the only way handlers reach fleet data; PR 4 serves
  `report()`, PR 5 uses `subscribe_with_report`, PR 6 uses
  `host_connection`/`host_spec` to fail fast and to build transports, E7
  adds `request(host, …)` here (through the connector's `send`, which then
  needs the handle to own a `FleetConnector` reference — E7 decides).
- The broadcast payload is the `FleetChange` JSON verbatim; PR 5 wraps
  nothing around it. New change kinds arrive automatically; readers skip
  unknown `kind`s.
- If E2 PR 1 has landed, `take_events()` replaces `events()` here with no
  other change.

### PR 4 — feat(gateway): herdr gateway serves health, fleet report and embedded assets over http · deps: 2, 3

**Goal:** `herdr gateway [--bind ADDR] [--config PATH]` runs: loads config,
enforces the bind policy, creates the tokens, starts the fleet runtime and
an axum server with the auth middleware, and serves `GET /health`,
`GET /api/gateway`, `GET /api/fleet` and the embedded `web/dist` — the first
network surface, loopback by default, with graceful shutdown.

**Files**

- `src/gateway/server.rs` (new): `AppState`, `router()`, `serve()`,
  `shutdown_signal()`.
- `src/gateway/middleware.rs` (new): `authenticate` layer, `Authed`
  extractor, `origin_check`, `AuthFailure` responses, `PeerAddr`.
- `src/gateway/http.rs` (new): `health`, `gateway_info`, `fleet_report`,
  `ApiError`.
- `src/gateway/assets.rs` (new): `EmbeddedAsset`, `ASSETS`, `lookup(path)`,
  SPA fallback.
- `src/gateway/run.rs` (new): `RunArgs`, `parse_run_args`, `run(args) ->
  io::Result<i32>`; the `gateway.json` marker.
- `src/gateway/mod.rs`: modules + dispatch of the bare command to `run`.
- `web/dist/index.html` (new, committed, < 4 KiB): Herdr Fleet placeholder
  (title, the E4 note, a `<script>` that fetches `/api/gateway` and prints
  `paired as <scope>` or `not paired — run: herdr gateway pair`).
- `build.rs` *(upstream file — minimal wiring)*: `println!("cargo:rerun-if-
  changed=web/dist");`.
- `tests/support/gateway.rs` (new) + one `pub mod gateway;` line in
  `tests/support/mod.rs` *(upstream file — minimal wiring)*:
  `Gateway::spawn(lab: &Lab, extra_config: &str) -> Gateway` (writes
  `[fleet]` for the lab sessions + `[gateway]`, runs `herdr gateway --bind
  127.0.0.1:0` with the lab env, parses `listening on …` from stdout,
  reads the token files), `http_get(path, headers) -> (u16, HeaderMap,
  String)` over `std::net::TcpStream`, `read_token()`, `control_token()`,
  `Drop` → SIGTERM + wait.
- `tests/fork_gateway.rs`: HTTP tests (below).

**Shapes/approach**

```rust
// run.rs
struct RunArgs { bind: Option<SocketAddr>, config: Option<PathBuf> }
pub(crate) fn run(args: &[String]) -> io::Result<i32>
// 1. --config → std::env::set_var(CONFIG_PATH_ENV_VAR, path) before anything else (decision (m))
// 2. Config::load(); print diagnostics; [gateway] invalid → exit 1
// 3. bind = args.bind.or(config.gateway.bind_addr()); BindPolicy::check → exit 1 with the message
// 4. TokenStore::load_or_create(gateway_dir()) → exit 1 on a refused mode
// 5. runtime = Builder::new_multi_thread().enable_all().build(); block_on(async {
//      FleetRuntime::start(&config) (Err → print diagnostics, exit 1)
//      TcpListener::bind(bind) → print "listening on http://<addr>" + write gateway.json (0600)
//      axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>())
//          .with_graceful_shutdown(shutdown_signal())   // ctrl_c + (unix) SIGTERM
//      then: fleet.shutdown().await; remove gateway.json })
// exit 0

// server.rs
#[derive(Clone)]
pub struct AppState {
    pub fleet: FleetHandle,
    pub auth: Arc<AuthState>,      // TokenStore (reloaded when the file stamp changes), DeviceStore, Mutex<AuthLimiter>, OriginAllowlist
    pub info: Arc<GatewayInfo>,    // client_version, loopback: bool, public_url
}
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(http::routes())                      // /health, /api/gateway, /api/fleet
        .fallback(assets::serve)                    // embedded web/dist, GET only
        .layer(middleware::from_fn_with_state(state.clone(), middleware::authenticate))
        .with_state(state)
}

// middleware.rs
pub struct Authed(pub Principal);                   // request extension set by `authenticate`
pub async fn authenticate(State, ConnectInfo<SocketAddr>, Request, Next) -> Response
// order: Origin header present → allowlist or 403 origin_not_allowed;
//        public path (/health, /pair, non-/api GET) → pass without a principal;
//        limiter.check(peer) → 429 + Retry-After;
//        Bearer → tokens.verify_bearer; else cookie herdr_gateway_device → devices.verify_cookie;
//        none → record_failure + 401 {"error":"unauthorized"}; ok → insert Authed, touch last_seen (debounced 60 s)
pub fn require(principal: &Principal, needed: TokenScope) -> Result<(), ApiError>   // 403 {"error":"forbidden","needed":"control"}
```

Responses: `GET /health` → `200 {"ok":true}`; `GET /api/gateway` → `200
{"schema":"herdr.gateway.info.v1","client_version":"0.8.2-fork","scope":
"read"|"control","loopback":true,"features":["fleet"]}` (PR 5 appends
`"events"`, PR 6 `"terminal"`, PR 8 `"pairing"`); `GET /api/fleet` → `200`
`FleetStatusReport` JSON (exactly `herdr fleet status --json`,
`Content-Type: application/json`, `Cache-Control: no-store`); every error
body is `{"error":"<snake_case_code>", "message"?: "…"}`. Assets:
`Content-Type` from a fixed extension table (`html`, `js`, `css`, `json`,
`webmanifest`, `svg`, `png`, `ico`, `woff2`), `Cache-Control: no-cache` for
`index.html`, immutable for hashed names later (E4). `GET` paths with no
extension fall back to `index.html` (SPA routing); everything else 404.
Request body limit 64 KiB; no `Server` header. Logs at `info` for bind/
stop, `warn` for auth failures (peer + reason, never the credential),
`debug` per request.

**Tests**

- In-crate, `axum::Router` driven with `tower::ServiceExt::oneshot` (via
  `axum::body`; no extra dep — `tower` is reachable as a transitive dep of
  axum only if re-exported; if not, use `axum_test`-free approach: bind on
  `127.0.0.1:0` inside the test and use `std::net::TcpStream` GETs): each
  handler with a `FleetHandle` built from `FleetState::test_new()`:
  `/health` 200 without auth; `/api/fleet` 401 without, 200 with the read
  token, 200 with control, 403 with a foreign origin, 429 after 5 bad
  tokens from one peer and still 401 (not 429) from another peer; `/`
  serves `index.html` with `text/html`; `/nope.png` 404; `/settings`
  (no extension) → `index.html`; `Authorization: Bearer` with a query-string
  token is ignored (401).
- `parse_run_args`: `--bind`, `--bind=`, `--config`, bad address → usage
  error 2.
- `tests/fork_gateway.rs`: `gateway_serves_health_and_fleet_for_the_lab`
  (lab up 2 → `Gateway::spawn` → `/health` 200; `/api/fleet` 401 bare, 200
  with bearer; JSON `schema == herdr.fleet.status.v1`, `hosts` has `lab-1`
  and `lab-2` `connected`, `hosts[].workspaces[0].label == "lab-N"`,
  `agents == []`), `gateway_refuses_non_loopback_bind_without_origins`
  (`--bind 0.0.0.0:0` → exit 1, stderr names `allowed_origins`, no
  listener), `gateway_token_files_are_private` (`0600`/`0700` modes;
  `chmod 644 read.token` → next start exits 1 naming the file),
  `gateway_stops_cleanly_on_sigterm` (exit 0, `gateway.json` removed, no
  `lab-*` process changed).

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
cat >> "$XDG_CONFIG_HOME/herdr-dev/config.toml" <<'EOF'
[fleet]
include_local = false
[[fleet.hosts]]
name = "lab-1"
kind = "local"
session = "lab-1"
[[fleet.hosts]]
name = "lab-2"
kind = "local"
session = "lab-2"
EOF
$H gateway --bind 127.0.0.1:7788 & GW=$!; sleep 2
ls -l "$XDG_CONFIG_HOME/herdr-dev/gateway/"                                   # drwx------ dir; -rw------- read.token control.token gateway.json
TOKEN=$(cat "$XDG_CONFIG_HOME/herdr-dev/gateway/read.token")
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/health          # 200
curl -s -w '\n%{http_code}\n' http://127.0.0.1:7788/api/fleet                  # {"error":"unauthorized"} 401
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7788/api/fleet \
  | python3 -c 'import json,sys;r=json.load(sys.stdin);print(r["schema"],[(h["id"],h["connection"]["state"],[w["label"] for w in h["workspaces"]]) for h in r["hosts"]],r["agents"])'
# → herdr.fleet.status.v1 [('lab-1','connected',['lab-1']),('lab-2','connected',['lab-2'])] []
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7788/api/gateway    # {"schema":"herdr.gateway.info.v1",...,"scope":"read","loopback":true,...}
curl -s -H "Authorization: Bearer $TOKEN" -H 'Origin: https://evil.example' -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/api/fleet   # 403
curl -s -H "Authorization: Bearer $TOKEN" -H 'Origin: http://127.0.0.1:7788' -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/api/fleet  # 200
for i in 1 2 3 4 5 6; do curl -s -o /dev/null -w '%{http_code} ' -H 'Authorization: Bearer 00' http://127.0.0.1:7788/api/fleet; done; echo   # 401 ×5 then 429
curl -s -o /dev/null -w '%{http_code} %{content_type}\n' http://127.0.0.1:7788/   # 200 text/html
# host failure is local: stop lab-2, the report shows it unavailable and lab-1 still connected
$H --session lab-2 session stop lab-2; sleep 2
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7788/api/fleet | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(h["id"],h["connection"]["state"]) for h in r["hosts"]])'
# → [('lab-1','connected'),('lab-2','unavailable')]
kill -TERM $GW; wait $GW; echo "exit=$?"; ls "$XDG_CONFIG_HOME/herdr-dev/gateway/gateway.json" 2>&1   # exit=0; No such file
$H gateway --bind 0.0.0.0:7788; echo "exit=$?"                                 # refusing to bind 0.0.0.0:7788: [gateway] allowed_origins is empty …; exit=1
bash scripts/fork/fleet-lab.sh down
```

Evidence: the status codes in order, the two `python3` lines, the `ls -l`
modes, the exit lines. Paste no token.

**Downstream**

- `AppState`, `Authed`, `require(principal, scope)`, `ApiError` and
  `router()`'s `merge(<area>::routes())` pattern are what PRs 5–8 extend;
  every new route is added through its own `routes()` function and every
  `/api/*` handler takes `Authed`.
- `/api/gateway.features` is the capability advertisement E4/E5/E7 read;
  append names, never remove.
- `tests/support/gateway.rs::Gateway` is the fixture for every later
  integration test and for E4's Playwright smoke; add helpers, never
  rename.
- `web/dist/index.html` is E4's to replace; `assets::ASSETS` is the table
  E4 fills from its build (E4 may generate the table).
- The runtime marker `gateway.json` is what `herdr gateway status` (PR 8),
  the systemd unit's docs and E5 read.

### PR 5 — feat(gateway): websocket fleet event stream · deps: 4

**Goal:** `GET /api/events` upgrades to a WebSocket that sends `hello`, the
full fleet report, then every `FleetChange` as it happens — the live feed
E4's Fleet screen and E6's push trigger consume — plus the python WebSocket
stand-in used by validation and tests.

**Files**

- `src/gateway/events.rs` (new): `routes()`, `events_ws`, `run_events(ws,
  stream, report)`.
- `src/gateway/protocol.rs` (new, pure): `EventsHello`, `EventsFleet`,
  `EVENTS_SCHEMA = "herdr.fleet.events.v1"`, `Resync`.
- `src/gateway/server.rs`: `.merge(events::routes())`; `features` gains
  `"events"`.
- `scripts/fork/ws-client.py` (new).
- `tests/support/gateway.rs`: `ws(path, args) -> Output` running the python
  client with the bearer header.
- `tests/fork_gateway.rs`: the events test.

**Shapes/approach**

```rust
pub fn routes() -> Router<AppState> { Router::new().route("/api/events", get(events_ws)) }
async fn events_ws(Authed(p): Authed, State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    // read scope suffices; ws.max_message_size(4096).max_frame_size(4096)
    ws.on_upgrade(move |socket| run_events(socket, state, p.scope))
}
async fn run_events(mut socket: WebSocket, state: AppState, scope: TokenScope) {
    let (mut stream, report) = state.fleet.subscribe_with_report();
    send_text(json!({"kind":"hello","schema":EVENTS_SCHEMA,"client_version":…,"scope":scope}));
    send_text(json!({"kind":"fleet","report":report}));
    loop { select! {
        item = stream.next() => match item {
            Some(ChangeItem::Change(json)) => send_text(json),                 // the FleetChange verbatim
            Some(ChangeItem::Lagged) => { send_text({"kind":"resync"}); resend fleet }
            None => break,                                                     // runtime stopped → close 1001
        },
        msg = socket.recv() => match msg { Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break, _ => {} },
        _ = ping_tick.tick() => socket.send(Message::Ping(vec![])),            // every 30 s; 2 missed pongs → close
    } }
}
```

Text frames only, one JSON object per message, no newlines. Inbound
messages from the client are ignored (a text/binary message above 4 KiB
closes 1009). `ws-client.py`:

```text
usage: ws-client.py URL [--header 'Name: value']... [--send JSON]... [--send-stdin]
                        [--max-messages N] [--timeout SECS] [--binary hex|len]
prints one line per message: `text <payload>` | `binary <hex or byte count>` | `close <code>`
exit 0 on close/--max-messages, 2 on handshake failure (prints the HTTP status), 3 on timeout
```

**Tests**

- In-crate (`#[tokio::test]` against a bound `127.0.0.1:0` router, client =
  `tokio_tungstenite` is *not* available directly — use the axum-side
  `WebSocket` only through a hand-written minimal client over
  `tokio::net::TcpStream` in the test module, or drive `run_events` with a
  fake `WebSocket`-like sink behind a small trait `EventSink` so the
  ordering logic is tested without sockets): hello → fleet → change order;
  `Lagged` produces `resync` then `fleet`; the runtime stopping closes the
  socket.
- `tests/fork_gateway.rs`: `events_stream_sends_report_then_host_delta`
  (connect with the read token via `ws-client.py --max-messages 3`; assert
  line 1 `kind == hello`, line 2 `kind == fleet` with both hosts
  `connected`; then stop `lab-2` while a second client runs with
  `--timeout 20 --max-messages 3` and assert a `host_connection` line for
  `lab-2` with `state == unavailable`), `events_rejects_missing_token`
  (`ws-client.py` exit 2 with status 401) and a foreign `Origin` (403).

**Real-server validation**

```bash
# lab + [fleet] + gateway as in PR 4; TOKEN = read token
python3 scripts/fork/ws-client.py ws://127.0.0.1:7788/api/events -H "Authorization: Bearer $TOKEN" --max-messages 2
# → text {"kind":"hello","schema":"herdr.fleet.events.v1",...}
#   text {"kind":"fleet","report":{"schema":"herdr.fleet.status.v1",...}}
python3 scripts/fork/ws-client.py ws://127.0.0.1:7788/api/events -H "Authorization: Bearer $TOKEN" --timeout 30 --max-messages 4 &
sleep 1; $H --session lab-2 session stop lab-2; wait
# → hello, fleet, then text {"kind":"host_connection","host":"lab-2","connection":{"state":"unavailable","reason":"…","retry_in_ms":1000}}
#   (and agent_removed lines if any agent was on lab-2 — none in the lab)
python3 scripts/fork/ws-client.py ws://127.0.0.1:7788/api/events; echo "exit=$?"                 # handshake 401, exit=2
python3 scripts/fork/ws-client.py ws://127.0.0.1:7788/api/events -H "Authorization: Bearer $TOKEN" -H 'Origin: https://evil.example'; echo "exit=$?"  # 403, exit=2
# status change through the stream: make lab-1's pane busy, expect an agent_* line only if a detectable agent runs there;
# with the lab's marker pane expect no agent lines — record that.
```

Evidence: the message lines (with `reason` text), the two exit codes.

**Downstream**

- `herdr.fleet.events.v1` message order is frozen: `hello`, `fleet`, then
  deltas; `resync` + `fleet` on lag. E4's reducer bootstraps from the
  `fleet` message (and may still `GET /api/fleet`), applies `FleetChange`
  kinds, skips unknown kinds, and reconnects with backoff. E6 subscribes
  in-process to `FleetHandle`, not to this socket.
- `scripts/fork/ws-client.py` is the WebSocket stand-in for every later PR
  and epic; add flags, never change the output line format.

### PR 6 — feat(gateway): websocket terminal observe stream over per-host transports · deps: 4

**Goal:** `GET /api/terminal/{host}/{pane}` streams a pane's rendered ANSI
frames to the browser, read-only: the browser sends `terminal.open` with
its geometry, the gateway opens a fresh observe connection to that host
through a gateway-scoped transport, and forwards `TerminalFrame`s as binary
messages with backpressure; `terminal.resize`/`terminal.scroll`/
`terminal.release` work in observe mode; `terminal.input` is refused.

**Files**

- `src/gateway/terminal.rs` (new): `routes()`, `terminal_ws`,
  `TerminalSession`, `open_observe`, the reader/writer threads.
- `src/gateway/transports.rs` (new): `HostTransports` (lazy, one
  `Mutex<Box<dyn HostTransport>>` per host, scope `gateway`).
- `src/gateway/protocol.rs`: `TerminalOpen`, `TerminalReady`,
  `TerminalClosed`, `TerminalError { code, message }`, `FRAME_HEADER_LEN =
  14`, `encode_frame(&TerminalFrame) -> Vec<u8>`, `decode_frame_header`.
- `src/fleet/transport/ssh.rs` (fork file, E1's; not owned by E2):
  `SshTransport::new_scoped(host, target, session, manage_ssh_config,
  scope: &str)` — `new` becomes `new_scoped(…, "")`-with-host semantics,
  i.e. the existing socket names are byte-identical.
- `src/fleet/transport/mod.rs`: `transport_for_scoped(spec, options, scope)`;
  `transport_for` delegates with the E1 scope.
- `src/client/terminal_sessions.rs` *(upstream file — minimal wiring)*:
  `pub(super)` → `pub(crate)` on `terminal_control_command_from_json`
  (one word; the JSON command vocabulary must stay byte-identical to the
  CLI's).
- `src/gateway/server.rs`: `.merge(terminal::routes())`; `features` gains
  `"terminal"`.
- `tests/fork_gateway.rs`: the observe test.

**Shapes/approach**

```rust
// transports.rs
pub struct HostTransports { specs: HashMap<HostId, HostSpec>, options: FleetConnectorOptions,
                            open: Mutex<HashMap<HostId, Arc<Mutex<Box<dyn HostTransport>>>>> }
impl HostTransports {
    pub fn connect(&self, host: &HostId) -> io::Result<LocalStream>   // blocking; run in spawn_blocking
    pub fn shutdown(&self)                                            // drop transports (after every session stream is closed)
}
// scope passed to transport_for_scoped is "gateway" → forward socket
//   /tmp/herdr-remote-<pid>-gateway-<host>-<target>-<session>.sock (distinct from the connector's)

// terminal.rs
pub fn routes() -> Router<AppState> { Router::new().route("/api/terminal/{host}/{pane}", get(terminal_ws)) }
async fn terminal_ws(Authed(p), State(state), Path((host, pane)): Path<(String, String)>, ws: WebSocketUpgrade) -> Response
// validate: HostId::new(&host) and refs::is_valid_resource_id(&pane) → else 404 {"error":"not_found"};
// ws.max_message_size(64 * 1024); on_upgrade(run_terminal(socket, state, p.scope, host, pane))

async fn run_terminal(socket, state, scope, host, pane) {
    // 1. first client message within 10 s must be terminal.open; else terminal.error bad_request, close 1002
    //    { "type":"terminal.open", "mode":"observe"|"control", "cols":u16, "rows":u16, "takeover":bool? }
    // 2. mode control needs TokenScope::Control → else terminal.error forbidden, close 1008  (PR 7 implements control)
    // 3. state.fleet.host_connection(&host): Unavailable/Incompatible/unknown host → terminal.error host_unavailable, close 1011
    // 4. spawn_blocking: stream = transports.connect(&host)?; write TerminalHello{version: PROTOCOL_VERSION, cols, rows,
    //    cell_width_px: 0, cell_height_px: 0, pixel_mouse: false}; read Welcome (MAX_FRAME_SIZE, read_timeout) and require
    //    encoding == TerminalAnsi && error.is_none(); write ObserveTerminal{ target: pane }
    // 5. TerminalSession::start(stream): reader thread → mpsc::Sender<ServerMessage>(2) via blocking_send;
    //    writer thread ← mpsc::Receiver<ClientMessage>(16); both stop on Detach/EOF/error
    // 6. send terminal.ready {mode, host, pane, ref, cols, rows}; loop select! {
    //      frame from reader: Terminal(f) → socket.send(Binary(encode_frame(&f))); ServerShutdown{reason} → terminal.closed + close 1000;
    //      msg from socket: Text → terminal_control_command_from_json → match { Input => (observe) terminal.error forbidden (no close),
    //                       Resize|AttachScroll|Detach => writer.send(msg) }; Binary/oversized → terminal.error bad_request; Close → break }
    // 7. on any exit: writer.send(Detach) best-effort, drop both halves, join threads (bounded), log at debug
}
```

`encode_frame`: `[0x01][seq u64 LE][width u16 LE][height u16 LE][full u8]
+ bytes`. Reader reads with `MAX_FRAME_SIZE` (the gateway never enables
graphics; a `Graphics` message is dropped). The bounded capacity-2 channel
is the backpressure: a browser that cannot keep up blocks the reader
thread, the server's render lane (capacity one) then defers full renders and
coalesces, so memory per session is bounded by two frames. Session
cardinality is logged at `info` on open/close with `host`/`pane`/`mode`,
never bytes. `terminal.scroll` in observe mode: PR 6 verifies against the
live server whether `AttachScroll` from a `TerminalObserve` client moves the
observer's virtual viewport; if the server ignores it, the gateway answers
`terminal.error {code:"unsupported"}` and the doc says so.

**Tests**

- Pure: `encode_frame`/`decode_frame_header` round-trip incl. a 2 MiB
  frame; `TerminalOpen` parsing (missing cols → error; cols 0 → error;
  unknown mode → error); the observe-mode gate refuses `terminal.input`
  and admits resize/scroll/release (table test on the pure `SessionPolicy
  { mode, scope }::admit(&ClientMessage) -> Result<(), TerminalErrorCode>`).
- `transport_for_scoped` with an ssh spec derives a forward socket path
  that differs from `transport_for`'s and both differ from `--remote`'s
  unscoped name (unit test in `transport/ssh.rs` next to E1's, using the
  existing fake-ssh shim for a full connect if cheap).
- Socket-level (`#[cfg(all(test, unix))]`): a fake terminal server
  (`bind_local_listener` under a throwaway `XDG_CONFIG_HOME`) that answers
  `TerminalHello` with `Welcome{TerminalAnsi}`, expects `ObserveTerminal
  {target}` and emits two `Terminal` frames then `ServerShutdown`:
  `TerminalSession` yields the frames in order and ends; `Detach` from the
  writer arrives at the fake server; a fake that answers `Welcome{error}`
  yields an `io::Error` naming it.
- `tests/fork_gateway.rs`: `terminal_observe_streams_marker_pane_frames`
  (lab up 2; `ws-client.py … /api/terminal/lab-2/<pane_2 id> --send
  '{"type":"terminal.open","mode":"observe","cols":80,"rows":24}'
  --max-messages 3 --binary hex`; assert line 1 `terminal.ready` with
  `ref == "lab-2/<pane>"`, line 2 `binary` whose header decodes to `full ==
  1, width == 80, height == 24` and whose body contains
  `herdr-fleet-lab:lab-2`; a `terminal.input` message → a `terminal.error
  forbidden` line and the socket stays open; `lab-2`'s pane text unchanged),
  `terminal_open_on_unknown_pane_reports_not_found` (`w9:p9` →
  `terminal.error pane_not_found` from the server's `ServerShutdown`
  reason, close), `terminal_open_on_unavailable_host_fails_fast`
  (`lab-2` stopped → `host_unavailable` without touching the socket).

**Real-server validation**

```bash
# lab + [fleet] + gateway as in PR 4; TOKEN = read token
PANE2="$HERDR_FLEET_LAB_PANE_2"
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-2/$PANE2" -H "Authorization: Bearer $TOKEN" \
  --send '{"type":"terminal.open","mode":"observe","cols":80,"rows":24}' --max-messages 2 --binary hex | tee /tmp/e3-pr6.txt
# → text {"type":"terminal.ready","mode":"observe","host":"lab-2","pane":"w1:p1","ref":"lab-2/w1:p1","cols":80,"rows":24}
#   binary 01<seq 8 bytes>5000 1800 01 1b5b... (header: full=1, 80x24) — decode and grep the marker:
python3 - <<'EOF'
import sys
for line in open('/tmp/e3-pr6.txt'):
    if line.startswith('binary '):
        b = bytes.fromhex(line.split()[1]); print('kind', b[0], 'seq', int.from_bytes(b[1:9],'little'), 'size', int.from_bytes(b[9:11],'little'), int.from_bytes(b[11:13],'little'), 'full', b[13]); print('marker' if b'herdr-fleet-lab:lab-2' in b[14:] else 'NO MARKER')
EOF
# read is safe: input in observe mode is refused, and the pane did not change
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-2/$PANE2" -H "Authorization: Bearer $TOKEN" \
  --send '{"type":"terminal.open","mode":"observe","cols":80,"rows":24}' --send '{"type":"terminal.input","text":"echo INJECTED\n"}' --max-messages 3
# → ready, one binary frame, text {"type":"terminal.error","code":"forbidden",...}
$H --session lab-2 pane read "$PANE2" --source recent | grep -c INJECTED           # 0
# live update: new output in the pane arrives as an incremental frame
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-1/$HERDR_FLEET_LAB_PANE_1" -H "Authorization: Bearer $TOKEN" \
  --send '{"type":"terminal.open","mode":"observe","cols":80,"rows":24}' --timeout 15 --max-messages 3 --binary len &
sleep 1; $H --session lab-1 pane run "$HERDR_FLEET_LAB_PANE_1" 'echo hello-from-lab-1'; wait   # third line: binary <n> with full=0
# many observers, no ownership: three concurrent clients on the same pane all get ready + a full frame
# ssh host (skip with a note if ssh-lab.sh up exits 3): [fleet] lab-ssh = kind "ssh", target "herdr-ssh-lab", session "lab-1"
bash scripts/fork/ssh-lab.sh up && eval "$(bash scripts/fork/ssh-lab.sh env)"
HOME=$HERDR_SSH_LAB_HOME $H gateway --bind 127.0.0.1:7788 & GW=$!; sleep 5
ls /tmp/herdr-remote-$GW-* ; ls /tmp/herdr-remote-$GW-gateway-* 2>/dev/null   # connector socket(s) and, only after a terminal opens, the gateway-scoped one
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-ssh/$HERDR_FLEET_LAB_PANE_1" -H "Authorization: Bearer $TOKEN" \
  --send '{"type":"terminal.open","mode":"observe","cols":80,"rows":24}' --max-messages 2 --binary len          # ready + full frame over ssh
kill -TERM $GW; wait $GW; echo "exit=$?"; ls /tmp/herdr-remote-$GW-* 2>&1        # exit=0 within 5 s; no sockets left
bash scripts/fork/ssh-lab.sh down; bash scripts/fork/fleet-lab.sh down
```

Evidence: the `terminal.ready` line, the decoded header line with `marker`,
the `forbidden` line and the `grep -c 0`, the `full=0` follow-up frame, the
ssh forward-socket names before/after, exit 0 with no sockets left.

**Downstream**

- `herdr.fleet.terminal.v1`: `terminal.open` first, `terminal.ready`, then
  binary frames with the 14-byte header; text `terminal.error {code}` codes
  are `bad_request`, `forbidden`, `host_unavailable`, `pane_not_found`,
  `unsupported`, `internal`; `terminal.closed {reason}` precedes a server-
  initiated close. E4's xterm.js writes `frame[14..]`; a `full == 1` frame
  may be preceded by a `reset()`.
- `HostTransports` is the only way the gateway opens a per-host stream; E7
  does not need it (E7 goes through the connector's endpoint lane).
- `SessionPolicy::admit` is the scope gate PR 7 extends with `Control`;
  input never bypasses it.
- Shutdown order — sessions, then transports, then the connector — is
  fixed (E1 drop hazard).

### PR 7 — feat(gateway): terminal control mode gated by the control scope · deps: 6

**Goal:** a device or bearer with the `control` scope can open a terminal
in `mode: "control"`: the gateway sends `ControlTerminal { target, takeover
}`, forwards `terminal.input` (text or base64 bytes) and a controller's
`terminal.resize` to the real PTY, honours `takeover`, and releases
ownership on `terminal.release`/close. `read` still cannot send a byte.

**Files**

- `src/gateway/terminal.rs`: the `control` arm of `open`, `SessionPolicy
  { mode: Control, scope: Control }`, takeover handling, the
  `already has an attached client` shutdown mapped to `terminal.error
  {code:"busy"}`.
- `src/gateway/protocol.rs`: `busy` code; `terminal.ready.mode` is
  `"control"`.
- `tests/fork_gateway.rs`: control tests.

**Shapes/approach**

`terminal.open { mode: "control", takeover: false }` requires
`principal.scope.allows(Control)` (else `forbidden`, close 1008, counted as
an auth failure for the limiter) and then `ControlTerminal { target: pane,
takeover }` instead of `ObserveTerminal`. In control mode every CLI command
is admitted: `terminal.input {text}` → `Input { data: text.into_bytes() }`,
`{bytes: base64}` → decoded (≤ 64 KiB; the CLI's decoder), `terminal.resize`
→ `Resize` (real PTY resize; `cell_*_px` 0), `terminal.scroll`,
`terminal.release` → `Detach` + `terminal.closed {reason:"released"}` +
close 1000. A `ServerShutdown` whose reason contains "already has an
attached client" becomes `terminal.error {code:"busy", message}` so E4 can
offer takeover. Nothing in E3 confirms destructive actions (there are none:
input is exactly what the console sends when you type). Logging on open:
`info` with `host`/`pane`/`mode`/`credential kind`; input bytes are never
logged.

**Tests**

- Pure `SessionPolicy` table: `(Observe, Read)` refuses input; `(Control,
  Read)` is unconstructible (`open` rejects); `(Control, Control)` admits
  everything; `(Observe, Control)` still refuses input (mode wins — a
  control-capable device watching read-only cannot type by accident).
- Socket-level fake terminal server: control open sends
  `ControlTerminal{takeover}` with the requested flag; `terminal.input
  {text}` arrives as `Input{data}` byte-exact; `{bytes}` base64 round-trip;
  `release` sends `Detach`; a fake `ServerShutdown{"… already has an attached
  client …"}` yields `busy`.
- `tests/fork_gateway.rs`: `terminal_control_types_into_the_right_pane`
  (open `lab-1/<pane_1>` in control with the **control** token, send
  `terminal.input {"text":"echo E3-CONTROL-lab-1\n"}`, release; `lab-1`'s
  pane read contains `E3-CONTROL-lab-1`, `lab-2`'s does not),
  `terminal_control_is_refused_for_read_scope` (read token → `forbidden`,
  close 1008; pane unchanged), `terminal_control_second_owner_needs_takeover`
  (two control clients on one pane: second without takeover → `busy`; with
  `takeover: true` → the first gets `terminal.closed`).

**Real-server validation**

```bash
# lab + [fleet] + gateway as in PR 4
CTRL=$(cat "$XDG_CONFIG_HOME/herdr-dev/gateway/control.token"); READ=$(cat "$XDG_CONFIG_HOME/herdr-dev/gateway/read.token")
P1="$HERDR_FLEET_LAB_PANE_1"; P2="$HERDR_FLEET_LAB_PANE_2"
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-1/$P1" -H "Authorization: Bearer $CTRL" \
  --send '{"type":"terminal.open","mode":"control","cols":80,"rows":24}' --send '{"type":"terminal.input","text":"echo E3-CONTROL-lab-1\n"}' \
  --send '{"type":"terminal.release"}' --max-messages 4
# → terminal.ready mode=control, a full frame, (an incremental frame), text {"type":"terminal.closed","reason":"released"}, close 1000
$H --session lab-1 pane read "$P1" --source recent | grep -c 'E3-CONTROL-lab-1'    # ≥1 (echo + output)
$H --session lab-2 pane read "$P2" --source recent | grep -c 'E3-CONTROL'          # 0  — input reached exactly lab-1
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-1/$P1" -H "Authorization: Bearer $READ" \
  --send '{"type":"terminal.open","mode":"control","cols":80,"rows":24}' --max-messages 1
# → text {"type":"terminal.error","code":"forbidden",...} then close 1008
# takeover: hold one controller in the background, open a second without takeover → busy; with takeover → first sees terminal.closed
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-1/$P1" -H "Authorization: Bearer $CTRL" \
  --send '{"type":"terminal.open","mode":"control","cols":80,"rows":24}' --timeout 30 --max-messages 4 & FIRST=$!
sleep 1; python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-1/$P1" -H "Authorization: Bearer $CTRL" \
  --send '{"type":"terminal.open","mode":"control","cols":80,"rows":24}' --max-messages 1                       # terminal.error busy
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-1/$P1" -H "Authorization: Bearer $CTRL" \
  --send '{"type":"terminal.open","mode":"control","cols":80,"rows":24,"takeover":true}' --max-messages 2       # ready + frame
wait $FIRST                                                                                                  # its last line: terminal.closed
# a controller resize resizes the real PTY, an observer resize does not:
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-1/$P1" -H "Authorization: Bearer $CTRL" \
  --send '{"type":"terminal.open","mode":"control","cols":60,"rows":20}' --send '{"type":"terminal.input","text":"stty size\n"}' --send '{"type":"terminal.release"}' --max-messages 4
$H --session lab-1 pane read "$P1" --source recent | grep -c '^20 60$'             # 1
```

Evidence: the pane greps (lab-1 ≥ 1, lab-2 0), the `forbidden`/`busy`/
`closed` lines, the `stty size` line.

**Downstream**

- Control on the terminal socket is the *only* input path in E3; E7 adds
  `POST /api/hosts/{host}/agents/{pane}/{prompt|keys|start|rename|close}`
  through the connector's endpoint lane, gated by the same
  `require(principal, Control)` plus `HostConnection::Connected.methods`.
- `busy`/`takeover` semantics are the server's; E4 shows "take over" on
  `busy`.

### PR 8 — feat(gateway): pairing urls with qr codes, device cookies, status and token rotation · deps: 4

**Goal:** the operator's loop: `herdr gateway pair [--control] [--ttl-secs
N]` prints a one-time pairing URL and a terminal QR code; opening it on the
phone (`GET /pair?code=…`) exchanges the code for a per-device cookie with
that scope and lands on the app; `herdr gateway status [--json]` reports
the running gateway and its devices; `herdr gateway rotate-token
<read|control>` rotates a token and revokes that scope's devices.

**Files**

- `src/gateway/pairing.rs` (new): `routes()` (`GET /pair`), `pair_handler`,
  cookie building.
- `src/gateway/ops.rs` (new): `pair_command`, `status_command`,
  `rotate_token_command`, `qr_text(url) -> Result<String, String>`.
- `src/gateway/mod.rs`: the `pair`/`status`/`rotate-token` arms; usage.
- `src/gateway/server.rs`: `.merge(pairing::routes())`; `features` gains
  `"pairing"`; `/api/gateway` gains `"device": {"id", "label"}` when the
  principal is a device.
- `src/cli/spec.rs` *(upstream file — minimal wiring, gated)*: the three
  subcommands under `gateway_command()`.
- `tests/fork_gateway.rs`: pairing tests.

**Shapes/approach**

```text
herdr gateway pair [--control] [--ttl-secs N] [--label TEXT] [--no-qr] [--json]
  → creates PairingStore code (scope read|control, ttl), prints:
    Pair this device with Herdr Fleet (read scope, valid 10 min):
      http://127.0.0.1:7788/pair?code=<id>.<secret>
    <QR: qrcode::QrCode::new(url) rendered with qrcode::render::unicode::Dense1x2, quiet zone 1>
    --json → {"url","scope","expires_unix"}
  base URL = [gateway] public_url if set, else http://<bind> (a 0.0.0.0 bind → error: set public_url)
herdr gateway status [--json]
  → reads gateway.json (pid alive? listen), GET /health on it, counts devices per scope, pending pairings;
    text table or {"running","pid","listen","devices":{"read":n,"control":n},"pairings_pending":n}; exit 0 running, 3 not running
herdr gateway rotate-token <read|control>
  → TokenStore::rotate + DeviceStore::revoke_scope; prints "rotated <scope> token; revoked <n> devices"; the running gateway
    reloads token/device files on the next request (file stamp check), no restart needed
```

`GET /pair?code=<id>.<secret>`: unauthenticated but rate-limited (a bad code
is an auth failure); `PairingStore::consume(code, now)` → on success
`DeviceStore::insert(DeviceRecord { id: 8 hex, scope, secret_sha256: digest
of a fresh 32-byte secret, label, created })`, respond `303 Location: /`
with `Set-Cookie: herdr_gateway_device=<id>.<secret>; Path=/; HttpOnly;
SameSite=Strict; Max-Age=31536000[; Secure]` (`Secure` when `public_url`
is https or the request arrived with `X-Forwarded-Proto: https` **and** the
peer is loopback — `tailscale serve` proxies from loopback); failure →
`403 {"error":"pairing_invalid"|"pairing_expired"}` (no distinction between
unknown and wrong secret). The pairing code never enters logs (log the id
only). Pairing sweeps expired files on every `pair`/`status`/consume.

**Tests**

- Pure: `qr_text` renders a known short URL to a stable string (golden,
  ~20 lines); `build_cookie` attributes; `--ttl-secs` bounds (30..86400).
- In-crate handler tests: valid code → 303 + cookie; the cookie then
  authorizes `/api/fleet` (200) and `/api/gateway` reports `via: device`;
  same code twice → 403; expired → 403; 5 bad codes → 429; the cookie of a
  device revoked by `rotate` → 401.
- `tests/fork_gateway.rs`: `pairing_url_exchanges_into_a_device_cookie`
  (`herdr gateway pair --json` under the lab env → `url`; `http_get` it →
  303 + `Set-Cookie`; `/api/fleet` with `Cookie:` → 200; second use → 403),
  `pair_control_then_rotate_revokes_the_device` (`pair --control`, cookie
  works for a control terminal open (reuse PR 7's helper), `rotate-token
  control` → the cookie gets 401 and the new control token works),
  `gateway_status_reports_running_and_devices` (exit 0 and `devices.read
  == 1` after one pairing; exit 3 after SIGTERM).

**Real-server validation**

```bash
# lab + [fleet] + gateway as in PR 4
$H gateway pair --json | tee /tmp/e3-pair.json | python3 -c 'import json,sys;r=json.load(sys.stdin);print(sorted(r),r["scope"])'   # ['expires_unix','scope','url'] read
$H gateway pair | head -3                                                     # the sentence + URL line + first QR row (QR renders in the terminal; paste only its size)
URL=$(python3 -c 'import json;print(json.load(open("/tmp/e3-pair.json"))["url"])')
curl -s -c /tmp/e3-jar -o /dev/null -w '%{http_code} %{redirect_url}\n' "$URL"      # 303 http://127.0.0.1:7788/
grep -c herdr_gateway_device /tmp/e3-jar                                       # 1 (HttpOnly flag present in the jar)
curl -s -b /tmp/e3-jar -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/api/fleet     # 200
curl -s -b /tmp/e3-jar http://127.0.0.1:7788/api/gateway | python3 -c 'import json,sys;r=json.load(sys.stdin);print(r["scope"],r["device"]["id"][:2]+"…")'   # read xx…
curl -s -o /dev/null -w '%{http_code}\n' "$URL"                                # 403 (one-time)
$H gateway status; echo "exit=$?"                                              # running, pid, listen, devices read=1 control=0; exit=0
$H gateway rotate-token read                                                   # rotated read token; revoked 1 devices
curl -s -b /tmp/e3-jar -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/api/fleet     # 401 (no restart happened)
curl -s -H "Authorization: Bearer $(cat $XDG_CONFIG_HOME/herdr-dev/gateway/read.token)" -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/api/fleet  # 200 with the new token
ls -l "$XDG_CONFIG_HOME/herdr-dev/gateway/" "$XDG_CONFIG_HOME/herdr-dev/gateway/pairings"   # all -rw-------, pairings empty
rm -f /tmp/e3-jar /tmp/e3-pair.json
```

Evidence: the status codes in order, the `sorted(r)` line, the status and
rotate lines, the `ls -l` modes. Never paste the URL or the cookie.

**Downstream**

- The pairing flow (`pair` → `GET /pair?code=` → cookie) is what E4's
  Settings screen and E5's docs describe; E4 may add a `POST /api/pair`
  JSON variant later without changing this one. E5's `--tailscale` sets
  `public_url` so the QR carries the `https://….ts.net` origin.
- `DeviceRecord.id` is the per-device identity E6 keys push subscriptions
  on; `rotate-token` revoking devices also drops their subscriptions (E6
  hooks `revoke_scope`).
- `herdr gateway status --json` fields are additive (`running`, `pid`,
  `listen`, `devices`, `pairings_pending`).

### PR 9 — docs: gateway guide, systemd unit, adr e3 review, roadmap drift · deps: 5, 7, 8

**Goal:** the user-facing reference for everything E3 shipped, the example
`systemd --user` unit, the ADR's E3 review, and factual drift fixed in the
roadmap's E3 section — plus the last janitoring (dead-code allows left for
"the first later PR").

**Files**

- `docs/fork/gateway.md` (new): what the gateway is, `[gateway]` keys
  (every key, type, default, diagnostics), running it (`herdr gateway`,
  `--bind`, `--config`, loopback vs non-loopback, the foreground-client
  geometry caveat and the "headless servers only" recommendation), tokens
  and files under `<config>/gateway/` (modes, rotation, revocation),
  pairing (`pair`, QR, cookie, TTL, one-time), `status`, the HTTP contract
  (`/health`, `/api/gateway`, `/api/fleet`, `/pair`, static assets), the
  WebSocket contracts (`herdr.fleet.events.v1` message order,
  `herdr.fleet.terminal.v1` including the binary header table and every
  `terminal.*` message with its fields and error codes), scopes and what
  `read` can never do, rate limiting, origin policy, shutdown behaviour,
  `scripts/fork/ws-client.py` usage, the systemd unit, troubleshooting
  (refused bind, 401/403/429, `busy`, ssh forward sockets). Real output
  from the lab pasted for `/api/fleet`, an events transcript and a decoded
  terminal frame header.
- `scripts/fork/systemd/herdr-gateway.service` (new, `%h`-relative,
  `ExecStart=%h/.local/bin/herdr gateway`, `Restart=on-failure`,
  `Environment=` none — config comes from `config.toml`) and the
  `systemctl --user enable --now herdr-gateway` recipe in the doc; E8
  installs it.
- `docs/fork/README.md`: a *Gateway* section (three commands + link), the
  fork-wiring table rows for every upstream file E3 touched
  (`Cargo.toml`, `build.rs`, `justfile`, `src/config/{model,io}.rs`,
  `src/config.rs`, `src/main.rs`, `src/cli.rs`, `src/cli/spec.rs`,
  `src/client/terminal_sessions.rs`, `scripts/config_reference_check.py`,
  `tests/support/mod.rs`), and `check-no-default-features` in the CI table.
- `docs/fork/decisions/0001-servers-stay-stock-ssh-transport.md`: an *E3
  review* subsection (what held, what was learned — expected: tokens on
  loopback, the gateway as a permanent foreground client, the gateway-
  scoped forward socket, backpressure through the server's render lane).
- `docs/fork/ROADMAP.md`: factual drift in the **E3 section only** (e.g.
  binary frames, `terminal.open`, `/api/gateway`, tokens on loopback); the
  status row stays `implement-epic`'s.
- `src/gateway/**`: remove any `#[allow(dead_code)]` whose named PR has
  landed; `src/fleet/hosts.rs` `HostId::as_str` allow (E1 PR 7 deferred it
  to "the first later PR touching the file") only if E2 has not already
  removed it.

**Tests** — docs-only PR: the gate (both feature sets) stays green;
`shellcheck` unaffected; every command in `gateway.md` is re-run against
the lab once and its output pasted verbatim (secrets redacted as
`<token>`).

**Real-server validation**

```bash
# lab + [fleet] + gateway as in PR 4; run every snippet in docs/fork/gateway.md in order and diff the pasted output against reality
systemd-analyze --user verify scripts/fork/systemd/herdr-gateway.service; echo "exit=$?"    # exit=0 (unit parses; do not enable it on this machine)
```

**Downstream**

- E4 links to `gateway.md` for the API and adds `web.md`; E5 adds
  `remote-access.md` and the `--tailscale` paragraph here; E8 installs the
  unit. Every later `[gateway]` key or endpoint is documented in
  `gateway.md` in the same PR that adds it.

## Critical files referenced (reuse, don't reinvent)

- `src/fleet/connector.rs` — `FleetConnector::{start, events, shutdown}`,
  `FleetConnectorOptions::for_config`, `FleetEvent::Host`, `INACTIVE_SURFACE`
  (never call `set_active`). `src/fleet/oneshot.rs` — the config → specs →
  connector → state recipe (`FleetSession::start`) and the socket-level
  fake-endpoint test idiom.
- `src/fleet/state.rs` — `FleetState::{new, apply, set_active_host,
  merged_agents}`, `FleetChange` (tagged `kind`), `HostConnection`,
  `test_new`/`assert_invariants_for_test`. `src/fleet/report.rs` —
  `FleetStatusReport::from_state`, `FLEET_STATUS_SCHEMA`. `src/fleet/hosts.rs`
  — `HostId`, `HostSpec`, `resolve_hosts`. `src/fleet/refs.rs` —
  `FleetPaneRef`, `is_valid_resource_id`.
- `src/fleet/transport/{mod,local,ssh}.rs` — `HostTransport`,
  `transport_for`, `LocalTransport`, `SshTransport::new` (+ PR 6's
  `new_scoped`); `src/remote/attach.rs::local_forward_socket_path_scoped`
  semantics (only the empty scope is unscoped).
- `src/protocol/wire.rs` (read-only) — `PROTOCOL_VERSION`, `MAX_FRAME_SIZE`,
  `ClientMessage::{TerminalHello, ObserveTerminal, ControlTerminal, Input,
  Resize, AttachScroll, Detach}`, `ServerMessage::{Welcome, Terminal,
  ServerShutdown}`, `TerminalFrame`, `RenderEncoding::TerminalAnsi`,
  `write_message`/`read_message`. `src/client/terminal_sessions.rs` —
  `terminal_control_command_from_json` (the JSON command vocabulary) and
  the frame loop shape; `src/client/handshake.rs::do_handshake` as the
  reference for the two-frame terminal hello (re-implemented, not widened).
- `src/ipc.rs` — `connect_local_stream`, `restrict_socket_permissions`
  (unix/windows twin idiom). `src/pane_graphics_files.rs` — `0700`/`0600`
  create-and-verify. `src/checksum.rs` — `sha2` usage.
- `src/server/headless/bootstrap.rs:40` — the multi-thread runtime shape.
  `src/server/headless.rs:1349,1760,2145` and `src/server/headless/tests/
  mod.rs:3088-3440` (read-only) — observer/controller semantics the gateway
  relies on.
- `src/config/model.rs` (`FleetConfig` pattern), `src/config/io.rs`
  (`KNOWN_TOP_LEVEL_CONFIG_KEYS`, live-reload sections, `config_dir`,
  `CONFIG_PATH_ENV_VAR`), `src/main.rs` (`DEFAULT_CONFIG` + block tests,
  bare-command list, usage), `src/cli.rs` / `src/cli/spec.rs` / `src/cli/
  fleet.rs` (dispatch, spec, hand-parsed args, exit codes 0/1/2),
  `src/build_info.rs` (`version()`, `is_fork()`).
- `tests/support/mod.rs`, `tests/support/fleet_lab.rs` (`Lab`),
  `tests/cli/fleet.rs` (`[fleet]` fixture), `tests/fork_fleet_lab.rs`
  (shelling out to a fork script from a test), `scripts/fork/fleet-lab.sh`,
  `scripts/fork/ssh-lab.sh`, `scripts/fork/gate.sh`, `justfile`,
  `.github/workflows/fork-ci.yml`, `scripts/config_reference_check.py`.
- Binding rules: `AGENTS.md` Universal Project Rules (state/runtime
  separation, multiplicative paths, runtime/client boundary, stable
  endpoint contract), Testing, Code Conventions; `.claude/rules/fork.md`;
  `docs/fork/ROADMAP.md` principles 1–8 and the E3 section;
  `docs/fork/decisions/0001-…md` (E0 and E1 reviews);
  `docs/fork/plans/e1-fleet-core.md` Downstream sections and
  `docs/fork/plans/e2-fleet-tui.md` PR 1 Downstream (the connector surface
  E2 keeps for E3); `docs/fork/fleet-core.md` "Driving the connector (E3)".

## End-to-end epic validation

After every PR is ✅, `implement-epic` proves the epic against real servers
(debug build, isolated lab; `ssh-lab.sh` exit 3 degrades the ssh step to a
recorded skip):

1. **Both builds.** `bash scripts/fork/gate.sh . ci` and `bash
   scripts/fork/gate.sh . ci-no-default` → `EXIT=0` each; `cargo build
   --no-default-features && target/debug/herdr gateway; echo $?` → `unknown
   command: gateway`, 2; `cargo build && target/debug/herdr --version` →
   `herdr 0.8.2-fork`.
2. **Fleet of three.** `fleet-lab.sh up 2` + `ssh-lab.sh up`; `[fleet]`
   with `lab-1` (local), `lab-2` (local), `lab-ssh` (`kind = "ssh"`,
   `target = "herdr-ssh-lab"`, `session = "lab-1"`), `include_local =
   false`; `HOME=$HERDR_SSH_LAB_HOME $H gateway --bind 127.0.0.1:7788 &`.
   `curl /health` → 200 without a token; `/api/fleet` → 401 bare, 200 with
   the read token, `hosts` = `lab-1`/`lab-2`/`lab-ssh` all `connected`,
   `workspaces[0].label` = `lab-1`/`lab-2`/`lab-1`, `agents == []`;
   `/api/gateway.features` ⊇ `["fleet","events","terminal","pairing"]`.
3. **Security invariants.** foreign `Origin` → 403; six bad bearers from
   one peer → 401 ×5 then 429 while another peer still gets 401;
   `--bind 0.0.0.0:7788` without origins → exit 1; token files `0600` in a
   `0700` dir; `chmod 644 read.token` → next start exits 1; `read` token
   opening `mode: control` → `forbidden` + close 1008; `terminal.input` in
   observe mode → `forbidden`, pane unchanged (`grep -c INJECTED` = 0).
4. **Live events.** `ws-client.py /api/events --timeout 30 --max-messages 4`
   → `hello`, `fleet`, then `host_connection lab-ssh unavailable` when
   `ssh-lab.sh down` runs, and `connected` + `snapshot` after `ssh-lab.sh
   up` (host-local: no line names `lab-1`/`lab-2`).
5. **Terminals on every transport.** Observe `lab-2/<pane>`: `terminal.ready`
   + a binary frame whose decoded header is `full=1 80×24` and whose body
   holds `herdr-fleet-lab:lab-2`; observe `lab-ssh/<pane>` over ssh → the
   same with `lab-1`'s marker and a gateway-scoped forward socket
   `/tmp/herdr-remote-<pid>-gateway-…` present while open; three
   concurrent observers on one pane all receive frames; a `pane run 'echo
   hello'` on the observed pane produces an incremental (`full=0`) frame
   within 2 s.
6. **Control reaches exactly one pane.** With the control token,
   `terminal.open control` on `lab-1/<pane>` + `terminal.input "echo
   E3-EPIC\n"` + `release` → `lab-1` pane read contains `E3-EPIC`, `lab-2`
   and `lab-ssh`'s target pane (the same server as `lab-1` — expect it
   there too, and state so) — then the same on `lab-2` → only `lab-2`
   gains it; a second controller without takeover → `busy`; with takeover
   → the first sees `terminal.closed`; controller `stty size` after a
   60×20 open prints `20 60`, an observer at 60×20 leaves the PTY size
   alone.
7. **Pairing loop.** `herdr gateway pair --json` → URL; `curl -c jar
   "$URL"` → 303 + `Set-Cookie … HttpOnly; SameSite=Strict`; the cookie
   gets `/api/fleet` 200 and `/api/gateway.scope == read`; the URL a second
   time → 403; `pair --control` → a cookie that opens a control terminal;
   `gateway status --json` → `running: true`, `devices.read == 1`,
   `devices.control == 1`; `rotate-token control` → the control cookie
   gets 401, the new control token 200, `devices.control == 0`.
8. **Shutdown hygiene.** `kill -TERM $GW; wait` → exit 0 within 5 s while
   two terminals were open; no `/tmp/herdr-remote-<pid>-*` sockets remain;
   `gateway.json` removed; `ssh-lab.sh down`, `fleet-lab.sh down`; from a
   shell without the lab env `herdr session list` shows no `lab-*`,
   `~/.config/herdr*/gateway` does not exist, and `ls -la ~/.ssh |
   sha256sum` is unchanged from before the run.
9. **Contracts and perf.** `git diff master -- src/protocol tests/fixtures`
   is empty; `grep -rn 'unwrap()' src/gateway` outside `#[cfg(test)]` is
   empty; `just bench-render-scale` medians are unchanged from `master`
   (the gateway adds no work to the server); with 15 observers on one pane
   the gateway RSS stays under 15 × 2 frames + baseline (report the `ps`
   number); `docs/fork/gateway.md` snippets match the live output.

Passing all nine is the acceptance criterion; `implement-epic` then flips
E3 to ✅ in `docs/fork/ROADMAP.md`.
