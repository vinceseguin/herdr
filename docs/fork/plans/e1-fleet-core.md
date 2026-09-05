# Epic E1 — Fleet core (multi-host runtime model)

## Context

**Goal (roadmap):** one client process holds connections to N herdr servers —
local named sessions and SSH hosts — and exposes a single merged, pure,
testable view of their hosts, workspaces, tabs, panes and agents. **Why:** the
TUI (E2) and the gateway (E3) need exactly the same aggregation. Built once as
pure state plus a connector, it follows upstream's "state is separated from
runtime" principle and is testable without sockets.

Scope contract = the roadmap's E1 deliverables: a `[fleet]` config section
(`include_local`, `[[fleet.hosts]]` with `name`/`kind`/`target`/`session`/
`enabled`, `herdr --default-config` updated); `src/fleet/` with a pure
`FleetState` (`HostId`, `HostConnection`, per-host snapshots keyed by
`boot_id`/`revision`, `active_host`, the merged agent list, per-host roll-ups,
typed `FleetPaneRef`-style ids with the `host/w1:p1` string form,
`test_new()`/`assert_invariants_for_test()`), a `FleetConnector` runtime
(local socket or the SSH stdio bridge, generation-1 handshake, reader thread,
1 s → 30 s reconnect backoff, every failure host-local), snapshot ingestion
that forwards pane surface frames only for the active host, the CLI
`herdr fleet status [--json]`, pure-state unit tests, and a `tests/`
integration test that boots two named sessions and asserts the merged status.
**Constraint downstream epics must honor:** no server or wire-protocol change;
`FleetState` stays free of ratatui, sockets, and async.

**Dependency chain:** E0 is ✅ (`origin/master` @ `7cea0271`, herdr
`0.8.2-fork`). E1 assumes exactly these E0 contracts:

- `scripts/fork/fleet-lab.sh up <n> | status [--json] | env | down` — sessions
  `lab-1 … lab-N` under `${HERDR_FLEET_LAB_ROOT:-/tmp/herdr-fleet-lab}`
  (`xdg/`, `runtime/`, marker `.herdr-fleet-lab`), each with one workspace
  labelled `lab-N` whose pane printed `herdr-fleet-lab:lab-N`; `env` exports
  `XDG_CONFIG_HOME`, `HERDR_FLEET_LAB_ROOT`, `HERDR_FLEET_LAB_SESSIONS`,
  `HERDR_FLEET_LAB_CLIENT_SOCKET_<N>`, `HERDR_FLEET_LAB_API_SOCKET_<N>`,
  `HERDR_FLEET_LAB_PANE_<N>`; `status --json` has `root`, `bin`, `sessions[]`
  (`name`, `running`, `pid`, `api_socket`, `client_socket`, `pane_id`). Fields
  are added, never renamed. `HERDR_BIN` selects the binary.
- `scripts/fork/gate.sh <worktree> [recipe]` prints `EXIT=<code>` last.
- Fork CI = `.github/workflows/fork-ci.yml` (`conventional-commits`,
  `check (ubuntu-latest)` = `just ci`, `shellcheck -S warning
  scripts/fork/*.sh`), live and green.
- `tests/support::build_version()` is the channel-aware version string
  (`0.8.2-fork`); never assert a bare `CARGO_PKG_VERSION`.
- `Version::parse("0.8.2-fork")` is `None`; nothing in E1 compares versions
  numerically — compatibility is the endpoint generation, per ADR 0001.

### Real current state (verified on `master` @ `7cea0271`, herdr 0.8.2-fork)

- **`src/fleet/` does not exist**; `grep -rn fleet src/` is empty. No `fleet`
  arm in `src/cli.rs::maybe_run` (`:95-131`), no `fleet_command()` in
  `src/cli/spec.rs`, no `fleet` field on `Config`.
- **Config:** `src/config/model.rs:310` `#[derive(Debug, Default, Deserialize)]
  #[serde(default)] pub struct Config { …, pub remote: RemoteConfig }`;
  `RemoteConfig { manage_ssh_config: bool }` (`:962-976`) is the section
  template (doc comment per field, hand-written `impl Default`).
  `src/config/io.rs:7-20` `KNOWN_TOP_LEVEL_CONFIG_KEYS` must gain `"fleet"` or
  the section is reported as an unknown key. `Config::collect_diagnostics()`
  (`src/config.rs:114`) chains per-section diagnostics. `herdr --default-config`
  prints the static literal `DEFAULT_CONFIG` (`src/main.rs:64`; the `[remote]`
  block is at `:392-401`); no Rust test round-trips it.
  `scripts/config_reference_check.py` runs only in `just release-docs-check`,
  not in `just ci`.
- **Endpoint protocol (frozen, reused as-is):** `src/protocol/wire.rs` —
  framing is `[u32 LE length][bincode]` via `protocol::write_message` /
  `protocol::read_message(reader, max_frame_size)` (`:1601`, `:1624`),
  `MAX_FRAME_SIZE = 2 MiB`, `MAX_GRAPHICS_FRAME_SIZE = 32 MiB`. The generation-1
  handshake is `ClientMessage::EndpointControl { kind: ENDPOINT_HELLO_KIND, data:
  json(EndpointClientHello) }` → `ServerMessage::EndpointControl { kind:
  ENDPOINT_WELCOME_KIND, data: json(EndpointServerWelcome { generation,
  server_version, snapshot_codec, surface_codec, input_codec, blob_codec,
  methods, error }) }`; snapshots arrive as `EndpointControl { kind:
  "shell.snapshot.v1", data: json(ClientShellSnapshot) }` (a binary
  `ServerMessage::ClientShellSnapshot` is a protocol error on this path).
  `ClientShellSnapshot` (`wire.rs:907`) carries `boot_id`, `revision`,
  `focused_*_id`, `agent_view_label`, `agent_order`, `workspaces[]`
  (`workspace_id`, `label`, `agent_status`, `focused`, …), `tabs[]`, `panes[]`,
  `agents[]` (`ClientShellAgent { pane_id, workspace_id, tab_id, name,
  display_agent, agent, title, terminal_title, agent_status:
  api::schema::AgentStatus, state_change_seq: u64, state_labels, tokens,
  focused }`), `commands[]`. Unknown JSON agent statuses already fall back to
  `AgentStatus::Unknown` (`deserialize_client_shell_agent_status`, `wire.rs:876`).
  Frames: `PaneSurfaceFrame { boot_id, projection_revision, surface_revision,
  … }` / `PaneSurfacePatch { boot_id, projection_revision,
  base_surface_revision, surface_revision, … }`. Commands:
  `ClientShellEndpointRequest { boot_id, request }` →
  `ClientShellEndpointResponseChunk { boot_id, request_id, final_chunk, data }`
  (server limits 1 MiB request, 512 KiB chunks). Per-client sizing:
  `ClientShellResize { cell_width_px, cell_height_px, surface_size, pixel_mouse }`,
  `ClientShellFocus { focused }`. `EndpointServerWelcome.methods` is the
  advertised shell-lane method list (`src/server/client_commands.rs:15`,
  37 entries — no `agent.*`, no `session.snapshot`; those are JSON-API-socket
  only).
- **Ids are per server:** workspace `w1`, tab `w1:t1`, pane `w1:p1`
  (`src/workspace.rs:109,144,148`, `encode_public_number`). `boot_id` is
  `"{pid}-{nanos}"`. `state_change_seq` is a per-server-boot counter
  (`src/app/state.rs:832`) — **not comparable across hosts**.
- **Existing single-host client:** `src/client/mod.rs:343 run_client_with_mode`
  connects once (`crate::ipc::connect_local_stream`), `do_handshake`
  (`src/client/handshake.rs:130`, `pub(super)`, `LOCAL_HANDSHAKE_READ_TIMEOUT =
  5 s`, `REMOTE_… = 60 s`), spawns one blocking `server_reader_thread` (`:1822`)
  feeding a `tokio::sync::mpsc` (cap 256), and **exits the process** on any
  failure — there is no reconnect anywhere in `src/client/`. The stale-drop /
  boot-reset rules to mirror are `ClientShellState::set_snapshot`
  (`src/client/shell/state.rs:1176`: drop when same `boot_id` and lower
  `revision`; a new `boot_id` always replaces and resets surface state),
  `set_pane_surface` (`:1481`) and `apply_pane_surface_patch`
  (`src/client/shell/surface_patch.rs:107`, strict `surface_revision + 1`
  chaining). Sidebar ordering is `status_priority` (Blocked 4 > Done 3 >
  Working 2 > Idle 1 > Unknown 0) then `Reverse(state_change_seq)`
  (`src/client/shell/agent_sidebar.rs:20`). The endpoint request lane
  (`src/client/endpoint_commands.rs`, one in-flight, 60 s timeout, chunk
  reassembly keyed by `(boot_id, request_id)`) is single-connection.
  `crate::client`'s public surface is only `run_client`, `run_terminal_attach`,
  `run_terminal_session_{observe,control}`, `ClientError` — nothing reusable
  for N hosts; the fleet module reimplements the small handshake.
- **SSH bridge (`src/remote/attach.rs`, 3365 lines, everything private except
  `RemoteLaunch`, `extract_remote_args`, `run_remote`, `RemoteKeybindings`, two
  env-var consts):** `SshStdioBridge { local_socket, socket_identity,
  should_stop, thread }` with `fn start(target, remote_herdr: RemoteHerdr,
  local_socket, session_name, ssh_options: Option<&ManagedSshOptions>)`
  (`:1661-1758`): `ipc::prepare_socket_path` → `bind_private_local_listener` →
  `socket_file_identity` → `restrict_socket_permissions(0o600)` →
  non-blocking accept loop that, per accepted local connection, spawns
  `ssh -T <target> "exec <shell_path>[ --session <name>] remote-client-bridge"`
  (`bridge_connection`, unix `:1817` / windows `:1872`;
  `remote_bridge_command` `:1617`) and pipes both ways; `Drop` = stop flag →
  (unix) unlink → join. The bridge does **not** own an ssh child; failures
  inside the accept thread are `eprintln!`ed. `RemoteSsh::new(target,
  manage_ssh_config)` (`:424`) wraps the managed `-F` config
  (`write_managed_ssh_config` `:1775`: `Include $HOME/.ssh/config`, `Include
  /etc/ssh/ssh_config`, `Host *` keepalives; control socket `-S … -o
  ControlMaster=auto -o ControlPersist=yes`; `-O exit` on drop).
  `local_forward_socket_path(target, session)` (`:2144`) is unique per
  `(pid, target, session)` only. `prepare_remote_herdr` (`:643`) = detect
  platform → `HERDR_REMOTE_BINARY` override → `remote_binary_candidates`
  (`:725`: `command -v herdr` via login shell then `/bin/sh`, then known dirs
  `$HOME/.local/bin`, brew, mise, nix) → first candidate whose `status client
  --json` reports `endpoint_protocol_generation == 1` → otherwise
  **interactive** install prompts and upload; `ensure_remote_server_ready`
  (`:1008`) may **interactively** stop/hand off a remote server. `run_remote`
  (`:162`) spawns `herdr client` with `HERDR_CLIENT_SOCKET_PATH` = forward
  socket. `main.rs:758-765` is the only `exit(1)`. The server side
  (`src/remote/host_unix.rs::run_remote_client_bridge`) spawns the daemon if
  the session is not listening and refuses a server whose generation ≠ 1.
  `platform::remote_ssh_config_paths()` derives `~/.ssh/config` from **`$HOME`**
  (`src/platform/unix_common.rs:26`), which is what makes an isolated SSH lab
  possible without touching the user's `~/.ssh`.
- **Sockets/paths:** `session::client_socket_path_for(Option<&str>)` =
  `<config>/herdr-client.sock` or `<config>/sessions/<name>/herdr-client.sock`;
  `config::config_dir()` honours `XDG_CONFIG_HOME` + `app_dir_name()`
  (`herdr-dev` for debug builds); `session::validate_name` (`src/session.rs:425`,
  `[A-Za-z0-9._-]`, ≤ 64). `api::read_runtime_status_at(&path, timeout)`
  (`src/api/status.rs:14`) returns `Ok(None)` for a missing/refused socket.
  `api::client::ApiClient::for_target(ConnectionTarget::SocketPath(_))` is the
  multi-target JSON API client (not needed by E1's shell-lane connector).
- **Tests/tooling:** `Cargo.toml` has no `[features]`, no dev-dependencies, no
  `[[test]]`; every `tests/*.rs` is auto-discovered. `tests/cli/harness.rs`
  (`pub(super)`, `tests/cli.rs` is `#![cfg(not(target_os = "macos"))]`) has
  `spawn_named_server(config_home, runtime_dir, session)`,
  `named_session_socket`, `wait_for_socket`, `run_named_cli_json(config_home,
  runtime_dir, args)`; `tests/cli/mod.rs` is a flat `mod` list. cargo-nextest
  runs **one process per test**, so a test may set `PATH` for a spawned
  `Command::new("ssh")` without affecting other tests (attach.rs's own tests
  already serialize env mutation behind `remote_env_lock()`).
  `tests/fork_fleet_lab.rs` has a `Lab` driver (`up`, `herdr(session, args)`,
  `Drop` → `down`). Pure-state test idiom: `AppState::test_new()`,
  `test_with_adversarial_identity_state()`, `assert_invariants_for_test()`
  (`src/app/state.rs:1021,1131,1140`; `Workspace` `src/workspace.rs:1174,1291`).
  No test anywhere uses `ssh`; no `#[ignore]` convention; no
  `.config/nextest.toml`.
- **This machine:** `sshd` is installed (`/usr/bin/sshd`) but **not running**
  (port 22 refused). A **user-space sshd** works without root: `sshd -f <cfg>`
  on `127.0.0.1:2299` with a throwaway host key, `AuthorizedKeysFile` under a
  scratch dir, `PasswordAuthentication no`, `UsePAM no`, `StrictModes no`, and
  `SetEnv HOME=<lab>/home XDG_CONFIG_HOME=<lab>/xdg PATH=<lab>/home/.local/bin:…`
  — verified: the remote session sees the overridden `HOME`/`PATH`/`XDG`, a
  login shell's `command -v herdr` resolves to `<lab>/home/.local/bin/herdr`,
  and both `ssh://localhost:2299` and a managed-style `Include`d client config
  authenticate in `BatchMode`. The user's herdr is `/usr/bin/herdr` (root-owned
  package). `SHELL` is bash. No `~/.ssh` file is read or written when
  `HOME=<lab>/home` is set for the client and `-F <lab>/home/.ssh/config` (or
  herdr's managed include, which reads `$HOME`) is used.

### Locked decisions

- **(a) SSH reuse — refactor `SshStdioBridge` + the remote-herdr discovery
  half of `prepare_remote_herdr` into a transport the connector calls
  directly** (roadmap default). Implemented as the *smallest behaviour-preserving
  diff* to the upstream file: the types stay in `src/remote/attach.rs` with
  visibility widened to `pub(crate)` (`src/remote.rs` already re-exports
  `attach::*`), plus three injection points — `SshStdioBridge::start_with(…,
  BridgeErrorSink)`, a `socket_scope` for `local_forward_socket_path`, and a
  `discover_remote_herdr(ssh) -> io::Result<Option<RemoteHerdr>>` split out of
  `prepare_remote_herdr` — and the fleet-side adapter lives in
  `src/fleet/transport/ssh.rs`. Written reason (fork rule "a change that
  reshapes an upstream file needs a written reason"): moving 800 lines into
  `src/fleet/` would make every upstream edit to the bridge a conflict against
  deleted code and would make `herdr --remote` depend on the fleet module;
  widening visibility keeps upstream's diffs applying cleanly. No subprocess
  per host (`herdr --remote`-style) — that would cost a process and a
  `herdr client` per host and cannot expose frames to one merged state.
- **(b) Id form — `host/w1:p1`** (roadmap default). Pane ids already embed the
  workspace (`w1:p1`), so the form is `{host}/{pane_id}`; workspace `host/w1`,
  tab `host/w1:t1`. Host ids never contain `/` (validated with
  `session::validate_name`'s character class), so `split_once('/')` parses.
- **(c) `[fleet]` lives in the main `config.toml`** (roadmap default), as a
  `FleetConfig` section on `Config` following `RemoteConfig`'s pattern.
- **(d) Merged agent order — status rank blocked → working → done → idle →
  unknown (the roadmap's text), then recency** *(auto default)*. Because
  `state_change_seq` is per server boot, recency is a `FleetState`-owned
  monotonic `fleet_change_seq` assigned whenever a host's agent first appears
  or its `state_change_seq`/status advances (descending), then host order
  (config order, local first), then `pane_id` — deterministic and
  cross-host-comparable. Per-host rows in E2's sidebar keep upstream's
  `status_priority` (Blocked > Done > Working > Idle > Unknown) so a host group
  looks like today's sidebar; the merged list is the fleet/gateway contract.
- **(e) Inactive hosts cost no presentation work** *(auto default)*: the
  connector keeps inactive hosts at a minimal `ClientSurfaceSize` (the server
  then emits tiny frames) and the reader drops `PaneSurface`/`PaneSurfacePatch`
  for any host that is not active before they reach the event channel; the
  active host gets a `ClientShellResize` to the real size on activation and
  receives a full frame (E2 accepts a brief redraw). No `ClientShellFocus
  { focused: false }` is sent for inactive hosts (it would change server-side
  notification behaviour); E2 may revisit. The protocol has no "unsubscribe
  surfaces" message and nothing is added.
- **(f) The connector never installs, uploads, stops or hands off a remote
  server** *(auto default; ADR 0001 E0 review)*. SSH hosts use discovery only
  (`discover_remote_herdr`); a host with no generation-1 herdr becomes
  `Unavailable { reason: "no herdr with endpoint generation 1 on host; run
  `herdr --remote <target>` once to install it" }`. The remote bridge still
  auto-starts a stopped server (upstream `host_unix.rs` behaviour, unchanged).
  `HERDR_REMOTE_BINARY` is ignored by the connector.
- **(g) Runtime shape — std threads per host, `tokio::sync::mpsc` for events**
  *(auto default)*: mirrors upstream's blocking `server_reader_thread` +
  `blocking_send` into a tokio channel; the CLI consumes with
  `blocking_recv`, E2's current-thread runtime and E3's axum runtime consume
  with `recv().await`. No async inside `src/fleet/state.rs`.
- **(h) `herdr fleet status` gains `--timeout-ms <n>` (default 5000) and
  `--watch`** *(auto default)*: one-shot settles when every enabled host is
  connected with a snapshot or unavailable/incompatible after its first
  attempt, or at the timeout; `--watch` keeps running and prints one JSON
  `FleetChange` per line (`--json`) or one text line per change — this is E3's
  delta stream and the only way to show reconnect against real servers from
  the CLI. Exit code 0 whenever the report was produced (unreachable hosts are
  data), 1 for invalid `[fleet]` config, 2 for usage.
- **(i) SSH validation stand-in — `scripts/fork/ssh-lab.sh`, a user-space sshd
  on `127.0.0.1:${HERDR_SSH_LAB_PORT:-2299}` layered on the fleet lab**
  *(auto default; port 22 is not available here)*. It creates
  `$HERDR_FLEET_LAB_ROOT/ssh/home/` as a fake `HOME` for both sides
  (`.ssh/config` alias `herdr-ssh-lab`, keypair, known_hosts,
  `.local/bin/herdr` wrapper that `exec`s the lab's `HERDR_BIN` with the lab's
  `XDG_*` and the `HERDR_*` overrides removed) and an `sshd_config` with
  `SetEnv HOME=… XDG_CONFIG_HOME=… PATH=…`. Client processes run with
  `HOME=$HERDR_SSH_LAB_HOME` so herdr's managed config includes the lab's
  `.ssh/config`. A fleet host is then `kind = "ssh", target = "herdr-ssh-lab",
  session = "lab-1"`. If `sshd` is missing, `ssh-lab.sh up` exits 3 with
  `sshd not found`, and every SSH validation degrades to the local-kind path
  and records that in the PR body.
- **(j) No new crate dependency** *(auto default)*: `serde`/`serde_json`/
  `toml`/`tokio`/`interprocess`/`tracing` cover everything. No `Cargo.toml` /
  `Cargo.lock` change in E1.
- **(k) Reserved host id `local`** *(auto default)*: the implicit default
  session host is `local`; a `[[fleet.hosts]]` named `local` while
  `include_local = true` is a config diagnostic.

### Sequencing hazards

- **Upstream files touched, and by which PR only:** `src/config/model.rs`,
  `src/config/io.rs`, `src/config.rs`, `src/main.rs` (`mod fleet;` +
  `DEFAULT_CONFIG` block) and `scripts/config_reference_check.py`
  (one `SKIPPED_SUBTREES` entry, see PR 1) — PR 1. `src/remote/attach.rs` — PR 3 only.
  `src/cli.rs`, `src/cli/spec.rs`, `src/main.rs` (help text + bare-command
  list) — PR 5. `tests/cli/mod.rs` — PR 5. Nothing else upstream is edited;
  `src/protocol/**`, `tests/fixtures/endpoint-*.json`, `justfile`,
  `Cargo.toml`, `Cargo.lock` are untouched by E1.
- **`src/main.rs` is edited by PR 1 and PR 5** in disjoint regions (module
  list + `DEFAULT_CONFIG` vs help text); PR 5 depends on PR 1 transitively, so
  there is no concurrent edit.
- **`src/fleet/mod.rs` is edited by PRs 1, 2, 5, 6** (adding `mod` lines).
  Waves keep at most one of them in flight except W4 `[6]` alone; the later PR
  in any wave must rebase onto `master` before its gate.
- **PR 3 (attach.rs refactor) and PR 4 (ssh lab) run in the same wave with
  PR 2:** PR 3's real-server validation needs PR 4 merged, so PR 3 depends on
  PR 4 and W2 is `[2, 3]` after W1 `[1, 4]`.
- **Two Rust builds at once** (W1, W2) is the memory hazard the gate lock
  exists for; each worktree has its own `target/`. Never bypass
  `scripts/fork/gate.sh`.
- **The ssh lab kills a pid and `rm -rf`s a directory**; it must only ever act
  under `$HERDR_FLEET_LAB_ROOT/ssh` with its own marker, and must never read
  or write `~/.ssh`, `~/.config/herdr*`, `~/.local/bin`.
- **`herdr --remote` validation must never reach the user's herdr.** The
  ssh lab's `SetEnv HOME/PATH/XDG_CONFIG_HOME` is what guarantees the remote
  side runs the lab wrapper; PR 3's validation asserts that (`server_version
  == 0.8.2-fork`, no new pid in the lab, `/usr/bin/herdr` and `~/.config`
  checksums unchanged).

## Status legend

✅ merged · 🔨 in progress · ⬜ not started · ⛔ blocked

## PR map

| # | Title | Group | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | feat: add fleet config section and host specs | A · Foundations | — | ✅ |
| 2 | feat: pure fleet state merges host snapshots and renders a status report | A · Foundations | 1 | ✅ |
| 3 | refactor: expose ssh stdio bridge and remote discovery for reuse | B · Transport | 4 | ✅ |
| 4 | feat: ssh lab script runs a user-space sshd against the fleet lab | B · Transport | — | ✅ |
| 5 | feat: fleet connector streams local hosts and herdr fleet status reports them | C · Connector and CLI | 2 | ⬜ |
| 6 | feat: fleet connector reaches ssh hosts through the shared bridge | C · Connector and CLI | 3, 5 | ⬜ |
| 7 | docs: fleet core reference, ssh lab guide, adr review | D · Docs | 6 | ⬜ |

**Wave preview (2-agent cap):** W1 `[1, 4]` → W2 `[2, 3]` → W3 `[5]` → W4
`[6]` → W5 `[7]`. Critical path 1 → 2 → 5 → 6 → 7. No PR touches
`Cargo.toml`/`Cargo.lock`.

**Model assignment:** tasks run on `opus`. Review agent must be **`fable`** for
**PR 3** (the SSH transport refactor of an upstream file; `herdr --remote`
must behave identically), **PR 4** (the script spawns `sshd`, kills pids,
`rm -rf`s a root, and sets `SetEnv HOME`; it must never touch `~/.ssh` or the
user's herdr), **PR 5** (`send(host, …)`/active-host gating could silently
route input or frames to the wrong host), and **PR 6** (ssh hosts: the remote
command string, never installing or stopping anything on a host).

## Verification (the gate — every PR)

```bash
bash scripts/fork/gate.sh <worktree>        # runs `just ci` under the machine-wide lock
echo "EXIT=$?"                              # read the EXIT= line; never pipe the wrapper
```

`just ci` = `cargo fmt --check` + `cargo clippy --all-targets --locked -D
warnings` + `cargo nextest run --locked` + python maintenance tests + bun
suites. While iterating use `bash scripts/fork/gate.sh <worktree> "test-one
<filter>"` (a filter that matches nothing exits 4).

Real-server validation is mandatory for every PR with runtime code (1, 3, 4,
5, 6) and is written per PR below; PR 2 is pure state (its evidence is the
unit suite plus the frozen fixture `tests/fixtures/endpoint-snapshot-v1.json`
round-tripping through `FleetState`), PR 7 walks its documented commands.
Always isolate: `bash scripts/fork/fleet-lab.sh up <n>` +
`eval "$(bash scripts/fork/fleet-lab.sh env)"` and
`H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV
target/debug/herdr"`; SSH validations add `eval "$(bash
scripts/fork/ssh-lab.sh env)"` and run herdr with `HOME=$HERDR_SSH_LAB_HOME`.
Tear down with `ssh-lab.sh down` then `fleet-lab.sh down`.

CI gotchas to expect:

- libghostty-vt needs Zig 0.15.2 (`mlugg/setup-zig`) — already in
  `fork-ci.yml`; no workflow edit in E1.
- `src/cli/spec.rs` tests (`spec_describes_all_completion_commands`,
  `every_spec_subcommand_renders_short_and_long_help`,
  `spec_passes_clap_invariants`) require every new command/subcommand to have
  `.about(...)`.
- `just check`'s `windows-lint` is not in the gate but the code must still
  compile for Windows: keep `src/fleet/` platform-neutral; gate unix-only
  helpers (`shutdown_local_stream_write`, the ssh lab test) with `#[cfg(unix)]`.
- `ubuntu-latest` may or may not ship `sshd`; `tests/fork_ssh_lab.rs` asserts
  one of two explicit outcomes (lab works end to end, or `up` fails with exit 3
  `sshd not found`) — it never silently passes.
- nextest boots real servers; PR 5's `tests/cli/fleet.rs` adds two named
  servers per test, well within the 30-minute job.
- `scripts/config_reference_check.py` is not in `just ci`; `[[fleet.hosts]]` is
  an array of tables that check does not model. Do not edit `docs/next/`.
- Scripts must be `shellcheck -S warning` clean; no `setsid` (macOS).

## Cross-cutting constraints (all PRs)

- **Additive, minimal wiring.** New code under `src/fleet/`, `src/cli/fleet.rs`,
  `scripts/fork/ssh-lab.sh`, `tests/cli/fleet.rs`, `tests/fork_ssh_lab.rs`,
  `docs/fork/`. Upstream edits are limited to the list in *Sequencing hazards*.
- **No wire or endpoint-contract change.** `src/protocol/wire.rs`,
  `src/protocol/endpoint.rs`, `tests/fixtures/endpoint-*.json` stay untouched;
  `PROTOCOL_VERSION` (22) is not bumped; no new `ClientMessage`/`ServerMessage`
  variant, no new `EndpointControl` kind. The connector is a plain
  generation-1 client: hello → welcome (generation 1 + the four `*_V1` codec
  names, else `Incompatible`) → JSON snapshots.
- **Servers stay stock.** Nothing in E1 changes `src/server/`, `src/app/`,
  `src/api/`. A fleet host may run upstream 0.8.2.
- **State is separated from runtime.** `src/fleet/state.rs` and
  `src/fleet/report.rs` import only `crate::protocol` data types,
  `crate::api::schema::AgentStatus`, `serde`, `std` — no `tokio`, `ratatui`,
  `interprocess`, `crate::ipc`, `crate::remote`. Enforce with a unit test that
  greps the file for forbidden `use` paths (precedent:
  `scripts/test_ui_hot_path_architecture.py` guards hot paths).
- **Host failure is local.** Every `io::Error`, handshake rejection, framing
  error, ssh exit, or discovery failure becomes `HostConnection::Unavailable
  { reason }` (or `Incompatible`) for that host; the connector never calls
  `std::process::exit`, never panics on host input, never propagates one
  host's error to another, and keeps reconnecting with 1 s → 30 s backoff.
- **Multiplicative performance.** Reader threads are per host; frames from
  inactive hosts are dropped before allocation into the event channel; the
  merged agent list is recomputed only when a snapshot or connection changes
  (cache in `FleetState`, invalidated by `apply`), never per render. No
  per-frame allocation for inactive hosts.
- **Code conventions.** No `unwrap()` in production code (tests may);
  `tracing` (`debug!`/`warn!` with `host = %id` fields) instead of
  `eprintln!` in fleet code; `#[allow]` only with a reason; `#[cfg(unix)]` /
  `#[cfg(windows)]` gating, never `cfg!` for OS behaviour; no new dependency.
- **Read is safe.** E1 exposes read paths plus a per-host endpoint request
  lane; nothing in E1 sends input on its own. `herdr fleet status` never
  sends `ClientShellPaneInput`.
- **Never** run `herdr server stop`, `herdr update`, or any command against
  the user's live session; never read/write `~/.ssh`, `~/.config/herdr*`,
  `~/.local/bin`; test fixtures use `localhost`, named sessions, throwaway
  `XDG_CONFIG_HOME` and a fake `HOME`.
- **Docs discipline.** Do not edit `docs/next/**`, root `README.md`,
  `CHANGELOG.md`, `skills/herdr/SKILL.md`, `distribution/**`. Fork docs go in
  `docs/fork/`. No `refs #<n>` lines. Commit trailers as the environment
  specifies.
- **Branches:** `feat/e1-pr1-fleet-config`, `feat/e1-pr2-fleet-state`,
  `refactor/e1-pr3-ssh-transport`, `feat/e1-pr4-ssh-lab`,
  `feat/e1-pr5-fleet-connector`, `feat/e1-pr6-fleet-ssh-hosts`,
  `docs/e1-pr7-fleet-core-docs`, each from the latest `origin/master`, in
  `.claude/worktrees/<slug>`, all git through `git -C`.

## Per-PR detail

### PR 1 — feat: add fleet config section and host specs · deps: —

**Goal:** `[fleet]` parses from `config.toml` with defaults, validates into a
list of typed host specs (the implicit `local` host first), reports
diagnostics through the existing config path, and appears in
`herdr --default-config`.

**Files**

- `src/config/model.rs` *(upstream file — minimal wiring)*: `pub fleet:
  FleetConfig` on `Config` (after `remote`), plus `FleetConfig`,
  `FleetHostConfig`, `FleetHostKind` (definitions live here so
  `KNOWN_TOP_LEVEL_CONFIG_KEYS`/`deserialize_with_ignored` treat it like every
  other section).
- `src/config/io.rs` *(upstream file — minimal wiring)*: add `"fleet"` to
  `KNOWN_TOP_LEVEL_CONFIG_KEYS`.
- `src/config.rs` *(upstream file — minimal wiring)*: re-export
  `FleetConfig`, `FleetHostConfig`, `FleetHostKind`; chain
  `self.fleet.diagnostics()` in `Config::collect_diagnostics()`.
- `src/main.rs` *(upstream file — minimal wiring)*: `mod fleet;` in the module
  list; a commented `[fleet]` block in `DEFAULT_CONFIG` after `[remote]`.
- `src/fleet/mod.rs` (new): module doc, `pub mod hosts;`, re-exports.
- `src/fleet/hosts.rs` (new): `HostId`, `HostKind`, `HostSpec`,
  `resolve_hosts`.

**Shapes/approach**

```rust
// src/config/model.rs
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct FleetConfig {
    /// Include this machine's default session as host "local". Default: true.
    pub include_local: bool,
    /// Additional hosts; see `[[fleet.hosts]]` in `herdr --default-config`.
    pub hosts: Vec<FleetHostConfig>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FleetHostConfig {
    pub name: String,            // required; id prefix; validate_name rules
    pub kind: FleetHostKind,     // "ssh" | "local"; default ssh
    pub target: Option<String>,  // ssh destination; required for ssh
    pub session: Option<String>, // named session; required for local
    pub enabled: bool,           // default true
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetHostKind { Ssh, Local }
impl FleetConfig { pub fn diagnostics(&self) -> Vec<String> }
```

`FleetConfig::diagnostics()` (pure): empty/invalid `name`
(`crate::session::validate_name`), duplicate names, `local` reserved while
`include_local`, `ssh` without `target`, `target` starting with `-` (the
`validate_remote_target` rule from `attach.rs`), `local` without `session`,
invalid `session` name. Messages follow upstream's `[section] key: reason`
style.

```rust
// src/fleet/hosts.rs
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HostId(String);            // FromStr/Display; never contains '/'
impl HostId { pub const LOCAL: &str = "local"; pub fn new(s: &str) -> Result<Self, String>; pub fn as_str(&self) -> &str }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKind { Local { session: Option<String> }, Ssh { target: String, session: Option<String> } }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec { pub id: HostId, pub kind: HostKind, pub enabled: bool }
pub fn resolve_hosts(config: &FleetConfig) -> Result<Vec<HostSpec>, Vec<String>>
```

`resolve_hosts` returns the implicit `local` host (`HostKind::Local { session:
None }`) first when `include_local`, then configured hosts in file order;
`Err` carries the same diagnostics as `FleetConfig::diagnostics()` so the CLI
can refuse to run on an invalid section while the TUI keeps upstream's
"diagnostic banner" behaviour.

`DEFAULT_CONFIG` block (commented, matching the roadmap sample):

```toml
[fleet]
# Hosts aggregated by `herdr fleet status` and the fork's fleet console.
# This machine's default session is always host "local" unless disabled.
# include_local = true
#
# [[fleet.hosts]]
# name = "workbox"        # display name and id prefix (workbox/w1:p1)
# kind = "ssh"            # "ssh" | "local"
# target = "workbox"      # ssh destination (alias, user@host, ssh://host:2222)
# session = "agents"      # optional named session on that host; required for kind = "local"
# enabled = true
```

**Tests**

- `src/config/model.rs`: `[fleet]` absent → defaults (`include_local = true`,
  no hosts); full sample parses; unknown key under `[fleet]`/`[[fleet.hosts]]`
  yields the standard ignored-key diagnostic (`deserialize_with_ignored`);
  each diagnostic rule above (one test per rule, plus a valid multi-host
  config with zero diagnostics); `Config::collect_diagnostics()` includes
  fleet diagnostics.
- `src/fleet/hosts.rs`: `HostId::new` accepts `validate_name` names, rejects
  `/`, empty, > 64; `resolve_hosts` orders local first, honours
  `include_local = false`, keeps `enabled = false` hosts (as disabled specs),
  returns `Err` with every diagnostic.
- `src/main.rs`: a test that the `[fleet]` block of `DEFAULT_CONFIG`
  (uncommented) parses into `Config` with zero diagnostics — the first
  round-trip test of the reference config; scope it to the fleet block so it
  is not brittle against upstream edits.

**Real-server validation**

```bash
cargo build
XDG_CONFIG_HOME=/tmp/herdr-e1-pr1 && mkdir -p $XDG_CONFIG_HOME/herdr-dev
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV XDG_CONFIG_HOME=$XDG_CONFIG_HOME target/debug/herdr"
$H --default-config | sed -n '/^\[fleet\]/,/^\[experimental\]/p'   # block present, commented
$H --default-config | sed 's/^# \(include_local\|\[\[fleet\|name\|kind\|target\|session\|enabled\)/\1/' > $XDG_CONFIG_HOME/herdr-dev/config.toml
$H config check   # or the existing config-diagnostic path: zero diagnostics
printf '[fleet]\ninclude_local = true\n[[fleet.hosts]]\nname = "local"\nkind = "local"\n' > $XDG_CONFIG_HOME/herdr-dev/config.toml
$H config check   # reports the reserved-name and missing-session diagnostics
```

(If `herdr config check` does not exist, use the path upstream uses to surface
`LoadedConfig.diagnostics` — `herdr status --json`'s `config_diagnostic` or
starting a server and reading the snapshot's `config_diagnostic` — and record
which one was used.) Evidence: the printed block, the zero-diagnostic run, the
two expected diagnostics. `rm -rf /tmp/herdr-e1-pr1`.

**Downstream**

- `FleetConfig`/`FleetHostConfig` field names are the user-facing contract;
  add fields, never rename. E5 adds nothing here (MagicDNS names are just
  `target`s).
- `HostSpec`/`HostKind` is what PR 5/6's connector consumes; `HostId::LOCAL`
  is the implicit host's id everywhere (CLI, JSON, E2 sidebar).
- `resolve_hosts` is the only place config becomes specs; E2's `herdr fleet`
  launch and E3's gateway call it, never `FleetConfig` directly.

**As built (merged)** — differences later PRs must know about:

- **`src/fleet/mod.rs` has no re-exports yet and carries
  `#[allow(dead_code)] pub mod hosts;`** with a reason comment: nothing in
  production calls the module until the connector lands, and test-only use
  does not satisfy the lint. The allow is on the `hosts` module only, *not* an
  inner `#![allow]` on `mod.rs`, so `state.rs`/`report.rs`/the connector are
  not silently covered. **PR 5 removes the allow from `hosts` once the
  connector consumes it**, and PRs 2/5/6 must add their own narrow allow (or
  none) rather than widening this one. Consumers use the
  `crate::fleet::hosts::…` path; add `pub use` lines when a consumer exists.
- **`scripts/config_reference_check.py` gained `"fleet"` in
  `SKIPPED_SUBTREES`.** `just ci`'s `maintenance-test` runs
  `scripts/test_config_reference_check.py` against the *real* config model,
  and it (a) refuses an un-skipped `Vec<struct>` like `fleet.hosts` and
  (b) demands a row in `docs/next/website/src/data/config-reference.json` for
  every other key. Fork rules forbid editing `docs/next/**`, so the whole
  `[fleet]` subtree is skipped and documented in prose under `docs/fork/`
  (PR 7). The skip is an exact match on the dotted path, so it masks no
  upstream drift outside `[fleet]`. **Any new `[fleet]` key is invisible to
  that check** — PR 7's reference doc is the only place it is documented.
- **`src/config/io.rs` also wires the live-reload path.**
  `load_live_config_from_str` builds a `Config::default()` and applies
  sections one at a time; without a `load_live_section(table, "fleet", …)`
  call plus `diagnostics.extend(config.fleet.diagnostics())`, `[fleet]`
  parsed at startup but silently reverted to defaults on
  `herdr server reload-config`. Both are now wired, so E2 can rely on
  `[fleet]` surviving a live reload and on a bad `[fleet]` being isolated to
  `invalid_sections == ["fleet"]`.
- **Extra diagnostics beyond the plan's list:** a `target` on a
  `kind = "local"` host (it is silently dropped by `host_spec`, so a typo
  would attach to a same-named *local* session instead of the machine named),
  and a `target` that is empty, whitespace-only or contains control
  characters (an ssh destination is one argv element). `HostId::new`'s error
  messages are `session::validate_name`'s with the leading `session name`
  rewritten to `host name`; a test pins all four messages so an upstream
  rewording is caught.
- **Config keys are `[fleet] include_local` and `[[fleet.hosts]]`
  `name`/`kind`/`target`/`session`/`enabled` exactly as specified**;
  `kind` defaults to `"ssh"`, `enabled` to `true`, and an unknown `kind`
  value fails the section (isolated per section on the live path).
- `resolve_hosts` is all-or-nothing: any diagnostic → `Err(diagnostics)`,
  including for `enabled = false` hosts, so ids stay unique and typos surface.
  PR 5's CLI exits 1 on `Err` (decision (h)).

### PR 2 — feat: pure fleet state merges host snapshots and renders a status report · deps: 1

**Goal:** `FleetState` — pure data, no sockets, no async — holds per-host
connection state and the latest snapshot, exposes host-qualified refs, the
merged blocked-first agent list, per-host roll-ups, an `apply` that returns
deltas, and a serializable `FleetStatusReport` that is the shape of
`herdr fleet status --json` and E3's `GET /api/fleet`.

**Files**

- `src/fleet/refs.rs` (new): `FleetPaneRef`, `FleetTabRef`,
  `FleetWorkspaceRef`.
- `src/fleet/state.rs` (new): `HostConnection`, `HostEvent`, `HostState`,
  `FleetState`, `MergedAgent`, `AgentRollup`, `FleetChange`, `Backoff`.
- `src/fleet/report.rs` (new): `FleetStatusReport` + `render_text`.
- `src/fleet/mod.rs`: `pub mod refs; pub mod report; pub mod state;`.

**Shapes/approach**

```rust
// refs.rs — Display "host/w1:p1", FromStr splits on the first '/'
pub struct FleetPaneRef { pub host: HostId, pub pane_id: String }
pub struct FleetTabRef { pub host: HostId, pub tab_id: String }
pub struct FleetWorkspaceRef { pub host: HostId, pub workspace_id: String }
// serde as the string form (Serialize/Deserialize via Display/FromStr)

// state.rs
pub enum HostConnection {
    Connecting { attempt: u32 },
    Connected { server_version: String, methods: Vec<String> },
    Unavailable { reason: String, retry_in: Option<Duration> },
    Incompatible { generation: Option<u32>, reason: String },
}
pub enum HostEvent {
    Connecting { attempt: u32 },
    Connected { server_version: String, methods: Vec<String> },
    Snapshot(Box<ClientShellSnapshot>),
    Unavailable { reason: String, retry_in: Option<Duration> },
    Incompatible { generation: Option<u32>, reason: String },
}
pub struct HostState { pub spec: HostSpec, pub connection: HostConnection,
    pub snapshot: Option<Box<ClientShellSnapshot>>, pub rollup: AgentRollup, /* private */ seen: HashMap<String, (u64 /*state_change_seq*/, AgentStatus, u64 /*fleet_change_seq*/)> }
pub struct AgentRollup { pub blocked: usize, pub working: usize, pub done: usize, pub idle: usize, pub unknown: usize } // + total()
pub struct MergedAgent { pub pane: FleetPaneRef, pub workspace: FleetWorkspaceRef, pub tab: FleetTabRef,
    pub workspace_label: String, pub name: Option<String>, pub title: Option<String>, pub agent: Option<String>,
    pub display_agent: Option<String>, pub agent_status: AgentStatus, pub state_change_seq: u64,
    pub fleet_change_seq: u64, pub focused: bool }
pub enum FleetChange {
    HostConnection { host: HostId, connection: HostConnection },
    Snapshot { host: HostId, boot_id: String, revision: u64 },      // structure changed
    AgentAdded { agent: MergedAgent }, AgentRemoved { pane: FleetPaneRef },
    AgentStatus { pane: FleetPaneRef, from: AgentStatus, to: AgentStatus },
    ActiveHost { host: Option<HostId> },
}
pub struct FleetState { hosts: Vec<HostState>, active_host: Option<HostId>, change_seq: u64, merged: Option<Vec<MergedAgent>> /* cache */ }
impl FleetState {
    pub fn new(specs: Vec<HostSpec>) -> Self;           // all Connecting{attempt:0}, active = first enabled
    pub fn hosts(&self) -> &[HostState];  pub fn host(&self, id: &HostId) -> Option<&HostState>;
    pub fn active_host(&self) -> Option<&HostId>;  pub fn set_active_host(&mut self, id: Option<HostId>) -> Vec<FleetChange>;
    pub fn apply(&mut self, host: &HostId, event: HostEvent) -> Vec<FleetChange>;  // unknown host → empty vec + tracing::warn
    pub fn merged_agents(&mut self) -> &[MergedAgent];  // cached; see decision (d)
    pub fn totals(&self) -> AgentRollup;
}
pub struct Backoff { current: Duration }  // new() = 1 s; next() doubles to a 30 s cap; reset()
```

Snapshot rules (mirror `ClientShellState::set_snapshot`): same `boot_id` and
`revision < current.revision` → ignored (no change); new `boot_id` always
replaces and clears `seen` (a restarted server's `state_change_seq` restarts);
otherwise replace, recompute the roll-up from `agents[].agent_status`, diff
`seen` to emit `AgentAdded`/`AgentRemoved`/`AgentStatus`, assign
`fleet_change_seq = ++change_seq` to agents that are new or whose
`(state_change_seq, agent_status)` advanced, and invalidate the merged cache.
A host that turns `Unavailable`/`Incompatible` keeps its last snapshot
(E2 dims it) but its agents leave the merged list and roll-up until it
reconnects; the transition emits `AgentRemoved` for each. Merged order:
`(status_rank, Reverse(fleet_change_seq), host_index, pane_id)` with
`status_rank` blocked 0 < working 1 < done 2 < idle 3 < unknown 4.

```rust
// report.rs — additive JSON contract (fields are added, never renamed)
#[derive(Serialize, Deserialize)]
pub struct FleetStatusReport { pub schema: String /* "herdr.fleet.status.v1" */, pub client_version: String,
    pub active_host: Option<HostId>, pub hosts: Vec<HostReport>, pub agents: Vec<AgentReport>, pub counts: AgentRollup }
pub struct HostReport { pub id: HostId, pub kind: &'static str /* "local"|"ssh" */, pub target: Option<String>,
    pub session: Option<String>, pub enabled: bool, pub connection: ConnectionReport,
    pub boot_id: Option<String>, pub revision: Option<u64>, pub counts: AgentRollup, pub workspaces: Vec<WorkspaceReport> }
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionReport { Connecting { attempt: u32 }, Connected { server_version: String },
    Unavailable { reason: String, retry_in_ms: Option<u64> }, Incompatible { generation: Option<u32>, reason: String },
    #[serde(other)] Unknown }
pub struct WorkspaceReport { pub r#ref: FleetWorkspaceRef, pub workspace_id: String, pub label: String, pub agent_status: AgentStatus, pub focused: bool }
pub struct AgentReport { pub r#ref: FleetPaneRef, pub host: HostId, pub pane_id: String, pub workspace_id: String, pub tab_id: String,
    pub workspace_label: String, pub name: Option<String>, pub title: Option<String>, pub agent: Option<String>, pub display_agent: Option<String>,
    pub agent_status: AgentStatus, pub state_change_seq: u64, pub fleet_change_seq: u64, pub focused: bool }
impl FleetStatusReport { pub fn from_state(state: &mut FleetState, client_version: &str) -> Self; pub fn render_text(&self) -> String }
impl FleetChange { /* Serialize with #[serde(tag = "kind", rename_all = "snake_case")] */ }
```

`render_text` prints a host table (`HOST KIND STATE VERSION BLOCKED WORKING
DONE IDLE UNKNOWN`) then one line per merged agent (`host/w1:p1 blocked
<workspace_label> <name or title>`); unavailable hosts show the reason.

Testing hooks under `#[cfg(test)] impl FleetState`: `test_new()` (two local
hosts, no snapshots), `test_with_adversarial_identity_state()` (two hosts whose
snapshots share `boot_id`, `w1:p1` and `state_change_seq`, one host
`Incompatible`, an `active_host` naming a disabled host),
`assert_invariants_for_test()` (unique host ids; `active_host` names a known
host; `rollup == recount(snapshot)` for connected hosts and is zero otherwise;
every `fleet_change_seq ≤ change_seq`; merged refs unique; merged order is
sorted by the key; cache, when present, equals a fresh computation).

**Tests**

- refs: `Display`/`FromStr` round-trips for pane/tab/workspace; rejects
  missing `/`, empty host, host with `/`; serde uses the string form.
- state: snapshot revision drop / boot replace / cross-boot `seen` reset;
  roll-up counts including `Unknown`; merged order across hosts with equal
  `state_change_seq` (deterministic by host index then pane id); a status
  change bumps `fleet_change_seq` and moves the agent to the front of its
  rank; `AgentAdded`/`Removed`/`Status` deltas exact; unknown host id in
  `apply` is a no-op; `Unavailable` removes agents from the merged list and
  emits removals, reconnection restores them; `set_active_host` on a
  disabled/unknown host is rejected (`Vec::new()`), `test_new().
  assert_invariants_for_test()` and the adversarial state after every
  mutation sequence; `Backoff` 1 → 2 → 4 → … → 30 → 30, `reset`.
- report: `from_state` on the frozen fixture
  `tests/fixtures/endpoint-snapshot-v1.json` loaded into two hosts produces
  refs prefixed by each host; JSON round-trips; `ConnectionReport::Unknown`
  deserializes from an unknown `state`; `render_text` golden string for a
  two-host state with one unavailable host; `FleetChange` JSON has `kind`.
- architecture guard: `state.rs`/`report.rs`/`refs.rs` contain no `use` of
  `tokio`, `ratatui`, `interprocess`, `crate::ipc`, `crate::remote`,
  `crate::client`.

**Real-server validation**

Pure state; no servers. Evidence = green `test-one fleet::` output and the
fixture-driven report test. (PR 5 proves the state against live hosts.)

**Downstream**

- `FleetState::apply`/`FleetChange` is E3's delta stream and E2's shell
  update hook; `FleetStatusReport` is `GET /api/fleet`. Both are
  append-only: new fields optional, new enum variants only with an `Unknown`
  fallback on the reading side.
- `merged_agents()` is the only sanctioned ordering; E2's *per-host* sidebar
  rows use upstream `status_priority`, E2/E4's *fleet* lists use this.
- `HostConnection::Connected.methods` is what E7's `request(host, …)`
  checks before enabling an action.
- `Backoff` is reused by PR 5's connector; do not duplicate.

**As built (merged)**

Shipped as `src/fleet/refs.rs`, `src/fleet/state.rs`, `src/fleet/report.rs`
plus the module wiring and the architecture guard in `src/fleet/mod.rs`. The
three modules carry `#[allow(dead_code)]` for the same reason PR 1's `hosts`
does; **PR 5 removes all four** as it becomes the first production consumer.
Deviations from the shapes above, all deliberate:

- `FleetChange::AgentAdded { agent: Box<MergedAgent> }` — boxed for
  `clippy::large_enum_variant` (`-D warnings`). The JSON is unchanged.
- `HostReport.kind` is `String`, not `&'static str`: the report has to
  `Deserialize`, and a reader must tolerate a transport name it does not know.
- A **disabled** host starts `Unavailable { reason: "host disabled in
  [fleet]" }` instead of `Connecting { attempt: 0 }` — the connector never
  opens it, so "connecting" forever would be a lie. Enabled hosts start
  `Connecting { attempt: 0 }` as specified.
- `FleetChange::HostConnection` serializes its connection **as
  `ConnectionReport`**, so one JSON vocabulary describes a connection
  everywhere. The advertised `methods` list is therefore not carried in the
  delta: a `HostConnection` rebuilt from JSON advertises nothing and every
  method-gated action fails closed. Read `methods` from `FleetState::host` or
  the report, never from the delta stream.
- Contribution is gated on `Connected` **and** a snapshot, not only on
  `Unavailable`/`Incompatible`: any transition out of `Connected` (including
  back to `Connecting`) emits `AgentRemoved` for that host's agents and zeroes
  its roll-up, and re-entering `Connected` re-emits `AgentAdded` keeping each
  agent's existing `fleet_change_seq` so a blip does not reshuffle the list.
- Snapshot ingestion drops any agent whose `pane_id`/`workspace_id`/`tab_id`
  is empty or contains `/` (`refs::is_valid_resource_id`): such an id would
  render a reference that parses back into a *different host*. Report
  workspaces are filtered the same way.
- Extras PR 5/6 may rely on: `HostConnection::{is_connected, state_name,
  reason}`, `HostState::{id, contributes_agents}`, `AgentRollup::total`,
  `Backoff::peek`, `FleetPaneRef::{new, host, id}` (and the tab/workspace
  equivalents), `report::FLEET_STATUS_SCHEMA`,
  `ConnectionReport::{state_name, reason, server_version, into_connection}`.
- `FleetChange` is `Serialize` **and** `Deserialize`; `FleetStatusReport`
  round-trips through JSON (a test asserts it).

### PR 3 — refactor: expose ssh stdio bridge and remote discovery for reuse · deps: 4

**Goal:** the SSH stdio bridge and remote-herdr discovery in
`src/remote/attach.rs` become callable from `src/fleet/` without owning the
process or a tty, with `herdr --remote` behaviour byte-for-byte unchanged.
Characterization tests land **before** the code moves.

**Files**

- `src/remote/attach.rs` *(upstream file — the one non-trivial edit, see
  decision (a))*.
- `src/remote.rs`: unchanged (`pub(crate) use attach::*;` already re-exports).

**Shapes/approach** (all additive; existing signatures keep working)

- Visibility to `pub(crate)`: `SshStdioBridge` (+ `start`), `RemoteSsh`
  (`new`, `target`, `options`, `command`, `base_command`, `sh_output`,
  `user_shell_output`), `ManagedSshOptions`, `RemoteHerdr` (`for_platform`,
  `with_shell_path`, `shell_path` field or accessor), `RemotePlatform`,
  `remote_bridge_command`, `local_forward_socket_path`,
  `remote_binary_supports_endpoint`, `remote_client_status`,
  `detect_remote_platform`, `remote_binary_candidates`.
- `pub(crate) enum BridgeErrorSink { Stderr, Report(Arc<dyn Fn(String) + Send + Sync>) }`
  and `SshStdioBridge::start_with(target, remote_herdr, local_socket,
  session_name, ssh_options, sink) -> io::Result<Self>`; `start(...)` becomes
  `start_with(..., BridgeErrorSink::Stderr)`. The accept thread reports
  `remote bridge failed: {err}` through the sink (unchanged text for
  `Stderr`).
- `pub(crate) fn local_forward_socket_path_scoped(scope: &str, target, session)
  -> PathBuf`; `local_forward_socket_path(target, session)` =
  `…_scoped("", …)` producing **exactly today's names** (scope folded into the
  hash and the readable name only when non-empty, e.g.
  `herdr-remote-{pid}-{scope}-{target}-{session}.sock`).
- `pub(crate) fn discover_remote_herdr(ssh: &RemoteSsh) ->
  io::Result<Option<RemoteHerdr>>` = detect platform → `remote_binary_candidates`
  → first candidate with `remote_binary_supports_endpoint`. **No override, no
  install, no prompt.** `prepare_remote_herdr` calls it after applying the
  `HERDR_REMOTE_BINARY` override exactly as today (override first, then
  discovery, then the existing install path) so its observable behaviour and
  ordering do not change.
- Nothing else moves. `bridge_connection`, `prepare_remote_bridge_stream`,
  `write_managed_ssh_config`, `run_remote`, `run_client_process`,
  `ensure_remote_server_ready`, the interactive confirmations and
  `extract_remote_args` are untouched.

**Tests** (characterization first — commit them in the branch before the
refactor commit so the diff shows they pass on both sides)

- Existing attach tests stay green unchanged (`bridge_socket_is_user_only`,
  `accepted_bridge_stream_is_reset_to_blocking`,
  `remote_bridge_command_uses_installed_binary`,
  `local_forward_socket_path_*`, managed-config tests, `extract_remote_args_*`).
- New golden tests: `remote_bridge_command` for default and named session
  (`exec "$HOME/.local/bin/herdr" --session 'lab-1' remote-client-bridge`);
  `local_forward_socket_path` readable/short names for fixed inputs equal the
  pre-refactor strings (compute them on `master` first and paste);
  `_scoped("", …) == unscoped`; two scopes with the same target/session give
  different paths.
- `bridge_proxies_a_local_connection_through_ssh_stdio` (unix): write a fake
  `ssh` script to a temp dir that appends its argv to a file and then
  `exec cat`s (a byte echo, ignoring the remote command), prepend the dir to
  `PATH` (nextest runs one process per test; take `remote_env_lock()` anyway),
  start the bridge with `RemoteHerdr::for_platform(RemotePlatform::local())`,
  connect to the local socket, write bytes, read them back, drop the bridge,
  and assert the argv file shows `-T <target> exec "$HOME/.local/bin/herdr"
  --session 'lab-1' remote-client-bridge` and that the socket file is gone. A
  second variant through `start_with(…, BridgeErrorSink::Report(_))` with the
  fake `ssh` exiting 7 asserts the sink receives `remote bridge failed: ssh
  bridge exited with exit status: 7`.
- `discover_remote_herdr` unit test through a fake `ssh` whose `-T … /bin/sh -s`
  script replies with canned `uname`/`command -v`/`status client --json`
  output (generation 1 → `Some`, generation 2 / no binary → `None`), and a
  test that `prepare_remote_herdr` with `HERDR_REMOTE_BINARY` pointing at a
  temp file still returns the override path first (today's behaviour).

**Real-server validation** (this is where the `fable` review focuses)

```bash
cargo build
bash scripts/fork/fleet-lab.sh up 1 && eval "$(bash scripts/fork/fleet-lab.sh env)"
bash scripts/fork/ssh-lab.sh up && eval "$(bash scripts/fork/ssh-lab.sh env)"
sha256sum /usr/bin/herdr > /tmp/herdr-e1-pr3.before; ls -la ~/.ssh ~/.local/bin 2>/dev/null >> /tmp/herdr-e1-pr3.before
# 1. `herdr --remote` end to end against the lab through the user-space sshd, in a PTY:
python3 - <<'EOF'
import os, pty, select, time, sys
b = os.path.abspath("target/debug/herdr")
env = {k: v for k, v in os.environ.items() if k not in ("HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH", "HERDR_ENV")}
env["HOME"] = os.environ["HERDR_SSH_LAB_HOME"]; env["TERM"] = "xterm-256color"
pid, fd = pty.fork()
if pid == 0:
    os.execve(b, [b, "--session", "lab-1", "--remote", os.environ["HERDR_SSH_LAB_TARGET"]], env)
buf = b""; deadline = time.time() + 90
while time.time() < deadline and b"herdr-fleet-lab:lab-1" not in buf:
    r, _, _ = select.select([fd], [], [], 1)
    if r:
        try: buf += os.read(fd, 65536)
        except OSError: break
print("MARKER_SEEN" if b"herdr-fleet-lab:lab-1" in buf else "MARKER_MISSING")
os.kill(pid, 15)
EOF
# 2. nothing outside the lab changed, and no extra server was started:
sha256sum /usr/bin/herdr | diff - <(head -1 /tmp/herdr-e1-pr3.before)
bash scripts/fork/fleet-lab.sh status --json      # still exactly one lab-1 pid
ls "$HERDR_SSH_LAB_HOME/.local/bin"                # only the wrapper; no uploaded binary
ls "$HERDR_FLEET_LAB_ROOT/xdg/herdr-dev/sessions"  # only lab-1
# 3. the same run with ssh-lab down prints the ssh error and exits 1 (no hang, no panic):
bash scripts/fork/ssh-lab.sh down
HOME=$HERDR_SSH_LAB_HOME env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr --session lab-1 --remote herdr-ssh-lab </dev/null; echo "exit=$?"
bash scripts/fork/fleet-lab.sh down
```

Evidence: `MARKER_SEEN`, unchanged checksums, one pid, exit 1 with the
`ssh:`/`Connection refused` hint from `print_remote_error_hint`. If `ssh-lab.sh
up` reports `sshd not found`, record it and validate steps 2–3 of the unit
suite only — and say so in the PR body.

*From PR 4 (shipped):* always run `ssh-lab.sh down` **before**
`fleet-lab.sh down` — the fleet lab's `down` deletes `<root>/ssh` and the sshd
pid file, after which the sshd cannot be identified. `ssh-lab.sh down` sweeps
the listener's per-connection processes too, so the `ControlMaster`/
`ControlPersist` master `RemoteSsh` opens does not keep the lab alive; there
is no need to `ssh -O exit` by hand. `HERDR_SSH_LAB_SSHD` overrides the sshd
binary and a bad value fails hard (exit 1), so a typo can never be mistaken
for `sshd not found`.

**Downstream**

- PR 6 consumes exactly: `RemoteSsh::new(target, manage_ssh_config)`,
  `discover_remote_herdr`, `local_forward_socket_path_scoped(host_id, …)`,
  `SshStdioBridge::start_with(…, BridgeErrorSink::Report(_))`,
  `crate::ipc::connect_local_stream(local_socket)`. It must not call
  `prepare_remote_herdr`, `ensure_remote_server_ready`, `run_remote`.
- Any later upstream change to `attach.rs` is merged, never rewritten: keep
  the added items at the end of their sections to minimise conflict surface.

**As shipped** (branch `refactor/e1-pr3-ssh-transport`, two commits: the
characterization tests first, then the refactor). Exact `pub(crate)` surface
in `src/remote/attach.rs`, all reachable through `crate::remote::*`:

```rust
pub(crate) struct RemoteSsh;                       // + new(String, bool), target(), options(),
                                                   //   command(), base_command(), sh_output(&str),
                                                   //   user_shell_output(&str)
pub(crate) struct ManagedSshOptions;               // opaque; only passed back to start_with
pub(crate) struct RemotePlatform;                  // + local()
pub(crate) struct RemoteHerdr {                    // + for_platform(RemotePlatform),
    pub(crate) shell_path: String, /* … */ }       //   with_shell_path(String)
pub(crate) fn detect_remote_platform(&RemoteSsh) -> io::Result<RemotePlatform>;
pub(crate) fn remote_binary_candidates(&RemoteSsh, &RemoteHerdr) -> io::Result<Vec<RemoteHerdr>>;
pub(crate) fn remote_client_status(&RemoteSsh, &RemoteHerdr) -> io::Result<Option<RemoteClientStatusJson>>;
pub(crate) fn remote_binary_supports_endpoint(&RemoteSsh, &RemoteHerdr) -> io::Result<bool>;
pub(crate) fn discover_remote_herdr(&RemoteSsh) -> io::Result<Option<RemoteHerdr>>;
pub(crate) fn remote_bridge_command(&RemoteHerdr, session_name: &str) -> String;
pub(crate) fn local_forward_socket_path(target: &str, session_name: &str) -> PathBuf;
pub(crate) fn local_forward_socket_path_scoped(scope: &str, target: &str, session_name: &str) -> PathBuf;
#[derive(Clone)]
pub(crate) enum BridgeErrorSink { Stderr, Report(Arc<dyn Fn(String) + Send + Sync>) }
pub(crate) struct SshStdioBridge;
impl SshStdioBridge {
    pub(crate) fn start(String, RemoteHerdr, PathBuf, String, Option<&ManagedSshOptions>) -> io::Result<Self>;
    pub(crate) fn start_with(String, RemoteHerdr, PathBuf, String, Option<&ManagedSshOptions>, BridgeErrorSink) -> io::Result<Self>;
}
```

Deltas from the shapes above, all additive:

- `remote_client_status` returning `RemoteClientStatusJson` forced that struct
  to `pub(crate)` too (`private_interfaces` is denied by `clippy -D warnings`).
- `RemoteHerdr.shell_path` is a `pub(crate)` **field**, not an accessor.
- `short_socket_hash` became `short_socket_hash_scoped(scope, target, session)`
  and folds `scope` into the hash only when it is non-empty, so the unscoped
  names — readable *and* hashed — are byte-identical to `master`'s.
- `prepare_remote_herdr` and `discover_remote_herdr` share one private
  `first_remote_herdr_supporting_endpoint(ssh, default, candidates)` helper
  instead of `prepare_remote_herdr` calling `discover_remote_herdr`: calling
  discovery would repeat `detect_remote_platform` + `remote_binary_candidates`
  (three extra ssh round-trips) because the install path still needs the
  candidate list. Sharing the helper keeps the ssh calls, their order, and the
  mixed error handling (candidate probes `unwrap_or(false)`, the default-path
  probe `?`) exactly as on `master`.
- `discover_remote_herdr` and `BridgeErrorSink::Report` carry
  `#[allow(dead_code)]` with a reason comment naming PR 6. **PR 6 must delete
  both allows** once `src/fleet/transport/ssh.rs` constructs them.
- The `Stderr` sink still prints `herdr: remote bridge failed: {err}` and
  `herdr: remote bridge listener failed: {err}`; the sink receives the message
  without the `herdr: ` prefix.

### PR 4 — feat: ssh lab script runs a user-space sshd against the fleet lab · deps: —

**Goal:** `scripts/fork/ssh-lab.sh up|down|status [--json]|env` turns the
fleet lab into an SSH-reachable "remote host" with zero root and zero contact
with the user's `~/.ssh`, `~/.config` or installed herdr.

**Files**

- `scripts/fork/ssh-lab.sh` (new; shellcheck-clean; `#!/usr/bin/env bash`,
  `set -euo pipefail`).
- `tests/fork_ssh_lab.rs` (new, `#![cfg(unix)]`, `pub mod support;`).
- No edit to `fleet-lab.sh` (its `status --json` provides `root`, `bin`).

**Shapes/approach**

- Root: `$HERDR_FLEET_LAB_ROOT/ssh` (default `/tmp/herdr-fleet-lab/ssh`);
  requires the fleet lab's `.herdr-fleet-lab` marker; writes its own
  `.herdr-ssh-lab` marker (`bin=`, `port=`, `created=`).
- `up`: resolve `sshd` (`command -v sshd`, then `/usr/sbin/sshd`,
  `/usr/bin/sshd`; **absolute path**, sshd re-exec requires it) and
  `ssh-keygen`; exit 3 `sshd not found` when missing. Port
  `${HERDR_SSH_LAB_PORT:-2299}`; refuse if already bound. Create
  `home/` (0700) with `.ssh/id_ed25519(.pub)`, `.ssh/known_hosts` (empty),
  `.ssh/config`:

  ```
  Host herdr-ssh-lab
    HostName 127.0.0.1
    Port <port>
    User <id -un>
    IdentityFile <root>/home/.ssh/id_ed25519
    IdentitiesOnly yes
    UserKnownHostsFile <root>/home/.ssh/known_hosts
    StrictHostKeyChecking no
    LogLevel ERROR
  ```

  `home/.local/bin/herdr` wrapper (0755):

  ```sh
  #!/bin/sh
  exec env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV -u HERDR_SESSION -u HERDR_CONFIG_PATH \
    XDG_CONFIG_HOME=<lab>/xdg XDG_RUNTIME_DIR=<lab>/runtime XDG_STATE_HOME=<lab>/state XDG_DATA_HOME=<lab>/data XDG_CACHE_HOME=<lab>/cache \
    "<bin from fleet-lab status --json>" "$@"
  ```

  `hostkey`, `authorized_keys` (the lab pubkey, 0600), `sshd_config`:

  ```
  Port <port>
  ListenAddress 127.0.0.1
  HostKey <root>/hostkey
  PidFile <root>/sshd.pid
  AuthorizedKeysFile <root>/authorized_keys
  PasswordAuthentication no
  KbdInteractiveAuthentication no
  PubkeyAuthentication yes
  UsePAM no
  StrictModes no
  LogLevel ERROR
  SetEnv HOME=<root>/home XDG_CONFIG_HOME=<lab>/xdg PATH=<root>/home/.local/bin:/usr/local/bin:/usr/bin:/bin
  ```

  Start `"$SSHD" -f <root>/sshd_config -E <root>/sshd.log`, wait for the pid
  file and a successful `ssh -F <root>/home/.ssh/config -o BatchMode=yes
  herdr-ssh-lab 'command -v herdr'` that prints the wrapper path (timeout
  `HERDR_FLEET_LAB_TIMEOUT_MS`); on failure undo everything.
- `status [--json]`: `{"root":…, "port":…, "running":bool, "pid":n|null,
  "target":"herdr-ssh-lab", "home":…, "ssh_config":…}`.
- `env`: `export HERDR_SSH_LAB_ROOT=…`, `HERDR_SSH_LAB_HOME=…`,
  `HERDR_SSH_LAB_TARGET=herdr-ssh-lab`, `HERDR_SSH_LAB_PORT=…`,
  `HERDR_SSH_LAB_SSH_CONFIG=…`. (Deliberately does **not** export `HOME`; the
  caller sets `HOME=$HERDR_SSH_LAB_HOME` on the herdr command only.)
- `down`: only with the marker; kill the pid from `sshd.pid` after checking
  `/proc/<pid>/cmdline` (or `ps -o args=`) contains `sshd` and
  `<root>/sshd_config`; `rm -rf <root>` only (never the fleet lab root);
  exit 0 when not up. Reuse `fleet-lab.sh`'s `assert_safe_root` rules for the
  parent root.

**Tests** (`tests/fork_ssh_lab.rs`, driven like `tests/fork_fleet_lab.rs`)

- `ssh_lab_reaches_the_debug_herdr_of_the_fleet_lab`: fleet lab `up 1` (own
  `HERDR_FLEET_LAB_ROOT`), then `ssh-lab.sh up`; if stderr says `sshd not
  found` assert exit 3 and stop (explicitly printing why); else `ssh -F
  $cfg -o BatchMode=yes herdr-ssh-lab 'herdr --session lab-1 status server
  --json'` contains `"version":"<support::build_version()>"` and the lab's
  session dir; `status --json` `running == true`; `env` prints the five
  exports; second `up` fails with `already up`; `down` removes the root and
  the sshd pid is gone; the fleet lab is still up afterwards (its `status
  --json` unchanged) and is then torn down by `Drop`.
- `ssh_lab_refuses_without_the_fleet_lab`: `up` with a fresh root exits
  non-zero mentioning the fleet lab marker.
- Move `Lab` from `tests/fork_fleet_lab.rs` into `tests/support/fleet_lab.rs`
  (`pub mod fleet_lab` under `support`) so both tests share it; keep
  `fork_fleet_lab.rs`'s four tests unchanged.

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
bash scripts/fork/ssh-lab.sh up && eval "$(bash scripts/fork/ssh-lab.sh env)"
ssh -F "$HERDR_SSH_LAB_SSH_CONFIG" -o BatchMode=yes herdr-ssh-lab 'echo $HOME; command -v herdr; herdr --session lab-2 status server --json'
#   → <root>/ssh/home, <root>/ssh/home/.local/bin/herdr, JSON with "version":"0.8.2-fork"
ssh -F "$HERDR_SSH_LAB_SSH_CONFIG" -o BatchMode=yes herdr-ssh-lab 'herdr session list --json'   # lab-1, lab-2 (lab XDG), never the user's sessions
ls -la ~/.ssh | sha256sum   # before/after identical; stat ~/.ssh/known_hosts mtime unchanged
bash scripts/fork/ssh-lab.sh status --json; bash scripts/fork/ssh-lab.sh down; pgrep -af "sshd -f $HERDR_FLEET_LAB_ROOT" || echo "sshd gone"
bash scripts/fork/fleet-lab.sh down
```

**Downstream** *(as shipped)*

- `env` exports exactly `HERDR_SSH_LAB_ROOT`, `HERDR_SSH_LAB_HOME`,
  `HERDR_SSH_LAB_TARGET` (`herdr-ssh-lab`), `HERDR_SSH_LAB_PORT`,
  `HERDR_SSH_LAB_SSH_CONFIG`; `status --json` has `root`, `port`, `running`,
  `pid`, `target`, `home`, `ssh_config`. These are the fixture E1 PR 3/6, E5
  and the E2E validation rely on; add fields, never rename. `up` exits 3 with
  `sshd not found` only when the machine has no sshd.
- The remote side runs the herdr binary recorded in the fleet lab's
  `.herdr-fleet-lab` marker (`bin=`), not whatever `HERDR_BIN` happens to be
  set to when `ssh-lab.sh` runs, so `--remote` and fleet SSH validations
  always exercise the same build as the lab's servers. `HERDR_BIN` is only a
  fallback for a marker without a `bin=` line.
- `support::fleet_lab::Lab` is the shared driver for later `tests/fork_*.rs`
  (`pub mod fleet_lab` under `tests/support/`, `Lab::new/up/run/run_with_bin/
  herdr/runtime_dir` plus `stdout_of`/`stderr_of`/`unique_root`).
- **Tear down `ssh-lab.sh down` *before* `fleet-lab.sh down`.** The fleet
  lab's `down` deletes the whole root including `<root>/ssh` and the sshd pid
  file, after which the sshd can no longer be identified; `ssh-lab.sh down`
  then exits 0 with a hint naming the orphan instead of guessing at a pid.
- `ssh-lab.sh down` stops the sshd listener **and** the per-connection
  processes it still had. This is what PR 3/6 depend on: `RemoteSsh` opens a
  `ControlMaster=auto ControlPersist=yes` master, whose session process
  outlives the listener's SIGTERM and would otherwise keep the lab alive.
- `HERDR_SSH_LAB_SSHD` overrides the sshd binary (absolute path). A
  misconfigured override is a hard error (exit 1), never exit 3 — exit 3
  makes callers skip SSH validation entirely and a typo must not do that
  silently.
- Lab root and herdr binary paths are restricted to `[A-Za-z0-9._/+@-]`:
  both are embedded unquoted in `sshd_config`, in the remote `/bin/sh`
  wrapper, and in `env` output callers `eval`.
- The generated `sshd_config` is `AllowUsers <id -un>`, loopback-only, and
  sets `PermitUserRC no` / `PermitUserEnvironment no` with an in-lab
  `AuthorizedKeysFile`, so sshd never opens the caller's `~/.ssh/rc`,
  `~/.ssh/authorized_keys` or `~/.ssh/environment` (it resolves those through
  the passwd home, not `$HOME`). All forwarding is off; `attach.rs` only uses
  `ssh -T <target> <cmd>` over stdio plus its own `-S` control socket, so
  nothing PR 3/6 needs is blocked.

### PR 5 — feat: fleet connector streams local hosts and herdr fleet status reports them · deps: 2

**Goal:** `FleetConnector` opens every enabled `HostKind::Local` host, runs the
generation-1 handshake, ingests snapshots into `FleetState`, forwards frames
for the active host only, reconnects with backoff, and offers a per-host
command/endpoint lane; `herdr fleet status [--json] [--timeout-ms <n>]
[--watch]` is the first user-visible surface and the bin-crate driver that
proves it against real servers. SSH hosts are reported `Unavailable { reason:
"ssh hosts land in PR 6" }` until PR 6.

**Files**

- `src/fleet/handshake.rs` (new): `HandshakeParams`, `HandshakeOutcome`,
  `endpoint_handshake`.
- `src/fleet/transport/mod.rs` (new): `HostTransport` trait + `open_local`.
- `src/fleet/transport/local.rs` (new).
- `src/fleet/connector.rs` (new): `FleetConnector`, `FleetEvent`,
  `HostCommand`, `HostSendError`, per-host supervisor/reader threads.
- `src/fleet/endpoint_lane.rs` (new): per-host request/response reassembly.
- `src/fleet/oneshot.rs` (new): `collect_status` / `watch`.
- `src/fleet/mod.rs`: add the modules, and **drop the four
  `#[allow(dead_code)]` attributes PRs 1 and 2 put on `hosts`, `refs`,
  `report` and `state`** — the connector is their first production consumer.
  Consume PR 2 as built: `FleetChange::AgentAdded` carries a
  `Box<MergedAgent>`; `FleetChange::HostConnection`'s JSON does not carry the
  advertised `methods` (read them from `FleetState::host`); a disabled host is
  already `Unavailable { reason: "host disabled in [fleet]" }` at
  `FleetState::new`, so the connector must not open it or emit events for it;
  any transition out of `Connected` (including back to `Connecting`) already
  retires that host's agents, so the connector only reports transport facts
  and never edits the merged list itself. Reuse `state::Backoff` (1 s → 30 s,
  `next`/`peek`/`reset`) rather than a second schedule.
- `src/cli/fleet.rs` (new); `src/cli.rs` *(upstream — one `mod fleet;` and one
  match arm)*; `src/cli/spec.rs` *(upstream — `fleet_command()` +
  `.subcommand(fleet_command())`)*; `src/main.rs` *(upstream — one usage line
  in `--help`, `"fleet"` in the bare-command list)*.
- `tests/cli/fleet.rs` (new); `tests/cli/mod.rs` *(upstream — `mod fleet;`)*.

**Shapes/approach**

```rust
// handshake.rs — the small gen-1 client hello, independent of crate::client
pub struct HandshakeParams { pub cell_width_px: u32, pub cell_height_px: u32, pub surface_size: ClientSurfaceSize,
    pub pixel_mouse: bool, pub mouse_capture: bool, pub read_timeout: Duration }
pub enum HandshakeOutcome { Connected(EndpointServerWelcome), Incompatible { generation: Option<u32>, reason: String }, Rejected { code: String, message: String } }
pub fn endpoint_handshake(stream: &mut LocalStream, params: &HandshakeParams) -> io::Result<HandshakeOutcome>
// sends EndpointControl{ENDPOINT_HELLO_KIND, EndpointClientHello{generation: 1, direct_graphics: false, endpoint_keybindings: false,
//   snapshot_codecs: [SNAPSHOT_CODEC_V1], surface_codecs: [SURFACE_CODEC_V1], input_codecs: [INPUT_CODEC_V1], blob_codecs: [BLOB_CODEC_V1], ..}}
// reads one ServerMessage with a recv timeout; anything but a welcome with generation 1 and the four exact codec names → Incompatible;
// welcome.error → Rejected; framing/io → Err. Mirrors client/handshake.rs::do_handshake's checks (LOCAL 5 s / REMOTE 60 s timeouts).

// transport/mod.rs
pub trait HostTransport: Send { fn connect(&mut self) -> io::Result<LocalStream>; fn read_timeout(&self) -> Duration; fn describe(&self) -> String; }
pub fn transport_for(spec: &HostSpec, options: &FleetConnectorOptions) -> Box<dyn HostTransport>   // Local now; Ssh in PR 6
// transport/local.rs: LocalTransport { socket: PathBuf } via session::client_socket_path_for(session) + ipc::connect_local_stream;
//   a missing socket → io::Error "no herdr server for session <s> at <path>" (host-local, not fatal)

// connector.rs
pub struct FleetConnectorOptions { pub handshake: HandshakeParams /* inactive size */, pub active_surface: ClientSurfaceSize,
    pub manage_ssh_config: bool, pub max_frame_size: usize /* MAX_FRAME_SIZE */ }
pub enum FleetEvent { Host { host: HostId, event: HostEvent }, Surface { host: HostId, frame: Box<PaneSurfaceFrame> },
    SurfacePatch { host: HostId, patch: Box<PaneSurfacePatch> }, Notification { host: HostId, notification: Box<SemanticNotification> },
    EndpointResponse { host: HostId, request_id: String, result: Result<Vec<u8>, String> },
    ServerMessage { host: HostId, message: Box<ServerMessage> } /* everything else the active host says (bell, clipboard, window title…) */ }
pub enum HostCommand { Resize(ClientSurfaceSize), PaneInput { pane_id: String, events: Vec<ClientPaneInputEvent> },
    Focus(bool), MouseCapture(bool), Endpoint { request_id: String, request: String /* JSON api::schema::Request */ }, Raw(Box<ClientMessage>) }
pub enum HostSendError { UnknownHost, NotConnected, Io(io::Error) }
pub struct FleetConnector { hosts: Vec<HostHandle>, events: tokio::sync::mpsc::Receiver<FleetEvent>, shutdown: Arc<AtomicBool> }
impl FleetConnector {
    pub fn start(specs: Vec<HostSpec>, options: FleetConnectorOptions) -> Self;         // spawns one supervisor thread per enabled host
    pub fn events(&mut self) -> &mut tokio::sync::mpsc::Receiver<FleetEvent>;
    pub fn send(&self, host: &HostId, command: HostCommand) -> Result<(), HostSendError>;
    pub fn set_active(&self, host: Option<&HostId>) -> Result<(), HostSendError>;      // flips per-host AtomicBool gates; resizes old→inactive size, new→active size
    pub fn shutdown(self);                                                              // stop flag, drop write halves, join with a bounded wait
}
// supervisor thread per host: loop { emit Connecting{attempt}; transport.connect() → handshake → emit Connected/Incompatible;
//   reader loop with protocol::read_message(stream, max_frame_size): EndpointControl(kind==ENDPOINT_SNAPSHOT_KIND) → HostEvent::Snapshot;
//   other "shell.snapshot.*" kinds → Incompatible; other EndpointControl kinds → tracing::debug, ignored; PaneSurface/PaneSurfacePatch →
//   forwarded only if the host's active gate is set (else dropped without allocation); ClientShellEndpointResponseChunk → endpoint_lane;
//   ServerShutdown/ClientShellError/EOF/FramingError → Unavailable{reason, retry_in: backoff.next()} → sleep (checking the stop flag every 100 ms) → retry.
//   Incompatible also retries (a host may be updated), at the 30 s cap. }
// Writer: Mutex<Option<LocalStream>> (try_clone of the read stream) per host; send() writes protocol::write_message under the lock.
// Events use tokio::sync::mpsc::channel(256) + blocking_send from threads (upstream server_reader_thread pattern).

// endpoint_lane.rs — one in-flight request per host, keyed (boot_id, request_id), 60 s expiry, chunk reassembly (mirror client/endpoint_commands.rs)

// oneshot.rs
pub fn collect_status(config: &Config, timeout: Duration) -> Result<FleetStatusReport, Vec<String>>   // Err = config diagnostics
pub fn watch(config: &Config, mut on_change: impl FnMut(&FleetState, &[FleetChange]) -> bool /* false = stop */) -> Result<(), Vec<String>>
// both build FleetState::new(resolve_hosts(..)?), start the connector with the CLI's inactive surface size and no active host,
// drain events with blocking_recv, apply, and settle per decision (h).
```

`src/cli/fleet.rs`: `pub(super) fn run_fleet_command(args: &[String]) ->
io::Result<i32>`; `status [--json] [--timeout-ms <n>] [--watch]` parsed by
hand like `src/cli/status.rs`; usage on stderr + exit 2; config diagnostics on
stderr + exit 1; `--json` prints `serde_json::to_string_pretty(report)`,
`--watch --json` prints the initial report then one `FleetChange` JSON per
line, flushing each; text mode prints `report.render_text()` (and, in watch
mode, one line per change). `spec.rs`: `fleet_command()` with `.about("Inspect
the configured fleet of herdr hosts")`, subcommand `status` with `about`,
`json_flag()`, `option("timeout-ms", "MS")`, `flag("watch")`.

Inactive surface size for the CLI and for inactive hosts: start with
`ClientSurfaceSize { cols: 20, rows: 5 }`; verify against a lab server that
the welcome/snapshot/frame path accepts it and shrink if the server tolerates
smaller — record the chosen constant `INACTIVE_SURFACE` with a comment.

**Tests**

- `handshake.rs` with an in-process fake endpoint (bind a temp local socket
  with `crate::ipc::bind_private_local_listener`, respond with
  `protocol::write_message`): compatible welcome → `Connected` with methods;
  generation 2 / wrong codec name → `Incompatible` carrying the generation;
  `welcome.error` → `Rejected`; a `ClientShellSnapshot` binary variant or
  `Welcome` → `Incompatible`; no reply → timeout error.
- `connector.rs` with a fake endpoint server that speaks hello/welcome then
  emits JSON snapshots (built from `tests/fixtures/endpoint-snapshot-v1.json`
  read at test time via `include_str!`): two fake hosts merge; a fake host
  that closes after the welcome yields `Unavailable` then reconnects (assert
  `Connecting{attempt: 2}` and a fresh `Connected`) with `retry_in = 1 s`;
  frames from a non-active host never appear on the channel while the active
  host's do; `set_active` sends the resize to both hosts (fake records
  received `ClientShellResize` sizes); `send` to an unknown/not-connected host
  errors without panicking; the endpoint lane reassembles two chunks and
  rejects a mismatched `boot_id`; `shutdown` joins within 2 s while a host is
  mid-backoff; stale-revision snapshots are dropped by the state (through the
  connector).
- `oneshot.rs`: settles early when all fake hosts are terminal; honours the
  timeout with a never-answering socket path (reports `Connecting`).
- `src/cli/spec.rs` existing invariants pass with the new command.
- `tests/cli/fleet.rs` (`#[cfg(unix)]`): `fleet_status_json_merges_two_named_sessions`
  — `spawn_named_server(alpha)`, `spawn_named_server(beta)`,
  `wait_for_socket` on both client sockets, write the fleet config into
  `config_home/<app_dir_name()>/config.toml` (`include_local = false`, two
  `kind = "local"` hosts), `run_named_cli_json(&config_home, &runtime_dir,
  &["fleet", "status", "--json"])` → `hosts.len() == 2`, both
  `connection.state == "connected"` with `server_version ==
  support::build_version()`, `agents` refs prefixed `alpha/` and `beta/`,
  `counts.total == agents.len()`; kill beta → rerun → beta `unavailable` with
  a non-empty reason, alpha still `connected`, exit code 0;
  `fleet_status_rejects_invalid_config` → exit 1 with the diagnostic on
  stderr; `fleet_status_text_lists_hosts` checks the table header and both
  host rows.

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 3 && eval "$(bash scripts/fork/fleet-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
cat >> "$XDG_CONFIG_HOME/herdr-dev/config.toml" <<'EOF'
[fleet]
include_local = true
[[fleet.hosts]]
name = "lab-1"
kind = "local"
session = "lab-1"
[[fleet.hosts]]
name = "lab-2"
kind = "local"
session = "lab-2"
[[fleet.hosts]]
name = "lab-3"
kind = "local"
session = "lab-3"
EOF
$H fleet status                       # table: local unavailable (no default session in the lab), lab-1..3 connected 0.8.2-fork
$H fleet status --json > /tmp/e1-pr5.json
python3 -c 'import json;r=json.load(open("/tmp/e1-pr5.json"));print(r["schema"],[(h["id"],h["connection"]["state"]) for h in r["hosts"]],[a["ref"] for a in r["agents"]])'
#   → herdr.fleet.status.v1 [('local','unavailable'),('lab-1','connected'),…] ['lab-1/w1:p1','lab-2/w1:p1','lab-3/w1:p1'] (order by fleet_change_seq)
# status change propagates: make lab-2's pane look busy, then blocked-first ordering is visible
$H --session lab-2 pane run "$HERDR_FLEET_LAB_PANE_2" 'yes > /dev/null'   # or a detection-visible agent if one is installed
$H fleet status --json | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(a["ref"],a["agent_status"]) for a in r["agents"]])'
# reconnect against a real server, watched live:
( $H fleet status --json --watch > /tmp/e1-pr5.watch & echo $! > /tmp/e1-pr5.pid ); sleep 2
$H --session lab-3 session stop lab-3; sleep 3
nohup env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr --session lab-3 server </dev/null >/dev/null 2>&1 &
sleep 8; kill "$(cat /tmp/e1-pr5.pid)"
grep -c '"kind": *"host_connection"' /tmp/e1-pr5.watch     # ≥ 3: unavailable → connecting → connected for lab-3
grep '"host": *"lab-3"' /tmp/e1-pr5.watch | tail -3
bash scripts/fork/fleet-lab.sh down     # note: lab-3 was restarted by hand; `down` must still leave no lab pids (pgrep -af 'herdr --session lab-')
```

Evidence: the table, the JSON summary line, the ordering line, the watch
lines showing lab-3's `unavailable` → `connected` transitions, exit codes,
and the user's own `herdr session list` (run **without** the lab env) showing
no `lab-*`. Also profile once: `herdr fleet status --timeout-ms 3000` with 1
vs 3 hosts — wall time must not scale with hosts (parallel supervisors).

**Downstream**

- E2 drives the connector as: `start` → `set_active(Some(host))` with the
  real `ClientSurfaceSize` → consume `FleetEvent::Surface`/`SurfacePatch`
  for the active host through `ClientShellState::set_pane_surface`/
  `apply_pane_surface_patch` → route `HostCommand::PaneInput`/`Resize`/
  `Focus`/`Endpoint` to the active host. Switching hosts is one
  `set_active` call; the first frame after it is a full `PaneSurface`.
- E3 drives it as: `start` (no active host) → apply every
  `FleetEvent::Host` to `FleetState` → broadcast `FleetChange`s over
  `/api/events` → serve `FleetStatusReport` on `/api/fleet`. Terminal
  streaming (E3) uses the separate observe/control path per host, not this
  connector's surfaces.
- E7's `request(host, Request)` is `HostCommand::Endpoint` + the
  `EndpointResponse` event, gated by `HostConnection::Connected.methods`.
- `FleetConnectorOptions.handshake`'s `endpoint_keybindings` is fixed `false`
  (local keybindings on every host); E2 does not offer `--remote-keybindings
  server` in fleet mode.
- `herdr fleet status --json` and the `--watch` NDJSON lines are frozen
  additive shapes (`schema: herdr.fleet.status.v1`).

### PR 6 — feat: fleet connector reaches ssh hosts through the shared bridge · deps: 3, 5

**Goal:** `HostKind::Ssh` hosts connect through PR 3's bridge — discovery,
private forward socket, `ssh … remote-client-bridge` per connection — with
every failure host-local, no install/stop/handoff, and reconnect covering
the whole chain.

**Files**

- `src/fleet/transport/ssh.rs` (new): `SshTransport`.
- `src/fleet/transport/mod.rs`: `transport_for` maps `HostKind::Ssh`.
- `src/fleet/connector.rs`: pass `manage_ssh_config`; remove the
  "ssh hosts land in PR 6" placeholder; use `REMOTE_HANDSHAKE_READ_TIMEOUT`
  (60 s) for ssh transports.

**Shapes/approach**

```rust
pub struct SshTransport { host: HostId, target: String, session: Option<String>, manage_ssh_config: bool,
    ssh: Option<RemoteSsh>, bridge: Option<SshStdioBridge>, local_socket: PathBuf, bridge_errors: Arc<Mutex<Option<String>>> }
impl HostTransport for SshTransport {
    fn connect(&mut self) -> io::Result<LocalStream> {
        // 1. RemoteSsh::new(target, manage_ssh_config) (kept across reconnects so the control socket is reused; rebuilt after an error)
        // 2. discover_remote_herdr(&ssh)? → None → io::Error(NotFound, "no herdr with endpoint generation 1 on host; run `herdr --remote <target>` once to install it")
        // 3. if no bridge: SshStdioBridge::start_with(target, herdr, local_forward_socket_path_scoped(host.as_str(), target, session_name), session_name, ssh.options(), BridgeErrorSink::Report(record into bridge_errors))
        // 4. ipc::connect_local_stream(&local_socket); when the bridge reported an error since the last connect, surface it as the reason
    }
    fn read_timeout(&self) -> Duration { 60 s }
    fn describe(&self) -> String { format!("ssh {target}{session}") }
}
```

PR 3 shipped this API; see its **As shipped** block for the exact
signatures. Two follow-ups belong to PR 6: delete the `#[allow(dead_code)]`
on `discover_remote_herdr` and on `BridgeErrorSink::Report` once the transport
constructs them, and read `RemoteHerdr.shell_path` as a field (there is no
accessor).

`session_name` = the host's `session` or `session::DEFAULT_SESSION_NAME`, so
`remote_bridge_command` appends `--session` exactly as `--remote` does. The
bridge stays up across reconnects (it is a listener); a failed ssh child
surfaces through the sink and the reader's EOF, which the supervisor turns
into `Unavailable { reason }` and backoff. `shutdown` drops the bridge (socket
file removed) and the `RemoteSsh` (control master `-O exit`). Two ssh hosts
with the same target/session get distinct forward sockets via the host-id
scope. Discovery output is cached per `SshTransport` after the first success
(re-run only after a discovery-level failure) so reconnects do not pay three
ssh round-trips.

**Tests**

- `transport/ssh.rs` with a fake `ssh` on `PATH` (PR 3's shim pattern): the
  shim answers the discovery scripts (`uname`, `command -v herdr`, `status
  client --json`) with canned output and, for the bridge command, `exec`s
  `python3 -c` proxying its stdio to the test's fake endpoint socket (python3
  is on CI; no new dependency). Assert: connect → handshake succeeds through
  the bridge; shim exit 255 → `connect` error names the bridge; discovery
  reporting generation 2 → the reason names `herdr --remote`; the forward
  socket path contains the host id scope; `Drop` removes the socket file.
- `connector.rs`: an ssh host and a local host in one connector — the ssh
  host failing never changes the local host's state (host-local failure
  invariant test).

**Real-server validation**

```bash
cargo build && bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
bash scripts/fork/ssh-lab.sh up && eval "$(bash scripts/fork/ssh-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV HOME=$HERDR_SSH_LAB_HOME target/debug/herdr"
cat >> "$XDG_CONFIG_HOME/herdr-dev/config.toml" <<'EOF'
[fleet]
include_local = false
[[fleet.hosts]]
name = "lab-ssh"
kind = "ssh"
target = "herdr-ssh-lab"
session = "lab-1"
[[fleet.hosts]]
name = "lab-2"
kind = "local"
session = "lab-2"
EOF
$H fleet status --timeout-ms 30000 --json | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(h["id"],h["kind"],h["connection"]) for h in r["hosts"]],[a["ref"] for a in r["agents"]])'
#   → lab-ssh ssh connected 0.8.2-fork, lab-2 local connected; agents lab-ssh/w1:p1, lab-2/w1:p1
ls "$HERDR_SSH_LAB_HOME/.local/bin"; bash scripts/fork/fleet-lab.sh status --json     # no upload, still exactly two lab pids
# failure is host-local, then recovers, watched live over ssh:
( $H fleet status --json --watch --timeout-ms 30000 > /tmp/e1-pr6.watch & echo $! > /tmp/e1-pr6.pid ); sleep 5
bash scripts/fork/ssh-lab.sh down; sleep 5; bash scripts/fork/ssh-lab.sh up; sleep 20; kill "$(cat /tmp/e1-pr6.pid)"
grep '"host": *"lab-ssh"' /tmp/e1-pr6.watch | grep -o '"state": *"[a-z]*"' | uniq   # connected → unavailable → connecting → connected
grep -c '"host": *"lab-2"' /tmp/e1-pr6.watch                                       # lab-2 never left connected (≤ 1 host_connection line)
# a host with no compatible herdr: point a second ssh host at the same sshd but PATH-less wrapper is not possible; instead use a bad target:
printf '[[fleet.hosts]]\nname = "nowhere"\nkind = "ssh"\ntarget = "ssh://127.0.0.1:1"\n' >> "$XDG_CONFIG_HOME/herdr-dev/config.toml"
$H fleet status --json | python3 -c 'import json,sys;r=json.load(sys.stdin);print([(h["id"],h["connection"]["state"]) for h in r["hosts"]])'   # nowhere unavailable, others connected, exit 0
bash scripts/fork/ssh-lab.sh down; bash scripts/fork/fleet-lab.sh down
```

Evidence: the two JSON summaries, the watch state sequence, the `lab-2`
count, untouched `~/.ssh`/`/usr/bin/herdr` checksums as in PR 3. If the ssh
lab is unavailable on the machine (`sshd not found`), run the unit suite and
the `nowhere` host check only, and record the degradation in the PR body.

**Downstream**

- E5's MagicDNS hosts are plain `target`s; nothing else changes. E5 documents
  `HERDR_SSH_LAB_*` as the local stand-in for a remote host.
- The connector never installs herdr on a host; `docs/fork/fleet-core.md`
  (PR 7) tells users to run `herdr --remote <target>` once per host.
- Forward sockets live where `platform::remote_bridge_endpoint_path` puts
  them, one per `(pid, host id)`; E3's long-running gateway must call
  `shutdown` on exit so they are unlinked.
- *From PR 4 (shipped):* tear the ssh lab down before the fleet lab, and rely
  on `ssh-lab.sh down` to sweep the `ControlPersist` master's session process
  — the `bash scripts/fork/ssh-lab.sh down; sleep 5; … up` reconnect step
  above really does cut every ssh path to the lab, which is what makes the
  host go `unavailable`.

### PR 7 — docs: fleet core reference, ssh lab guide, adr review · deps: 6

**Goal:** a user can configure `[fleet]`, read `herdr fleet status`, and a
developer can run the ssh lab; ADR 0001 records what E1 learned.

**Files**

- `docs/fork/fleet-core.md` (new): `[fleet]` reference (every key, the
  reserved `local` host, validation rules), host ids (`host/w1:p1`),
  `herdr fleet status` text/JSON/`--watch` with a real example of each,
  connection states and what each means, the "run `herdr --remote <target>`
  once to install" rule, reconnect behaviour, what the connector never does,
  and the E2/E3 contracts in one short "for developers" section (module map
  of `src/fleet/`).
- `docs/fork/README.md`: a *Fleet core* section (config snippet, the status
  command) and an *SSH lab* section (`ssh-lab.sh up|status|env|down`, the
  `HOME=$HERDR_SSH_LAB_HOME` rule, isolation guarantees, `sshd not found`
  fallback); link `fleet-core.md`; add `scripts/fork/ssh-lab.sh`,
  `tests/fork_ssh_lab.rs`, `src/cli/fleet.rs` to the fork-owned/wired file
  lists.
- `docs/fork/decisions/0001-servers-stay-stock-ssh-transport.md`: an
  *E1 review* subsection (the visibility-widening choice and why, the
  never-install policy, `state_change_seq` non-comparability, the user-space
  sshd stand-in). Nothing in *Decision* is amended unless E1 contradicted it.
- `docs/fork/ROADMAP.md`: no status flip here (`implement-epic` owns it);
  only correct factual drift in the E1 section if any (e.g. the added
  `--timeout-ms`/`--watch` flags).

**Tests:** none (docs). Every command block is executed verbatim during
validation.

**Real-server validation**

Walk `fleet-core.md` and the README sections top to bottom against
`fleet-lab.sh up 2` + `ssh-lab.sh up`; paste the real outputs into the docs
(trimmed), then `ssh-lab.sh down`, `fleet-lab.sh down`, and confirm the
user's `herdr session list` shows no `lab-*`.

**Downstream**

- E2 adds `docs/fork/fleet.md` (TUI keys/limits) and links here rather than
  duplicating the config reference; E3 adds `gateway.md`; E5
  `remote-access.md` points at the ssh lab section for local testing.

## Critical files referenced (reuse, don't reinvent)

- `src/protocol/wire.rs` — `write_message`/`read_message`, `MAX_FRAME_SIZE`,
  `ClientMessage::{EndpointControl, ClientShellResize, ClientShellPaneInput,
  ClientShellFocus, ClientShellMouseCapture, ClientShellEndpointRequest}`,
  `ServerMessage::{EndpointControl, PaneSurface, PaneSurfacePatch,
  SemanticNotification, ClientShellError, ServerShutdown,
  ClientShellEndpointResponseChunk}`, `ClientShellSnapshot` and children,
  `ClientSurfaceSize` — **read-only**.
- `src/protocol/endpoint.rs` — `ENDPOINT_PROTOCOL_GENERATION`,
  `ENDPOINT_HELLO_KIND`, `ENDPOINT_WELCOME_KIND`, `ENDPOINT_SNAPSHOT_KIND`,
  `SNAPSHOT_CODEC_V1`, `SURFACE_CODEC_V1`, `INPUT_CODEC_V1`, `BLOB_CODEC_V1`,
  `EndpointClientHello`, `EndpointServerWelcome` — **read-only**.
- `src/client/handshake.rs:130` `do_handshake` — the reference for the
  fleet handshake's checks and timeouts (not callable: `pub(super)`).
- `src/client/mod.rs:1822` `server_reader_thread` — reader thread + tokio
  mpsc `blocking_send` pattern; `:1722` EndpointControl kind handling.
- `src/client/shell/state.rs:1176,1481`, `src/client/shell/surface_patch.rs:107`
  — snapshot/surface/patch acceptance rules to mirror.
- `src/client/shell/agent_sidebar.rs:20`, `src/client/shell.rs::status_priority`
  — upstream per-host ordering (E2), distinct from the merged order.
- `src/client/endpoint_commands.rs` — one-in-flight lane, 60 s timeout, chunk
  reassembly to mirror per host.
- `src/api/schema/common.rs:154` `AgentStatus { Idle, Working, Blocked, Done,
  Unknown }`.
- `src/remote/attach.rs` — `SshStdioBridge` (`:1661`), `bridge_connection`
  (`:1817`/`:1872`), `remote_bridge_command` (`:1617`), `RemoteSsh` (`:418`),
  `ManagedSshOptions` (`:401`), `RemoteHerdr` (`:247`), `RemotePlatform`
  (`:201`), `prepare_remote_herdr` (`:643`), `remote_binary_candidates`
  (`:725`), `remote_binary_supports_endpoint` (`:879`),
  `local_forward_socket_path` (`:2144`), `write_managed_ssh_config` (`:1775`),
  tests from `:2187` (`remote_env_lock`, `bridge_socket_is_user_only`).
- `src/remote/host_unix.rs` — `run_remote_client_bridge` (server side; spawns
  the daemon if needed, refuses generation ≠ 1).
- `src/remote.rs` — `pub(crate) use attach::*`, `shell_quote`,
  `print_remote_error_hint`.
- `src/platform/unix_common.rs:26` `remote_ssh_config_paths` (uses `$HOME`),
  `src/platform/mod.rs` `remote_bridge_endpoint_path`.
- `src/ipc.rs` — `connect_local_stream`, `bind_private_local_listener`,
  `prepare_socket_path`, `socket_file_identity`, `remove_socket_file_if_owned`,
  `restrict_socket_permissions`, `LocalStream`.
- `src/session.rs` — `client_socket_path_for`, `validate_name`,
  `DEFAULT_SESSION_NAME`; `src/config/io.rs` — `config_dir`, `app_dir_name`,
  `KNOWN_TOP_LEVEL_CONFIG_KEYS`, `Config::load`; `src/config/model.rs:962`
  `RemoteConfig` (section template); `src/config.rs:114`
  `collect_diagnostics`.
- `src/cli.rs:95` `maybe_run`; `src/cli/spec.rs` `command()`, `json_flag`,
  `option`, `flag`, the `spec_*` tests; `src/cli/status.rs`
  `parse_status_args`/`print_json`; `src/cli/api.rs` `api_snapshot`.
- `src/main.rs:64` `DEFAULT_CONFIG`, `:585-700` help text, `:739` bare-command
  list, `:758-765` `--remote` exit site.
- `src/build_info.rs` `version()` (client version in reports).
- `src/app/state.rs:1021,1131,1140`, `src/workspace.rs:1174,1291` — the
  `test_new`/adversarial/invariants idiom.
- `tests/cli/harness.rs` — `spawn_named_server` (`:163`), `named_session_socket`
  (`:155`), `app_dir_name` (`:147`), `run_named_cli_json` (`:245`),
  `wait_for_socket`; `tests/cli/sessions.rs:1-20` two-server template;
  `tests/support/mod.rs` — `build_version`, `register_runtime_dir`,
  `cleanup_test_base`, `client_shell_handshake` (fixture for fake-server
  tests); `tests/fork_fleet_lab.rs` `Lab`; `tests/multi_client.rs` PTY
  spawning with `portable_pty`.
- `tests/fixtures/endpoint-snapshot-v1.json`, `endpoint-welcome-v1.json` —
  golden inputs (never edited).
- `scripts/fork/fleet-lab.sh` (`status --json`, `env`, `assert_safe_root`,
  pid re-validation), `scripts/fork/gate.sh`.
- Binding rules: `AGENTS.md` → Universal Project Rules (state/runtime
  separation, multiplicative paths, runtime/client boundary, stable endpoint
  contract), Testing, Code Conventions; `.claude/rules/fork.md`;
  `docs/fork/ROADMAP.md` principles 1–8 and E1;
  `docs/fork/decisions/0001-servers-stay-stock-ssh-transport.md`;
  `docs/fork/plans/e0-fork-foundations.md` Downstream sections.

## End-to-end epic validation

Run by `implement-epic` after PRs 1–7 are ✅ and merged, from the root
checkout on `master` (`git -C <root> pull --ff-only`), with
`bash scripts/fork/gate.sh <root>` → `EXIT=0` first.

1. **Fleet of three kinds of host.** `cargo build`; `fleet-lab.sh up 3`;
   `ssh-lab.sh up`; `eval` both `env`s; write `[fleet]` with
   `include_local = true`, `lab-1`/`lab-2` as `kind = "local"`, `lab-ssh`
   (`kind = "ssh"`, `target = "herdr-ssh-lab"`, `session = "lab-3"`), and
   `nowhere` (`kind = "ssh"`, `target = "ssh://127.0.0.1:1"`). Run
   `HOME=$HERDR_SSH_LAB_HOME $H fleet status --timeout-ms 30000 --json`.
   Assert: `schema == "herdr.fleet.status.v1"`; `client_version ==
   0.8.2-fork`; hosts in config order with `local` first; `local` and
   `nowhere` `unavailable` with non-empty reasons; `lab-1`, `lab-2`, `lab-ssh`
   `connected` with `server_version == 0.8.2-fork` and `boot_id`/`revision`
   set; `agents` = exactly `lab-1/w1:p1`, `lab-2/w1:p1`, `lab-ssh/w1:p1`, each
   with `workspace_label == lab-N`; `counts.total == 3`; exit code 0.
   `FleetPaneRef` round-trip: every `agents[].ref` parses back to
   `(host, pane_id)`.
2. **Ordering and deltas.** Start `--watch --json` in the background; drive
   an agent-status change on `lab-2` (a detection-visible agent if installed,
   otherwise `pane run … 'yes >/dev/null'` for `working`); assert an
   `agent_status` change line for `lab-2/w1:p1` and that a subsequent
   one-shot report lists it first within its rank with the highest
   `fleet_change_seq`.
3. **Host failure is local, reconnect works, over both transports.** With
   the watch still running: `ssh-lab.sh down` → `lab-ssh` goes `unavailable`
   while `lab-1`/`lab-2` emit no connection change; `ssh-lab.sh up` → `lab-ssh`
   returns to `connected` within 60 s; `session stop lab-1` then restart it
   by hand → `lab-1` `unavailable` → `connecting` → `connected`. The watch
   process never exits or panics (`kill` it at the end; exit by signal only).
4. **`herdr --remote` is unchanged.** PR 3's PTY script against
   `herdr-ssh-lab` prints `MARKER_SEEN`; `/usr/bin/herdr`, `~/.ssh`,
   `~/.config/herdr*` checksums/mtimes unchanged before vs after the whole
   run; `fleet-lab.sh status --json` shows exactly the lab pids.
5. **Pure state and contracts.** `bash scripts/fork/gate.sh <root>
   "test-one fleet"` → `EXIT=0`; the architecture guard test passes;
   `git diff --stat upstream/master -- src/protocol tests/fixtures` is empty.
6. **Isolation.** From a shell **without** the lab env, `herdr session list`
   shows no `lab-*`; `ls ~/.ssh` unchanged. Then `ssh-lab.sh down`,
   `fleet-lab.sh down`; `pgrep -af 'herdr --session lab-'` and
   `pgrep -af 'sshd -f /tmp/herdr-fleet-lab'` are empty; no
   `/tmp/herdr-fleet-lab` or `/tmp/herdr-e1-*` left behind.
7. Flip E1 to ✅ in `docs/fork/ROADMAP.md` only when 1–6 all hold.
