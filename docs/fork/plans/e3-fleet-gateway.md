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

**This is a re-plan.** The first E3 plan was verified on `master @ 09919e4a`.
Since then three things landed on `master` (now `3d649ce3`, herdr
`0.8.2-fork`): upstream #3670 "manage multiple ssh machines from one client"
(`herdr machine …`, saved SSH profiles in `src/client/endpoint/catalog.rs`,
a rewritten `src/remote/attach.rs`, new `src/remote/{saved,args,process,
restart_policy}.rs`); the retirement of the fork's E2 fleet TUI (fork PR #33
reverted #22 and #24–#31; #23 — connector client options, active-surface
tracking and `take_events()` — was kept in `src/fleet/connector.rs` /
`handshake.rs`); and the upstream sync PR #34 whose merge notes live in
[ADR 0002](../decisions/0002-adopt-upstream-multi-machine-client.md). What
changed in this plan versus the first one, and why:

- **E2 no longer exists.** Every "E2 is landing concurrently" hazard, the
  ban on `take_events()`/`for_client`, and the references to
  `tests/support/fleet_tui.rs`, `just bench-fleet-scale` and `[fleet.keys]`
  are gone. Nothing under `src/client/` is fork-owned any more; E3 still
  never edits it except one visibility word in `terminal_sessions.rs` (PR 6).
- **PR 3 is built on the connector API as it exists after #23**:
  `take_events()` (an owned receiver for the gateway's `select!` task, which
  is dropped before `shutdown`), `FleetConnectorOptions { handshake, active,
  manage_ssh_config, max_frame_size, endpoint_timeout }`, `ActiveGeometry`,
  `set_active_geometry` (never called by the gateway).
- **The gateway is a passive reader** (decision (p)): the generation-1 hello
  gained `surface_active` in #3670, and the server no longer makes a client
  that sends `false` its foreground client or lets it claim pane geometry.
  This removes the E1 caveat that drove the old "hold connections only while
  someone reads" rule — the gateway can hold every host permanently.
- **The ssh child's stderr is handled through upstream's own switch**
  (decision (q)): `SshStdioBridge::start_with` now takes `noninteractive`,
  which nulls the child's stderr and applies `BatchMode=yes`; the fleet
  threads it through as a connector option the gateway turns on. No fd
  juggling in the gateway.
- **Saved machine profiles become an opt-in host source** (decision (r),
  the "E3 decides" item of ADR 0002) in a small separable PR 9; PR 10 is the
  docs PR. Ten PRs instead of nine.
- Every path, symbol and line reference below was re-verified on
  `3d649ce3`; stale ones (`client_transport.rs:669-786`, `headless.rs:1160`,
  `terminal_sessions.rs` at 267 lines, `do_handshake` at `:131`, the
  "shutdown unlinks forward sockets" claim, `src/remote/saved.rs` as the
  profile store) were rewritten.

**Dependency chain:** E1 is ✅; E2 is ♻️ superseded (ADR 0002). E3 assumes
exactly these E1 contracts (verified in the code):

- `src/fleet/connector.rs`: `FleetConnector::start(specs, options) -> Self`
  (`:364`), `events(&mut self) -> Option<&mut mpsc::Receiver<FleetEvent>>`
  (`:459`), `take_events(&mut self) -> Option<mpsc::Receiver<FleetEvent>>`
  (`:474`, second call `None`; **drop the receiver before `shutdown`**),
  `shutdown(self)` (`:558`: sets the stop flag, closes the receiver if still
  owned, half-closes every stream through the connector-local
  `shutdown_stream_write` (`:1383-1392`, the replacement for upstream's
  removed `ipc::shutdown_local_stream_write`), waits `SHUTDOWN_JOIN_TIMEOUT
  = 2 s` on the supervisor exit channel, then detaches), `send` (`:687`),
  `set_active`/`set_active_geometry` (`:485`/`:532`, never called by the
  gateway), `FleetConnectorOptions` (`:126-143`: `handshake:
  HandshakeParams`, `active: ActiveGeometry`, `manage_ssh_config`,
  `max_frame_size = MAX_FRAME_SIZE`, `endpoint_timeout = 60 s`) with
  `for_config(&Config)` (`:151`, reads `[remote].manage_ssh_config`) and the
  unused `for_client` (`:167`); `INACTIVE_SURFACE` (`:72`, 120×40 = the
  default headless size); `FleetEvent::{Host, Surface, SurfacePatch,
  Notification, EndpointResponse, ServerMessage}` (`:199-227`); an inactive
  host's surface frames are dropped in the reader thread (`:1155-1173`).
  **Forward sockets are unlinked by `SshTransport::drop`** when a supervisor
  thread exits and drops its transport, not by `shutdown` itself; a
  supervisor still running after the 2 s wait is detached and its socket
  stays until process exit — so the gateway must close every stream and wait
  for the connector before returning from `main`.
- `src/fleet/handshake.rs`: `HandshakeParams { cell_width_px,
  cell_height_px, surface_size, pixel_mouse, mouse_capture, read_timeout }`
  (`:45-52`), `read_only(surface_size)` (`:56`), `for_client(…)` (`:83`),
  `endpoint_handshake(&mut LocalStream, &HandshakeParams) ->
  io::Result<HandshakeOutcome>` (`:123`) which hardcodes `surface_active:
  true` at `:140-144` with the comment "E3 may flip it for hosts with no
  active surface". `HandshakeOutcome::{Connected, Incompatible, Rejected}`.
- `src/fleet/state.rs`: `FleetState::{new, hosts, host, active_host,
  set_active_host, apply, merged_agents, totals}` (`:358-469`);
  `HostConnection::{Connecting{attempt}, Connected{server_version, methods},
  Unavailable{reason, retry_in}, Incompatible{generation, reason}}` +
  `is_connected/state_name/reason`; `FleetChange` tagged `kind`
  (`host_connection`, `snapshot`, `agent_added`, `agent_removed`,
  `agent_status`, `active_host`; `:281-319`), `Serialize + Deserialize`,
  **no catch-all** — a reader skips unknown kinds; the `HostConnection` delta
  serializes as `ConnectionReport` without `methods`. `test_new()`,
  `test_with_adversarial_identity_state()`, `assert_invariants_for_test()`
  are `#[cfg(test)]` on a separate impl block (`:822-970`) and reachable only
  from in-crate tests.
- `src/fleet/report.rs`: `FLEET_STATUS_SCHEMA = "herdr.fleet.status.v1"`
  (`:19`), `FleetStatusReport::from_state(&mut FleetState, client_version)`
  (`:220`, `&mut` for the merged cache), `HostReport`, `ConnectionReport`
  (`#[serde(other)] Unknown`), `WorkspaceReport`, `AgentReport`; every key
  always present.
- `src/fleet/hosts.rs`: `HostId` (`new` validates `[A-Za-z0-9._-]` via
  `validate_fleet_host_name`, never contains `/`; `local()`, `as_str`),
  `HostKind::{Local{session}, Ssh{target, session}}`, `HostSpec { id, kind,
  enabled }`, `resolve_hosts(&FleetConfig) -> Result<Vec<HostSpec>,
  Vec<String>>` (`:149`, all-or-nothing). `src/fleet/refs.rs`:
  `FleetPaneRef::new(host, pane_id)`, `Display`/`FromStr` as `host/w1:p1`,
  `is_valid_resource_id` (`:24`).
- `src/fleet/transport/`: `trait HostTransport { connect(&mut self) ->
  io::Result<LocalStream>; read_timeout(); describe() }` (`mod.rs:25`),
  `transport_for(&HostSpec, &FleetConnectorOptions) -> Result<Box<dyn
  HostTransport>, String>` (`mod.rs:41`), `LocalTransport::new(session)`,
  `SshTransport::new(host, target, session, manage_ssh_config)` (`ssh.rs:60`)
  which derives its forward socket once at
  `local_forward_socket_path_scoped(host.as_str(), target, session)`
  (`ssh.rs:70`) and starts the bridge with `SshStdioBridge::start_with(…,
  noninteractive: false, BridgeErrorSink::Report(_))` (`ssh.rs:169-179`).
  **A second `SshTransport` with the same scope in the same process fails
  with `AddrInUse`**, so the gateway's terminal streams need their own scope
  (PR 6). `Drop` (`ssh.rs:230-238`) clears the bridge before the ssh session.
- `src/fleet/oneshot.rs`: `FleetSession::start(&Config)` (`:36`; resolves
  hosts, `FleetState::new`, `set_active_host(None)`, `FleetConnector::start`
  with `for_config`), `settle`, `next_changes` (`blocking_recv` — must not
  run inside a runtime), `report`, `shutdown`; the model for "resolve config
  → specs → connector → state", which the gateway re-does asynchronously.
- `src/fleet/mod.rs:33-227` is an architecture guard: `hosts`, `refs`,
  `report`, `state` may not name `tokio`, `ratatui`, `interprocess`,
  `crate::ipc`, `crate::remote` or `crate::client`. New fleet modules are not
  covered unless added to `PURE_MODULES`; PR 9 adds its pure mapper there.
- E1 constraints recorded in the roadmap and `docs/fork/fleet-core.md`:
  `agents[]` excludes plain panes (the fleet lab's marker panes are **not**
  agents — `/api/fleet` from the lab shows `workspaces[]` but empty
  `agents[]`); `fleet_change_seq` is per `FleetState` instance; on unix
  dropping an `SshTransport` while a bridged stream is still open blocks
  until the ssh child exits — release streams first.

### Real current state (verified on `master` @ `3d649ce3`)

- **No gateway anywhere.** `src/gateway/`, `web/`, `[gateway]`,
  `tests/fork_gateway.rs`, `docs/fork/gateway.md`, `scripts/fork/ws-client.py`
  do not exist. `grep -rn 'cfg(feature' src/` is empty and **`Cargo.toml`
  has no `[features]`, `[lib]`, `[dev-dependencies]` or `[profile]`
  section** (sections: `[package]`, `[dependencies]` `:23-48`,
  `[patch.crates-io]` `:50`, `[target.'cfg(windows)'.dependencies]` `:53`),
  so `gateway` is the crate's first feature and the `--no-default-features`
  build path is untested today. The crate is a single bin (`herdr`), so
  `tests/*.rs` cannot `use herdr::…` and drive the binary through
  `env!("CARGO_BIN_EXE_herdr")`; handler-level tests must be in-crate
  `#[cfg(test)]` modules under `src/gateway/`.
- **Dependencies.** `tokio = { version = "1", features = ["rt-multi-thread",
  "macros", "sync", "time", "process", "io-util"] }` — **no `net`, no
  `signal`**; `serde`/`serde_json`, `base64 = "0.22.1"`, `sha2 = "0.10"`,
  `bytes = "1"`, `time = "0.3.47"`, `tracing`/`tracing-subscriber`,
  `interprocess = "2.4.2"`, `bincode = "2"`, `clap 4.5` (`std,help,usage`),
  `clap_complete`, `ratatui 0.30`, `crossterm 0.29`, `portable-pty` (patched),
  `libc`, `regex`, `toml 0.8`, `schemars`, `png`, `ctrlc`, `jsonc-parser`,
  `serde_ignored`, `unicode-width`. `Cargo.lock` (267 packages) has **no**
  `axum`, `hyper`, `http`, `tower`, `tower-http`, `tungstenite`,
  `tokio-tungstenite`, `qrcode`, `subtle`, `constant_time_eq`, `hmac`,
  `url`, `mime_guess`, `include_dir`, `rustls`, `ring`; it *does* carry
  `rand 0.8.5`, `getrandom 0.3.4` + `0.4.2`, `futures-util 0.3.33`, `uuid
  1.22.0`, `tokio 1.50.0` transitively. `just ci` runs `--locked`, so
  `Cargo.lock` ships in the same PR as `Cargo.toml`. `rust-toolchain.toml`
  pins `1.96.1`; edition 2021. `.cargo/config.toml` sets `[env]
  HERDR_BUILD_CHANNEL = "fork"`. `build.rs` builds libghostty-vt with Zig
  0.15.2 and emits `rerun-if-changed` for the vendored tree only; it does
  not know about `web/`. `.gitignore` has no `web/` entry (only
  `node_modules/`), so a committed `web/dist` is tracked by default.
- **Upstream #3670 in the tree.** Saved SSH machines live in
  `src/client/endpoint/catalog.rs`: `pub(crate) struct SavedSshEndpoint { id:
  ProfileId, label, target, session, enabled }` (`:20-25`,
  `deny_unknown_fields`, validated by `:45` — target via
  `remote::validate_remote_target`, session via `session::validate_name`,
  label ≤ 128 bytes, no passwords), `EndpointCatalog::load_profiles() ->
  Result<Vec<SavedSshEndpoint>, String>` (`:106`, reads only the catalog),
  stored as pretty JSON at `catalog_path() = config::state_dir()/client/
  endpoints.json` (`:369`, private-mode write, ≤ 64 profiles). Both are
  re-exported at `crate::client::endpoint::*` (`src/client/endpoint.rs:16`;
  `ProfileId` at `:27`, 32 lowercase hex chars). `herdr machine list|add|
  rename|remove|enable|disable` is `src/cli/machine.rs:30`
  `run_machine_command`, wired at `src/cli.rs:30`/`:119`,
  `src/cli/spec.rs:37` (`machine::command()` in `src/cli/spec/machine.rs`)
  and `src/main.rs:761`. **`src/remote/saved.rs` is not the profile store**:
  it is the saved-machine ssh bridge (`connect_saved_ssh`,
  `saved_ssh_failure_needs_attention` `:49` — a reusable "needs human
  attention" classifier). `src/remote/args.rs` (`validate_remote_target`
  `:118`), `process.rs` (`wait_with_output_timeout`, `pub(super)`) and
  `restart_policy.rs` (pure, `pub(super)`) are internal to `remote`.
- **The generation-1 hello has a passive-reader hook.**
  `src/protocol/endpoint.rs:46` `EndpointClientHello.surface_active: bool`
  with `#[serde(default = "default_true")]` and **no `deny_unknown_fields`**,
  so a server that predates the field ignores it. Server semantics for
  `false` (`src/server/headless.rs`): the connect arm sets
  `foreground_client_id` only `if surface_active` (`:2024`);
  `promote_client_to_foreground` returns early for an inactive client
  (`:908-915`); a `ClientResize` from it is ignored (`:2248`);
  `claim_unowned_shell_tab_geometry`/`resize_shell_tab_if_controller`
  (`src/server/headless/client_views.rs:708-737`) refuse it; and
  `render_targets` (`src/server/clients.rs`) still includes every shell
  client with a writer, so **snapshot updates keep flowing to a passive
  client** (`src/server/headless/render.rs:409-450`). The fleet currently
  sends `true` (`src/fleet/handshake.rs:140-144`). The console's own
  `do_handshake` (`src/client/handshake.rs:154`, `pub(super)`) takes
  `surface_active` as its last parameter.
- **The ssh bridge has a daemon mode.** `SshStdioBridge::start_with(target,
  remote_herdr, local_socket, session_name, ssh_options, noninteractive,
  errors)` (`src/remote/attach.rs:1843-1851`); `bridge_connection`
  (`:2048-2072`) spawns the per-connection `ssh` with `.stderr(if
  noninteractive { Stdio::null() } else { Stdio::inherit() })` and, when
  noninteractive, `apply_noninteractive_ssh_options` (`:575-591`:
  `BatchMode=yes`, `NumberOfPasswordPrompts=0`, `StrictHostKeyChecking=yes`,
  `ConnectTimeout=10`, `ConnectionAttempts=1`, keepalives). Discovery probes
  (`RemoteSsh::sh_output`/`user_shell_output`, `:414-460`) always pipe
  stderr into the returned error, so the bridge child is the only site that
  inherits fd 2. `RemoteSsh::new_noninteractive` (`:383`) is `pub(super)`
  and drops the session name and managed config; E3 does not need it.
  `BridgeErrorSink::{Stderr, Log, Report(Arc<dyn Fn(String)>)}` (`:1935`).
- **Runtime shape to copy.** No `#[tokio::main]`; the long-running daemon
  path is `src/server/headless/bootstrap.rs:40-43`
  `tokio::runtime::Builder::new_multi_thread().enable_all().build()` +
  `rt.block_on`. Every socket read/write on the herdr side is **blocking
  std I/O**: `protocol::write_message(&mut W, &M)` / `read_message(&mut R,
  max_frame_size)` (`src/protocol/wire.rs:1601`/`:1624`, `[u32 LE len]
  [bincode]`), `crate::ipc::connect_local_stream(&Path) ->
  io::Result<LocalStream>` (`src/ipc.rs:35`; `LocalStream =
  interprocess::local_socket::Stream`, `:11`).
- **Observe/control wire path (frozen, reused as-is).** A client-socket
  connection has one mode for its lifetime, chosen by its first message in
  `handle_client_handshake` (`src/server/client_transport.rs:646-830`):
  `ClientMessage::TerminalHello { version: PROTOCOL_VERSION (22), cols, rows,
  cell_width_px, cell_height_px, pixel_mouse }` (tag 0, arm at `:692`) →
  `ServerMessage::Welcome { version, encoding, error: None }` where
  `encoding` is `RenderEncoding::TerminalAnsi` whenever there are no shell
  options (`:801-805`) and the connection is `ClientConnectionMode::
  TerminalPending` (`src/server/clients.rs:12-17`: `ClientShell`,
  `TerminalPending`, `TerminalAttach{terminal_id}`,
  `TerminalObserve{terminal_id}` — **control is attach**); then
  `ClientMessage::ObserveTerminal { target }` (tag 7) or `ControlTerminal {
  target, takeover }` (tag 8) (`:1029-1037` → `ServerEvent::
  ClientObserveTerminal`/`ClientControlTerminal`, dispatched at
  `src/server/headless.rs:2051-2058`), where `target` resolves server-side
  through `resolve_terminal_target_id_string` (`headless.rs:1172`: raw
  terminal id, then `app.resolve_terminal_target` for a public pane id
  `w1:p1` or an agent target; unknown → `ServerShutdown { reason: "terminal
  session observe failed: terminal target … not found" }` and disconnect).
  Output is **one message type**, `ServerMessage::Terminal(TerminalFrame {
  seq: u64, width: u16, height: u16, full: bool, bytes: Vec<u8> })`
  (`wire.rs:1271`/`:1346`) — already-diffed ANSI, `full: false` is the patch;
  `Graphics` (`:1349`) and everything else is dropped by the CLI. Inputs on
  a control connection reuse the direct terminal vocabulary: `Input { data }`
  (tag 1), `Resize { cols, rows, cell_width_px, cell_height_px, pixel_mouse }`
  (tag 3), `AttachScroll { source: Wheel | PageKey{input}, direction:
  Up|Down, lines, column, row, modifiers }` (tag 6), `Detach` (tag 4 —
  "release"). Server facts that matter: observers are unlimited and never
  own the terminal (`terminal_observe_allows_multiple_clients_without_attach_
  ownership`, `src/server/headless/tests/mod.rs:3152`); **an observer's
  `Resize` changes only its own client-local viewport** (`ServerEvent::
  ClientResize`, `headless.rs:2205-2222`), while a controller's `Resize`
  resizes the real PTY (`:2182-2204`, the only path that calls
  `runtime.resize`); control is single-owner — a second `ControlTerminal`
  without `takeover` gets `ServerShutdown { "… already has an attached
  client; retry with --takeover" }` **and is disconnected**
  (`headless.rs:1826-1838`), and a takeover evicts the previous owner the
  same way; mode is one-way (observe → control on the same connection is
  refused); an observed hidden pane keeps rendering; the server's per-client
  render lane has capacity one (`ClientWriterQueueState.render:
  Option<Vec<u8>>`, `client_transport.rs:251-310`; `render.rs:633-651`
  defers a full render on `TrySendError::Full`), so a slow reader coalesces
  frames rather than desyncing. Caps: hello and non-graphics frames use
  `MAX_FRAME_SIZE = 2 MiB` (`wire.rs:24`); `MAX_GRAPHICS_FRAME_SIZE = 32 MiB`
  (`:29`) only for graphics, which the gateway never enables.
- **The CLI twin of that path**, `src/client/terminal_sessions.rs` (277
  lines): `pub fn run_terminal_session_observe(target, cols, rows)` (`:18`) /
  `run_terminal_session_control(target, takeover, cols, rows)` (`:26`);
  private `connect_terminal_session_stream` (`:72`; hardcodes
  `client_socket_path()`, calls `std::process::exit(1)` on failure — not
  reusable), private `write_terminal_session_output` (`:122`; the read loop,
  reads with `MAX_GRAPHICS_FRAME_SIZE`, emits NDJSON `{"type":"terminal.
  frame","seq","encoding":"ansi","width","height","full","bytes":<base64>}`
  / `{"type":"terminal.closed","reason"}`), and `pub(super) fn
  terminal_control_command_from_json(raw: &str) -> Result<ClientMessage,
  String>` (`:208`) mapping `terminal.input {text | bytes}`,
  `terminal.resize {cols, rows, cell_width_px, cell_height_px}`,
  `terminal.scroll {direction, lines, source, column, row, modifiers}`,
  `terminal.release` (tests at `src/client/tests/mod.rs:806-849`). Defaults
  120×40 (`src/cli.rs:632-633`); no SIGWINCH handling, no `--encoding`
  flag. The terminal hello is two frames and is re-implemented in the
  gateway rather than widening `src/client/handshake.rs`.
- **CLI wiring pattern** (E1's `fleet`): `src/cli.rs:28` `mod fleet;` and
  `:117` `"fleet" => fleet::run_fleet_command(&args[2..])?` in `maybe_run`
  (`pub(super) fn run_fleet_command(args: &[String]) -> io::Result<i32>`,
  `src/cli/fleet.rs:26`, hand-parsed args, `help` → 0, usage error → 2);
  `src/cli/spec.rs:34` `.subcommand(fleet_command())` + `fn fleet_command()`
  at `:138-148` (help and completions only; invariants
  `spec_describes_all_completion_commands`,
  `every_spec_subcommand_renders_short_and_long_help`,
  `spec_passes_clap_invariants`); `src/main.rs:751-773` the bare-command
  allowlist (`"fleet"` at `:760`, `"machine"` at `:761` — a missing entry
  makes `herdr gateway` exit 2 "unknown command") and `:612` the `--help`
  usage line for fleet (`:607` for machine). `build_info::is_fork()` gates
  only the `herdr update` help line (`:601-605`).
- **Config pattern** (E1's `[fleet]`): `src/config/model.rs:979-989`
  `FleetConfig { include_local, hosts }` (`#[derive(Debug, Deserialize)]
  #[serde(default)]`, hand-written `Default` at `:991`, pure `diagnostics()`
  at `:1046`), `Config.fleet` at `:323` (last field); `src/config/io.rs:7-21`
  `KNOWN_TOP_LEVEL_CONFIG_KEYS` allowlist (alphabetical; `"gateway"` goes
  between `"fleet"` and `"keys"`), `load_live_section(table, "fleet", …)` at
  `:353-360` + `diagnostics.extend(config.fleet.diagnostics())` at `:363`
  on the live-reload path, `Config::collect_diagnostics()`
  (`src/config.rs:117-130`, an explicit chain ending in
  `.chain(self.fleet.diagnostics())`); `src/main.rs:65` `DEFAULT_CONFIG`
  with the fully commented `[fleet]` block at `:404-414` and two contract
  tests (`default_config_fleet_block_parses_without_diagnostics` `:910`,
  `default_config_fleet_block_is_commented_out` `:940`);
  `scripts/config_reference_check.py:36` `SKIPPED_SUBTREES = ("keys.command",
  "fleet")` — `scripts/test_config_reference_check.py` runs inside `just ci`
  (`maintenance-test`) and fails on any new key outside a skipped subtree
  because fork rules forbid editing `docs/next/**`. Paths:
  `config::config_dir()` (`io.rs:31`, `$XDG_CONFIG_HOME/<app>`;
  `app_dir_name()` = `herdr-dev` in debug, `herdr` in release),
  `state_dir()` (`:38`), `config_path()` (`:170`, `HERDR_CONFIG_PATH` =
  `config::CONFIG_PATH_ENV_VAR` override checked first), `Config::load() ->
  LoadedConfig` (no path parameter — `--session` works by setting
  `HERDR_SESSION` before anything runs, `src/session.rs:55-91`).
  `[server] headless_cols/headless_rows` (`DEFAULT_CONFIG` `:224-226`) is
  applied live on `herdr server reload-config` (`src/app/mod.rs:872-875`) —
  the lever PR 3's passivity evidence uses.
- **Secrets/permissions precedent.** `0o600`/`0o700` via
  `std::os::unix::fs::{OpenOptionsExt, PermissionsExt}` in
  `src/pane_graphics_files.rs` (`DIRECTORY_MODE`/`FILE_MODE` `:13-15`,
  exact-mode verification at `:298`/`:320`) and `src/ipc.rs:328-338`
  `restrict_socket_permissions` with a `#[cfg(windows)]` no-op twin (`just
  check`'s `windows-lint` clippy-compiles the bin for
  `x86_64-pc-windows-msvc`). **No constant-time compare and no CSPRNG exist
  in `src/`** — unique ids are `AtomicU64`/pid+nanos (`ProfileId::generate`
  is SHA-256 of `pid:nanos:sequence`, not a secret).
- **Tests/tooling.** `tests/support/mod.rs` (`pub mod fleet_lab;` is the
  only module; `build_version()` `:35`, `wait_for_socket` `:119`,
  `client_handshake` `:295` = a hand-encoded `TerminalHello`,
  `read_server_message` `:368` → `(tag, payload)`, pid/runtime-dir hygiene),
  `tests/support/fleet_lab.rs` `Lab::{new, up, run, run_with_bin, herdr,
  runtime_dir}` + `unique_root`/`stdout_of`/`stderr_of`/`STEP_TIMEOUT_MS`
  (`Drop` runs `down`), `tests/fork_fleet_lab.rs`/`tests/fork_ssh_lab.rs`
  (`#![cfg(unix)]`, the `fork_*` naming), `tests/cli/fleet.rs` (writes
  `[fleet]` into `<config_home>/<app>/config.toml`, `TWO_LOCAL_HOSTS`).
  `scripts/fork/` = `dev-setup.sh`, `fleet-lab.sh`, `gate.sh`, `ssh-lab.sh`
  only. `fleet-lab.sh up N | status [--json] | env | down` writes only
  `onboarding = false` into `<root>/xdg/{herdr,herdr-dev}/config.toml` —
  tests append `[fleet]`/`[gateway]` themselves; `env` exports
  `XDG_CONFIG_HOME`, `HERDR_FLEET_LAB_{ROOT,RUNTIME_DIR,SESSIONS}`,
  `HERDR_FLEET_LAB_{CLIENT_SOCKET,API_SOCKET,PANE}_N`. `scripts/fork/ssh-lab.sh`
  gives one ssh host (`herdr-ssh-lab`, exit 3 = no sshd; exports
  `HERDR_SSH_LAB_{ROOT,HOME,TARGET,PORT,SSH_CONFIG}`). `justfile`: `ci` =
  `lint` (`cargo fmt --check`, `cargo clippy --all-targets --locked -- -D
  warnings`) + `cargo nextest run --locked -E "{{filter}}" --status-level
  fail --final-status-level slow --failure-output final --success-output
  never` + `maintenance-test` + `ui-hot-path-architecture-test` +
  `integration-assets-test` + `plugin-marketplace-test`; **no recipe passes
  `--features`**; `bench-fleet-scale` no longer exists. Fork CI
  (`.github/workflows/fork-ci.yml`): `conventional-commits`, `check
  (ubuntu-latest)` (toolchain 1.96.1 + just/nextest + bun 1.3.14 + Zig
  0.15.2 + rust-cache key `fork-ubuntu-latest` → `just ci`, 30 min),
  `shellcheck -S warning scripts/fork/*.sh` (non-recursive). No `websocat`;
  `python3` ≥ 3.10 stdlib and `curl` are available. `assets/fork/{logo.svg,
  logo-192.png, logo-512.png, README.md}` exist for E4. `docs/fork/README.md`
  keeps the fork-wiring table at `:453-464` (with a duplicated
  `src/remote/attach.rs` row to fold) and the CI table at `:396-400`.
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
  gateway owns the `FleetConnector` inside `FleetRuntime`; one task owns the
  receiver from `take_events()`, awaits it in a `select!` with the stop
  signal, and folds `FleetEvent::Host` into a `std::sync::Mutex<FleetState>`
  (never held across an `.await`), broadcasting every `FleetChange` through
  `tokio::sync::broadcast` (capacity 1024, `Arc<str>` pre-serialized JSON).
  Handlers read the mutex briefly. Terminal sessions are blocking std
  threads per session (reader + writer) bridged to the WebSocket task with
  bounded channels (capacity 2) so a slow browser applies backpressure to
  the server's render lane instead of buffering; the gateway therefore never
  holds more than two frames per open terminal. Graceful stop on
  SIGINT/SIGTERM: stop accepting → close terminal sessions (send `Detach`,
  drop streams, join threads) → drop terminal transports → drop the event
  receiver → `connector.shutdown()` (in `spawn_blocking`) → remove the
  runtime marker → return from `main` (which is what finally drops any
  detached supervisor's `SshTransport` and its forward socket).
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
  options, scope)` (additive, fork-owned files) so the gateway holds one
  `Mutex<Box<dyn HostTransport>>` per host and calls `connect()` once per
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
  default)*: set `HERDR_CONFIG_PATH` (`config::CONFIG_PATH_ENV_VAR`) at the
  top of `run_gateway_command` before any thread exists, so
  `config_path()`, `Config::load()` and a later live reload all agree.
  `--bind` overrides `[gateway] bind`.
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
- **(p) The gateway is a passive reader: its aggregator hello sends
  `surface_active: false`** *(auto default; ADR 0002 recommended it)*. PR 3
  adds `HandshakeParams.surface_active: bool` and makes
  `HandshakeParams::read_only` set it to `false`, so both consumers with no
  active host — the gateway and `herdr fleet status` — become passive;
  `for_client` (unused since E2's retirement) keeps `true` because a console
  that activates a host needs the server to honour its resizes. Reason: the
  E2/E1 finding that a connecting client shell becomes the host's foreground
  client and sets its effective pane geometry (`headless.rs:2024`,
  `client_views.rs:708`) no longer applies to a client that sends `false`;
  snapshots still flow (`render_targets` includes every shell client), which
  is all the fleet reads; and a pre-#3670 server ignores the field (no
  `deny_unknown_fields`), degrading to the old foreground behaviour instead
  of refusing the hello. The gateway may therefore hold every configured
  host for as long as it runs. The residual documented limitation becomes:
  *against a server older than #3670 the gateway is that host's foreground
  client at 120×40 (a no-op for a headless default-size server)*.
- **(q) Daemon ssh: the fleet passes `noninteractive: true` to the bridge
  when a consumer asks for it** *(auto default)*. PR 3 adds
  `FleetConnectorOptions.ssh_noninteractive: bool` (default `false`, so
  `herdr fleet status` is byte-identical) and
  `FleetConnectorOptions::for_daemon(&Config)` (= `for_config` +
  `ssh_noninteractive: true`), threaded through `transport_for` into
  `SshTransport::new(…, noninteractive)` and on to
  `SshStdioBridge::start_with(…, noninteractive, sink)`. Effect for the
  gateway: the per-connection `ssh` child's stderr is `Stdio::null()`
  (nothing is ever painted on the daemon's stderr/journal by ssh itself),
  and `BatchMode=yes`/`NumberOfPasswordPrompts=0`/`StrictHostKeyChecking=
  yes`/`ConnectTimeout=10` guarantee the daemon never blocks on a prompt.
  Failures still reach the host's `Unavailable { reason }` through
  `BridgeErrorSink::Report` (exit status) and the next attempt's discovery
  probe (which pipes ssh's stderr into its error text), exactly the chain
  `fleet-core.md` documents. Discovery probes keep `RemoteSsh::new`
  (managed config, control master, interactive flag): they pipe stderr and
  cannot prompt without a tty, so no `attach.rs` edit is needed; a probe to
  a hung host blocks only that host's supervisor thread. This is the
  smallest option — no fd redirection in the gateway, no new E1 hook.
- **(r) Saved machine profiles are an opt-in host source, `[fleet]
  include_machines = false`** *(auto default — the smallest additive option
  that adopts the ADR 0002 item)*. Reason: after ADR 0002 the console's host
  list is `herdr machine …`; a phone user who already ran `herdr machine
  add` (which installs herdr on the host — the fleet's one precondition)
  should not have to duplicate every target into `[[fleet.hosts]]`, and the
  gateway and `herdr fleet status` must show the same fleet, so the switch
  lives in `[fleet]`, not `[gateway]`. Shape (PR 9, separable): a pure
  mapper `src/fleet/machines.rs::machine_host_specs(&[MachineProfile],
  &[HostSpec]) -> Result<Vec<HostSpec>, Vec<String>>` over a fleet-owned
  `MachineProfile { label, target, session, enabled }`, plus one adapter
  `src/fleet/hosts_source.rs::hosts_for_config(&Config)` that calls
  `resolve_hosts` and, when `include_machines`, `crate::client::endpoint::
  EndpointCatalog::load_profiles()`; `FleetSession::start` and
  `FleetRuntime::start` both call the adapter. Host id = the machine label
  when `HostId::new(label)` accepts it, else the label lowercased with runs
  of other characters folded to `-`; an id that still fails or collides with
  another machine or a `[[fleet.hosts]]` name is a diagnostic naming the
  machine and the fix (`herdr machine rename <id> --label <valid>`), and the
  fleet fails all-or-nothing like `resolve_hosts`. `kind` is `ssh`,
  `session` is the profile's explicit session, `enabled` follows the
  profile. E1's `[[fleet.hosts]]` keys and `resolve_hosts` are untouched.
  Default `false` keeps every existing config byte-identical.
- **(s) Events are taken, not borrowed** *(auto default)*: `FleetRuntime`
  calls `take_events()` once, gives the receiver to the fold task, keeps the
  `FleetConnector` in the struct (E7's `request(host, …)` needs `send`), and
  drops the receiver before `shutdown` as `connector.rs:469-471` requires.

### Sequencing hazards

- **Upstream files touched, and by which PR only:** `Cargo.toml`,
  `Cargo.lock`, `justfile`, `.github/workflows/fork-ci.yml`, `src/main.rs`
  (`mod gateway;`, allowlist, usage), `src/cli.rs`, `src/cli/spec.rs` — PR 1.
  `src/config/model.rs` (`GatewayConfig` after `FleetConfig`),
  `src/config/io.rs`, `src/config.rs` (one `.chain`), `src/main.rs`
  (`DEFAULT_CONFIG` block + two tests), `scripts/config_reference_check.py`
  — PR 2. `build.rs` (one `rerun-if-changed`), `tests/support/mod.rs` (one
  `pub mod gateway;`) — PR 4. `src/client/terminal_sessions.rs` (one
  visibility widening, `pub(super)` → `pub(crate)`, on
  `terminal_control_command_from_json`) — PR 6. `src/cli/spec.rs` (three
  gated subcommands under `gateway_command()`) — PR 8. `src/config/model.rs`
  (one `FleetConfig` field), `src/main.rs` (one commented line in the
  `[fleet]` block) — PR 9. Never: `src/protocol/**`, `src/server/**`,
  `src/app/**`, `src/remote/**` (the E1 hooks are enough), `src/client/**`
  beyond the one word, `tests/fixtures/endpoint-*.json`, `docs/next/**`.
- **Fork-owned fleet files touched:** PR 3 edits `src/fleet/handshake.rs`
  (`surface_active` field), `src/fleet/connector.rs` (`ssh_noninteractive`,
  `for_daemon`), `src/fleet/transport/{mod,ssh}.rs` (thread the flag) and
  two paragraphs of `docs/fork/fleet-core.md`; PR 6 edits
  `src/fleet/transport/{mod,ssh}.rs` again (`new_scoped`,
  `transport_for_scoped`) — sequenced after PR 3 through PR 4; PR 9 adds
  `src/fleet/machines.rs`, `src/fleet/hosts_source.rs`, edits
  `src/fleet/mod.rs` (two `pub mod` lines + `PURE_MODULES`) and
  `src/fleet/oneshot.rs` (one call), and the `[fleet]` prose of
  `fleet-core.md`. PR 3 and PR 9 never run in the same wave.
- **PR 1 changes `Cargo.toml`/`Cargo.lock` and runs alone** in its wave; it
  is the only dependency change of the epic — every later PR is dep-free.
- **`src/gateway/server.rs` (router) and `tests/fork_gateway.rs` are edited
  by PRs 4, 5, 6, 7, 8.** Waves keep at most two of those in flight (W4
  `[5, 6]`, W5 `[7, 8]`); the later PR in a wave rebases onto `master`
  before its gate and re-runs the integration test. Each PR adds its routes
  as a separate `fn <area>_routes() -> Router<AppState>` merged in `router()`
  so the diffs are one line apart, not interleaved. `src/gateway/fleet.rs`
  is created by PR 3 and edited only by PR 9 (one call swap); PR 4 consumes
  it without editing it, so W3 `[4, 9]` is collision-free.
- **Two Rust builds at once** (W2, W3, W4, W5) is the memory hazard the
  gate lock exists for. Never bypass `scripts/fork/gate.sh`.
- **The gateway holds every configured host for as long as it runs.** With
  decision (p) that is passive against current servers, but every live
  validation still points `[fleet]` at lab sessions only (`include_local =
  false`), under the lab's `XDG_CONFIG_HOME`, never at the user's default
  session: the user's real server must never see a fork test client, and a
  pre-#3670 server would still be resized.
- **Secrets in evidence.** Validation transcripts paste status codes, JSON
  keys and frame headers — never a token, cookie or pairing URL. Test
  fixtures generate tokens under a throwaway `XDG_CONFIG_HOME`; nothing
  under `~/.config/herdr*` or `~/.local/state/herdr*` is read or written by
  any test (PR 9's machine fixture writes `endpoints.json` under a throwaway
  `XDG_STATE_HOME`).
- **CI runs the crate twice** from PR 1 on (`just ci` and `just
  ci-no-default`); the second job shares the toolchain/Zig steps but not the
  cargo cache key, so budget ~10 extra minutes per run.
  `tests/live_handoff.rs` `wait_for_file` remains a known upstream flake
  (`gh run rerun --failed -R vinceseguin/herdr`).
- **Upstream syncs during the epic** merge upstream's side of
  `src/client/**` and `src/remote/**` (ADR 0002). E3's only exposure is the
  one `pub(crate)` word in `terminal_sessions.rs` (PR 6) and the already
  re-applied E1 hooks in `attach.rs`; `EndpointCatalog::load_profiles` and
  `SavedSshEndpoint` (PR 9) are `pub(crate)` upstream items — a rename
  breaks the build loudly at merge time, never silently.

## Status legend

✅ merged · 🔨 in progress · ⬜ not started · ⛔ blocked

## PR map

| # | Title | Group | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | chore: add gateway cargo feature with axum, qrcode and token dependencies | A · Foundations | — | ✅ |
| 2 | feat(gateway): gateway config section, token store, bind and origin policy | A · Foundations | 1 | ✅ |
| 3 | feat(gateway): passive async fleet runtime folding connector events into shared state | A · Foundations | 1 | ✅ |
| 4 | feat(gateway): herdr gateway serves health, fleet report and embedded assets over http | B · HTTP | 2, 3 | ⬜ |
| 5 | feat(gateway): websocket fleet event stream | C · Streams | 4 | ⬜ |
| 6 | feat(gateway): websocket terminal observe stream over per-host transports | C · Streams | 4 | ⬜ |
| 7 | feat(gateway): terminal control mode gated by the control scope | C · Streams | 6 | ⬜ |
| 8 | feat(gateway): pairing urls with qr codes, device cookies, status and token rotation | D · Ops | 4 | ⬜ |
| 9 | feat(fleet): opt-in hosts from saved machine profiles | D · Ops | 3 | ⬜ |
| 10 | docs: gateway guide, systemd unit, adr e3 review, roadmap drift | E · Docs | 5, 7, 8, 9 | ⬜ |

**Wave preview (2-agent cap, Cargo PR alone):** W1 `[1]` → W2 `[2, 3]` →
W3 `[4, 9]` → W4 `[5, 6]` → W5 `[7, 8]` → W6 `[10]`. Critical path 1 → 2 →
4 → 6 → 7 → 10. Only PR 1 touches `Cargo.toml`/`Cargo.lock`.

**Model assignment:** tasks run on `opus`. Review agent must be **`fable`**
for **PR 2** (token files, constant-time compare, rate limiter, bind/origin
policy), **PR 4** (the auth middleware and route-level scope gates — the
first network surface), **PR 6** (a terminal stream attached to the wrong
host or pane is a silent mis-route; ssh transport scoping and drop order),
**PR 7** (input reaching a terminal: scope gate, takeover, release), **PR 8**
(pairing exchange, cookie minting, rotation/revocation) and **PR 9** (host
identity derived from machine labels — a wrong mapping routes a terminal to
the wrong machine). PRs 1, 3, 5 and 10 review on `opus`.

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

Real-server validation is mandatory for every PR with runtime code (2–9):
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
SSH validations (PRs 3, 6, 9) add `ssh-lab.sh up` + `eval "$(bash
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
  fails to build. PR 9's fleet code is **not** feature-gated (the fleet
  ships in both builds).
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
  fails; `[fleet] include_machines` (PR 9) is already inside the skipped
  `fleet` subtree. Both are documented only under `docs/fork/`.
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
  `src/protocol/**`, `src/server/**`, `src/app/**`, `src/remote/**`,
  `tests/fixtures/endpoint-*.json` have an empty diff for the whole epic.
  The gateway speaks the frozen `TerminalHello`/`ObserveTerminal`/
  `ControlTerminal`/`Input`/`Resize`/`AttachScroll`/`Detach` messages and
  the generation-1 endpoint handshake E1 already speaks (now with
  `surface_active: false`, an existing optional field), nothing else.
- **Additive, mergeable code.** All new code is `src/gateway/**`,
  `src/fleet/{machines,hosts_source}.rs`, `web/**`,
  `scripts/fork/ws-client.py`, `scripts/fork/systemd/**`,
  `tests/fork_gateway.rs`, `tests/support/gateway.rs`, `docs/fork/**`.
  Upstream edits are limited to the list in *Sequencing hazards*; each is a
  handful of lines and lands in exactly one PR.
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
  policy), `src/gateway/protocol.rs` (message shapes, the binary frame
  header) and `src/fleet/machines.rs` are pure, sync, testable without
  sockets or a runtime; only `fleet.rs`, `terminal.rs`, `server.rs` and the
  handlers touch tokio, axum or streams. `FleetState` stays the single
  source of truth; the gateway keeps no second copy of hosts or agents.
- **Host failure is local.** An unavailable host is data in `/api/fleet` and
  a `host_connection` delta, never a 5xx; a terminal open on a down host is
  a `terminal.error` on that socket only; a slow browser stalls its own
  terminal session only; a hung ssh probe blocks only that host's
  supervisor thread. Nothing in the gateway calls `std::process::exit`
  after startup or panics because of what a host or a client sent.
- **Passive by construction.** The gateway never calls `set_active`,
  `set_active_geometry` or sends a `ClientShellResize`; its aggregator hello
  is `read_only` (`surface_active: false`, 120×40); its terminal streams are
  separate observe/control connections that own exactly the semantics the
  server gives them.
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
- **Docs.** `docs/fork/gateway.md` (PR 10) is the only reference for
  `[gateway]` keys, the HTTP/WS contract and the token files;
  `docs/fork/fleet-core.md` stays the only reference for `[fleet]` (PRs 3
  and 9 update it in the same PR as the behaviour). Every earlier PR adds
  its keys/endpoints to a running *Reference notes* list in its PR body so
  PR 10 needs no re-derivation. Never edit `docs/next/**`, root
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
  "gateway")] mod gateway;` after `mod fleet;` (`:25`); `#[cfg(feature =
  "gateway")] "gateway",` in the bare-command allowlist (`:752-767`); a
  gated `println!("       herdr gateway [--bind ADDR] …")` usage line next
  to the `fleet` one (`:612`).
- `src/cli.rs` *(upstream file — minimal wiring)*: `#[cfg(feature =
  "gateway")] "gateway" => crate::gateway::run_gateway_command(&args[2..])?,`
  next to `:117`.
- `src/cli/spec.rs` *(upstream file — minimal wiring)*: `fn
  gateway_command() -> Command` (gated, next to `fleet_command()` `:138`)
  and a gated `.subcommand(…)` at `:34`; keep the three spec invariants
  green under both feature sets.
- `justfile` *(upstream file — minimal wiring)*: `lint-no-default` and
  `ci-no-default` recipes (below).
- `.github/workflows/fork-ci.yml` (fork-owned): job
  `check-no-default-features` (`name: check-no-default-features
  (ubuntu-latest)`).
- `docs/fork/README.md`: one row per upstream file in the fork-wiring table
  (`Cargo.toml` feature + deps, `justfile`, `src/main.rs`/`src/cli.rs`/
  `src/cli/spec.rs` gateway arms) and the CI job in the CI table; fold the
  duplicated `src/remote/attach.rs` row while there.

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
port]`, nothing else is accepted). Also decline the deps E6/E8 will need
(`web-push` etc.) — one PR, one reason set.

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

**Landed** (merged; gate `EXIT=0` for both `ci` and `ci-no-default`)

- `Cargo.toml` grew exactly the planned `[features]` block and the four
  optional crates; resolved versions are `axum 0.8.9`, `qrcode 0.14.1`,
  `subtle 2.6.1`, and `getrandom` stayed at the `0.3.4` the lock already
  carried (no new `getrandom` entry). `Cargo.lock` gained **33** packages,
  all of them inside axum's `http1`/`ws`/`json`/`query` closure or tokio's
  `net`: `hyper 1.11.1`, `http 1.5.0`, `tower`, `matchit`, `socket2`,
  `tokio-tungstenite 0.29.0` (**not** the 0.30.0 the registry note
  predicted) and `tungstenite 0.29.0`. Correction to the plan's reasoning:
  choosing `getrandom` over `rand` did avoid a *direct* subtree, but
  `rand 0.9.5` + `rand_chacha` + `ppv-lite86` + `zerocopy` arrive anyway as
  `tungstenite`'s RFC 6455 masking dependency. `image` is **not** in the
  lock (`qrcode`'s lock entry has no dependencies at all), and
  `cargo tree --no-default-features -e normal` matches none of
  axum/qrcode/subtle/hyper/tungstenite.
- **Two shared constants** in `src/gateway/mod.rs` instead of one
  `GATEWAY_USAGE`: `GATEWAY_COMMAND_LINE` (`herdr gateway [--bind ADDR]
  [--config PATH]`, also printed by `src/main.rs`'s `--help` so the two
  cannot drift) and `GATEWAY_STAGING_NOTE`. The staging note is appended to
  both help surfaces — `gateway_help()` and `gateway_command()`'s
  `.after_help(…)` — because PR 1 advertises `--bind`/`--config` while
  nothing runs them. **PR 4 must delete `GATEWAY_STAGING_NOTE`, its two
  call sites and the `the_staging_note_matches_the_dispatch` test**; that
  test asserts `--bind` still exits 2, so it fails the moment the run path
  lands and cannot be forgotten.
- `herdr gateway --help` and `-h` never reach `run_gateway_command`:
  `spec::print_requested_help` intercepts them and renders clap's help
  (exit 0). The `--help`/`-h` arms inside `run_gateway_command` are
  defensive, matching `run_fleet_command`/`run_channel_command`.
- `tests/fork_gateway.rs` is gated `#![cfg(all(unix, feature = "gateway"))]`,
  **without** the planned `not(target_os = "macos")`: these tests only spawn
  the binary and read stdout, so the macOS exclusion that `tests/cli.rs`
  needs (servers, PTYs) does not apply, and keeping it would have silently
  dropped every appended E3 test on macOS.
- `just ci-no-default` takes the same `filter='all()'` parameter as `ci`;
  both it and `lint-no-default` are `[unix]`, so Windows sees no dangling
  recipe dependency. Test counts: 3436 (default) vs 3429
  (`--no-default-features`).
- `just windows-lint` is green with the new dependency set (axum, qrcode,
  subtle, hyper-util, matchit and tokio `net`/`signal` all clippy-compile
  for `x86_64-pc-windows-msvc` under `-D warnings`), so no PR in this epic
  starts from a broken Windows target.
- Known stale text, deliberately not touched here: `.claude/rules/fork.md`'s
  *The gate (every PR)* section still describes green as `just ci` alone.
  `docs/fork/README.md` and this plan's *Verification* section both carry
  the two-feature-set rule; fold the rules file in with PR 10's docs pass.

### PR 2 — feat(gateway): gateway config section, token store, bind and origin policy · deps: 1

**Goal:** the pure half of the gateway's security model: a `[gateway]`
config section with defaults and diagnostics, a token store that creates
and verifies `0600` token files with constant-time digest compares, device
and pairing records, a failure rate limiter, and the bind/origin policy —
all testable without a socket or a runtime.

**Files**

- `src/config/model.rs` *(upstream file — minimal wiring)*: `GatewayConfig`
  appended after `FleetConfig` (`:979-1063`), `pub gateway: GatewayConfig`
  as the new last field of `Config` (after `:323`), `impl Default`,
  `diagnostics()`.
- `src/config/io.rs` *(upstream file — minimal wiring)*: `"gateway"` in
  `KNOWN_TOP_LEVEL_CONFIG_KEYS` (`:7-21`); `load_live_section(table,
  "gateway", …)` after `:360` + `diagnostics.extend(config.gateway.
  diagnostics())` after `:363`.
- `src/config.rs` *(upstream file — minimal wiring)*: `.chain(self.gateway.
  diagnostics())` at the end of `collect_diagnostics` (`:117-130`).
- `src/main.rs` *(upstream file — minimal wiring)*: a fully commented
  `[gateway]` block in `DEFAULT_CONFIG` after `[fleet]` (`:404-414`), plus
  `default_config_gateway_block_parses_without_diagnostics` and
  `default_config_gateway_block_is_commented_out` mirroring the fleet tests
  (`:910`, `:940`).
- `scripts/config_reference_check.py` *(upstream file — minimal wiring)*:
  `"gateway"` in `SKIPPED_SUBTREES` (`:36`) with the same fork comment as
  `fleet`.
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

**Landed** (merged; gate `EXIT=0` for both `ci` and `ci-no-default`, and
`just windows-lint` `EXIT=0`)

- **Exact API PR 4 and PR 8 call.** `src/gateway/auth.rs`: `TokenScope::
  {allows, as_str, token_file_name, all}`; `TokenDigest::{of, ct_eq,
  ct_eq_choice, to_hex, from_hex}`; `random_secret_hex()`; `unix_now()`;
  `Credential::{Bearer, Device{id}}` and `Principal::{bearer, device,
  allows}`; `TokenStore::{load_or_create, load, verify_bearer, rotate,
  digest}`; `DeviceRecord`, `DeviceStore::{load, devices, verify_cookie,
  insert, revoke_scope, revoke_id, touch, persist}`; `PairingCode`,
  `PairingError::{NotFound, Expired, Invalid, Io}`, `PairingStore::{new,
  dir, create, consume, sweep_expired}`; `AuthLimiter::{new, from_config,
  check, record_failure, tracked_peers}` and `MAX_TRACKED_PEERS`.
  `src/gateway/policy.rs`: `BindPolicy::check`, `OriginAllowlist::{for_bind,
  allows, is_empty, origins, requires_secure_cookies}`.
  `src/gateway/paths.rs`: `gateway_dir`, `pairings_dir`, `READ_TOKEN_FILE`,
  `CONTROL_TOKEN_FILE`, `DEVICES_FILE`, `PAIRINGS_DIR`, `RUNTIME_FILE`,
  `DIRECTORY_MODE`/`FILE_MODE` (unix), `create_private_dir`,
  `verify_private_dir`, `verify_private_file`, `write_private_file`,
  `read_private_file`.
- **`[gateway]` keys and defaults**, as shipped: `bind = "127.0.0.1:7788"`,
  `allowed_origins = []`, `public_url = ""`, `auth_failure_limit = 5`,
  `auth_failure_window_secs = 60`, `pairing_ttl_secs = 600` (valid range
  `30..=86400`). A bad value is a diagnostic naming the key, never a parse
  failure, and never fatal: `[gateway]` refuses nothing at load time, because
  a `--bind` flag can still make the same config valid. **`BindPolicy::check`
  at startup is the only refusal**, and PR 4 owns calling it.
- **Origins normalize the way browsers serialize them** (RFC 6454 §6.1), a
  correction to the plan's "exact on scheme/host/port": a scheme's default
  port folds to `None`, so a configured `HTTPS://Fleet.Example:443` matches
  the `https://fleet.example` a browser actually sends, and a bracketed IPv6
  literal is canonicalized (`[0:0:0:0:0:0:0:1]` → `[::1]`). Port `0`, a
  signed port (`+80`), non-ASCII hosts (punycode required), userinfo, paths,
  queries, fragments, `null` and any non-http(s) scheme are refused. The
  parser is `crate::config::parse_gateway_origin` returning `GatewayOrigin
  { scheme, host, port }` — it lives in `src/config/model.rs`, not in
  `policy.rs`, because `[gateway]` is unconditional while the gateway module
  is feature-gated, so one parser serves both builds.
- **The three gateway re-exports are feature-gated.** `pub use
  self::model::{parse_gateway_origin, GatewayConfig, GatewayOrigin}` in
  `src/config.rs` carries `#[cfg(feature = "gateway")]`; without it a
  `--no-default-features` build fails `-D warnings` on unused imports. The
  `GatewayConfig` *field* on `Config` stays unconditional.
- **`effective_*` accessors exist and PR 4 must use them.** The diagnostics
  promise "using 60"/"using 5"/"using 600"/"using 127.0.0.1:7788";
  `GatewayConfig::{effective_bind_addr, effective_auth_failure_limit,
  effective_auth_failure_window, effective_pairing_ttl_secs}` and
  `DEFAULT_GATEWAY_BIND_ADDR` deliver them, and `AuthLimiter::from_config`
  wraps the two limiter values, so a configured `auth_failure_window_secs =
  0` cannot silently disable rate limiting.
- **Security decisions the review pass added.** A token store whose two files
  hold the *same* secret is refused (a copy-pasted `read.token` must never
  grant control). Every pairing failure except expiry collapses to
  `NotFound`, and a wrong secret never deletes a valid code, so guessing an
  id learns nothing; only success and expiry delete a file. The failure
  limiter evicts unblocked peers before blocked ones, so a flood of fresh
  addresses cannot clear a victim's counter, and a success never resets a
  window (no oracle). The gateway directory is created `0700` with
  `DirBuilder::mode`, not chmod'ed after, and the private temp file name
  carries a per-process sequence so two writers to one file cannot delete
  each other's temp.
- **Deferred, with reasons.** `TokenStore` has **no** `loaded_at: FileStamp`
  (the plan's shape): nothing reloads a token file yet, so PR 8's
  `rotate-token` must reload the store explicitly after rotating and must
  call `DeviceStore::revoke_scope` itself — `rotate` only rewrites the file.
  `DeviceStore::touch` mutates in memory and does **not** persist (the
  gateway must not write a file per request); the caller batches
  `persist()`. Token plaintext is not zeroized after digesting (no `zeroize`
  dependency; out of the epic's dependency budget). Windows keeps no-op mode
  verification, as the plan allows.
- Every item in the three new modules carries a module-level
  `#![allow(dead_code)]` with a comment naming PR 4 and PR 8; **PR 4 must
  narrow or delete those allows** as it consumes the API. `[gateway]` is in
  `SKIPPED_SUBTREES` of `scripts/config_reference_check.py`, so PR 10's
  `docs/fork/gateway.md` is the only reference for these keys.

### PR 3 — feat(gateway): passive async fleet runtime folding connector events into shared state · deps: 1

**Goal:** the gateway's fleet half: start the E1 connector from config as a
**passive daemon consumer** (`surface_active: false`, noninteractive ssh
bridges), own its event receiver in a tokio task, fold every
`FleetEvent::Host` into a shared `FleetState`, broadcast each `FleetChange`
pre-serialized to any number of subscribers, serve `FleetStatusReport` on
demand, and shut down cleanly — with no HTTP yet. The two fleet-side
switches also fix the E1 caveats for `herdr fleet status`.

**Files**

- `src/gateway/fleet.rs` (new): `FleetRuntime`, `FleetHandle`,
  `ChangeStream`.
- `src/gateway/mod.rs`: `mod fleet;` (+ narrow allow naming PR 4).
- `src/fleet/handshake.rs` (fork file): `HandshakeParams.surface_active:
  bool`; `read_only` sets `false`, `for_client` sets `true`;
  `endpoint_handshake` writes `params.surface_active` instead of the literal
  at `:140-144` (delete the "E3 may flip it" comment).
- `src/fleet/connector.rs` (fork file): `FleetConnectorOptions.
  ssh_noninteractive: bool` (default `false`), `for_daemon(&Config)`;
  refresh the `INACTIVE_SURFACE` doc block (`:49-71`) — the foreground
  caveat now applies only to servers older than #3670 or to `for_client`.
- `src/fleet/transport/mod.rs` / `ssh.rs` (fork files): `transport_for`
  passes `options.ssh_noninteractive`; `SshTransport::new(host, target,
  session, manage_ssh_config, noninteractive)` stores it and hands it to
  `SshStdioBridge::start_with` at `ssh.rs:177` (replacing the literal
  `false`); `describe()` unchanged.
- `src/fleet/oneshot.rs`: no change (`for_config` stays interactive; the
  `read_only` flip reaches it automatically).
- `docs/fork/fleet-core.md`: the "What the fleet never does" foreground
  paragraph (`:544-557`) and "The ssh child inherits stderr" (`:507-514`)
  rewritten to the new facts; "Driving the connector (E3)" (`:617-632`)
  names `for_daemon` and `take_events`.

**Shapes/approach**

```rust
// src/fleet/connector.rs (additive)
pub struct FleetConnectorOptions { /* … */ pub ssh_noninteractive: bool }
impl FleetConnectorOptions {
    /// A daemon with no tty: ssh bridges run BatchMode with stderr discarded (attach.rs `bridge_connection`).
    pub fn for_daemon(config: &Config) -> Self { Self { ssh_noninteractive: true, ..Self::for_config(config) } }
}

// src/gateway/fleet.rs
pub struct FleetRuntime { task: JoinHandle<()>, stop: watch::Sender<bool>, connector: FleetConnector, handle: FleetHandle }
#[derive(Clone)]
pub struct FleetHandle {
    state: Arc<Mutex<FleetState>>,                 // std Mutex; never held across .await
    changes: broadcast::Sender<Arc<str>>,          // each FleetChange serialized once
    client_version: Arc<str>,
}
impl FleetRuntime {
    /// resolve_hosts(&config.fleet) → FleetState::new(specs) (active host cleared, as `herdr fleet status` reports)
    /// → FleetConnector::start(specs, FleetConnectorOptions::for_daemon(config)) → take_events() → spawn the fold task.
    pub fn start(config: &Config) -> Result<Self, Vec<String>>      // Err = [fleet] diagnostics (exit 1 in PR 4)
    pub fn handle(&self) -> FleetHandle
    /// Signal stop, await the task (which drops the receiver), then `connector.shutdown()` in spawn_blocking.
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

The fold task owns `events: mpsc::Receiver<FleetEvent>` from
`take_events()` and loops `select! { _ = stop.changed() => break, ev =
events.recv() => match ev { Some(FleetEvent::Host{host, event}) => { let
changes = state.lock().apply(&host, event); for c in changes { let _ =
changes_tx.send(serde_json::to_string(&c)?.into()); } } Some(_) => {}
/* surfaces never arrive (no active host); notifications/endpoint responses
are ignored in E3 */ None => break } }` and drops the receiver on exit.
`shutdown` sends the stop signal, awaits the task, then runs
`connector.shutdown()` inside `spawn_blocking` (bounded 2 s) so a request in
flight never blocks the runtime. `set_active_host(None)` once at
construction so `active_host` is `null` exactly as `herdr fleet status
--json` prints it. Nothing here imports axum.

**Tests** (in-crate; `#[cfg(all(test, unix))]` for the socket ones)

- `handshake.rs`: `read_only(..).surface_active == false`,
  `for_client(..).surface_active == true`; against a fake listener the hello
  JSON carries `"surface_active":false` for `read_only` (decode the
  `EndpointControl` data as `EndpointClientHello`).
- `connector.rs`/`ssh.rs`: `for_daemon` sets `ssh_noninteractive`; with the
  existing `fake_ssh` shim, a transport built with `noninteractive: true`
  records `-o BatchMode=yes` in its trace and one built with `false` does
  not; `FleetConnectorOptions::default().ssh_noninteractive == false`.
- Pure fan-out: with a `FleetState::test_new()` behind a `FleetHandle`
  built by a test constructor, applying a synthetic `HostEvent::Connected`
  + `Snapshot` yields the serialized `host_connection`/`snapshot`/
  `agent_added` strings on two subscribers, identical `Arc` pointers.
- `subscribe_with_report` ordering: a change applied concurrently is either
  in the report or in the stream, never in neither (loop 200 iterations
  with a spawned applier).
- Lag: a subscriber that stops reading past the capacity gets `Lagged`
  then resumes.
- Against real sockets, the `connector.rs` `test_support::FakeHost` idiom:
  `FleetRuntime::start` reports the host `connected`, the subscriber
  receives `host_connection` + `snapshot`, the fake host's received hello
  has `surface_active == false`; killing the fake yields `unavailable`;
  `shutdown().await` completes within 3 s and the fake saw the half-close.
- `start` on a `[fleet]` naming `local` while `include_local = true` →
  `Err(diagnostics)`.

**Real-server validation**

No CLI surface yet; prove passivity and the unchanged status path against
the lab (the pane geometry lever is `[server] headless_cols/rows`, applied
live by `server reload-config`):

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
cat >> "$XDG_CONFIG_HOME/herdr-dev/config.toml" <<'EOF'
[server]
headless_cols = 100
headless_rows = 30
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
$H --session lab-1 server reload-config; sleep 1
$H --session lab-1 pane run "$HERDR_FLEET_LAB_PANE_1" 'stty size'; sleep 1
$H --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | grep -c '^30 100$'      # 1 — the lab pane is 100x30
$H fleet status --watch --json --timeout-ms 3000 > /tmp/e3-pr3-watch.ndjson & W=$!; sleep 2      # a passive reader is attached
$H --session lab-1 pane run "$HERDR_FLEET_LAB_PANE_1" 'stty size'; sleep 1
$H --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | grep -c '^30 100$'      # 2 — still 100x30 (a 120x40 foreground client would print "40 120")
kill -INT $W; wait $W
head -1 /tmp/e3-pr3-watch.ndjson | python3 -c 'import json,sys;r=json.load(sys.stdin);print(r["schema"],[(h["id"],h["connection"]["state"]) for h in r["hosts"]])'
# → herdr.fleet.status.v1 [('lab-1','connected'),('lab-2','connected')]  (unchanged by this PR)
# ssh: BatchMode is visible in the ssh child's argv while a daemon-mode consumer is connected (skip with a note if ssh-lab.sh up exits 3)
bash scripts/fork/ssh-lab.sh up && eval "$(bash scripts/fork/ssh-lab.sh env)"
printf '[[fleet.hosts]]\nname = "lab-ssh"\nkind = "ssh"\ntarget = "herdr-ssh-lab"\nsession = "lab-1"\n' >> "$XDG_CONFIG_HOME/herdr-dev/config.toml"
HOME=$HERDR_SSH_LAB_HOME $H fleet status --json | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(h["id"],h["connection"]["state"]) for h in r["hosts"]])'   # lab-ssh connected (interactive path, unchanged)
bash scripts/fork/gate.sh <worktree> "test-one gateway::fleet"           # EXIT=0 (the daemon path is covered by the fake_ssh trace test)
bash scripts/fork/ssh-lab.sh down; bash scripts/fork/fleet-lab.sh down
```

Evidence: the two `grep -c` lines (`1`, then `2`), the unchanged status
line, the ssh host `connected`, and the `test-one` `EXIT=0`. Record in the
PR body that on `master` before this PR the second grep prints `1` and the
pane shows `40 120` (the E1 caveat reproduced, then fixed).

**Downstream**

- `FleetHandle` is the only way handlers reach fleet data; PR 4 serves
  `report()`, PR 5 uses `subscribe_with_report`, PR 6 uses
  `host_connection`/`host_spec` to fail fast and to build transports, PR 9
  swaps `resolve_hosts(&config.fleet)` for `fleet::hosts_source::
  hosts_for_config(config)` in `start`, E7 adds `request(host, …)` here
  through `FleetRuntime.connector.send`.
- The broadcast payload is the `FleetChange` JSON verbatim; PR 5 wraps
  nothing around it. New change kinds arrive automatically; readers skip
  unknown `kind`s.
- `FleetConnectorOptions::for_daemon` is the gateway's only options
  constructor (PR 6's `HostTransports` reuses the same value so terminal
  bridges are noninteractive too). `read_only` is passive from now on; a
  future console consumer must use `for_client`.
- The passivity guarantee is server-side (`surface_active`); the residual
  for a pre-#3670 server is documented in `fleet-core.md`, not worked
  around.

**Landed** (merged; gate `EXIT=0` for both `ci` and `ci-no-default`)

- **The shapes are as planned, with three additions PRs 4/5/6/9 must know:**
  - `ChangeStream` is **not** a newtype over the broadcast receiver. It carries
    the runtime's stop latch too (`{ changes: broadcast::Receiver<Arc<str>>,
    stop: watch::Receiver<bool>, stopped: bool }`), because "the stream ends
    when the last sender drops" is false in the gateway: the router holds a
    `FleetHandle` — and with it a broadcast sender — for the life of the
    process, so a `/api/events` task would wait forever on a stopped fleet.
    `FleetRuntime::shutdown` latches the watch with `send_replace` (plain
    `send` leaves the value untouched when it sees no receivers) *before*
    dropping its own handle. `next()` drains what is already queued, then
    returns `None` for good.
  - `FleetHandle::subscribe()` (no report) is **private**. PR 5 must call
    `subscribe_with_report`: a subscriber with no starting report cannot tell
    a delta it missed from one that never happened.
  - `FleetRuntime::start` **must be called from inside a tokio runtime**
    (it spawns the fold task). PR 4 builds it inside `rt.block_on`, not before.
- **Gap-freeness is enforced by the lock, not by ordering luck.** The fold task
  applies *and publishes* under the same `state` lock that
  `subscribe_with_report` holds while it subscribes and snapshots, so a change
  is in the report **xor** on the stream — never both, never neither. A
  200-iteration concurrent test pins it. `broadcast::Sender::send` never
  blocks, so this holds no lock across an `.await`.
- **`for_daemon` needs a feature-scoped allow.** Its only production caller is
  the gateway, which `--no-default-features` compiles out, so it carries
  `#[cfg_attr(not(feature = "gateway"), allow(dead_code))]` — scoped to that
  build rather than unconditional, so a default build that stops calling it
  still fails the lint. `src/gateway/mod.rs`'s `mod fleet;` carries the
  planned `#[allow(dead_code)]` naming PR 4; **PR 4 removes it.**
- **The ssh switch reaches the bridged children, not the discovery probes.**
  `SshTransport::ssh_session()` builds `RemoteSsh::new(...)`, which is
  hard-coded interactive, so `discover_remote_herdr`'s `uname -s` / binary
  probe still runs without `BatchMode` and without a timeout even under
  `for_daemon`. Nothing is painted on a daemon's terminal (those probes pipe
  both stdout and stderr), and a hung probe blocks only that host's supervisor
  thread — which this plan's cross-cutting constraints already accept. Closing
  it needs a noninteractive `RemoteSsh` constructor in `src/remote/attach.rs`,
  and *Sequencing hazards* requires `src/remote/**` to have an empty diff for
  the whole epic, so it is **documented** in `for_daemon` and in
  `docs/fork/fleet-core.md`, not worked around.
- **`transport_for` grew a `#[cfg(test)]` trait method**,
  `HostTransport::ssh_noninteractive_for_test() -> Option<bool>`, so the wiring
  test can assert the daemon flag reached the transport without an `Any`
  downcast in production code. PR 6's `new_scoped`/`transport_for_scoped` must
  thread `ssh_noninteractive` the same way.
- **A console's hello is `surface_active: true` for *every* host**, not only
  the active one: `HandshakeParams::for_client` builds one hello the connector
  reuses. A console's inactive hosts are therefore protected by
  `INACTIVE_SURFACE`, not by the flag. Unchanged from pre-PR behaviour (every
  hello sent `true`), and `for_client` has no production caller since the E2
  TUI was retired — but it is the trap a future console author would hit.
- **Residual accepted:** if the fold task panics, deltas stop silently while
  `report()` keeps working. Guarding it would mean `catch_unwind` around the
  fold; PR 4's own supervision is the better place to notice a dead runtime.
- **Validation note for later PRs:** `scripts/fork/fleet-lab.sh` defaults to
  the shared root `/tmp/herdr-fleet-lab`. Two agents validating at once collide
  there — one agent's `down` deletes the root out from under the other's
  still-running servers. Set `HERDR_FLEET_LAB_ROOT` per task when a wave runs
  two PRs with live validation.
- **The lab pane is not a shell.** `fleet-lab.sh`'s pane `execs` a sleep loop,
  so the plan's `pane run … 'stty size'` on `$HERDR_FLEET_LAB_PANE_N` is echoed
  and never executed, and `[server] headless_cols/rows` + `server
  reload-config` does **not** resize an existing pane (only panes created
  afterwards). Split a fresh pane (`pane split <pane> --direction down`) and
  measure that one; the A/B that matters is whether the number *changes* when a
  fleet consumer attaches.

### PR 4 — feat(gateway): herdr gateway serves health, fleet report and embedded assets over http · deps: 2, 3

> **From PR 3 (landed):** `FleetRuntime::start` spawns a tokio task, so build
> it **inside** `rt.block_on`, never before. Delete `src/gateway/mod.rs`'s
> `#[allow(dead_code)]` on `mod fleet;` in this PR (PR 3 named it for you), and
> `GATEWAY_STAGING_NOTE` with its test, as PR 1 recorded. `FleetRuntime::start`
> returns `Err(Vec<String>)` for `[fleet]` diagnostics — print them and exit 1.
> Keep the `FleetHandle` in the router state; it is `Clone` and every method on
> it is sync and bounded. Call `FleetRuntime::shutdown().await` on SIGTERM.

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
- `src/gateway/mod.rs`: modules + dispatch of the bare command to `run`;
  **delete `GATEWAY_STAGING_NOTE`** (PR 1), its `gateway_help()` line, the
  `.after_help(…)` in `src/cli/spec.rs`'s `gateway_command()` and the
  `the_staging_note_matches_the_dispatch` test — that test fails as soon as
  `--bind` stops exiting 2, which is this PR.
- `web/dist/index.html` (new, committed, < 4 KiB): Herdr Fleet placeholder
  (title, the E4 note, a `<script>` that fetches `/api/gateway` and prints
  `paired as <scope>` or `not paired — run: herdr gateway pair`).
- `build.rs` *(upstream file — minimal wiring)*: `println!("cargo:rerun-if-
  changed=web/dist");` next to the existing `rerun-if-changed` lines.
- `tests/support/gateway.rs` (new) + one `pub mod gateway;` line in
  `tests/support/mod.rs` *(upstream file — minimal wiring)* after `:11`:
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
// 1. --config → std::env::set_var(config::CONFIG_PATH_ENV_VAR, path) before anything else (decision (m))
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

- In-crate: bind the router on `127.0.0.1:0` inside a `#[tokio::test]` and
  drive it with `std::net::TcpStream` GETs (no `tower` dev-dependency) with
  a `FleetHandle` built from `FleetState::test_new()`: `/health` 200
  without auth; `/api/fleet` 401 without, 200 with the read token, 200 with
  control, 403 with a foreign origin, 429 after 5 bad tokens from one peer
  and still 401 (not 429) from another peer; `/` serves `index.html` with
  `text/html`; `/nope.png` 404; `/settings` (no extension) → `index.html`;
  a query-string token is ignored (401).
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
# passive: the gateway holds both hosts, yet the lab pane keeps its own geometry
$H --session lab-1 pane run "$HERDR_FLEET_LAB_PANE_1" 'stty size'; sleep 1
$H --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | tail -3     # 40 120 (the lab's own headless default; unchanged with the gateway attached)
# host failure is local: stop lab-2, the report shows it unavailable and lab-1 still connected
$H --session lab-2 session stop lab-2; sleep 2
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7788/api/fleet | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(h["id"],h["connection"]["state"]) for h in r["hosts"]])'
# → [('lab-1','connected'),('lab-2','unavailable')]
kill -TERM $GW; wait $GW; echo "exit=$?"; ls "$XDG_CONFIG_HOME/herdr-dev/gateway/gateway.json" 2>&1   # exit=0; No such file
$H gateway --bind 0.0.0.0:7788; echo "exit=$?"                                 # refusing to bind 0.0.0.0:7788: [gateway] allowed_origins is empty …; exit=1
bash scripts/fork/fleet-lab.sh down
```

Evidence: the status codes in order, the two `python3` lines, the `ls -l`
modes, the `stty size` line, the exit lines. Paste no token.

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

> **From PR 3 (landed):** use `FleetHandle::subscribe_with_report()` — the
> report-less `subscribe()` is private, because a subscriber with no starting
> report cannot tell a delta it missed from one that never happened. Drive the
> socket with `ChangeStream::next() -> Option<ChangeItem>`: `Change(Arc<str>)`
> is the `FleetChange` JSON verbatim (newline-free, wrap nothing around it),
> `Lagged` means the subscriber overflowed the 256-deep channel and should be
> sent a `Resync` rather than a delta, and `None` means the runtime stopped —
> close the socket, do not treat it as an error. `next()` is cancel-safe, so it
> may sit in a `select!` arm.

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

- In-crate: drive `run_events` through a small `EventSink` trait (the axum
  `WebSocket` and a `Vec<String>` test sink both implement it) so the
  ordering logic is tested without sockets: hello → fleet → change order;
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

> **From PR 3 (landed):** `FleetHandle::host_connection(&HostId)` and
> `host_spec(&HostId)` are the fail-fast pair — `None` means the host is not
> configured at all (a `terminal.error`, never a 5xx). Reuse
> `FleetConnectorOptions::for_daemon(config)` so terminal bridges are
> noninteractive too. When threading the new `new_scoped` /
> `transport_for_scoped`, carry `options.ssh_noninteractive` exactly as
> `transport_for` does, and extend the `#[cfg(test)]`
> `HostTransport::ssh_noninteractive_for_test()` wiring assertion to the scoped
> constructor — a dropped argument there hands a gateway an interactive ssh
> child with no other symptom.

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
- `src/fleet/transport/ssh.rs` (fork file): `SshTransport::new_scoped(host,
  target, session, manage_ssh_config, noninteractive, scope: &str)` — `new`
  becomes `new_scoped(…, "")`-with-host semantics, i.e. the existing socket
  names are byte-identical.
- `src/fleet/transport/mod.rs`: `transport_for_scoped(spec, options, scope)`;
  `transport_for` delegates with the E1 scope.
- `src/client/terminal_sessions.rs` *(upstream file — minimal wiring)*:
  `pub(super)` → `pub(crate)` on `terminal_control_command_from_json`
  (`:208`, one word; the JSON command vocabulary must stay byte-identical
  to the CLI's).
- `src/gateway/server.rs`: `.merge(terminal::routes())`; `features` gains
  `"terminal"`.
- `tests/fork_gateway.rs`: the observe test.

**Shapes/approach**

```rust
// transports.rs
pub struct HostTransports { specs: HashMap<HostId, HostSpec>, options: FleetConnectorOptions /* for_daemon */,
                            open: Mutex<HashMap<HostId, Arc<Mutex<Box<dyn HostTransport>>>>> }
impl HostTransports {
    pub fn connect(&self, host: &HostId) -> io::Result<LocalStream>   // blocking; run in spawn_blocking
    pub fn shutdown(&self)                                            // drop transports (after every session stream is closed)
}
// scope passed to transport_for_scoped is "gateway" → forward socket
//   /tmp/herdr-remote-<pid>-gateway-<host>-<target>-<session>.sock (distinct from the connector's);
//   the bridge is noninteractive (options.ssh_noninteractive), so no ssh output ever reaches the daemon's stderr

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
observer's client-local viewport; if the server ignores it, the gateway
answers `terminal.error {code:"unsupported"}` and the doc says so. An
observer's `terminal.resize` is client-local by server design
(`headless.rs:2205-2222`): every observer gets its own viewport size.

**Tests**

- Pure: `encode_frame`/`decode_frame_header` round-trip incl. a 2 MiB
  frame; `TerminalOpen` parsing (missing cols → error; cols 0 → error;
  unknown mode → error); the observe-mode gate refuses `terminal.input`
  and admits resize/scroll/release (table test on the pure `SessionPolicy
  { mode, scope }::admit(&ClientMessage) -> Result<(), TerminalErrorCode>`).
- `transport_for_scoped` with an ssh spec derives a forward socket path
  that differs from `transport_for`'s and both differ from `--remote`'s
  unscoped name (unit test in `transport/ssh.rs` next to E1's, using the
  existing `fake_ssh` shim for a full connect if cheap; the shim trace
  shows `BatchMode=yes` for the gateway's options).
- Socket-level (`#[cfg(all(test, unix))]`): a fake terminal server
  (`bind_private_local_listener` under a throwaway `XDG_CONFIG_HOME`) that
  answers `TerminalHello` with `Welcome{TerminalAnsi}`, expects
  `ObserveTerminal {target}` and emits two `Terminal` frames then
  `ServerShutdown`: `TerminalSession` yields the frames in order and ends;
  `Detach` from the writer arrives at the fake server; a fake that answers
  `Welcome{error}` yields an `io::Error` naming it.
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
HOME=$HERDR_SSH_LAB_HOME $H gateway --bind 127.0.0.1:7788 2>/tmp/e3-pr6-gw.err & GW=$!; sleep 5
ls /tmp/herdr-remote-$GW-* ; ls /tmp/herdr-remote-$GW-gateway-* 2>/dev/null   # connector socket(s) and, only after a terminal opens, the gateway-scoped one
python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-ssh/$HERDR_FLEET_LAB_PANE_1" -H "Authorization: Bearer $TOKEN" \
  --send '{"type":"terminal.open","mode":"observe","cols":80,"rows":24}' --max-messages 2 --binary len          # ready + full frame over ssh
ps -o args= -p "$(pgrep -f 'ssh .*-T herdr-ssh-lab' | head -1)" | grep -c 'BatchMode=yes'                        # 1 — the bridge child runs noninteractive
kill -TERM $GW; wait $GW; echo "exit=$?"; ls /tmp/herdr-remote-$GW-* 2>&1        # exit=0 within 5 s; no sockets left
grep -c '' /tmp/e3-pr6-gw.err                                                    # 0 lines from ssh on the daemon's stderr (tracing goes elsewhere)
bash scripts/fork/ssh-lab.sh down; bash scripts/fork/fleet-lab.sh down
```

Evidence: the `terminal.ready` line, the decoded header line with `marker`,
the `forbidden` line and the `grep -c 0`, the `full=0` follow-up frame, the
ssh forward-socket names before/after, the `BatchMode` grep, exit 0 with no
sockets left, the empty stderr capture.

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
takeover }` instead of `ObserveTerminal` — server-side this is the
`TerminalAttach` mode, the single-owner slot. In control mode every CLI
command is admitted: `terminal.input {text}` → `Input { data:
text.into_bytes() }`, `{bytes: base64}` → decoded (≤ 64 KiB; the CLI's
decoder), `terminal.resize` → `Resize` (real PTY resize; `cell_*_px` 0),
`terminal.scroll`, `terminal.release` → `Detach` + `terminal.closed
{reason:"released"}` + close 1000. A `ServerShutdown` whose reason contains
"already has an attached client" becomes `terminal.error {code:"busy",
message}` so E4 can offer takeover; the server disconnects that connection
(`headless.rs:1826-1838`), so the gateway closes the WebSocket with 1000
after the error. A controller evicted by another's takeover receives the
server's shutdown as `terminal.closed {reason:"taken_over"}` (the gateway
classifies the reason text; anything else stays `terminal.closed
{reason:<server text>}`). Nothing in E3 confirms destructive actions (there
are none: input is exactly what the console sends when you type). Logging
on open: `info` with `host`/`pane`/`mode`/`credential kind`; input bytes
are never logged.

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
wait $FIRST                                                                                                  # its last line: terminal.closed reason=taken_over
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
- `busy`/`takeover`/`taken_over` semantics are the server's; E4 shows
  "take over" on `busy` and a banner on `taken_over`.

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

### PR 9 — feat(fleet): opt-in hosts from saved machine profiles · deps: 3

> **From PR 3 (landed):** the call to swap is in `FleetRuntime::start`
> (`src/gateway/fleet.rs`), which today reads
> `resolve_hosts(&config.fleet)` and passes the same `Vec<HostSpec>` to both
> `FleetState::new` and `FleetConnector::start` through the private
> `FleetRuntime::over(specs, connector)`. Keep those two lists identical — a
> spec in one and not the other makes an event address a host the state does
> not have. `src/fleet/oneshot.rs`'s `FleetSession::start` needs the same swap.

**Goal:** decision (r): with `[fleet] include_machines = true`, every
enabled machine saved by `herdr machine add` (upstream #3670's
`endpoints.json`) becomes an ssh fleet host next to `[[fleet.hosts]]`, for
both `herdr fleet status` and the gateway, through one shared resolver —
without touching E1's keys or `resolve_hosts`.

**Files**

- `src/fleet/machines.rs` (new, pure — added to `PURE_MODULES` in
  `src/fleet/mod.rs`): `MachineProfile`, `machine_host_id(label)`,
  `machine_host_specs(profiles, existing) -> Result<Vec<HostSpec>,
  Vec<String>>`.
- `src/fleet/hosts_source.rs` (new, the only fleet module that names
  `crate::client`): `hosts_for_config(&Config) -> Result<Vec<HostSpec>,
  Vec<String>>`.
- `src/fleet/mod.rs` (fork file): two `pub mod` lines; `machines` in
  `PURE_MODULES`.
- `src/fleet/oneshot.rs` (fork file): `FleetSession::start` calls
  `hosts_for_config(config)` instead of `resolve_hosts(&config.fleet)`.
- `src/gateway/fleet.rs`: the same one-line swap in `FleetRuntime::start`.
- `src/config/model.rs` *(upstream file — minimal wiring)*: `FleetConfig.
  include_machines: bool` (default `false`), no new diagnostic.
- `src/main.rs` *(upstream file — minimal wiring)*: one commented line
  `# include_machines = false` with a one-line comment in the `[fleet]`
  block of `DEFAULT_CONFIG` (`:404-414`); the two fleet block tests still
  pass unchanged.
- `docs/fork/fleet-core.md`: the `[fleet]` key table (`include_machines`),
  the quoted `DEFAULT_CONFIG` block, a short "Saved machines as hosts"
  subsection (id derivation, diagnostics, `herdr machine rename` as the
  fix), and the ssh-lab recipe below.
- `tests/cli/fleet.rs`: one test writing `endpoints.json` under a throwaway
  `XDG_STATE_HOME`.

**Shapes/approach**

```rust
// src/fleet/machines.rs (pure)
pub struct MachineProfile { pub label: String, pub target: String, pub session: String, pub enabled: bool }
/// The label when `HostId::new` accepts it; otherwise lowercased with every run of other chars folded to '-'.
pub fn machine_host_id(label: &str) -> Result<HostId, String>
/// Appends one `HostKind::Ssh { target, session: Some(session) }` spec per profile, in catalog order.
/// Diagnostics (all-or-nothing, like `resolve_hosts`): an id that still fails validation, an id equal to
/// `HostId::LOCAL`, an id colliding with `existing` or with another machine — each names the machine label and
/// says `rename it with: herdr machine rename <id> --label <valid-name>`.
pub fn machine_host_specs(profiles: &[MachineProfile], existing: &[HostSpec]) -> Result<Vec<HostSpec>, Vec<String>>

// src/fleet/hosts_source.rs (adapter)
pub fn hosts_for_config(config: &Config) -> Result<Vec<HostSpec>, Vec<String>> {
    let mut specs = resolve_hosts(&config.fleet)?;
    if config.fleet.include_machines {
        let profiles = crate::client::endpoint::EndpointCatalog::load_profiles()   // Err(String) → one diagnostic
            .map_err(|e| vec![format!("saved machines unavailable: {e}")])?;
        let profiles: Vec<MachineProfile> = profiles.iter().map(|p| MachineProfile { label: p.label.clone(),
            target: p.target.clone(), session: p.session.clone(), enabled: p.enabled }).collect();
        specs.extend(machine_host_specs(&profiles, &specs)?);
    }
    Ok(specs)
}
```

A missing `endpoints.json` is an empty list, not an error (upstream's
`load_from_path` already returns an empty catalog for an absent file —
verify and pin with a test). Disabled machines appear as `enabled = false`
hosts, exactly like `[[fleet.hosts]] enabled = false` (reported
`unavailable: host disabled`). The report's `HostReport.kind` stays `"ssh"`;
no new report field, so `/api/fleet` and `herdr.fleet.status.v1` are
unchanged.

**Tests**

- Pure (`machines.rs`): `machine_host_id("workbox") == workbox`;
  `"My Laptop (home)"` → `my-laptop-home`; `"///"` → diagnostic; `"local"`
  → diagnostic naming the reserved id; two machines folding to the same id
  → one diagnostic naming both; a collision with an existing
  `[[fleet.hosts]]` name → diagnostic; order preserved; `enabled` carried.
  `assert_invariants_for_test` on a `FleetState::new` built from the mixed
  list.
- `hosts_source.rs` (unix, throwaway `XDG_STATE_HOME`): `include_machines
  = false` ignores an existing catalog; `true` with no file → only
  `[[fleet.hosts]]`; `true` with two profiles → four specs; a corrupt file →
  `Err` with one diagnostic.
- `src/config/model.rs`: default `false`; parses `true`; `DEFAULT_CONFIG`
  block tests unchanged.
- `tests/cli/fleet.rs`: `fleet_status_includes_saved_machines_when_enabled`
  — write `endpoints.json` (`{"version":1,"selected_profile":null,"ssh":
  [{"id":"<32 hex>","label":"lab-ssh","target":"127.0.0.1","session":
  "lab-1","enabled":true}]}`) under `XDG_STATE_HOME`, `[fleet]
  include_machines = true`, `herdr fleet status --json` lists `lab-ssh` as
  `kind == "ssh"` (state `unavailable` is fine — no sshd in this test).

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
bash scripts/fork/ssh-lab.sh up && eval "$(bash scripts/fork/ssh-lab.sh env)"          # exit 3 → record and run the unavailable-host variant below
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
export XDG_STATE_HOME="$HERDR_FLEET_LAB_ROOT/state"; mkdir -p "$XDG_STATE_HOME/herdr-dev/client"
ID=$(python3 -c 'import secrets;print(secrets.token_hex(16))')
printf '{"version":1,"selected_profile":null,"ssh":[{"id":"%s","label":"Lab SSH","target":"herdr-ssh-lab","session":"lab-1","enabled":true}]}\n' "$ID" > "$XDG_STATE_HOME/herdr-dev/client/endpoints.json"
chmod 600 "$XDG_STATE_HOME/herdr-dev/client/endpoints.json"
cat >> "$XDG_CONFIG_HOME/herdr-dev/config.toml" <<'EOF'
[fleet]
include_local = false
include_machines = true
[[fleet.hosts]]
name = "lab-2"
kind = "local"
session = "lab-2"
EOF
HOME=$HERDR_SSH_LAB_HOME $H machine list --json | python3 -c 'import json,sys;print([(m["label"],m["target"]) for m in json.load(sys.stdin)])'   # [('Lab SSH','herdr-ssh-lab')] — upstream reads the same file
HOME=$HERDR_SSH_LAB_HOME $H fleet status --json | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(h["id"],h["kind"],h["target"],h["connection"]["state"],[w["label"] for w in h["workspaces"]]) for h in r["hosts"]])'
# → [('lab-2','local',None,'connected',['lab-2']),('lab-ssh','ssh','herdr-ssh-lab','connected',['lab-1'])]   (id derived from "Lab SSH")
sed -i 's/"label":"Lab SSH"/"label":"lab-2"/' "$XDG_STATE_HOME/herdr-dev/client/endpoints.json"
$H fleet status --json; echo "exit=$?"     # one diagnostic naming machine "lab-2" colliding with [[fleet.hosts]] lab-2 and the rename command; exit=1
sed -i 's/include_machines = true/include_machines = false/' "$XDG_CONFIG_HOME/herdr-dev/config.toml"
$H fleet status --json | python3 -c 'import json,sys;print([h["id"] for h in json.load(sys.stdin)["hosts"]])'   # ['lab-2'] — opt-in really is off by default
bash scripts/fork/ssh-lab.sh down; bash scripts/fork/fleet-lab.sh down
ls ~/.local/state/herdr*/client/endpoints.json 2>&1 | grep -c 'No such file\|herdr-fork-never-wrote-this'   # the user's real catalog was never created or touched (compare mtime if it exists)
```

Evidence: the `machine list` line, the two-host status line with the
derived id, the collision diagnostic + `exit=1`, the opt-out line. If the
gateway is already merged, also `GET /api/fleet` listing `lab-ssh` (the
E2E validation covers it otherwise).

**Downstream**

- `hosts_for_config` is the one resolver for every fleet consumer; E5's
  tailnet machines arrive through `herdr machine add <host>.<tailnet>.ts.net`
  with no fleet change. E7 may key per-machine actions on the derived id.
- The id derivation is frozen (label first, slug fallback); a future
  `[[fleet.machines]] id = …` override is the escape hatch if a user needs a
  stable id for a label they will not rename — not in E3.
- The fork's only dependency on upstream's catalog is
  `EndpointCatalog::load_profiles()` + five `SavedSshEndpoint` fields; a
  sync that renames them breaks the build in `hosts_source.rs`, nowhere
  else.

### PR 10 — docs: gateway guide, systemd unit, adr e3 review, roadmap drift · deps: 5, 7, 8, 9

**Goal:** the user-facing reference for everything E3 shipped, the example
`systemd --user` unit, the ADR's E3 review, and factual drift fixed in the
roadmap's E3 section — plus the last janitoring (dead-code allows left for
"the first later PR").

**Files**

- `docs/fork/gateway.md` (new): what the gateway is, `[gateway]` keys
  (every key, type, default, diagnostics), running it (`herdr gateway`,
  `--bind`, `--config`, loopback vs non-loopback, the passive-reader
  guarantee and the pre-#3670 residual, `include_machines`), tokens and
  files under `<config>/gateway/` (modes, rotation, revocation), pairing
  (`pair`, QR, cookie, TTL, one-time), `status`, the HTTP contract
  (`/health`, `/api/gateway`, `/api/fleet`, `/pair`, static assets), the
  WebSocket contracts (`herdr.fleet.events.v1` message order,
  `herdr.fleet.terminal.v1` including the binary header table and every
  `terminal.*` message with its fields and error codes), scopes and what
  `read` can never do, rate limiting, origin policy, shutdown behaviour,
  ssh in daemon mode (BatchMode, discarded stderr, how failures surface),
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
  loopback, the passive hello, noninteractive bridges, the gateway-scoped
  forward socket, backpressure through the server's render lane);
  `0002-adopt-upstream-multi-machine-client.md`: one line closing the
  "E3 decides" item with decision (r).
- `docs/fork/ROADMAP.md`: factual drift in the **E3 section only** (e.g.
  binary frames, `terminal.open`, `/api/gateway`, tokens on loopback,
  `include_machines`); the status row stays `implement-epic`'s.
- `src/gateway/**`: remove any `#[allow(dead_code)]` whose named PR has
  landed; `src/fleet/hosts.rs` `HostId::as_str` allow (E1 PR 7 deferred it
  to "the first later PR touching the file") if still present.

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

- `src/fleet/connector.rs` — `FleetConnector::{start, take_events,
  shutdown, send}`, `FleetConnectorOptions::{for_config, for_daemon}`,
  `FleetEvent::Host`, `INACTIVE_SURFACE` (never call `set_active` or
  `set_active_geometry`); `test_support::FakeHost` for socket-level tests.
  `src/fleet/handshake.rs` — `HandshakeParams::read_only` (passive) and
  `endpoint_handshake`. `src/fleet/oneshot.rs` — the config → specs →
  connector → state recipe (`FleetSession::start`).
- `src/fleet/state.rs` — `FleetState::{new, apply, set_active_host,
  merged_agents}`, `FleetChange` (tagged `kind`), `HostConnection`,
  `test_new`/`assert_invariants_for_test`. `src/fleet/report.rs` —
  `FleetStatusReport::from_state`, `FLEET_STATUS_SCHEMA`. `src/fleet/hosts.rs`
  — `HostId`, `HostSpec`, `resolve_hosts`. `src/fleet/refs.rs` —
  `FleetPaneRef`, `is_valid_resource_id`. `src/fleet/mod.rs` — the purity
  guard (`PURE_MODULES`, `FORBIDDEN`).
- `src/fleet/transport/{mod,local,ssh}.rs` — `HostTransport`,
  `transport_for`, `LocalTransport`, `SshTransport::new` (+ PR 3's
  `noninteractive`, PR 6's `new_scoped`), `fake_ssh` shim;
  `src/remote/attach.rs` (read-only) — `SshStdioBridge::start_with`
  (`:1843`), `bridge_connection` stdio (`:2057-2072`),
  `apply_noninteractive_ssh_options` (`:575`),
  `local_forward_socket_path_scoped` (`:2320`; only the empty scope is
  unscoped), `BridgeErrorSink` (`:1935`).
- `src/client/endpoint/catalog.rs` (read-only) — `SavedSshEndpoint`,
  `EndpointCatalog::load_profiles`, `catalog_path`; `src/cli/machine.rs`
  (read-only) — the `herdr machine` surface PR 9's docs point at.
- `src/protocol/wire.rs` (read-only) — `PROTOCOL_VERSION`, `MAX_FRAME_SIZE`,
  `ClientMessage::{TerminalHello, ObserveTerminal, ControlTerminal, Input,
  Resize, AttachScroll, Detach}`, `ServerMessage::{Welcome, Terminal,
  ServerShutdown}`, `TerminalFrame`, `RenderEncoding::TerminalAnsi`,
  `write_message`/`read_message`. `src/protocol/endpoint.rs` (read-only) —
  `EndpointClientHello.surface_active`. `src/client/terminal_sessions.rs` —
  `terminal_control_command_from_json` (the JSON command vocabulary) and
  the frame loop shape; `src/client/handshake.rs::do_handshake` as the
  reference for the two-frame terminal hello (re-implemented, not widened).
- `src/ipc.rs` — `connect_local_stream`, `bind_private_local_listener`,
  `restrict_socket_permissions` (unix/windows twin idiom).
  `src/pane_graphics_files.rs` — `0700`/`0600` create-and-verify.
  `src/checksum.rs` — `sha2` usage.
- `src/server/headless/bootstrap.rs:40-43` — the multi-thread runtime
  shape. `src/server/headless.rs:1172, 1365-1400, 1826-1838, 2024,
  2166-2223`, `src/server/headless/client_views.rs:708`,
  `src/server/clients.rs:12-17`, `src/server/headless/render.rs:409-450,
  633-651` and `src/server/headless/tests/mod.rs:3152` (read-only) —
  passive-client, observer/controller and render-lane semantics the
  gateway relies on.
- `src/config/model.rs` (`FleetConfig` pattern), `src/config/io.rs`
  (`KNOWN_TOP_LEVEL_CONFIG_KEYS`, live-reload sections, `config_dir`,
  `state_dir`), `src/config.rs` (`collect_diagnostics`,
  `CONFIG_PATH_ENV_VAR`), `src/main.rs` (`DEFAULT_CONFIG` + block tests,
  bare-command list, usage), `src/cli.rs` / `src/cli/spec.rs` /
  `src/cli/fleet.rs` (dispatch, spec, hand-parsed args, exit codes 0/1/2),
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
  `docs/fork/decisions/0001-…md` (E0 and E1 reviews) and `0002-…md` (the
  sync policy, the E1 hooks, the `surface_active` and stderr notes);
  `docs/fork/plans/e1-fleet-core.md` Downstream sections and
  `docs/fork/plans/e2-fleet-tui.md` PR 1 Downstream (the connector surface
  #23 kept: `for_client`, `take_events`, `set_active_geometry`);
  `docs/fork/fleet-core.md` "Driving the connector (E3)".

## End-to-end epic validation

After every PR is ✅, `implement-epic` proves the epic against real servers
(debug build, isolated lab; `ssh-lab.sh` exit 3 degrades the ssh steps to a
recorded skip):

1. **Both builds.** `bash scripts/fork/gate.sh . ci` and `bash
   scripts/fork/gate.sh . ci-no-default` → `EXIT=0` each; `cargo build
   --no-default-features && target/debug/herdr gateway; echo $?` → `unknown
   command: gateway`, 2; `cargo build && target/debug/herdr --version` →
   `herdr 0.8.2-fork`.
2. **Fleet of three, two sources.** `fleet-lab.sh up 2` + `ssh-lab.sh up`;
   `[fleet]` with `lab-1` (local), `lab-2` (local), `include_local =
   false`, `include_machines = true`, and a saved machine `Lab SSH`
   (`herdr-ssh-lab`, session `lab-1`) written to `endpoints.json` under the
   lab's `XDG_STATE_HOME`; `HOME=$HERDR_SSH_LAB_HOME $H gateway --bind
   127.0.0.1:7788 2>/tmp/e3-gw.err &`. `curl /health` → 200 without a
   token; `/api/fleet` → 401 bare, 200 with the read token, `hosts` =
   `lab-1`/`lab-2`/`lab-ssh` all `connected` (`lab-ssh` with `kind ==
   "ssh"`, `target == "herdr-ssh-lab"`), `workspaces[0].label` =
   `lab-1`/`lab-2`/`lab-1`, `agents == []`; `/api/gateway.features` ⊇
   `["fleet","events","terminal","pairing"]`; `herdr fleet status --json`
   under the same env lists the same three hosts.
3. **Security invariants.** foreign `Origin` → 403; six bad bearers from
   one peer → 401 ×5 then 429 while another peer still gets 401;
   `--bind 0.0.0.0:7788` without origins → exit 1; token files `0600` in a
   `0700` dir; `chmod 644 read.token` → next start exits 1; `read` token
   opening `mode: control` → `forbidden` + close 1008; `terminal.input` in
   observe mode → `forbidden`, pane unchanged (`grep -c INJECTED` = 0).
4. **Passive and quiet.** With the gateway holding all three hosts: set
   `[server] headless_cols = 100 / headless_rows = 30` on `lab-1`, `server
   reload-config`, `pane run 'stty size'` → `30 100` (the gateway did not
   become the foreground client); `/tmp/e3-gw.err` contains no ssh output;
   the bridge child's argv contains `BatchMode=yes`.
5. **Live events.** `ws-client.py /api/events --timeout 30 --max-messages 4`
   → `hello`, `fleet`, then `host_connection lab-ssh unavailable` when
   `ssh-lab.sh down` runs, and `connected` + `snapshot` after `ssh-lab.sh
   up` (host-local: no line names `lab-1`/`lab-2`).
6. **Terminals on every transport.** Observe `lab-2/<pane>`: `terminal.ready`
   + a binary frame whose decoded header is `full=1 80×24` and whose body
   holds `herdr-fleet-lab:lab-2`; observe `lab-ssh/<pane>` over ssh → the
   same with `lab-1`'s marker and a gateway-scoped forward socket
   `/tmp/herdr-remote-<pid>-gateway-…` present while open; three
   concurrent observers on one pane all receive frames; a `pane run 'echo
   hello'` on the observed pane produces an incremental (`full=0`) frame
   within 2 s.
7. **Control reaches exactly one pane.** With the control token,
   `terminal.open control` on `lab-1/<pane>` + `terminal.input "echo
   E3-EPIC\n"` + `release` → `lab-1` pane read contains `E3-EPIC`, `lab-2`
   does not, and `lab-ssh`'s target pane (the same server as `lab-1`) does
   — state so; then the same on `lab-2` → only `lab-2` gains it; a second
   controller without takeover → `busy`; with takeover → the first sees
   `terminal.closed reason=taken_over`; controller `stty size` after a
   60×20 open prints `20 60`, an observer at 60×20 leaves the PTY size
   alone.
8. **Pairing loop.** `herdr gateway pair --json` → URL; `curl -c jar
   "$URL"` → 303 + `Set-Cookie … HttpOnly; SameSite=Strict`; the cookie
   gets `/api/fleet` 200 and `/api/gateway.scope == read`; the URL a second
   time → 403; `pair --control` → a cookie that opens a control terminal;
   `gateway status --json` → `running: true`, `devices.read == 1`,
   `devices.control == 1`; `rotate-token control` → the control cookie
   gets 401, the new control token 200, `devices.control == 0`.
9. **Shutdown hygiene.** `kill -TERM $GW; wait` → exit 0 within 5 s while
   two terminals were open; no `/tmp/herdr-remote-<pid>-*` sockets remain;
   `gateway.json` removed; `ssh-lab.sh down`, `fleet-lab.sh down`; from a
   shell without the lab env `herdr session list` shows no `lab-*`,
   `~/.config/herdr*/gateway` does not exist,
   `~/.local/state/herdr*/client/endpoints.json` is untouched (absent, or
   same mtime as before), and `ls -la ~/.ssh | sha256sum` is unchanged from
   before the run.
10. **Contracts and perf.** `git diff master -- src/protocol src/server
    src/remote tests/fixtures` is empty; `grep -rn 'unwrap()' src/gateway`
    outside `#[cfg(test)]` is empty; `just bench-render-scale` medians are
    unchanged from `master` (the gateway adds no work to the server); with
    15 observers on one pane the gateway RSS stays under 15 × 2 frames +
    baseline (report the `ps` number); `docs/fork/gateway.md` snippets
    match the live output.

Passing all ten is the acceptance criterion; `implement-epic` then flips
E3 to ✅ in `docs/fork/ROADMAP.md`.
