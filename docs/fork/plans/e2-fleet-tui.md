# Epic E2 — Fleet TUI (one console, every machine)

## Context

**Goal (roadmap):** `herdr fleet` on the console machine shows every host's
workspaces and agents in the sidebar with live status, and lets you pick any
host and work in it at full fidelity. **Why:** this is MVP 1 — "see all the
sessions at once" from one computer.

Scope contract = the roadmap's E2 deliverables: launch (`herdr fleet
[--session <name>]`, `herdr --fleet` alias) reusing `run_client_with_mode` /
the client-owned shell with N streams behind a `FleetConnector`; a sidebar
host group per configured host (`▸ workbox · 2 blocked · 3 working`) with that
host's workspaces and agents, unreachable hosts dimmed with the reason, local
host first; host switching by clicking a host header or through a picker
overlay bound to `prefix+shift+h` by default; the pane area renders the active
host only; input, resize, focus, scroll and endpoint commands go to the active
host; selecting an agent on another host switches the active host and focuses
it there; notifications prefixed `[host]` with sound per existing config;
local keybindings on every host (no `--remote-keybindings server`); a host
dropping shows "reconnecting…" and keeps the rest usable, an *active* host
dropping shows a reconnect notice instead of exiting; `docs/fork/fleet.md`;
shell-state unit tests without PTYs and a `tests/` run against the fleet lab
asserting the sidebar lists both hosts' agents and input reaches the right
server. **Performance constraint:** sidebar work is × hosts × agents; keep it
O(visible rows), no per-frame allocation for inactive hosts, profile 1 vs 5
hosts × 15 agents before merging. **Out of scope:** mixed-host pane layouts.

The product name for fork-owned surfaces is **Herdr Fleet** (short "Fleet");
the binary, commands and paths stay `herdr`.

**Dependency chain:** E1 is ✅ (`origin/master` @ `7320530c`, herdr
`0.8.2-fork`). E2 assumes exactly these E1 contracts (verified in the code):

- `src/fleet/connector.rs`: `FleetConnector::start(specs, options)`,
  `events(&mut self) -> Option<&mut mpsc::Receiver<FleetEvent>>` (PR 1
  changed this from `&mut …`; `None` once `take_events` moved it out),
  `set_active(&self, Option<&HostId>)`, `active_host()`,
  `send(&self, &HostId, HostCommand)`, `shutdown(self)`;
  `FleetEvent::{Host, Surface, SurfacePatch, Notification, EndpointResponse,
  ServerMessage}` (all host-tagged); `HostCommand::{Resize, PaneInput, Focus,
  MouseCapture, Endpoint{request_id, request}, Raw(Box<ClientMessage>)}` —
  `Raw` refuses `ClientShellEndpointRequest` and `EndpointControl`, accepts and
  bookkeeps `ClientShellResize`; `HostSendError::{UnknownHost, NotConnected,
  Io, Refused}`; `FleetConnectorOptions { handshake: HandshakeParams,
  active: ActiveGeometry, manage_ssh_config, max_frame_size,
  endpoint_timeout }` with `for_config(&Config)` (PR 1 replaced the bare
  `active_surface: ClientSurfaceSize` with `active: ActiveGeometry`); `INACTIVE_SURFACE` = `DEFAULT_HEADLESS_COLS ×
  DEFAULT_HEADLESS_ROWS` (120×40). Frames from inactive hosts are dropped in
  the reader thread; `set_active` deactivates the old host before activating
  the new one and resizes both. `send(Endpoint)` accepted ⇒ exactly one
  `EndpointResponse` follows.
- `src/fleet/state.rs`: `FleetState::{new, hosts, host, active_host,
  set_active_host, apply, merged_agents, totals}`, `HostState { spec,
  connection, snapshot, rollup }`, `HostConnection::{Connecting{attempt},
  Connected{server_version, methods}, Unavailable{reason, retry_in},
  Incompatible{generation, reason}}` with `is_connected/state_name/reason`,
  `FleetChange::{HostConnection, Snapshot, AgentAdded, AgentRemoved,
  AgentStatus, ActiveHost}`, `AgentRollup`, `MergedAgent`, `Backoff`;
  `test_new()`, `test_with_adversarial_identity_state()`,
  `assert_invariants_for_test()`. Merged order is
  `(status_rank, Reverse(fleet_change_seq), host_index, pane_id)` and is the
  only sanctioned fleet-wide order; per-host rows keep upstream's
  `status_priority` (Blocked > Done > Working > Idle > Unknown).
- `src/fleet/refs.rs`: `FleetPaneRef/FleetTabRef/FleetWorkspaceRef` with the
  `host/w1:p1` string form. `src/fleet/hosts.rs`: `HostId` (`HostId::LOCAL`,
  `local()`, `as_str`, `is_local`), `HostSpec`, `resolve_hosts(&FleetConfig)
  -> Result<Vec<HostSpec>, Vec<String>>` (all-or-nothing; `local` first).
- E1 constraints recorded in the roadmap: `agents[]` excludes plain panes (the
  fleet lab's marker panes are **not** agents); `fleet_change_seq` is per
  `FleetState` instance; an ssh host's forward socket is unlinked only by
  `shutdown`, so the console **must** call it on every exit path; on unix
  dropping an `SshTransport`/`SshStdioBridge` while a bridged stream is still
  open blocks until the ssh child exits — release streams first (the
  connector's `shutdown` already half-closes them). Plus, from
  `docs/fork/fleet-core.md`: a connecting client shell becomes each host's
  *foreground* client (its surface is the host's effective pane geometry), so
  E2 holds fleet connections only while the console runs; the ssh child
  inherits stderr, so a full-screen consumer must redirect it;
  `FleetConnectorOptions.active_surface` was fixed at `start`, so a reconnect
  of the active host re-handshook at that size — **PR 1 fixed this**: the
  connector owns an `ActiveGeometry` that `set_active_geometry` updates and
  every handshake re-reads;
  `HandshakeParams::read_only` sends cell size 0, `pixel_mouse: false`,
  `mouse_capture: false`, `direct_graphics: false`, `endpoint_keybindings:
  false`.

### Real current state (verified on `master` @ `7320530c`)

- **Client entry.** `src/client/mod.rs:319 run_client()` →
  `:343 run_client_with_mode(attach_request: Option<(String, bool)>,
  attach_escape: Option<AttachEscapeState>, log_message)`. There is no mode
  enum and no socket/stream parameter: the socket is
  `server::socket_paths::client_socket_path()` (env override is how
  `--remote` points the client at its forward socket), the keybinding source
  is `client_shell_keybinding_source()` (`src/client/handshake.rs:39`, from
  `HERDR_REMOTE_KEYBINDINGS`). Sequence: `Config::load()` →
  `ClientShellConfig::from_config(..).with_startup_config_diagnostic(..)
  .with_startup_onboarding(..).with_keybinding_source(..)
  .with_local_endpoint(&socket_path)` → `connect_local_stream` (failure:
  `eprintln` + `exit(1)`) → `initial_terminal_geometry` (the
  "terminal reported a zero-sized grid" refusal is
  `platform::terminal_grid_size`, `src/platform/mod.rs:90`) →
  `do_handshake` (`src/client/handshake.rs:130`) → `setup_terminal` → panic
  hook → current-thread tokio runtime → `ctrlc` → `rt.block_on(run_client_loop(
  stream, cols, rows, cell_w, cell_h, exact, should_quit, ClientLoopConfig,
  encoding, endpoint_methods, attach_escape))` (`:717`) → terminal restore →
  on `Err`: print, `exit(1)` unless `ServerShutdown{reason: "detached"}` or a
  connection loss during terminal hangup.
- **The loop (`run_client_loop`, ~1100 lines).** Threads: stdin reader
  (`input::stdin_reader_loop`), `resize_poll_loop`, and
  `server_reader_thread(read_stream, tx, quit, max_frame_size)` (`:1822`)
  feeding `tokio::sync::mpsc::channel::<ClientLoopEvent>(256)` (`:769`).
  `ClientLoopEvent` (`:291`): `StdinInput`, `PixelMouse`,
  `DirectGraphicsResponse`, `Resize`, `TerminalUnavailable`,
  `ServerMessage(Box<ServerMessage>)`, `ServerDisconnected`, `Timer`. The
  writable half is a bare `let mut write_stream = stream;` (`:889`) threaded
  as `&mut LocalStream` into `finish_client_shell_input` (`:658`),
  `install_client_shell_snapshot` (`:619`), `dispatch_client_shell_actions`
  (`:529`), `EndpointCommands::send_next(stream)` and **21** call sites of
  `write_to_server(stream: &mut LocalStream, msg)` (`:1876`). Arms that
  matter: `ServerMessage::PaneSurface` → `shell.set_pane_surface` +
  `compose` + `present_frame` (`:1278`); `PaneSurfacePatch` →
  `apply_pane_surface_patch` (`:1290`); `SemanticNotification` →
  `shell.receive_notification(event, now)` + `handle_shell_notification_effects`
  (`:1484`); `ClientShellEndpointResponseChunk` → `endpoint_commands
  .receive_chunk` → `shell.handle_endpoint_result(boot_id, request_id,
  result)` (`:1513`); `EndpointControl{kind == ENDPOINT_SNAPSHOT_KIND}` →
  `serde_json` → `install_client_shell_snapshot` (`:1722`);
  `ServerShutdown` → `Err`; `ServerDisconnected` → `Err(ConnectionLost)`
  (`:1755`) — **the client has no reconnect path at all**.
  `ClientShellAction` (`src/client/shell/state.rs:280`): `Endpoint{boot_id,
  request}`, `ClipboardWrite`, `Request(ClientMessage)`, `OpenSafeWebUrl`,
  `ReplayMouse`, `Keybind(KeybindAction)` (the last is a `debug!` no-op at
  `mod.rs:557`). `EndpointCommands` (`src/client/endpoint_commands.rs`):
  one in flight, queue of `(boot_id, Box<Request>)`, `send_next(stream)`
  writes `ClientShellEndpointRequest`, `receive_chunk` reassembles,
  `expire(now)` after 60 s.
- **Shell state.** `ClientShellState` (`src/client/shell/state.rs:860`) owns
  **one** `snapshot: Option<Box<ClientShellSnapshot>>` and **one**
  `pane_surface: Option<PaneSurfaceFrame>`; ~200 call sites read
  `self.snapshot` directly. `set_snapshot` (`:1176`) drops a lower revision
  of the same `boot_id`, and on a **boot change** resets surface, leases,
  popup, drags, scroll offsets, pending requests, notifications, overlay,
  selection and copy mode; `invalidate_pane_surface` (`:1697`);
  `set_endpoint_methods` (`:1081`); `surface_size(cols, rows)` (`:1168`);
  `compose(cols, rows) -> Option<FrameData>` (`composition.rs:23`) refuses to
  draw unless `snapshot.revision == surface.projection_revision`, then blits
  the surface into `layout.pane_surface`. `compose` mutates `hits`,
  `last_composed_size`, `workspace_scroll`, `reveal_*` — the hit map is a
  product of rendering; a fleet sidebar follows the same pattern.
  `ClientShellOverlay` (`:598`) is one `Option` with the `Navigator`
  (`ClientNavigatorOverlay { query, search_focused, selected, scroll, filter,
  expanded_workspaces }`, rows `ClientNavigatorRow { depth, label, meta,
  status, current, target }` built by `client_navigator_rows`
  (`overlays.rs:672`), rendered by `render_navigator_overlay`
  (`overlays.rs:753`), driven by `open_navigator_overlay` /
  `move_navigator_selection` / `accept_navigator_selection`
  (`overlay_input.rs:193-290`)) as the list-picker template. `record_binding`
  (`actions.rs:4`) maps `KeybindAction` to behaviour; prefix-mode resolution
  is `resolve_non_indexed_action(&self.config.keybinds.keybinds, key,
  KeybindDispatch::Prefix)` (`input.rs:757`).
- **Sidebar.** `render_sidebar` (`src/client/shell/sidebar.rs:180`) splits the
  area with `crate::ui::expanded_sidebar_sections`, draws `" spaces"`, builds
  `row_heights`/`gaps` **for every** `WorkspaceEntry { index, indented,
  last_child }` (`workspace_entries`, `:454`; `workspace_rows` allocates
  tokens per workspace — already O(all workspaces) per compose), scrolls with
  `scroll::list_scroll_metrics`, draws with `skip(workspace_scroll)` +
  `break` at the bottom (O(visible)), then `render_agent_panel`
  (`agent_sidebar.rs:52`) with `ordered_agent_pane_ids` (`:20`: by
  `(Reverse(status_priority), Reverse(state_change_seq))` unless the snapshot
  carries `agent_order`). Glyphs/colours: `status_icon`, `status_dot`,
  `status_priority`, `status_text`, `status_color` in `src/client/shell.rs`.
  Hit rects live on `ShellHitMap` (`state.rs:131`: `workspaces:
  Vec<WorkspaceHit>`, `agents: Vec<(Rect, String)>`, …), all host-unqualified.
  There is **no app title bar**; the mode bar (`render.rs:16`) shows only when
  `mode != Terminal` or `endpoint_error.is_some()`.
- **Notifications.** `SemanticNotification { kind, title, body, sound, agent,
  workspace_id, tab_id, pane_id, position }` (frozen wire type);
  `receive_notification(event, now)` → `(Vec<ClientShellNotificationEffect>,
  repaint)` (`notifications.rs:264`); `ClientPendingNotification { event,
  deadline, validate_state }`; `notification_still_current` and
  `notification_target_is_active` (`:377`) compare ids against
  `self.snapshot` — meaningful only for the active host. Sound gating is
  `handle_shell_notification_effects` (`src/client/notifications.rs:9`).
- **Keybindings.** `KeybindAction` (`src/input/keybindings.rs:20`), table row
  in `resolve_non_indexed_action` (`:100`); compiled `Keybinds` fields in
  `src/config/keybinds.rs:329` (`empty_action!` `:497`, `apply_action!`
  `:625`, `parse_action_bindings(field, &BindingConfig, &mut BindingRegistry,
  &mut diagnostics, source)` `:810`); help rows in
  `src/input/keybind_help.rs`. `KeysConfig` keys are enumerated by
  `scripts/config_reference_check.py` against
  `docs/next/website/src/data/config-reference.json` (62 `keys.*` rows) inside
  `just ci`'s `maintenance-test`; `SKIPPED_SUBTREES = ("keys.command",
  "fleet")` skips **struct-typed** fields only. A new `[keys]` leaf therefore
  fails the gate unless `docs/next/**` is edited, which fork rules forbid.
- **CLI/launch.** `src/cli.rs:116` `"fleet" => fleet::run_fleet_command(
  &args[2..])?` and `src/cli/fleet.rs:26` prints help + exit 2 for a bare
  `herdr fleet`; the `"server"` arm (`:106`) shows the `Option<i32>` → `NotCli`
  pattern. `src/main.rs:733` `known_flags`, `:758` bare-command list
  (`"fleet"` present), `:773` the `--remote` launch, `:781` default launch.
  `session::configure_from_args` consumes `--session` before any of this.
- **Tests/tooling.** `portable-pty` is a normal dependency (`openpty(PtySize
  { rows, cols, .. })` sets a real winsize, so a PTY-spawned client never sees
  a zero grid). `tests/multi_client.rs` never reads the screen; the screen
  reading prior art is `tests/client_mode.rs` (`spawn_pty_drain(reader) ->
  SharedOutput`, `read_output`, `attach_thin_client`, key bytes through
  `master.take_writer()`, `HERDR_DISABLE_SOUND=1`). `tests/support/fleet_lab.rs`
  `Lab::{new, up, herdr, runtime_dir}` drives `scripts/fork/fleet-lab.sh`
  (`XDG_CONFIG_HOME=<root>/xdg`, sessions `lab-N`, marker pane
  `herdr-fleet-lab:lab-N`; `env` prints no `[fleet]` snippet — tests write
  it into `<root>/xdg/<app_dir_name>/config.toml`, see `tests/cli/fleet.rs:32`).
  No `TestBackend`, no `vt100`, no criterion, no `benches/`, no `[features]`,
  no `[dev-dependencies]`. `just bench-render-scale` = an `#[ignore]`d test
  `render_scale_profile` in `src/server/render_scale_benchmark.rs`
  (`CARDINALITIES [1, 15, 50]`, 120×40, `summarize`/`print_stage` with
  `median_vs_1x`/`p95_vs_1x`). `scripts/test_ui_hot_path_architecture.py`
  scans `src/ui.rs`, `src/ui/**`, `src/server/render_stream.rs` for
  `input_state`, `keyboard_state_ansi`, `kitty_keyboard_state_ansi`,
  `screen_text_snapshot`, `foreground_job(` outside `#[cfg(test)] mod` blocks
  — `src/client/` and `src/fleet/` are not scanned, and the fork's own guard
  (`src/fleet/mod.rs` `the_pure_fleet_modules_import_no_runtime`) forbids
  `tokio`/`ratatui`/`interprocess`/`crate::ipc`/`crate::remote`/`crate::client`
  in the pure modules. Fork CI check names: `check (ubuntu-latest)`,
  `conventional-commits`, `shellcheck`; `tests/live_handoff.rs`
  `wait_for_file` is a known flake (`gh run rerun --failed`).

### Locked decisions

- **(a) Host switch UX — sidebar host groups + a picker overlay reusing the
  existing overlay pattern** (roadmap default). Host headers are rows in both
  sidebar sections; the picker is a new `ClientShellOverlay::HostPicker`
  modelled on `Navigator` (list, `↑↓`/`j k`, `enter`, `esc`, `1-9` jump,
  mouse rows). No tabs-per-host.
- **(b) Inactive hosts' surfaces are not prefetched** (roadmap default). A
  switch is one `FleetConnector::set_active`; the pane area shows a one-line
  "switching to <host>…" notice until the first full `PaneSurface` arrives.
- **(c) One shell, one active host** *(auto default)*. `ClientShellState`
  keeps its single `snapshot`/`pane_surface`, always the **active** host's.
  Other hosts live in `FleetState` (owned by the loop) and reach the shell
  only as a pure, pre-rendered row model (`FleetSidebarModel`) rebuilt on
  `FleetChange`, never per frame. This keeps the ~200 `self.snapshot` readers
  and every overlay untouched, and makes "input goes to the active host" true
  by construction: the shell's ids are the active host's ids.
- **(d) The seam is a `ServerLink`, not a second loop** *(auto default;
  written reason for reshaping `src/client/mod.rs`)*. `run_client_loop` stays
  the one event loop. `write_stream: LocalStream` becomes `ServerLink`
  (`Single(LocalStream)` | `Fleet(FleetLink)`), `write_to_server` takes
  `&mut ServerLink` so its 21 call sites do not change, and a
  `ClientLoopEvent::Fleet(Box<FleetEvent>)` branch joins the `select!`. A
  fleet event is *translated* into the existing `ServerMessage` arms
  (`FleetEvent::Surface` → `ServerMessage::PaneSurface`, etc.) after a host
  check, so no rendering or input arm is duplicated. Duplicating the loop
  (~1100 lines) would fork every future upstream fix; a trait object per
  write would touch all 21 sites.
- **(e) `herdr fleet` = bare command; `herdr --fleet` alias** *(auto
  default)*. `run_fleet_command` returns `Option<i32>`; `None` for a bare
  `herdr fleet` makes `maybe_run` answer `NotCli` (the `server` arm's
  pattern), and `main.rs` launches `client::run_fleet()`. `herdr fleet status`
  is unchanged. `--session <name>` keeps its global meaning: the implicit
  `local` host is *this process's* configured session
  (`client_socket_path_for(None)` already honours it).
- **(f) Host picker key is `[fleet.keys] host_picker = "prefix+shift+h"`**
  *(auto default — deviation from the roadmap's `[keys]` wording, with reason)*.
  A `[keys]` leaf is enumerated by upstream's `config_reference_check.py`
  and needs a row in `docs/next/website/src/data/config-reference.json`,
  which fork rules forbid editing; `SKIPPED_SUBTREES` skips only struct-typed
  fields. `[fleet]` is already skipped and documented in `docs/fork/`. The
  binding uses the same `BindingConfig` syntax and is compiled through the
  same `parse_action_bindings` + `BindingRegistry` (conflicts with `[keys]`
  bindings are diagnosed), resolves in prefix mode like every other action
  (`KeybindAction::HostPicker`), and follows `reload_config`. In a non-fleet
  client the action is a no-op with a `debug!`.
- **(g) Local keybindings only; `endpoint_keybindings: false`** (roadmap).
  `ClientShellKeybindingSource::Local` is forced on the fleet path;
  `HERDR_REMOTE_KEYBINDINGS` is ignored with a `warn!` if set.
- **(h) No kitty graphics and no direct graphics in fleet mode v1** *(auto
  default)*. The hello is per host at connect time with the inactive size;
  `direct_graphics` stays `false`, `kitty_graphics_enabled` is `false`, so
  `MAX_FRAME_SIZE` applies. Documented limit in `docs/fork/fleet.md`.
- **(i) The connector lives exactly as long as the console** (E1 constraint).
  Started after the terminal is set up, `shutdown()` on every exit path
  (quit, detach, Ctrl-C, terminal hangup, panic hook's restore path runs
  first, then the normal return). Terminal setup happens **before** any host
  handshake (unlike the single-host client): host failures are host-local and
  visible in the sidebar, never a reason to refuse the console.
- **(j) The process's stderr is redirected to the herdr log while the console
  runs** *(auto default)*, unix only (`libc::dup2` over fd 2 after
  `init_logging`, original fd restored on exit). This is how the inherited
  ssh child stderr stops painting over the TUI without editing
  `bridge_connection` in `src/remote/attach.rs`.
- **(k) Host order in the sidebar = config order, `local` first** (roadmap;
  same as `herdr fleet status`). Inside a host group, workspaces keep
  snapshot order and agents keep upstream's `ordered_agent_pane_ids` order;
  the merged fleet order is used only by the picker's "recent" hint and by
  E3/E4.
- **(l) Perf measurement = `just bench-fleet-scale`** *(auto default)*: an
  `#[ignore]`d in-binary test `fleet_sidebar_scale_profile` next to
  `render_scale_profile`'s conventions, composing a 120×40 fleet shell for
  `HOST_CARDINALITIES [1, 5]` × `AGENTS_PER_HOST 15` (synthetic snapshots
  from `tests/fixtures/endpoint-snapshot-v1.json`), reporting compose
  median/p95 and the 5-vs-1 ratio, plus the `FleetSidebarModel::rebuild`
  cost per change. Acceptance: compose median at 5 hosts ≤ 1.15× 1 host with
  inactive groups collapsed and ≤ 1.5× expanded (drawing is O(visible rows),
  bounded by 40 rows); rebuild is O(hosts × agents) per **change**, never per
  frame. Every perf-relevant PR (5, 6, 8) pastes this table in its PR body.
- **(m) No new crate** *(auto default)*. `portable-pty`, `regex`,
  `serde_json`, `libc`, `ratatui`, `crossterm`, `tokio` cover everything,
  including the PTY test harness. No `Cargo.toml`/`Cargo.lock` change in E2.
- **(n) Validation stand-ins** *(auto default)*: `scripts/fork/fleet-lab.sh
  up N` for local hosts, `ssh-lab.sh` for one ssh host (`sshd not found` ⇒
  exit 3 ⇒ record the degradation), a fork-owned `scripts/fork/tui-drive.py`
  PTY driver (real `TIOCSWINSZ`, ANSI-stripped screen text, key injection)
  for manual runs, and `tests/support/fleet_tui.rs` for the integration
  tests. Agents for the "15 agents" profile are synthetic (the lab's marker
  panes are not agents); the live checks use `pane run … 'yes >/dev/null'`
  for a `working` status where a detectable agent is not installed.

### Sequencing hazards

- **Upstream files touched, and by which PR only:** `src/client/mod.rs`
  (PR 3: `ServerLink`, `ClientLoopEvent::Fleet`, `ClientState.fleet`,
  `mod link;`, `mod fleet;` — and PR 4 adds only the `run_fleet` re-export
  line); `src/client/endpoint_commands.rs` (PR 3); `src/client/shell.rs`
  (PR 5: `mod fleet;` + `mod fleet_sidebar;`; PR 6: `mod fleet_overlay;`);
  `src/client/shell/state.rs` (PR 5: one `ShellHitMap` field, one
  `ClientShellState` field, one `ClientShellAction` variant; PR 6: one
  `ClientShellOverlay` variant + its `kind()` arm; PR 7: one
  `ClientPendingNotification` field); `src/client/shell/sidebar.rs`,
  `agent_sidebar.rs`, `mouse.rs` (PR 5: one fleet delegation each);
  `src/client/shell/overlay_input.rs`, `overlays.rs`, `actions.rs`,
  `input.rs` (PR 6: one arm each); `src/client/shell/notifications.rs`
  (PR 7); `src/client/shell/composition.rs` (PR 8: one notice hook);
  `src/cli.rs`, `src/cli/fleet.rs`, `src/cli/spec.rs`, `src/main.rs`
  (PR 4); `src/config/model.rs` (PR 6: `FleetConfig.keys` — inside E1's fork
  section), `src/config/keybinds.rs`, `src/input/keybindings.rs`,
  `src/input/keybind_help.rs` (PR 6); `justfile` (PR 5: one recipe);
  `src/fleet/connector.rs`, `handshake.rs` (PR 1), `src/fleet/mod.rs`
  (PR 2). Never: `src/protocol/**`, `src/server/**`, `src/app/**`,
  `src/remote/attach.rs`, `tests/fixtures/endpoint-*.json`, `Cargo.*`,
  `docs/next/**`.
- **`src/client/fleet.rs`, `src/client/shell/fleet.rs`,
  `tests/fork_fleet_tui.rs`, `tests/support/fleet_tui.rs` are edited by PRs
  4–8.** Waves keep at most two of those PRs in flight (W5 `[6, 8]`); the
  later PR in a wave rebases onto `master` before its gate and re-runs the
  PTY test.
- **PR 1 and PR 2 both edit `src/fleet/mod.rs`** (PR 1: test-support
  re-export; PR 2: `pub mod sidebar;` + guard list). Same rule: the later one
  rebases.
- **PR 3 is a behaviour-preserving refactor of `src/client/mod.rs`**: land the
  characterization evidence (the whole existing client test suite,
  `tests/client_mode.rs`, `tests/multi_client.rs`) green on both sides; the
  `Fleet` link variant is dead code until PR 4 and carries an allow with a
  reason.
- **Two Rust builds at once** (W1, W5) is the memory hazard the gate lock
  exists for; never bypass `scripts/fork/gate.sh`.
- **The console resizes hosts.** Every live validation runs against lab
  sessions only; never point a `[fleet]` config at the user's default session
  (`include_local = false` in every fixture) and never run the console from a
  shell whose `XDG_CONFIG_HOME` is the user's.

## Status legend

✅ merged · 🔨 in progress · ⬜ not started · ⛔ blocked

## PR map

| # | Title | Group | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | feat(fleet): connector client options, active-surface tracking and takeable events | A · Foundations | — | ✅ |
| 2 | feat(fleet): pure sidebar and host picker models | A · Foundations | — | ✅ |
| 3 | refactor: route client writes and fleet events through a server link seam | B · Console loop | 1 | ✅ |
| 4 | feat: herdr fleet opens the client shell over the fleet connector | B · Console loop | 2, 3 | ✅ |
| 5 | feat(fleet): sidebar host groups with live status and click-to-switch | C · Fleet UX | 4 | ✅ |
| 6 | feat(fleet): host picker overlay and fleet.keys host_picker binding | C · Fleet UX | 5 | ⬜ |
| 7 | feat(fleet): host-aware notifications and cross-host notification targets | C · Fleet UX | 5 | ⬜ |
| 8 | feat(fleet): reconnect notice for the active host and resize on reconnect | C · Fleet UX | 5 | ⬜ |
| 9 | docs: fleet console guide, adr e2 review, roadmap drift | D · Docs | 6, 7, 8 | ⬜ |

**Wave preview (2-agent cap):** W1 `[1, 2]` → W2 `[3]` → W3 `[4]` → W4 `[5]`
→ W5 `[6, 8]` → W6 `[7]` → W7 `[9]`. Critical path 1 → 3 → 4 → 5 → 6 → 9. No
PR touches `Cargo.toml`/`Cargo.lock`.

**Model assignment:** tasks run on `opus`. Review agent must be **`fable`**
for **PR 3** (the link seam: a write that lands on the wrong host, or an
event attributed to the wrong host, is a silent mis-route), **PR 4** (the
first console: input, resize, focus and endpoint requests reach exactly the
active host; `shutdown` on every exit path), **PR 5** (click-to-switch changes
the routing target; host-qualified hit map), **PR 6** (picker switch and the
keybinding), **PR 7** (`open_notification_target` switches hosts), and
**PR 8** (input while the active host is down must be dropped, never queued
to a stale or reconnected stream). PRs 1, 2 and 9 review on `opus`.

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
matches nothing exits 4).

Real-server validation is mandatory for every PR with runtime code (1, 3–8):
`cargo build`, `bash scripts/fork/fleet-lab.sh up <n>`, `eval "$(bash
scripts/fork/fleet-lab.sh env)"`, a `[fleet]` block with `include_local =
false` appended to `$XDG_CONFIG_HOME/herdr-dev/config.toml`, and
`H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV
target/debug/herdr"`. The console is driven through a PTY
(`scripts/fork/tui-drive.py` from PR 4; before that, the python `pty.fork()`
snippet in PR 3 — **it must set a real window size with `TIOCSWINSZ`** or the
client refuses with "terminal reported a zero-sized grid"). Each PR states
what the PTY check asserts on screen (ANSI-stripped text) and which server
received input (`$H --session lab-N pane read "$HERDR_FLEET_LAB_PANE_N"
--source recent`). SSH validations add `ssh-lab.sh up` + `eval "$(bash
scripts/fork/ssh-lab.sh env)"` and run herdr with `HOME=$HERDR_SSH_LAB_HOME`;
tear down with `ssh-lab.sh down` then `fleet-lab.sh down`, and confirm from a
shell **without** the lab env that `herdr session list` shows no `lab-*`.

CI gotchas to expect:

- Zig 0.15.2 for libghostty-vt is already in `fork-ci.yml`; no workflow edit.
- `src/cli/spec.rs` invariants (`spec_describes_all_completion_commands`,
  `every_spec_subcommand_renders_short_and_long_help`,
  `spec_passes_clap_invariants`) — PR 4's `.about` change must keep them green.
- `scripts/test_config_reference_check.py` runs in `just ci` against the real
  model: any new key outside `[fleet]` fails the gate (decision (f)).
- `just check`'s `windows-lint` is not in the gate but the code must compile
  for Windows: gate the stderr redirect, `libc`, and PTY tests with
  `#[cfg(unix)]`; keep `src/fleet/sidebar.rs` platform-neutral.
- PTY integration tests boot two lab servers each; keep them to one test
  binary (`tests/fork_fleet_tui.rs`) with a shared `Lab`, well inside the
  30-minute job. Sound is disabled with `HERDR_DISABLE_SOUND=1`.
- `tests/live_handoff.rs` `wait_for_file` timeouts are a known upstream
  flake: `gh run rerun --failed -R vinceseguin/herdr`.
- Scripts must be `shellcheck -S warning` clean; `tui-drive.py` is python
  (not shellchecked) and must run on python ≥ 3.10 with the stdlib only.

## Cross-cutting constraints (all PRs)

- **Additive, minimal wiring.** New code under `src/fleet/sidebar.rs`,
  `src/client/link.rs`, `src/client/fleet.rs`, `src/client/shell/fleet.rs`,
  `src/client/shell/fleet_sidebar.rs`, `src/client/shell/fleet_overlay.rs`,
  `src/client/shell/tests/fleet*.rs`, `scripts/fork/tui-drive.py`,
  `tests/support/fleet_tui.rs`, `tests/fork_fleet_tui.rs`, `docs/fork/`.
  Upstream edits are limited to the list in *Sequencing hazards*; each is a
  `mod` line, one field, one enum variant, one match arm, one delegation
  call, or the `ServerLink` type change in `src/client/mod.rs` (decision (d)).
  Fork files inside `src/client/shell/` extend `ClientShellState` through
  their own `impl ClientShellState` blocks (fields are `pub(super)`), so new
  behaviour needs no edits to upstream method bodies.
- **No wire or endpoint-contract change.** `src/protocol/wire.rs`,
  `src/protocol/endpoint.rs`, `tests/fixtures/endpoint-*.json` stay
  untouched; `PROTOCOL_VERSION` (22) is not bumped; no new message, no new
  `EndpointControl` kind, no new hello field. Servers stay stock: nothing in
  `src/server/`, `src/app/`, `src/api/` changes; a fleet host may run
  upstream 0.8.2.
- **The active host is the only routing target, and it is explicit.** Every
  write goes through `ServerLink`; `FleetLink` sends to
  `connector.send(&self.active, …)` where `active` is the one `HostId` the
  loop set; every inbound fleet event is compared against that same id before
  it reaches the shell; a mismatch is dropped with `tracing::debug!(host,
  active, kind)`. No "current host" fallback anywhere. Mixed-host layouts are
  out of scope; the shell never holds two hosts' surfaces.
- **State is separated from runtime.** `src/fleet/sidebar.rs` is pure (added
  to the `the_pure_fleet_modules_import_no_runtime` guard). `FleetState` stays
  free of ratatui, sockets and async. `FleetSidebarModel` is rebuilt only in
  response to `FleetChange` (and collapse/active changes), cached, and read by
  the renderer; the renderer never formats, allocates or sorts for inactive
  hosts — header strings, counts and glyph choices are precomputed at rebuild.
- **Multiplicative performance.** Per-frame sidebar work is O(visible rows)
  across all hosts; inactive-host row heights are constant (1) and cached;
  the active host keeps today's `workspace_rows` path. Frames from inactive
  hosts never reach the loop (connector gate) and, if one does (a switch in
  flight), it is dropped before allocation into the shell. PRs 5, 6 and 8
  report `just bench-fleet-scale` (decision (l)).
- **Host failure is local.** A host's `Unavailable`/`Incompatible`/
  `Connecting` is sidebar state; the console never exits because of a host,
  never blocks on one, and never calls `std::process::exit` from fleet code.
  The active host dropping is a pane-area notice (PR 8). `connector.shutdown()`
  runs on every exit path; the events receiver is dropped first.
- **Read is safe, control is explicit.** Nothing is sent to a host that the
  user did not type or click into; inactive hosts receive only the connector's
  own resize on activation/deactivation (E1 behaviour). No `ClientShellFocus
  { focused: false }` is sent to inactive hosts.
- **Code conventions.** No `unwrap()` in production code; `tracing` with
  `host = %id` fields; `#[allow]` only with a reason naming the PR that
  removes it; `#[cfg(unix)]`/`#[cfg(windows)]` gating; no new dependency.
- **Never** run `herdr server stop`, `herdr update`, or any command against
  the user's live session; never read/write `~/.ssh`, `~/.config/herdr*`.
  Fixtures use named sessions, throwaway `XDG_CONFIG_HOME`, `include_local =
  false`, and a fake `HOME` for ssh.
- **Docs discipline.** Do not edit `docs/next/**`, root `README.md`,
  `CHANGELOG.md`, `skills/herdr/SKILL.md`, `distribution/**`. Fork docs go in
  `docs/fork/` (`fleet.md` for the console; `fleet-core.md` stays the
  `[fleet]` reference and gains the `[fleet.keys]` table in PR 6). No `refs
  #<n>` lines. Commit trailers as the environment specifies.
- **Branches:** `feat/e2-pr1-connector-client-options`,
  `feat/e2-pr2-sidebar-model`, `refactor/e2-pr3-server-link`,
  `feat/e2-pr4-fleet-console`, `feat/e2-pr5-sidebar-host-groups`,
  `feat/e2-pr6-host-picker`, `feat/e2-pr7-fleet-notifications`,
  `feat/e2-pr8-reconnect-notice`, `docs/e2-pr9-fleet-docs`, each from the
  latest `origin/master`, in `.claude/worktrees/<slug>`, all git through
  `git -C`.

## Per-PR detail

### PR 1 — feat(fleet): connector client options, active-surface tracking and takeable events · deps: —

**Goal:** the connector can be driven by a full-screen client: a hello with
the real cell geometry and mouse settings, an active surface that follows
the terminal (including across a reconnect of the active host), an event
receiver the loop can `select!` on while still calling `send`, and reusable
fake-endpoint test support for `src/client` tests.

**Files**

- `src/fleet/connector.rs`: `FleetConnectorOptions::for_client`,
  `set_active_geometry`, `take_events`, `shutdown` tolerant of a taken
  receiver, `#[cfg(test)] pub(crate) mod test_support`.
- `src/fleet/handshake.rs`: `HandshakeParams::for_client`.
- `src/fleet/mod.rs`: nothing unless a re-export is needed for
  `test_support`.

**Shapes/approach**

```rust
// handshake.rs
impl HandshakeParams {
    /// A console's hello for every host: real cell geometry and mouse policy,
    /// the *inactive* surface size, no direct graphics, local keybindings.
    pub fn for_client(cell_width_px: u32, cell_height_px: u32, pixel_mouse: bool,
        mouse_capture: bool) -> Self   // surface_size: INACTIVE_SURFACE, read_timeout: LOCAL (transport overrides for ssh)
}

// connector.rs
pub struct ActiveGeometry { pub surface: ClientSurfaceSize, pub cell_width_px: u32,
    pub cell_height_px: u32, pub pixel_mouse: bool }
impl FleetConnectorOptions {
    pub fn for_client(config: &Config, handshake: HandshakeParams, active: ActiveGeometry) -> Self
}
impl FleetConnector {
    /// Replace the geometry announced to whichever host is (or becomes) active.
    /// Sends a resize to the current active host immediately; the supervisor's
    /// post-handshake activation and `set_active` read the same cell, so a
    /// reconnecting active host comes back at the last size, not the start size.
    pub fn set_active_geometry(&self, geometry: ActiveGeometry) -> Result<(), HostSendError>;
    /// The event stream, detached so the caller can own it while still using `send`.
    /// Second call returns `None`. `shutdown` no longer needs the receiver: the
    /// caller drops it (releasing supervisors parked on a full channel) first.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<FleetEvent>>;
}
```

`active_surface: ClientSurfaceSize` on the options becomes the initial value
of a `Mutex<ActiveGeometry>` on the connector; `announce_surface` and the
supervisor's "host connected while active" path read it. `resize_message`
uses the geometry's cell size and `pixel_mouse` (today it uses the hello's).
`HostCommand::Resize` / a raw `ClientShellResize` to the active host also
update the cell, so the two paths cannot disagree. `test_support` exposes the
existing fake endpoint (`FakeEndpoint`: bind, welcome, snapshot, record
received messages, connection count) and `FleetConnector::start_with` behind
`#[cfg(test)]` for `src/client/fleet.rs` tests. Remove the E1 `#[allow]`
comments that named E2 as the consumer only where this PR consumes them; the
new items carry their own allow naming PR 3/4.

**As built (PR 1, merged).** Deviations from the shapes above, all
deliberate:

- `FleetConnectorOptions.active_surface: ClientSurfaceSize` became
  `active: ActiveGeometry`. The connector then owns that value in an
  `Arc<Mutex<ActiveGeometry>>` shared with every supervisor, so
  `wanted_geometry()` (ex-`wanted_surface`) reads the *current* console
  geometry when a host handshakes. A host that drops while active therefore
  re-handshakes at the latest geometry, hello included — the E1 gap, closed
  in the hello rather than with a post-handshake resize.
- `ActiveGeometry` carries the cell metrics and `pixel_mouse` as well as the
  surface, and `HostLinkState.surface: Option<ClientSurfaceSize>` became
  `announced: Option<ActiveGeometry>`, so a cell-size-only change (font
  change, a different terminal) is still announced. `ActiveGeometry::default()`
  is the read-only collector's geometry (`INACTIVE_SURFACE`, zero cells, no
  pixel mouse), which is what `for_config` keeps using.
- `FleetConnector::events()` returns `Option<&mut Receiver>` (it cannot hand
  back a reference once `take_events` moved the receiver out). `oneshot.rs`
  treats `None` as "nothing else can arrive". `shutdown` closes the receiver
  when it still owns it and logs a `debug!` when it does not; **the caller
  must drop a taken receiver before `shutdown`**, or a supervisor parked on a
  full channel is only detached after the 2 s wait.
- `HostCommand::Resize` and a raw `ClientShellResize` aimed at the *active*
  host also update the shared `ActiveGeometry`, so PR 3/4's existing
  `ClientShellResize` write through the link and `set_active_geometry` cannot
  disagree.
- Lock order, documented in the module header and relied on by every path:
  `active_host` → one host's `HostLinkState` → `active_geometry`. A
  supervisor never takes `active_host` (it reads its own `active` atomic), and
  nothing takes two host link locks at once.
- The fake endpoint server is `connector::test_support` — module and items
  `pub(crate)`, gated `#[cfg(all(test, unix))]` like the tests it came from
  (it binds a local socket by path and half-closes it). The type kept its
  existing name **`FakeHost`** (the plan said `FakeEndpoint`), and the
  connector builder is `fake_connector(&[(id, &FakeHost)], options)`.
  `FleetConnector::start_with` is `pub(crate)`. Also exported: `Behaviour`,
  `scratch_dir`, `snapshot`, `snapshot_message`, `surface_message`,
  `drain_until`, `connected_with_snapshot`, `wait_for`, `hello_surface`,
  `hello_geometry`, `resizes`, `resize_geometry`.
- `src/fleet/oneshot.rs` is a fourth edited file (two `events()` call sites),
  beyond this PR's Files list. Fork-owned and benign.
- Two things the review deferred, for later PRs to own: (1) a supervisor
  parked in `blocking_send` can only be released by the *receiver*, so a
  console that shuts down while still holding a taken receiver leaks those
  threads (bounded 2 s wait, `warn!`, detach — pinned by a test); PR 4 must
  drop the receiver first, as the cross-cutting constraints already say.
  (2) `set_active` and `set_active_geometry` write to a socket while holding
  the active-host and link locks (pre-existing from E1), so a host that stops
  reading could stall the console loop — **PR 8** owns "the active host is
  down" and should decide whether a write timeout is needed.

**Tests**

- `set_active_geometry` before `set_active`: the activation resize carries the
  new size; after: the active host receives one resize with it; a fake host
  that drops and reconnects while active is re-announced at the *latest*
  geometry (the deferred E1 gap, now pinned).
- `take_events` twice → `None`; `shutdown` after the receiver was dropped
  joins within 2 s with a host mid-backoff and with a host parked on a full
  channel.
- `HandshakeParams::for_client` hello JSON: `cell_width_px`/`pixel_mouse`/
  `mouse_capture` as given, `surface_size == INACTIVE_SURFACE`,
  `direct_graphics == false`, `endpoint_keybindings == false`.

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
# [fleet] with lab-1, lab-2 as kind = "local" (see Verification); status must be unchanged by this PR:
$H fleet status --json | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(h["id"],h["connection"]["state"]) for h in r["hosts"]])'
# geometry is a no-op for a read-only status run: a 120-col line in lab-1's pane stays unwrapped
# NOTE (from PR 1's run): the marker pane `exec`s `sh -c 'while :; do sleep 60; done'`,
# so `pane run` types into a sleeping shell and nothing executes. Measure the host's
# grid directly instead — `pane layout` reports the server-side area, and `stty size`
# in a pane that *does* have a shell (`workspace create`) reports the pty:
$H --session lab-1 workspace create --label probe   # -> w2:p1, a real shell
$H --session lab-1 pane layout --pane w2:p1         # area 120x40 (herdr's headless default)
$H fleet status --watch &                           # hold the fleet connections open
$H --session lab-1 pane run w2:p1 'stty size'       # 40 119 with and without the fleet client
bash scripts/fork/fleet-lab.sh down
```

Evidence: unchanged status output, an unreflowed host grid while a fleet
client is connected, green `test-one fleet::connector`.

**Downstream**

- PR 3/4 drive: `FleetConnector::start(specs, FleetConnectorOptions::
  for_client(..))` → `take_events()` → `set_active_geometry` on every
  `ClientLoopEvent::Resize` → `set_active(Some(host))`.
- E3 keeps using `for_config`; nothing here changes `herdr fleet status`.

### PR 2 — feat(fleet): pure sidebar and host picker models · deps: —

**Goal:** a pure, cached row model for the fleet sidebar and the host picker,
built from `FleetState`, with precomputed strings and glyph choices so the
renderer does no formatting per frame.

**Files**

- `src/fleet/sidebar.rs` (new, pure).
- `src/fleet/mod.rs`: `pub mod sidebar;` and add it to the purity guard list.
- `src/fleet/hosts.rs`: drop the `#[allow(dead_code)]` on `HostId::as_str`
  (E1's deferred janitoring; this PR consumes it).

**Shapes/approach**

```rust
pub enum HostRowState { Connected, Connecting { attempt: u32 }, Unavailable, Incompatible }
pub struct HostHeaderRow { pub host: HostId, pub active: bool, pub collapsed: bool,
    pub state: HostRowState, pub enabled: bool /* see "As built" */,
    pub label: String /* "▸ lab-2 · 2 blocked · 3 working" */,
    pub reason: Option<String> /* dimmed second line when not connected */, pub rollup: AgentRollup }
pub struct WorkspaceRow { pub workspace: FleetWorkspaceRef, pub label: String,
    pub status: AgentStatus, pub focused: bool }
pub struct AgentRow { pub pane: FleetPaneRef, pub label: String /* name or title */,
    pub status: AgentStatus, pub focused: bool, pub state_change_seq: u64 }
// Borrows (deviation from the sketch below): `visible_rows` is walked per frame,
// and cloning a String per visible row per frame is the allocation this model exists
// to avoid.
pub enum FleetSidebarRow<'a> { HostHeader(&'a HostHeaderRow), Workspace(&'a WorkspaceRow), Agent(&'a AgentRow) }
pub struct HostGroup { pub host: HostId, pub header: HostHeaderRow,
    pub workspaces: Vec<WorkspaceRow>, pub agents: Vec<AgentRow> }
pub struct FleetSidebarModel { pub groups: Vec<HostGroup> /* config order, local first */,
    pub generation: u64 }
impl FleetSidebarModel {
    // `sort` is upstream's live agent-panel preference; see "As built" below.
    pub fn rebuild(&mut self, state: &FleetState, collapsed: &HashSet<HostId>,
        sort: AgentPanelSortConfig);
    pub fn group(&self, host: &HostId) -> Option<&HostGroup>;
    pub fn agent_status(&self, pane: &FleetPaneRef) -> Option<AgentStatus>;   // PR 7 uses it
    pub fn visible_rows(&self) -> impl Iterator<Item = FleetSidebarRow<'_>>;  // header, then body unless collapsed
    pub fn visible_row_count(&self) -> usize;                                 // O(hosts), for scroll metrics
}
pub struct HostPickerRow { pub host: HostId, pub active: bool, pub state: HostRowState,
    pub enabled: bool /* see "As built" */,
    pub label: String /* "lab-2  connected 0.8.2-fork  2 blocked" */, pub kind: &'static str }
pub fn host_picker_rows(state: &FleetState) -> Vec<HostPickerRow>;
pub fn host_status_rank(status: AgentStatus) -> u8;   // Blocked 4, Done 3, Working 2, Idle 1, Unknown 0 — upstream's status_priority
```

Per-host agent order: `(Reverse(host_status_rank), Reverse(state_change_seq))`
(upstream `ordered_agent_pane_ids` semantics; `agent_order` from the snapshot
is honoured when `agent_view_label` is set). Workspaces keep snapshot order;
`focused` from the snapshot. A host with a stale snapshot while
`Unavailable`/`Connecting` keeps its rows (dimmed by the renderer via
`state`) — matches `herdr fleet status`'s "kept while down" rule. Labels are
plain strings (no colour); glyphs come from `status` at draw time through the
existing `status_icon`. `generation` increments on every rebuild so the
renderer can cache derived heights per generation.

**As built** (merged; 24 unit tests in `src/fleet/sidebar.rs`)

- **`rebuild` takes a fourth argument, `sort: AgentPanelSortConfig`.** Locked
  decision (k) says agents keep upstream's `ordered_agent_pane_ids` order, and
  that order is a *function of* the user's `[ui] agent_panel_sort`: upstream
  applies the `(Reverse(status_priority), Reverse(state_change_seq))` sort only
  under `Priority` and leaves the snapshot's own workspace grouping alone under
  `Spaces` — which is the shipped default (`src/config/model.rs` `#[default]`).
  A named view (`agent_view_label` set) still wins over both. The preference is
  a parameter, not something a pure module reads for itself, because the user
  can also toggle it live by clicking the agent panel's sort label
  (`src/client/shell/mouse.rs:1938`).
- **`FleetSidebarRow` borrows** (`FleetSidebarRow<'a>`, and `row.host()`
  returns `&'a HostId`), so `visible_rows` is allocation-free per frame and
  PR 5 needs no clone to build a host-qualified hit rect.
- **`enabled: bool` added to `HostHeaderRow` and `HostPickerRow`**, from
  `HostSpec.enabled`. A disabled host is still listed (it is configuration the
  user can see, and dropping it would desynchronize the picker's `1-9` indices
  from the sidebar's groups), but `FleetState::set_active_host` refuses it —
  so PR 5's click-to-switch and PR 6's picker must ignore a row with
  `enabled == false` rather than route to it.
- **Header labels** are `"{▾|▸} {host} · {detail}"`, where detail is the
  non-zero counts (`"2 blocked · 1 working"`), `"no agents"` for a connected
  host whose snapshot has none, `"loading"` for a connected host with no
  snapshot yet, and otherwise the connection summary (`"connecting"`,
  `"reconnecting (attempt 2)"`, `"unavailable"`, `"incompatible"`).
  `host_picker_rows` labels are `"{host}  {connection summary}[  {counts|reason}]"`.
- **Workspaces are filtered through `is_valid_resource_id`** at row-building
  time. Agents are filtered once at ingestion (`retain_addressable_agents`);
  workspaces are not, so a workspace id holding `/` would otherwise mint a
  reference whose string form parses back into a *different* host.
- `host_status_rank` is a copy of upstream's private `status_priority` (the
  purity guard forbids `crate::client` in this module); a test `include_str!`s
  `src/client/shell.rs` and asserts each arm, so upstream renumbering the table
  fails the gate instead of silently reordering fleet groups.
- `HostId::as_str` lost its `#[allow(dead_code)]`; `is_local` keeps its own
  (PR 6's ssh socket scope). The module carries one `#![allow(dead_code)]`
  whose comment says PR 5 deletes it.

**Tests**

- `rebuild` on `FleetState::test_new()` and on the adversarial state: groups
  in host order, `local` first; a disabled host appears with its reason; an
  `Unavailable` host keeps its last snapshot's rows; the active flag follows
  `state.active_host()`; collapsed hosts have `header.collapsed` and no rows
  are dropped (collapse is a render decision).
- Agent order inside a group equals upstream's priority (table pinned);
  `agent_order` honoured; unknown status ranks last.
- `agent_status(pane)` after a status delta; `host_picker_rows` labels for
  each connection state; `host_status_rank` table.
- Purity guard: `sidebar.rs` imports no runtime crates.

**Real-server validation**

Pure state; no servers. Evidence = green `test-one fleet::sidebar` and the
guard test.

**Downstream**

- PR 5 renders `FleetSidebarModel`; PR 6 renders `host_picker_rows`; PR 7
  uses `agent_status`. E4's phone list uses `merged_agents()`, not this model.
- Rows carry `FleetPaneRef`/`FleetWorkspaceRef`: every hit rect built from
  them is host-qualified.
- PR 5 must pass `config.agent_panel_sort` into `rebuild` and treat the agent
  panel's sort toggle as a model-invalidating event, alongside `FleetChange`
  and collapse/active changes.
- `visible_row_count()` counts a header as **one** row. `HostHeaderRow.reason`
  is a *second* dimmed line, so PR 5 either folds the reason into the header
  line or makes its own height cache treat a header as 1-or-2; the
  cross-cutting "inactive-host row heights are constant (1)" wording assumes
  the former.

### PR 3 — refactor: route client writes and fleet events through a server link seam · deps: 1

**Goal:** `run_client_loop` writes through a `ServerLink` and can receive
`FleetEvent`s, with single-host behaviour byte-for-byte unchanged. The
`Fleet` variant is complete but unreachable until PR 4.

**Files**

- `src/client/link.rs` (new): `ServerLink`, `FleetLink`, `LinkWriteError`.
- `src/client/mod.rs` *(upstream file — the one reshaping edit, decision
  (d))*: `mod link;` + `mod fleet;`, `write_to_server(link: &mut ServerLink,
  msg)`, `let mut write_stream = ServerLink::Single(stream)`, the helper
  parameter types (`finish_client_shell_input`,
  `install_client_shell_snapshot`, `dispatch_client_shell_actions`,
  `sync_client_shell_keyboard_report_all`, …: `&mut LocalStream` → `&mut
  ServerLink`), `ClientLoopEvent::Fleet(Box<FleetEvent>)`, a
  `fleet_events: Option<mpsc::Receiver<FleetEvent>>` loop input with a
  `select!` branch guarded by `if fleet_events.is_some()`, and
  `ClientState.fleet: Option<fleet::FleetClientState>` (constructed `None`).
- `src/client/endpoint_commands.rs` *(upstream file — minimal wiring)*:
  `send_next(&mut self, link: &mut ServerLink)` and a new
  `complete(&mut self, request_id, result: Result<Vec<u8>, String>) ->
  Option<EndpointCommandResult>` (fleet responses arrive reassembled).
- `src/client/fleet.rs` (new, stub): `pub(super) struct FleetClientState`
  with the fields PR 4 fills (`state: FleetState`, `connector: FleetConnector`,
  `active: HostId`, `pending_switch: Option<HostId>`) and
  `pub(super) fn translate(&mut self, event: FleetEvent) -> Translated`
  where `Translated::{Server(Box<ServerMessage>), Snapshot(Box<ClientShellSnapshot>),
  EndpointResponse{request_id, result}, Changes(Vec<FleetChange>), Dropped}`.

**Shapes/approach**

```rust
// link.rs
pub(super) enum ServerLink { Single(LocalStream), Fleet(FleetLink) }
pub(super) struct FleetLink { connector: Arc<FleetConnector>, active: HostId }
impl ServerLink {
    pub(super) fn write(&mut self, msg: &ClientMessage) -> io::Result<()>;
    // Single: protocol::write_message (today's write_to_server body).
    // Fleet: match msg — ClientShellEndpointRequest → unreachable here (EndpointCommands sends
    //   HostCommand::Endpoint); Detach → Err(LinkWriteError::Detached) which the loop turns into the
    //   clean ServerShutdown{"detached"} exit; everything else → connector.send(&active, HostCommand::Raw(msg)).
    //   HostSendError::NotConnected / Io → Ok(()) + tracing::debug (host-local; the supervisor reconnects;
    //   PR 8 makes it visible). UnknownHost/Refused → io::Error (a bug, surfaced).
    pub(super) fn set_active(&mut self, host: HostId);   // Fleet only
}
```

The `Fleet` payload holds an `Arc<FleetConnector>` because `send` takes
`&self` and the loop holds the receiver separately (PR 1's `take_events`).
`ClientLoopEvent::Fleet` is handled at the top of the loop body: `Translated::
Server(msg)` is re-dispatched into the existing `ServerMessage` match (no arm
duplicated), `Snapshot` goes to `install_client_shell_snapshot`,
`EndpointResponse` to `endpoint_commands.complete` + `shell
.handle_endpoint_result`, `Changes` to `fleet::apply_changes` (a no-op until
PR 5), `Dropped` continues. In this PR the branch is compiled but
`fleet_events` is always `None` and `ClientState.fleet` is always `None`.

**Tests** (characterization first: the whole existing client suite,
`tests/client_mode.rs`, `tests/multi_client.rs`, `tests/detach_reattach.rs`
must pass unchanged — commit them green on the pre-refactor tree in the
branch's first commit if any needed touching, which they should not)

- `link.rs`: `Single` writes exactly `write_message` bytes (compare against a
  reader on a socketpair); `Fleet` against `test_support::FakeHost`
  through a real `FleetConnector` (`fake_connector(&[("alpha", &alpha),
  ("beta", &beta)], options)`; the module is `#[cfg(all(test, unix))]`, so
  those tests are unix-gated): `ClientShellPaneInput` reaches the active
  fake host and **not** the other one; `Detach` → `Detached`; a write while
  the active host is `NotConnected` returns `Ok` and logs; `set_active`
  redirects the next write.
- `endpoint_commands.rs`: `send_next` on `Fleet` sends `HostCommand::Endpoint
  { request_id == request.id }`; `complete` with a matching id yields the
  result and clears in-flight; a foreign id is ignored; `expire` unchanged.
- `fleet.rs::translate`: `Surface{host != active}` → `Dropped`;
  `Surface{active}` → `Server(PaneSurface)`; `Notification` → `Server` with
  the title prefixed `[host] ` (see PR 4/7 for targeting); `Host{Snapshot}`
  for the active host → `Snapshot`, for another host → `Changes`.

**Real-server validation** (behaviour preservation)

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 1 && eval "$(bash scripts/fork/fleet-lab.sh env)"
python3 - <<'EOF'
import os, pty, select, time, fcntl, termios, struct
b = os.path.abspath("target/debug/herdr")
env = {k: v for k, v in os.environ.items() if k not in ("HERDR_SOCKET_PATH","HERDR_CLIENT_SOCKET_PATH","HERDR_ENV")}
env["TERM"] = "xterm-256color"; env["HERDR_DISABLE_SOUND"] = "1"
pid, fd = pty.fork()
if pid == 0:
    os.execve(b, [b, "--session", "lab-1"], env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
buf = b""; deadline = time.time() + 30
while time.time() < deadline and b"herdr-fleet-lab:lab-1" not in buf:
    r, _, _ = select.select([fd], [], [], 1)
    if r:
        try: buf += os.read(fd, 65536)
        except OSError: break
print("MARKER_SEEN" if b"herdr-fleet-lab:lab-1" in buf else "MARKER_MISSING")
os.write(fd, b"echo seam-ok\r"); time.sleep(1); os.write(fd, b"\x02q"); time.sleep(1)
EOF
env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | grep -c seam-ok   # 1
bash scripts/fork/fleet-lab.sh down
```

Evidence: `MARKER_SEEN`, the typed line reached the pane through
`ServerLink::Single`, detach (`prefix+q`) exits cleanly, gate green.

**As built (PR 3, merged).** Deviations from the shapes above, all
deliberate:

- **`ClientLink` replaces the separate `fleet_events` parameter.**
  `run_client_loop`'s first argument is `ClientLink { link: ServerLink,
  fleet_events: Option<mpsc::Receiver<FleetEvent>> }` — the loop's whole
  server side in one value. Two parameters would have pushed the signature to
  12 arguments and failed `clippy::too_many_arguments`. `ClientLink::single
  (stream)` is the single-host constructor; PR 4 builds the fleet one.
- **`Rc<FleetConnector>`, not `Arc`,** in both `FleetLink` and
  `FleetClientState`. `FleetConnector` owns a `std::sync::mpsc::Receiver`
  (the supervisors' completion channel), so it is `!Sync` and `Arc::new`
  fails `clippy::arc_with_non_send_sync`. `run_client_loop` runs under
  `rt.block_on` on a `new_current_thread` runtime and spawns nothing, so the
  future needs no `Send` bound and both owners live on one thread. **PR 4's
  exit path is `Rc::try_unwrap(connector).shutdown()`** after dropping the
  taken receiver and the link.
- **`LinkWriteError` has three variants:** `Io`, `Detached` and
  `HostUnavailable(String)`. `Detach` on a fleet link returns `Detached`,
  which `link::io_result` turns into `Ok(())` with a `debug!` — no
  synthesized `ServerShutdown{"detached"}` is needed, because all four
  `Detach` call sites are already `let _ = write(…)` followed by an immediate
  clean return. `HostUnavailable` exists because `FleetConnector::send`
  answers **only** endpoint requests it accepted: a `NotConnected` endpoint
  request would otherwise hold `EndpointCommands`' single lane forever
  (upstream's `expire` keeps the lane after its 60 s timeout, waiting for a
  late answer that cannot come). A raw write still drops `NotConnected`/`Io`
  with a `debug!` per the plan; an endpoint request records the reason on the
  in-flight command (`InFlightCommand.unavailable`) and the next `expire`
  tick fails it with `endpoint_unavailable` **and releases the lane**. That
  field is only ever set by a fleet link, so the single-host path is
  untouched.
- **`Translated::Snapshot { snapshot, changes }`** carries the fleet model's
  changes alongside the projection, so an active host's snapshot cannot
  silently drop its `FleetChange`s. The active host's snapshot is cloned once
  per snapshot event (the shell draws it, the fleet model counts its agents);
  no other host's is.
- **`endpoint_commands::send_next` writes through
  `ServerLink::write_endpoint_request(boot_id, request_id, request)`**, which
  owns the per-link correlation: the boot id on the wire for a single host,
  `HostCommand::Endpoint { request_id }` for a fleet host. `complete
  (request_id, result)` matches only the in-flight request id.
- **The `select!` fleet branch is unguarded**: `ev = next_fleet_event(
  fleet_events.as_mut())`, where `next_fleet_event` returns
  `std::future::pending()` both when there is no receiver and when the channel
  closed. An `if fleet_events.is_some()` guard plus a `None` arm would have
  spun the loop once the last supervisor exited.
- **`write_to_server` kept its name and its 21 call sites**; the raw-stream
  body moved to `link::write_stream_message`, which `terminal_sessions.rs`
  (four sites, its own stream) and the pre-loop `AttachTerminal` write use.
  `finish_client_shell_input`, `install_client_shell_snapshot`,
  `dispatch_client_shell_actions`, `write_remote_image_to_server` and
  `write_attach_semantic_action` take `&mut ServerLink`.
- **One helper was extracted rather than duplicated:**
  `finish_endpoint_command(state, completed, …) -> Result<bool, ClientError>`
  holds what the `ClientShellEndpointResponseChunk` arm used to do inline, so
  the fleet path replays mouse events, repaints and detaches identically.
  Its one `expect("shell endpoint response")` became a let-else returning
  `Ok(false)` (unreachable: a non-empty replay only comes from
  `handle_endpoint_result`, which needs a shell).
- **Characterization:** no file under `tests/` was touched;
  `tests/client_mode.rs`, `tests/multi_client.rs`, `tests/detach_reattach.rs`
  and the rest of the client suite pass unchanged (3336 tests green).
- **Hazard recorded for PR 4/5:** `translate` drops an *inactive* host's
  `EndpointResponse`, so a request still in flight to the old host at switch
  time is released only when the switch resets the lane. Whichever PR lands
  the host switch must add `EndpointCommands::reset()` and call it there; it
  does not exist yet.

**Downstream**

- PR 4 constructs `ServerLink::Fleet` and `Some(fleet_events)`; nothing else
  in the loop changes for fleet mode.
- `ServerLink::write` is the **only** write path; E7's per-host requests go
  through `FleetLink` with an explicit host, never around it.
- Upstream edits to `write_to_server` call sites merge cleanly (same text);
  a new upstream helper taking `&mut LocalStream` must be given `&mut
  ServerLink` at merge time — note this in the README's fork-wiring table
  (PR 9).

### PR 4 — feat: herdr fleet opens the client shell over the fleet connector · deps: 2, 3

**Goal:** `herdr fleet` (and `herdr --fleet`) opens the client-owned shell with
N host connections; the sidebar and pane area show the active host exactly
as today; pane input, resize, focus, mouse, endpoint commands go to the
active host; every exit path shuts the connector down. Host groups come in
PR 5 — this PR proves the loop.

**Files**

- `src/client/fleet.rs`: `run_fleet()`, `FleetClientState` filled in,
  `translate` completed, `StderrRedirect` (unix), exit/shutdown handling.
- `src/client/mod.rs` *(upstream — minimal wiring)*: `pub use fleet::run_fleet;`
  and a `fleet: Option<fleet::FleetLaunch>` third argument on
  `run_client_with_mode` (or a `run_client_with_launch(ClientLaunch)` wrapper
  it delegates to — pick whichever keeps the existing two callers untouched)
  so the fleet path shares config loading, geometry, terminal setup, panic
  hook, runtime, `ctrlc`, and the exit handling. **As built in PR 3:**
  `run_client_loop` already takes a `link::ClientLink`, so the fleet branch
  passes `ClientLink { link: ServerLink::Fleet(FleetLink::new(connector,
  active)), fleet_events: Some(rx) }` and sets `ClientState.fleet =
  Some(FleetClientState { .. })` — no further loop surgery. The reader thread
  and the write stream's `set_nonblocking(false)` are already inside
  `if let ServerLink::Single`, so a fleet console starts neither.
- `src/cli/fleet.rs` (fork file): `run_fleet_command -> io::Result<Option<i32>>`
  (`None` for a bare `herdr fleet`); help text mentions the console.
- `src/cli.rs` *(upstream — the `fleet` arm becomes the `server`-arm pattern)*;
  `src/cli/spec.rs` *(upstream — `fleet_command().about("Open the Fleet
  console, or inspect the configured fleet")`)*; `src/main.rs` *(upstream —
  `"--fleet"` in `known_flags`, one usage line, the launch branch before the
  `--remote` branch)*.
- `scripts/fork/tui-drive.py` (new): `--cols --rows --timeout --expect TEXT
  [--expect …] --keys BYTES … --dump -- <cmd…>`; spawns in a pty with
  `TIOCSWINSZ`, strips ANSI (CSI/OSC/charset sequences) into a rolling screen
  text, waits for each expectation in order, injects keys, prints the final
  stripped text with `--dump`, exit 0/1.
- `tests/support/fleet_tui.rs` (new): `FleetConsole::spawn(lab: &Lab,
  config_toml: &str, cols, rows) -> FleetConsole` (portable-pty, env from the
  lab, `HERDR_DISABLE_SOUND=1`, `TERM=xterm-256color`), `wait_for_text(needle,
  timeout) -> bool` (ANSI-stripped accumulated output), `send(bytes)`,
  `screen_text()`, `Drop` (send `prefix+q`, then kill).
- `tests/fork_fleet_tui.rs` (new, `#![cfg(unix)]`):
  `fleet_console_shows_the_active_host_and_routes_input_to_it`.

**Shapes/approach**

```rust
// src/client/fleet.rs
pub fn run_fleet() -> io::Result<()>;   // main.rs entry
pub(super) struct FleetLaunch { pub specs: Vec<HostSpec>, pub options: FleetConnectorOptions }
pub(super) struct FleetClientState { pub state: FleetState, pub connector: Rc<FleetConnector>,
    pub active: HostId, pub pending_switch: Option<HostId>, pub sidebar: FleetSidebarModel,
    pub collapsed: HashSet<HostId>, pub stderr: Option<StderrRedirect> }
```

Launch sequence (in `run_client_with_mode`'s fleet branch): `Config::load()`
→ `resolve_hosts(&config.fleet)` (`Err(diagnostics)` → print each, exit 1;
zero enabled hosts → "no fleet hosts enabled in [fleet]; see docs/fork/
fleet-core.md", exit 1) → `ClientShellConfig::from_config(..)
.with_keybinding_source(Local).with_local_endpoint(&config_dir().join(
"fleet"))` (chrome preferences persist per console, not per host) →
`initial_terminal_geometry(false, false)` → `setup_terminal` → panic hook →
`StderrRedirect::install()` (unix) → `FleetConnector::start(specs,
FleetConnectorOptions::for_client(&config, HandshakeParams::for_client(cell_w,
cell_h, exact && unix, mouse_capture), ActiveGeometry { surface:
shell_config.initial_surface_size(cols, rows), .. }))` → `take_events()` →
`set_active(Some(first enabled host))` → `run_client_loop(ClientLink { link:
ServerLink::Fleet(..), fleet_events: Some(rx) }, …, endpoint_methods: None)`
(one `ClientLink` argument, PR 3 As built). Exit: after the loop
returns (any reason) drop the receiver, drop the link, then
`Rc::try_unwrap(connector)` — `Rc`, not `Arc` (PR 3 As built) —
`.shutdown()` (fall back to `Drop` with a `warn!` if another owner remains),
restore stderr, then the existing terminal-restore/exit code. The fleet
branch never `exit(1)`s for a host reason.

Event translation (completes PR 3's stub): `Host{host, event}` → `state.apply`
→ if `host == active`: `Connected{methods}` → `shell.set_endpoint_methods
(Some)`, `Snapshot` → `Translated::Snapshot` (+ `Changes`), a transition out
of `Connected` → `Changes` (PR 8 renders it); `Surface`/`SurfacePatch`/
`ServerMessage` → `Server` only when `host == active`, else `Dropped`;
`Notification` → title `[{host}] {title}` then `Server(SemanticNotification)`
(all hosts; PR 7 fixes targeting); `EndpointResponse` → for the active host
only. `ClientLoopEvent::Resize` additionally calls `connector
.set_active_geometry(..)` (the existing `ClientShellResize` write still goes
through the link, which the connector bookkeeps). `Detach` (`prefix+q`) ends
the console. Set `is_remote_client` semantics: fleet mode is not a remote
client process; `remote_image_paste_key` inactive.

**Tests**

- `src/client/fleet.rs` with two `test_support::FakeEndpoint`s: after start,
  the first host is active and receives the activation resize; `translate` of
  a `Surface` from the second host is `Dropped`; a `PaneInput` write through
  `ServerLink::Fleet` is recorded by host 1 only; a fake host that never
  answers does not delay the loop's start (the console loop is entered before
  any handshake completes — assert with a never-answering listener);
  shutdown after `take_events` releases both supervisors.
- `run_fleet_command`: bare → `None`; `status …` unchanged; `herdr fleet
  bogus` → usage, exit 2. `spec.rs` invariants green. `main.rs`: `--fleet`
  accepted, `--fleet` with `--remote` rejected (exit 2, message).
- `tests/fork_fleet_tui.rs`: `Lab::up("2")`, write `[fleet]` (lab-1, lab-2,
  `include_local = false`) into `<root>/xdg/<app_dir_name>/config.toml`,
  `FleetConsole::spawn(.., 120, 40)`; `wait_for_text("herdr-fleet-lab:lab-1",
  20 s)` (the active host's pane renders); `send(b"echo tui-1\r")`; `Lab::herdr
  ("lab-1", ["pane","read",pane_1,"--source","recent"])` contains `tui-1`
  and lab-2's does not; drop → console exits within 5 s and, on Linux,
  `support::herdr_server_pids_for_runtime_dir` still lists exactly the two
  lab servers (the console started nothing).

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
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 30 \
  --expect 'herdr-fleet-lab:lab-1' --keys 'echo from-console\r' --expect 'from-console' --keys '\x02q' --dump \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV HERDR_DISABLE_SOUND=1 target/debug/herdr fleet
$H --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | grep -c from-console   # 1
$H --session lab-2 pane read "$HERDR_FLEET_LAB_PANE_2" --source recent | grep -c from-console   # 0
ls /tmp/herdr-remote-* 2>/dev/null | grep -c "$(pgrep -f 'herdr fleet$' || echo none)"        # no forward sockets left (local hosts have none)
$H --fleet --help | grep -c fleet     # alias accepted by the arg parser
bash scripts/fork/fleet-lab.sh down
```

Also once with the ssh lab: `lab-1` as `kind = "ssh", target =
"herdr-ssh-lab"` run with `HOME=$HERDR_SSH_LAB_HOME`; after `prefix+q`, no
`/tmp/herdr-remote-<pid>-lab-1-*.sock` remains (shutdown ran) and the herdr
log (not the terminal) holds any ssh stderr. Evidence: the dumped screen
text, both `pane read` counts, the socket check, exit code 0.

**As built (PR 4, merged).** Deviations from the shapes above, all deliberate:

- **`ClientLaunch` / `run_client_with_launch`, not a fourth parameter.**
  `run_client_with_mode` keeps its signature and its two callers and delegates
  to `run_client_with_launch(ClientLaunch { attach_request, attach_escape,
  log_message, fleet: Option<fleet::FleetLaunch> })`. `FleetLaunch` is a
  marker: the hosts come from the config that path already loads and the
  geometry from the terminal it already reads, so carrying `specs`/`options`
  would mean loading the config twice. `run_fleet()` is
  `run_client_with_launch(ClientLaunch::fleet())`.
- **`ClientLink` grew a third field, `fleet: Option<FleetClientState>`**
  (fork-owned `src/client/link.rs`), rather than a new `run_client_loop`
  parameter: the link and the console state must name the same host from the
  loop's first iteration, and the signature is already at clippy's argument
  limit. `run_client_loop` destructures the link at the top and moves the
  state straight into `ClientState.fleet`.
- **The single-host path became `Option`-shaped, not branched.** `stream:
  Option<LocalStream>` and `handshake: Option<HandshakeResult>`; a fleet
  console has neither. The loop's encoding falls back to
  `RenderEncoding::SemanticFrame` (only a `debug!` reads it) and
  `endpoint_methods` to `None` — the active host publishes its own when it
  connects.
- **`Translated::EndpointMethods { methods, changes }` is a fifth variant.**
  The active host's `HostEvent::Connected` publishes its method list to the
  shell (which gates commands on it); every other host's stays a `Change`.
  A test pins that another host's connection never touches the method list.
- **The console never re-picks the active host.** `console_link` reads
  `FleetConnector::active_host()` back and gives that id to the link and to
  `FleetState::set_active_host`, so the connector's own duplicate-id dedupe and
  "first *enabled* spec" rule cannot disagree with the state's. A test covers a
  disabled first spec.
- **`Console` is a guard, not a code path.** `open_console` returns
  `(ClientLink, Console)`; the launch path holds the `Console` across
  `rt.block_on`, so *every* exit runs `Rc::try_unwrap(connector).shutdown()` —
  including a panic, which unwinds through it after the hook restored the
  terminal. The loop's own locals (the taken receiver, the link, `ClientState
  .fleet`) drop when the loop future completes, i.e. **before** the guard, which
  is the "drop the receiver first" rule PR 1 recorded. `drop(console)` is
  explicit right after `block_on` because the error path below it ends in
  `std::process::exit(1)`, which runs no destructors.
- **stderr (decision (j)) is a static plus a guard.** `install_console_stderr`
  (unix) opens `herdr-client.log` with `O_APPEND` — the same flag the tracing
  writer uses, so the two interleave safely — saves fd 2 with
  `pty::fd::duplicate_cloexec_fd` into an `AtomicI32`, and `dup2`s the log over
  fd 2. `restore_console_stderr()` swaps the static, so it is idempotent and
  safe from the panic hook, which now calls it; `ConsoleStderr` restores on
  `Drop`, so a failed `open_console` cannot print its error into a log the user
  cannot see.
- **A terminal resize is announced, not written twice.** In fleet mode the
  `ClientLoopEvent::Resize` arm calls `FleetClientState::announce_geometry`
  (→ `set_active_geometry`, which adopts unconditionally and writes only when
  the active host is connected *and* the geometry changed) and **skips** the
  link write: `HostCommand::Raw(ClientShellResize)` would have sent the active
  host the same resize a second time.
- **An inactive host's notification carries no target.** The shell resolves
  `workspace_id`/`tab_id`/`pane_id` against the *active* host's snapshot
  (`notification_still_current`, `notification_target_is_active`, the toast's
  click → `PaneFocus`), and every server starts at `w1`/`w1:p1`, so another
  host's ids would be validated, suppressed or focused on the wrong machine.
  Until PR 7 targets across hosts, `translate` clears the three ids for
  `host != active` and keeps the `[host] ` prefix, the agent name and the
  sound.
- **`tests/cli/fleet.rs` changed** (fork-owned, from E1): bare `herdr fleet` is
  no longer a usage error, so it moved out of `fleet_usage_errors_exit_two`'s
  list into its own assertion (not exit 2, and never the usage line).
- Decision (h) in practice: `kitty_graphics_enabled` and
  `pixel_geometry_enabled` are forced false in fleet mode, so
  `initial_terminal_geometry(false, false)` reports cell size 0 and
  `exact == false`; `remote_image_paste_key` is `None` and `is_remote_client`
  is `false` regardless of `HERDR_REMOTE_KEYBINDINGS` (which decision (g)
  ignores with a `warn!`).
- `--fleet` combined with `--remote` is already rejected by `main.rs`'s
  existing "`--remote` can only be used with the default launch command" guard
  (exit 2); no new check was needed.
- **Deferred, with owners:** `EndpointCommands::reset()` (PR 3's recorded
  hazard) is **not** added — PR 4 has no host switch, and an active host that
  drops fails its in-flight lane through the connector's
  `report_lane_failure`, so nothing can hit the hazard yet. **PR 5 must add it
  and call it in `switch_host`.** `herdr --fleet status` silently opens the
  console (the trailing word is ignored); cosmetic, unfixed.

**Real-server evidence (PR 4).** Two lab hosts, then the same run with lab-1
behind `ssh-lab.sh` (`sshd` was present, so no degradation to record): the
console renders lab-1's marker pane, `echo` reaches lab-1's pane and not
lab-2's, `prefix+q` exits 0, no `/tmp/herdr-remote-*` socket survives, and the
herdr log — not the terminal — carries the ssh transport's output.

**Downstream**

- `FleetClientState` is the loop-side owner of `FleetState`; PRs 5–8 add to
  it, never a second state. `fleet::switch_host(state, shell, link, host,
  then_focus)` (PR 5) is the only place the active host changes.
- `tests/support/fleet_tui.rs` and `scripts/fork/tui-drive.py` are the PTY
  fixtures for every later PR and for E7/E8 validation; add helpers, never
  rename. **As built:** `FleetConsole::{spawn, spawn_with_args, screen_text,
  wait_for_text, send, detach, wait_for_exit}` plus the free helpers
  `append_lab_config`, `lab_fleet_config(count)`, `lab_pane_id(lab, index)`,
  `pane_text(lab, session, pane)`, `lab_client_socket`, `assert_screen`,
  `strip_ansi`, `app_dir_name` and `DETACH_KEYS`; the module is
  `#[cfg(unix)] pub mod fleet_tui` in `tests/support/mod.rs` and carries a
  file-level `#![allow(dead_code)]` because each test binary uses a different
  subset. `FleetConsole` takes the pty writer once at spawn
  (`MasterPty::take_writer` panics on a second call) and its `Drop` sends
  `prefix+q`, waits 5 s, then kills. `tui-drive.py` keeps its documented flags
  (`--cols --rows --timeout --expect --keys --dump -- cmd…`); expectations
  match only text produced *after* the previous step, and it reaps its child
  from `finally`.
- The console never installs, stops or hands off a herdr (E1 decision (f)).
  Pinned by `tests/fork_fleet_tui.rs`, which compares
  `support::herdr_server_pids_for_runtime_dir` before and during the console.

### PR 5 — feat(fleet): sidebar host groups with live status and click-to-switch · deps: 4

**Goal:** the sidebar shows a host group per configured host — header with
counts, that host's workspaces and agents with the existing glyphs, dimmed
unreachable hosts with the reason, collapse by click — and clicking a host
header, workspace or agent of another host switches the active host (and
focuses the target there). Profiled at 1 vs 5 hosts × 15 agents.

**Files**

- `src/client/shell/fleet.rs` (new): `FleetShellState`, `impl
  ClientShellState { fn fleet_sidebar_update, fn reset_for_host_switch, … }`,
  `FleetSidebarHit`, `FleetShellAction`.
- `src/client/shell/fleet_sidebar.rs` (new): `render_fleet_spaces`,
  `render_fleet_agents`, row drawing, height cache.
- `src/client/shell/state.rs` *(upstream — three one-line additions)*:
  `ClientShellState.fleet: Option<FleetShellState>`, `ShellHitMap.fleet_rows:
  Vec<(Rect, FleetSidebarHit)>`, `ClientShellAction::Fleet(FleetShellAction)`.
  Note `FleetSidebarRow<'a>` borrows (PR 2 As built), so a hit rect can hold a
  cloned `HostId`/ref without cloning the model.
- `src/client/shell.rs` *(upstream — `mod fleet; mod fleet_sidebar;`)*.
- `src/client/shell/sidebar.rs`, `agent_sidebar.rs` *(upstream — one
  delegation each: when `state.fleet.is_some()` the section body is drawn by
  `fleet_sidebar::render_fleet_spaces/agents`, which reuse
  `render_workspace_rows`/`render_agent_panel` internals for the active
  host)*; `src/client/shell/mouse.rs` *(upstream — one `fleet_rows` hit test
  before the workspace/agent hit tests)*.
- `src/client/fleet.rs`: `switch_host`, `apply_changes` (rebuild model →
  `shell.fleet_sidebar_update`), handle `ClientShellAction::Fleet`.
  **From PR 4:** `FleetClientState { state, connector: Rc<FleetConnector>,
  active, pending_switch }` already exists and is reached through
  `ClientState.fleet`; `apply_changes(fleet, changes)` is already called from
  the loop's fleet branch for every translated event (it only `debug!`s
  today). `pending_switch` carries an `#[allow(dead_code)]` whose comment says
  PR 5 writes it — remove the allow. `switch_host` **must** add and call
  `EndpointCommands::reset()` (PR 3's recorded hazard: `translate` drops an
  inactive host's `EndpointResponse`, so a request in flight to the old host
  would hold the single lane forever); it does not exist yet. **PR 2
  as-built:** `FleetSidebarModel::rebuild(&state, &collapsed, sort)` takes the
  live `config.agent_panel_sort`, and the agent panel's sort toggle
  (`src/client/shell/mouse.rs:1938`) invalidates the model like a
  `FleetChange` does. Ignore a hit on a header/row whose
  `HostHeaderRow.enabled` is `false` — `set_active_host` refuses a disabled
  host. Header rows count as one row in `visible_row_count()`; if the
  `reason` line is drawn separately, the height cache owns that second line.
- `src/client/shell/tests/fleet_sidebar.rs` (new) and
  `src/client/shell/tests/fleet_scale.rs` (new, `#[ignore]` profile);
  `justfile` *(upstream — `bench-fleet-scale` recipe after
  `bench-render-scale`)*.
- `tests/fork_fleet_tui.rs`: `sidebar_lists_every_host_and_click_switches`.

**Shapes/approach**

```rust
// src/client/shell/fleet.rs
pub(super) struct FleetShellState { pub model: FleetSidebarModel, pub active: HostId,
    pub switching_to: Option<HostId>, pub heights_generation: u64, pub group_heights: Vec<u16> /* cache */ }
pub(super) enum FleetSidebarHit { HostHeader(HostId), HostCollapse(HostId),
    Workspace(FleetWorkspaceRef), Agent(FleetPaneRef) }
pub(crate) enum FleetShellAction { SwitchHost { host: HostId, then_focus: Option<FleetFocusTarget> },
    ToggleCollapsed(HostId) }
pub(crate) enum FleetFocusTarget { Workspace(String), Pane(String) }   // ids on the target host
impl ClientShellState {
    pub(crate) fn fleet_sidebar_update(&mut self, model: FleetSidebarModel, active: HostId);
    pub(crate) fn reset_for_host_switch(&mut self);   // pane_surface = None, hits, leases, popup, scroll, pending requests, overlay stays
}
```

Rendering: in fleet mode the spaces section is, per host in model order, a
header row (`▾`/`▸`, host name, `· N blocked · M working` from the cached
label, connection text dimmed when not connected, second dimmed line with
the reason when `Unavailable`/`Incompatible`) followed by — for the active
host — today's `workspace_entries` rows (unchanged code, so worktree
grouping, tokens and existing hits keep working) and — for other expanded
hosts — one 1-line row per workspace (`status_icon` + label). The agents
section mirrors it: header per host, active host's `render_agent_panel` rows,
other hosts' 1-line agent rows. Heights: the active host's `row_heights` as
today; every other row is 1, cached per `model.generation`; one
`list_scroll_metrics` over the concatenation; draw with `skip(scroll)` and
`break` at the bottom. No formatting for inactive hosts at draw time. Active
host header is highlighted; a host being switched to shows `…`.

Hits: header → `SwitchHost{host, None}` (or `ToggleCollapsed` on the
`▾`/`▸` cell); another host's workspace → `SwitchHost{host, Workspace(id)}`;
another host's agent → `SwitchHost{host, Pane(id)}`; the active host's rows
keep today's behaviour (focus requests on the active host).

`switch_host` (loop side, `src/client/fleet.rs`): refuse if the host is
disabled or already active; `pending_switch = Some(host)`; `connector
.set_active_geometry(current)`; `connector.set_active(Some(&host))`; `link
.set_active(host)`; `state.set_active_host(Some(host))`; `endpoint_commands
.reset()`; `shell.reset_for_host_switch()`; if the host has a snapshot:
`shell.set_snapshot(snapshot.clone())` + `set_endpoint_methods(methods)`;
model rebuild; `then_focus` → enqueue the `pane.focus`/`workspace.focus`
request the navigator uses (`accept_navigator_selection`'s mapping) once the
snapshot is installed; the pane area shows "switching to <host>…" until the
first `PaneSurface` (`Translated::Server(PaneSurface)` for the new active
host clears `pending_switch`). Input typed during the switch is routed to
the new host by construction (the link's active id changed first); the
shell's snapshot is already the new host's, so pane ids match.

Perf profile: `fleet_sidebar_scale_profile` builds `FleetState` with H hosts
whose snapshots are the fixture with 15 synthetic agents each (status spread
across the five values), a `ClientShellState` in fleet mode with the model
installed, and times `compose(120, 40)` for H ∈ {1, 5} with inactive groups
collapsed and expanded, plus `FleetSidebarModel::rebuild` per H; prints the
`render_scale_benchmark` style table. Recipe: `just bench-fleet-scale` =
`cargo test --release --locked --bin herdr fleet_sidebar_scale_profile --
--ignored --nocapture --test-threads=1`.

**Tests**

- `tests/fleet_sidebar.rs` (shell, no PTY): with a two-host model, `compose`
  output contains both headers, the inactive host's workspace label and agent
  names, dimmed styling for an `Unavailable` host and its reason; the active
  host's rows are the existing ones; collapsing hides rows; `hits.fleet_rows`
  carry `FleetPaneRef`s of the right host; a click on the other host's agent
  yields `ClientShellAction::Fleet(SwitchHost{host, Pane})` and **no**
  `PaneFocus` request; a click on the active host's agent yields the usual
  focus request and no fleet action; scroll metrics with 5 hosts × 15 agents
  cover every row; row model generation change invalidates the height cache.
- `src/client/fleet.rs`: `switch_host` sequence against two fake endpoints —
  the old host receives the inactive resize, the new the active one; a
  `Surface` from the old host after the switch is `Dropped`; input after the
  switch is recorded by the new host only; switching to a disabled host is
  refused with a log; `then_focus` sends exactly one focus request to the
  new host after its snapshot.
- `tests/fork_fleet_tui.rs`: both host headers and both `lab-N` workspace
  labels visible; click (SGR mouse press bytes at the lab-2 header row found
  from the screen text) → `wait_for_text("herdr-fleet-lab:lab-2")`; type a
  marker → lab-2's pane has it, lab-1's does not.

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 5 && eval "$(bash scripts/fork/fleet-lab.sh env)"
# [fleet] with lab-1..lab-5 as local hosts (include_local = false)
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 30 \
  --expect 'lab-1' --expect 'lab-5' --dump -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV HERDR_DISABLE_SOUND=1 target/debug/herdr fleet \
  | tee /tmp/e2-pr5.screen | grep -E '▸|▾' | head        # one header per host with counts
# a working agent on lab-3 shows up under its header without switching:
$H --session lab-3 pane run "$HERDR_FLEET_LAB_PANE_3" 'yes >/dev/null'
python3 scripts/fork/tui-drive.py … --expect 'lab-3' --dump … | grep -A3 'lab-3'   # row shows the working glyph
# click-to-switch: send an SGR press on the lab-2 header (row from the dump), then type
python3 scripts/fork/tui-drive.py … --keys '\x1b[<0;3;ROW M\x1b[<0;3;ROW m' --expect 'herdr-fleet-lab:lab-2' --keys 'echo via-click\r' --keys '\x02q' …
$H --session lab-2 pane read "$HERDR_FLEET_LAB_PANE_2" --source recent | grep -c via-click   # 1; lab-1: 0
$H --session lab-4 session stop lab-4; sleep 2   # dimmed group with the reason, console still usable
python3 scripts/fork/tui-drive.py … --expect 'lab-4' --dump … | grep -A2 'lab-4'   # "unavailable" + reason, dimmed
just bench-fleet-scale     # paste the table: compose median 5 vs 1 hosts ≤ 1.15× collapsed, ≤ 1.5× expanded
bash scripts/fork/fleet-lab.sh down
```

Evidence: the screen dumps, the two `pane read` counts, the bench table.

**As built (PR 5, merged).** Deviations from the shapes above, all deliberate:

- **`src/client/shell/fleet_sidebar.rs` is a `#[path]` submodule of `render`,
  not of `shell`** — the same trick `render.rs` already uses for `sidebar.rs`,
  `tabs.rs` and `overlays.rs`. That is what lets it reuse
  `render::sidebar`'s row renderers for the active host through `pub(super)`
  rather than copying them. `src/client/shell.rs` therefore gains only
  `mod fleet;` plus a `pub(crate) use fleet::{FleetFocusTarget,
  FleetShellAction, FleetSidebarHit};` re-export.
- **PR 2's flat row order was removed.** The sidebar is *two* lists (spaces
  above, agents below) and each host contributes a header to both, so the
  renderer walks `FleetSidebarModel::groups` once per section.
  `FleetSidebarRow`, `visible_rows` and both `visible_row_count`s therefore had
  no consumer and no PR that would give them one (E4's phone list uses
  `merged_agents()`, as this plan already says), so they are gone rather than
  carrying an open-ended `#[allow(dead_code)]`; the five tests that covered
  them assert the same facts over `groups`.
- **Two upstream files beyond the plan's list, one line each:**
  `src/client/shell/render.rs` (`ShellRenderState.fleet: Option<&FleetShellState>`,
  plus `hits.fleet_rows.clear()` in the `!mouse_capture` reset and the
  `#[path]` module line) and `src/client/shell/composition.rs` (`fleet:
  self.fleet.as_ref()` in the render state it builds). The model has to reach
  the renderer somehow, and those are the two places that build it.
- **`src/client/mod.rs` is edited by this PR** (the plan reserved it for PR 3/4).
  `dispatch_client_shell_actions` takes `&mut ClientState` instead of `&mut
  Vec<Child>` and returns the mouse replay plus whether the console needs
  recomposing. Reason: a fleet action
  changes the routing target, so it needs the shell, the fleet state, the link
  and the endpoint lane at once; and the frame its callers composed *before*
  the dispatch describes the machine the console just left, so they must
  recompose rather than present it. `handle_fleet_event` also gained the new
  `apply_changes(state, changes) -> bool` signature, a `flush_pending_focus`
  call after the active host's snapshot installs, `sync_switching_notice`, and
  `fleet::present(state)` on the arms that change only the sidebar (another
  host's status is nothing else in the loop would repaint for).
- **`sidebar.rs` and `agent_sidebar.rs` were split, not just delegated.**
  `render_sidebar`'s workspace body became `render_workspace_body` and
  `render_agent_panel`'s chrome became `render_agent_panel_header` +
  `render_no_matching_agents`, so both the single-host and the console paths
  draw the identical rows; `workspace_rows`, `render_workspace_rows`,
  `displayed_workspace_status`, `parent_group_key`, `agent_rows`,
  `render_agent_row` and `AgentRow` became `pub(super)`. Both delegations live
  in `sidebar.rs` (`agent_sidebar.rs` has none). The diff on `sidebar.rs` is
  large because a block moved, not because it was rewritten.
- **`fleet_sidebar_update(model, active, switching_to)` takes three arguments**
  (the plan sketched two): both are read by the same frame and a separate
  setter would let a caller install one without the other.
  `set_fleet_switching(Option<HostId>) -> bool` updates just the marker in
  place, so a switch finishing costs no model clone.
- **`fleet_sidebar_matches` gates the repaint.** A rebuild is cheap; installing
  one is a whole-console compose. An inactive host bumping its revision
  changes no visible row, so the loop compares the rebuilt groups against what
  the shell already holds and installs only on a difference.
- **Header heights are 1-or-2 and cached** (`FleetShellState::header_heights`,
  keyed by `model.generation`): the reason is drawn as a second dimmed line
  under the header rather than folded into the label, because at an 18-36
  column sidebar the folded form truncates the reason away. That is the one
  variable height an inactive host has; every other fleet row is exactly one
  line.
- **`EndpointCommands::reset()` added and called** in `switch_host`, the hazard
  PR 3 recorded and PR 4 deferred. Plus `is_idle()` (`#[cfg(test)]`) so a test
  can pin that the lane is actually released.
- **`switch_host` is split into `switch_target_allowed` + `retarget_host`**, so
  the routing half — connector, link, fleet state, endpoint lane — is testable
  against `test_support::FakeHost` without a terminal or a shell. Order is:
  announce geometry → `connector.set_active` → `state.set_active_host` →
  `link.set_active` → `endpoint_commands.reset()` → shell reset → install the
  new host's snapshot and methods → rebuild → flush the pending focus.
- **`reset_for_host_switch` clears `hits`, and that is the point.**
  `ShellHitMap::workspaces`/`agents` hold the *old* host's bare server-side
  ids and a click resolves them against whatever host is active now, so they
  must not outlive the switch. The console is therefore unclickable for the
  window between the switch and the new host's first frame — deliberate, and
  the safe direction.
- **`FleetShellAction::SortChanged`** is a third variant: agent order inside
  every group is a function of `[ui] agent_panel_sort`, which the user can
  toggle by clicking the agent panel's sort label, so that click pushes an
  action that rebuilds the model. Other writers of the preference (a config
  reload) are picked up at the next fleet change, because `apply_changes`
  always reads the live value from the shell.
- **`scripts/fork/tui-drive.py` gained `--redraw`** (resize the window and
  back, then forget the text so far). A client draws frame *diffs*, so a
  screen that changed one character only ever wrote that character: without a
  forced full repaint, no `--dump` can be read as a screen. Every later PR's
  screen evidence should use it.
- **The console composes from a placeholder when the active host cannot draw**
  — the review found that without it, switching to a host that is down or
  still connecting bricked the console: `compose` returned `None`, so the
  terminal kept the *previous* host's frame, the sidebar could not show the
  switch, and — because `reset_for_host_switch` empties the hit map — no
  further click registered anywhere. `FleetShellState` now carries an empty
  projection/surface pair and a precomputed `pane_notice`; `compose` falls back
  to it **only** in fleet mode and only when there is no snapshot or no
  surface (the revision-mismatch skip and the single-host client are
  unchanged, both pinned by tests), and `render_pane_notice` draws
  `switching to <host>…` over the pane area. This is the `composition.rs`
  hook the plan budgeted for **PR 8**, landed early because PR 5 is what
  creates the state it covers; PR 8 replaces the text with the reconnect
  notice.
- **A terminal resize while the active host cannot render** used to leave the
  console without a hit map for the same reason (`invalidate_pane_surface`
  clears `hits` and waits for a surface that never comes), so the Resize arm
  calls `fleet::present_after_resize`, which recomposes only when a switch is
  pending or the active host is not `Connected`.
- **Requests produced before a switch go to the host that produced them.**
  A click on another host's row and keystrokes can arrive in one stdin batch:
  `finish_client_shell_input` now writes `outcome.requests` *before* the
  dispatch when the batch contains a `SwitchHost`, and
  `dispatch_client_shell_actions` drops an `Endpoint` action that follows a
  switch in the same batch (`FleetActionOutcome { repaint, switched }`). The
  invariant is that a message only ever reaches the host whose ids it carries.
- **`reset_for_host_switch` had to be a complete boot reset.** Because it drops
  the snapshot, the new host's `set_snapshot` does *not* take upstream's
  `boot_changed` path, so everything that path clears is cleared here —
  including `overlay = None` (a deviation from the plan's "overlay stays": a
  Rename/ConfirmClose/Worktree/ContextMenu overlay holds the previous host's
  ids and the Navigator indexes its projection), the selection autoscroll and
  highlight deadlines, copy feedback, `endpoint_notice_seen`, the mobile
  switcher fields and `dismissed_product_announcement`.
- Behaviour worth knowing: what is typed while the target host is not
  connected is *dropped* by `FleetLink::send`, never queued, which is the rule
  **PR 8** makes visible; and `switching to <host>…` stays on a host that never
  comes back until PR 8 replaces it with the reconnect notice.

**Downstream**

- `FleetShellAction::SwitchHost` is the one way to change hosts; PR 6 (picker),
  PR 7 (notification target) and E7 (picker actions) emit it. It is dispatched
  from `dispatch_client_shell_actions`, so any surface that emits it must
  return it in `ClientShellInput::actions` like the sidebar does.
- `hits.fleet_rows` is host-qualified; any new clickable fleet element uses
  `FleetSidebarHit`, never a bare pane id.
- E7 adds a `Prompt`/`SendKeys` variant to `FleetShellAction` without
  switching; it reuses `FleetLink` with an explicit host.
- **PR 6:** `host_picker_rows`, `HostPickerRow` and `HostRowState::state_name`
  carry `#[allow(dead_code)]` naming PR 6 — delete those three attributes when
  the overlay lands. The picker must ignore a row whose `enabled` is `false`
  (`switch_target_allowed` refuses it anyway, with a `warn!`) and must not
  switch to the host that is already active. `FleetShellState` is
  `pub(super)` inside `client::shell`, so the overlay renderer belongs in that
  module tree too.
- **PR 7:** `FleetSidebarModel::agent_status` carries an `#[allow(dead_code)]`
  naming PR 7 — delete it. A cross-host notification target is a
  `FleetShellAction::SwitchHost { then_focus }`; the focus is already deferred
  until the target host has a projection (`FleetClientState::pending_focus` +
  `flush_pending_focus`, called from the loop's snapshot arm), so PR 7 needs no
  second mechanism.
- **PR 8:** `FleetShellState::switching_to` and `FleetClientState::
  pending_switch` are wired end to end — the header draws `…` on the host being
  switched to, and `translate` clears the flag on that host's first surface.
  **The `composition.rs` notice hook already exists** (`FleetShellState::
  placeholder` + `render_pane_notice`, reached from `compose` when the active
  host has no projection or no surface): PR 8 replaces the notice *text* for a
  host that is `Unavailable`/`Connecting` rather than adding a second path, and
  should decide when "switching to <host>…" becomes "reconnecting to
  <host>…". `fleet::present(state)` and `fleet::present_after_resize(state)`
  are the fork-owned compose-and-present helpers. PR 8 also owns the deferred
  items the PR 5 review listed: `reveal_workspace`/`workspace_drop_target_at`
  index the active host's `workspace_entries` against a fleet row list that
  also holds headers and other hosts' rows (Navigate-mode reveal can land one
  row off, the drag indicator can draw on a fleet row), and a host's *first*
  connection attempt reads as "reconnecting (attempt 1)".

### PR 6 — feat(fleet): host picker overlay and fleet.keys host_picker binding · deps: 5

**From PR 5 (as built):** the picker emits `ClientShellAction::Fleet(
FleetShellAction::SwitchHost { host, then_focus: None })`, which
`dispatch_client_shell_actions` (`src/client/mod.rs`) already routes to
`fleet::handle_shell_action`; nothing new is needed on the loop side. Remove
the `#[allow(dead_code)]` on `host_picker_rows`, `HostPickerRow` and
`HostRowState::state_name` in `src/fleet/sidebar.rs` — this PR consumes them.
A row with `enabled == false` and the row for the active host are not switch
targets (`switch_target_allowed` refuses both).

**Goal:** `prefix+shift+h` (configurable as `[fleet.keys] host_picker`) opens a
host picker overlay listing every host with state and counts; `enter` /
click / `1-9` switches; `esc` closes. The action is a no-op outside fleet
mode.

**Files**

- `src/config/model.rs` *(upstream file — inside E1's `[fleet]` section)*:
  `FleetConfig.keys: FleetKeysConfig { host_picker: BindingConfig }` with
  default `BindingConfig::one("prefix+shift+h")`, doc comments; the
  `DEFAULT_CONFIG` `[fleet]` block in `src/main.rs` gains a commented
  `[fleet.keys]` / `# host_picker = "prefix+shift+h"` (and `docs/fork/
  fleet-core.md` quotes that block verbatim — update the quote and the key
  table in the same PR).
- `src/config/keybinds.rs` *(upstream — `Keybinds.host_picker:
  ActionKeybinds`, `empty_action!()`, one `parse_action_bindings(
  "fleet.keys.host_picker", &self.fleet.keys.host_picker, …)` call)*;
  `src/input/keybindings.rs` *(upstream — `KeybindAction::HostPicker` + table
  row)*; `src/input/keybind_help.rs` *(upstream — one help row)*.
- `src/client/shell/fleet_overlay.rs` (new): `ClientHostPickerOverlay`,
  `open_host_picker`, `move_host_picker_selection`, `accept_host_picker`,
  `render_host_picker_overlay`, mouse rows.
- `src/client/shell/state.rs` *(upstream — `ClientShellOverlay::HostPicker(
  ClientHostPickerOverlay)` + `ClientShellOverlayKind::HostPicker` + the
  `kind()` arm; `ShellHitMap.host_picker_rows`)*; `src/client/shell/
  overlays.rs`, `overlay_input.rs`, `actions.rs` (`record_binding`'s
  `HostPicker` arm), `mouse.rs` *(upstream — one arm/delegation each)*;
  `src/client/shell.rs` *(`mod fleet_overlay;`)*.
- `src/client/shell/tests/fleet_picker.rs` (new); `tests/fork_fleet_tui.rs`:
  `host_picker_switches_hosts_from_the_keyboard`. **PR 2 as-built:**
  `HostPickerRow` carries `enabled`; a disabled host is listed (so `1-9`
  indices match the sidebar's groups) but selecting it must be a no-op.

**Shapes/approach**

```rust
pub(super) struct ClientHostPickerOverlay { pub rows: Vec<HostPickerRow> /* from fleet::sidebar::host_picker_rows */,
    pub selected: usize, pub scroll: usize }
```

Rows: `<n>  <host>  <state>  <version>  <N blocked · M working>` with the
active host marked `●` and unreachable hosts dimmed (selectable — switching to
a down host shows PR 8's reconnect notice rather than refusing). The overlay
is not dimmed-background (like `Navigator`). Keys: `↑`/`↓`/`j`/`k`, `1`–`9`,
`enter` → `ClientShellAction::Fleet(SwitchHost{host, None})` + close, `esc`
/ the binding again → close. Rows are rebuilt from `FleetState` when the
overlay opens and on `fleet_sidebar_update` while open. `record_binding`'s
`HostPicker` arm: `if self.fleet.is_some() { open } else { debug!("host
picker outside fleet mode") }`. The help overlay lists it under a "Fleet"
heading only in fleet mode.

**Tests**

- Config: default binding; `[fleet.keys] host_picker = "prefix+h"` parses;
  a conflict with `[keys] goto = "prefix+shift+h"` produces the standard
  duplicate-binding diagnostic naming `fleet.keys.host_picker`; the
  `DEFAULT_CONFIG` `[fleet]` block round-trip test from E1 still passes with
  the new sub-table; live reload of `[fleet]` keeps the key.
- Shell: opening in fleet mode renders every host row; `enter` on another
  host yields `SwitchHost`; `esc` closes; `1`–`9` jump; mouse row click;
  outside fleet mode the action is a no-op and no overlay opens; the
  overlay's rows refresh when a host changes state while open.
- PTY: `\x02H` → picker text visible (`lab-2`, `connected`); `\x1b[B\r` → the
  lab-2 pane marker visible; typed marker lands on lab-2.

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 3 && eval "$(bash scripts/fork/fleet-lab.sh env)"
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 30 \
  --expect 'herdr-fleet-lab:lab-1' --keys '\x02H' --expect 'connected' --keys '3\r' \
  --expect 'herdr-fleet-lab:lab-3' --keys 'echo via-picker\r' --keys '\x02q' --dump \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV HERDR_DISABLE_SOUND=1 target/debug/herdr fleet
$H --session lab-3 pane read "$HERDR_FLEET_LAB_PANE_3" --source recent | grep -c via-picker   # 1; lab-1 and lab-2: 0
printf '[fleet.keys]\nhost_picker = "prefix+h"\n' >> "$XDG_CONFIG_HOME/herdr-dev/config.toml"; $H config check   # ok; then rerun with '\x02h'
$H --default-config | sed -n '/^\[fleet\]/,/^\[experimental\]/p' | grep host_picker
just bench-fleet-scale   # unchanged within noise (the picker is not on the frame path)
bash scripts/fork/fleet-lab.sh down
```

**Downstream**

- `[fleet.keys]` is the home for future fleet-only bindings (E7's
  "prompt on host…"); document each in `fleet-core.md`'s key table.
- `KeybindAction::HostPicker` exists in every build; non-fleet clients ignore
  it.

### PR 7 — feat(fleet): host-aware notifications and cross-host notification targets · deps: 5

**From PR 5 (as built):** opening another host's notification is a
`FleetShellAction::SwitchHost { host, then_focus: Some(FleetFocusTarget::
Pane(id)) }`; `FleetClientState::pending_focus` + `fleet::flush_pending_focus`
already hold that focus until the target host has installed a projection, so
this PR adds the *target*, not a second deferral. Remove the
`#[allow(dead_code)]` on `FleetSidebarModel::agent_status`.

**From PR 4:** `translate` currently **clears** `workspace_id`, `tab_id` and
`pane_id` on a notification from an inactive host (they would otherwise be
validated, suppressed or focused against the active host's snapshot — every
server starts at `w1`/`w1:p1`). PR 7 replaces that with a real host-qualified
target: keep the ids, carry the host beside them, and make
`notification_still_current` / `notification_target_is_active` /
`open_notification_target` host-aware. Do not simply delete the clearing
without doing so.

**Goal:** notifications from any host surface prefixed `[host]` with the
existing sound rules, are validated against *their* host's state (not the
active host's ids), and `open_notification_target` (`prefix+o`) on a remote
host's notification switches to that host and focuses the target.

**Files**

- `src/client/shell/state.rs` *(upstream — `ClientPendingNotification.host:
  Option<HostId>` and `ClientVisibleNotification.host: Option<HostId>`,
  default `None`)*.
- `src/client/shell/notifications.rs` *(upstream — `receive_notification`
  gains a `host: Option<HostId>` parameter via a fleet wrapper:
  `receive_fleet_notification(host, event, now)` in `shell/fleet.rs` sets the
  host then calls the existing body; `notification_still_current` /
  `notification_target_is_active` consult `self.fleet` when `host` is set and
  differs from the active host)*.
- `src/client/shell/fleet.rs`: the wrapper; `open_notification_target` in
  fleet mode → `FleetShellAction::SwitchHost{host, then_focus: Pane(pane_id)}`
  when the visible notification's host is not active.
- `src/client/fleet.rs`: `translate(Notification)` passes the host through
  instead of only prefixing the title.
- `src/client/shell/tests/fleet_notifications.rs` (new);
  `tests/fork_fleet_tui.rs`: `remote_host_notification_is_prefixed_and_opens_there`.

**Shapes/approach**

Prefix: `[lab-2] reviewer needs input` — always, including the active host
(consistent; the roadmap's example). Validation for a non-active host uses
`FleetSidebarModel::agent_status(pane)` (PR 2) as the "still current" check
and never suppresses because the *active* host's focused tab shares an id.
Sound: unchanged (`handle_shell_notification_effects` gates on the agent
label). Terminal/System toast titles carry the prefix too. `prefix+o` on a
remote-host notification emits `SwitchHost{host, Pane}` and dismisses the
notification.

**Tests**

- Shell: a blocked notification from host B while A is active is shown with
  `[B] `, not suppressed by A's focused ids; it is suppressed when B's model
  says the agent is no longer blocked; `prefix+o` yields `SwitchHost{B,
  Pane}`; a notification for the active host behaves exactly as upstream
  (existing notification tests still pass).
- PTY: with a detectable agent unavailable in the lab, use the server's
  `notification` CLI (`herdr --session lab-2 notification …` if present) or
  drive `pane run` into a blocked-looking state; assert `[lab-2]` appears on
  screen while lab-1 is active; `\x02o` → lab-2's marker visible.

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
# trigger a semantic notification on lab-2 (agent CLI path used in tests/cli for notifications; record the exact command used)
python3 scripts/fork/tui-drive.py … --expect 'herdr-fleet-lab:lab-1' --expect '[lab-2]' --keys '\x02o' --expect 'herdr-fleet-lab:lab-2' --keys '\x02q' --dump …
# sound config honoured: HERDR_DISABLE_SOUND unset + [ui] sound = "off" → no `play` in the log at debug level
bash scripts/fork/fleet-lab.sh down
```

**Downstream**

- E6's push and E4's toasts take the host from `FleetEvent::Notification`,
  the same source; the `[host]` prefix is a TUI presentation choice, not a
  server fact.

### PR 8 — feat(fleet): reconnect notice for the active host and resize on reconnect · deps: 5

**From PR 5 (as built):** `FleetShellState::switching_to` and
`FleetClientState::pending_switch` already track a switch in flight (the
header draws `…`; `translate` clears it on the new host's first surface), and
`ClientShellState::reset_for_host_switch` already drops the old host's
surface, hit map and snapshot. So the two states this PR must draw in the pane
area are the same one: the console has no surface to compose. Note `compose`
returns `None` without a surface, so the notice has to be drawn *instead of*
the normal composition path, not inside it. `fleet::present(state)` is the
fork-owned compose-and-present helper. Input typed while the active host is
down is already dropped by `FleetLink::send` with a `debug!` — this PR makes
that visible, it must not start queueing it.

**From PR 4:** "resize on reconnect" is already true — the
`ClientLoopEvent::Resize` arm calls `FleetClientState::announce_geometry`,
which adopts the geometry in the connector whether or not the active host is
connected, and the supervisor's hello reads it (PR 1 As built). That arm also
**skips** the link write for `ClientShellResize` in fleet mode, so do not
re-add one. What is left for PR 8 is the *notice*: a host leaving `Connected`
reaches the loop as `Translated::Changes` only, and the pane area still shows
the last frame. PR 1 also deferred to PR 8 the question of whether
`set_active`/`set_active_geometry` need a write timeout, since both write to a
socket while holding the active-host and link locks.

**Goal:** when the active host drops, the pane area shows a reconnect notice
(host, reason, attempt) instead of the stale surface, input to it is
discarded (never queued), the rest of the console stays usable, and when it
comes back the pane area repaints at the current terminal size.

**Files**

- `src/client/shell/fleet.rs`: `pane_area_notice(&self) -> Option<FleetNotice>`
  (`Connecting{attempt}`/`Unavailable{reason, retry_in}`/`Incompatible`/
  `switching`), `input_allowed()`.
- `src/client/shell/composition.rs` *(upstream — one hook: when
  `self.fleet.as_ref().and_then(pane_area_notice)` is `Some`, draw the notice
  into `layout.pane_surface` instead of blitting)*.
- `src/client/shell/input.rs` *(upstream — one guard: pane-bound input is
  dropped with a repaint when `!input_allowed()`; chrome, overlays and the
  picker still work)*.
- `src/client/fleet.rs`: `apply_changes` marks the active host's connection
  on the shell; on `Connected` for the active host: `set_endpoint_methods`,
  `set_active_geometry(current)` (the connector then announces the real size
  — PR 1's guarantee), `invalidate_pane_surface`; on `Snapshot` the existing
  install path runs.
- `src/client/link.rs`: `FleetLink::write` while the active host is not
  connected returns `Ok` and counts a dropped write (log at debug; the shell
  guard makes it rare).
- `src/client/shell/tests/fleet_reconnect.rs` (new); `tests/fork_fleet_tui.rs`:
  `active_host_drop_shows_reconnect_and_recovers`.

**Shapes/approach**

Notice text (centered, dimmed box in the pane area):
`lab-2 · reconnecting (attempt 3) · host closed the connection · retry in 4 s`
or `lab-2 · incompatible: <reason>`; while switching: `switching to lab-2…`.
The sidebar header shows the same state (PR 5). Nothing here changes the
connector's backoff. Mouse events in the pane area are ignored while the
notice shows; the sidebar and picker remain live, so the user can switch to
another host.

**Tests**

- Shell: with the active host `Unavailable`, `compose` draws the notice and
  no surface; key presses produce no `ClientShellPaneInput`; a click on
  another host still yields `SwitchHost`; on `Connected` + snapshot + surface
  the notice disappears.
- `src/client/fleet.rs`: fake active host closes after welcome → `Changes`
  carry `Unavailable`; it reconnects → the fake records a resize at the
  *current* geometry (after a `Resize` event was applied while it was down),
  then a full surface is translated to `Server(PaneSurface)`.
- PTY: `session stop lab-1` while active → `wait_for_text("reconnecting")`;
  typing produces nothing on any host; restart lab-1 by hand → marker
  visible again within 60 s; the console never exited.

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
bash scripts/fork/ssh-lab.sh up && eval "$(bash scripts/fork/ssh-lab.sh env)"     # lab-ssh = ssh host to lab-1's session
# [fleet]: lab-ssh (kind = "ssh", target = "herdr-ssh-lab", session = "lab-1"), lab-2 (local)
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 90 --expect 'herdr-fleet-lab:lab-1' --keys '' --dump \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV HOME=$HERDR_SSH_LAB_HOME HERDR_DISABLE_SOUND=1 target/debug/herdr fleet &
sleep 5; bash scripts/fork/ssh-lab.sh down; sleep 5     # active host drops over ssh
# expect 'reconnecting' + 'remote bridge failed' text on screen; lab-2 header still 'connected'; typing 'x' lands nowhere
bash scripts/fork/ssh-lab.sh up; sleep 30                 # expect the lab-1 marker back; the pane is 120-col wide (no 20-col wrap)
$H --session lab-1 pane run "$HERDR_FLEET_LAB_PANE_1" 'printf "%0110d\n" 0'   # line stays unwrapped on screen after reconnect
# quit; then: no /tmp/herdr-remote-<pid>-lab-ssh-*.sock left; ssh stderr is in the herdr log, not the screen dump
bash scripts/fork/ssh-lab.sh down; bash scripts/fork/fleet-lab.sh down
```

If `ssh-lab.sh up` exits 3 (`sshd not found`), run the same with two local
hosts and `session stop`/restart, and record the degradation in the PR body.
`just bench-fleet-scale` unchanged within noise.

**Downstream**

- `input_allowed()` is the gate E7 must respect for cross-host prompts to a
  down host (fail with a notice, never queue).
- The notice is TUI presentation; E3/E4 read `HostConnection` directly.

### PR 9 — docs: fleet console guide, adr e2 review, roadmap drift · deps: 6, 7, 8

**Goal:** a user can run the console, read the sidebar, switch hosts, and
know the limits; a developer knows the seam; ADR 0001 records what E2 learned.

**Files**

- `docs/fork/fleet.md` (new): launch (`herdr fleet`, `herdr --fleet`,
  `--session`), the sidebar (host groups, glyphs, dimmed hosts, collapse),
  switching (click, picker, `[fleet.keys] host_picker`), what goes to which
  host, notifications (`[host]` prefix, `prefix+o`), failure UX (reconnecting,
  active-host notice), limits (no mixed-host layouts, no kitty graphics,
  local keybindings only, the foreground-geometry caveat: run the console
  only while in use; ssh stderr goes to the log), and the fleet lab / ssh lab
  walkthrough with `tui-drive.py`. Real screen dumps from validation.
- `docs/fork/fleet-core.md`: cross-link; confirm the `[fleet.keys]` table
  and the updated `DEFAULT_CONFIG` quote (PR 6 did the edit; verify).
- `docs/fork/README.md`: a *Fleet console* section; add `src/client/link.rs`,
  `src/client/fleet.rs`, `src/client/shell/fleet*.rs`, `scripts/fork/
  tui-drive.py`, `tests/fork_fleet_tui.rs`, `tests/support/fleet_tui.rs` to
  the fork-owned lists, and the upstream files that now carry fork wiring
  (`src/client/mod.rs`, `endpoint_commands.rs`, `shell.rs`, `shell/state.rs`,
  `shell/{sidebar,agent_sidebar,mouse,overlays,overlay_input,actions,input,
  notifications,composition}.rs`, `src/config/keybinds.rs`,
  `src/input/{keybindings,keybind_help}.rs`, `src/cli/spec.rs`, `justfile`)
  to the *Keeping up with upstream* table, with the merge note from PR 3.
- `docs/fork/decisions/0001-servers-stay-stock-ssh-transport.md`: an
  *E2 review* subsection (the link seam and why not a second loop; one shell,
  one active host; the `[fleet.keys]` placement and the docs-gate reason;
  stderr redirection instead of an `attach.rs` edit; anything the console
  learned about foreground geometry). Nothing in *Decision* is amended unless
  E2 contradicted it.
- `docs/fork/ROADMAP.md`: correct factual drift in the E2 section only (the
  key's config location; `tui-drive.py`); no status flip (`implement-epic`
  owns it).

**Tests:** none (docs). Every command block is executed verbatim during
validation.

**Real-server validation**

Walk `fleet.md` top to bottom against `fleet-lab.sh up 3` + `ssh-lab.sh up`;
paste real screen dumps (trimmed); `ssh-lab.sh down`, `fleet-lab.sh down`;
confirm the user's `herdr session list` shows no `lab-*`.

**Downstream**

- E7 adds its console actions to `fleet.md`; E8's install doc links here for
  "console machine".

## Critical files referenced (reuse, don't reinvent)

- `src/client/mod.rs` — `run_client_with_mode` (`:343`), `run_client_loop`
  (`:717`), `ClientLoopEvent` (`:291`), `server_reader_thread` (`:1822`),
  `write_to_server` (`:1876`), `dispatch_client_shell_actions` (`:529`),
  `install_client_shell_snapshot` (`:619`), `finish_client_shell_input`
  (`:658`), the `ServerMessage` arms (`:1269-1750`), exit handling
  (`:503-537`).
- `src/client/endpoint_commands.rs` — `EndpointCommands::{enqueue,
  send_next, expire, receive_chunk}`, `EndpointCommandResult`.
- `src/client/handshake.rs` — `do_handshake` (`:130`) as the reference for
  what a client hello carries; `client_shell_keybinding_source` (`:39`).
- `src/client/shell.rs` — module list, `status_icon`/`status_dot`/
  `status_priority`/`status_text`/`status_color`, `blit_pane_surface`.
- `src/client/shell/state.rs` — `ClientShellConfig` (`:71`),
  `ClientShellAction` (`:280`), `ClientShellOverlay` (`:598`),
  `ClientNavigatorOverlay`/`ClientNavigatorRow` (`:377-394`),
  `ClientShellState` (`:860`), `set_snapshot` (`:1176`),
  `invalidate_pane_surface` (`:1697`), `set_endpoint_methods` (`:1081`),
  `ShellHitMap` (`:131`).
- `src/client/shell/sidebar.rs` (`render_sidebar` `:180`,
  `workspace_entries` `:454`, `workspace_rows` `:592`),
  `agent_sidebar.rs` (`ordered_agent_pane_ids` `:20`, `render_agent_panel`
  `:52`), `scroll.rs` (`list_scroll_metrics`, `list_scroll_start_to_reveal`,
  `render_list_scrollbar`), `composition.rs` (`compose` `:23`),
  `overlays.rs` (`render_navigator_overlay` `:753`, `client_navigator_rows`
  `:672`), `overlay_input.rs` (`open_navigator_overlay` `:193`,
  `accept_navigator_selection` `:231`), `actions.rs` (`record_binding` `:4`,
  `handle_endpoint_result` `:471`), `input.rs` (prefix resolution `:757`),
  `notifications.rs` (`receive_notification` `:264`,
  `notification_target_is_active` `:377`), `render.rs` (`render_mode_bar`
  `:16`, `put_text`), `mouse.rs` (`handle_mouse`, hit tests).
- `src/client/shell/tests/mod.rs` — `snapshot()`, `surface()` fixtures and
  the test-module pattern; `src/client/tests/mod.rs`.
- `src/fleet/connector.rs` (`FleetConnector`, `FleetEvent`, `HostCommand`,
  `HostSendError`, `INACTIVE_SURFACE`, `resize_message`, `announce_surface`,
  the supervisor, the fake endpoint tests), `handshake.rs`
  (`HandshakeParams::read_only`), `state.rs`, `refs.rs`, `hosts.rs`,
  `oneshot.rs` (`FleetSession` — the blocking driver E2 does **not** use),
  `mod.rs` (purity guard test).
- `src/config/keybinds.rs` (`Keybinds` `:329`, `apply_action!` `:552`,
  `parse_action_bindings` `:810`, `BindingRegistry`), `src/config/model.rs`
  (`FleetConfig`, `KeysConfig` `:334`), `src/input/keybindings.rs`
  (`KeybindAction` `:20`, `resolve_non_indexed_action` `:90`),
  `src/input/keybind_help.rs`.
- `src/cli.rs` (`maybe_run` `:95`, the `server` arm pattern `:106`),
  `src/cli/fleet.rs`, `src/cli/spec.rs` (`fleet_command`), `src/main.rs`
  (`:495-790`).
- `src/server/render_scale_benchmark.rs` — `summarize`, `print_stage`,
  `StageStats`, the `#[ignore]` profile pattern; `justfile:84`
  `bench-render-scale`.
- `tests/client_mode.rs` (`spawn_pty_drain` `:659`, `read_output` `:678`,
  `attach_thin_client` `:693`, key injection `:350`), `tests/multi_client.rs`
  (`spawn_client` `:117` env recipe, `SpawnedHerdr` drop), `tests/support/
  mod.rs` (`build_version`, `register_spawned_herdr_pid`,
  `herdr_server_pids_for_runtime_dir`, `cleanup_test_base`),
  `tests/support/fleet_lab.rs` (`Lab`), `tests/cli/fleet.rs` (the `[fleet]`
  fixture), `tests/fixtures/endpoint-snapshot-v1.json` (never edited).
- `scripts/fork/fleet-lab.sh`, `ssh-lab.sh`, `gate.sh`;
  `scripts/test_ui_hot_path_architecture.py`;
  `scripts/config_reference_check.py` (`SKIPPED_SUBTREES`).
- Binding rules: `AGENTS.md` → Universal Project Rules (pure render,
  multiplicative paths, runtime/client boundary, stable endpoint contract),
  Testing, Code Conventions; `.claude/rules/fork.md`; `docs/fork/ROADMAP.md`
  principles 1–8 and E2; `docs/fork/decisions/0001-servers-stay-stock-ssh-transport.md`
  (E0 and E1 reviews); `docs/fork/plans/e1-fleet-core.md` Downstream and
  *As built* sections; `docs/fork/fleet-core.md` "For developers".

## End-to-end epic validation

Run by `implement-epic` after PRs 1–9 are ✅ and merged, from the root
checkout on `master` (`git -C <root> pull --ff-only`), with
`bash scripts/fork/gate.sh <root>` → `EXIT=0` first.

1. **A fleet of three kinds of host.** `cargo build`; `fleet-lab.sh up 3`;
   `ssh-lab.sh up`; `eval` both `env`s; `[fleet]` with `include_local = false`,
   `lab-1`/`lab-2` local, `lab-ssh` (`kind = "ssh"`, `target =
   "herdr-ssh-lab"`, `session = "lab-3"`), `nowhere` (`kind = "ssh"`,
   `target = "ssh://127.0.0.1:1"`). Launch `herdr fleet` through
   `tui-drive.py` (120×40, `HOME=$HERDR_SSH_LAB_HOME`). Assert on the
   stripped screen: four host headers in config order; `lab-1` active with
   its pane marker rendered; `lab-2` and `lab-ssh` headers `connected` with
   their `lab-N` workspace labels; `nowhere` dimmed with a non-empty reason;
   the herdr log records the `active host lab-1` change (the `fleet status`
   CLI has its own connector, so it cannot observe the console's active host).
2. **Input goes exactly where it should.** Type `echo e2-1` → `pane read`
   on lab-1 has it, lab-2/lab-3 do not. Open the picker (`\x02H`), choose
   `lab-ssh` → its marker renders within 60 s; type `echo e2-ssh` → lab-3's
   pane (over ssh) has it, lab-1's does not. Click lab-2's agent/workspace
   row → lab-2 active and focused; type `echo e2-2` → lab-2 only. Resize the
   PTY to 100×30 (`tui-drive.py --resize 100x30`) → the active host's pane
   reflows; the two inactive hosts' panes stay at `INACTIVE_SURFACE`
   (`pane read` of a 120-col line on lab-1 stays unwrapped).
3. **Status is live without switching.** `pane run … 'yes >/dev/null'` on
   lab-1 while lab-2 is active → lab-1's header count / row glyph changes on
   screen within 5 s; kill it → reverts.
4. **Host failure is local, over both transports.** `ssh-lab.sh down` →
   `lab-ssh` header shows `reconnecting…`/the reason; lab-2 stays usable
   (type, read back). Switch to `lab-ssh` → the pane-area notice; typed
   input lands nowhere. `ssh-lab.sh up` → marker back within 60 s at the
   current size. `session stop lab-1`, restart by hand → same for a local
   host. The console never exits, never panics (log has no `panic`).
5. **Notifications.** A notification on a non-active host appears as
   `[lab-N] …`; `\x02o` switches there and focuses the pane.
6. **Clean exit.** `\x02q` → exit 0 within 5 s; no `/tmp/herdr-remote-<pid>-
   lab-ssh-*.sock`; `fleet-lab.sh status --json` still lists exactly the
   three lab pids; ssh stderr text is in the herdr log, not in the screen
   dump; `ls -la ~/.ssh | sha256sum` unchanged; from a shell **without** the
   lab env `herdr session list` shows no `lab-*`.
7. **Perf and contracts.** `just bench-fleet-scale` → compose median 5 vs 1
   hosts ≤ 1.15× (collapsed) and ≤ 1.5× (expanded), pasted into the epic
   report; `bash scripts/fork/gate.sh <root> "test-one fleet"` → `EXIT=0`;
   `tests/fork_fleet_tui.rs` green; `git diff --stat upstream/master --
   src/protocol tests/fixtures src/server src/app src/api` is empty.
8. **Isolation and teardown.** `ssh-lab.sh down`, `fleet-lab.sh down`;
   `pgrep -af 'herdr --session lab-'` and `pgrep -af 'sshd -f
   /tmp/herdr-fleet-lab'` empty; no `/tmp/herdr-fleet-lab` or `/tmp/e2-*`
   left behind.
9. Flip E2 to ✅ in `docs/fork/ROADMAP.md` only when 1–8 all hold.
