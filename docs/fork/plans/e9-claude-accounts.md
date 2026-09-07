# Epic E9 — Claude account profiles per agent, switchable mid-session

## Context

**Goal (roadmap):** choose which Claude account (Pro/Max licence) each Claude
Code agent runs under when it starts, see at a glance which account every
agent is using, and move a running agent to another account — keeping its
conversation — when one account runs out of usage. **Why:** usage limits are
per account. With a second Max licence the only way to keep working today is
to log out and back in inside Claude Code, which is manual, global to the
machine, and loses track of which agent is on which account. herdr already
knows how Claude Code is launched, where its config directory is
(`CLAUDE_CONFIG_DIR`, `src/integration/env.rs`), and which Claude session id
an agent holds for resume (`src/agent_resume.rs`); an account is just a Claude
config directory with its own `.credentials.json` and `.claude.json`
(`oauthAccount`).

Scope contract = the roadmap's E9 deliverables: `[[accounts]]` profiles in
`config.toml` with `herdr account list|add|login|status|default`; choose at
start (`herdr agent start --account <name>` and a TUI launcher following the
existing dialog language; stock servers unchanged); see it (sidebar,
`herdr agent list`, `herdr fleet status --json`, and through E3/E4 the phone);
switch mid-session (`herdr agent switch-account <pane> <name>` and a confirmed
TUI action that resumes the same Claude session under the new profile);
limit awareness (a `usage_limit` rule in `src/detect/manifests/claude.toml`,
detect and suggest, never auto-switch); `docs/fork/accounts.md`; tests that
use fake `claude` stubs under throwaway profile directories.

**Dependency chain:** E9 depends on **E0 only** (✅). It is independent of the
fleet chain; its phone half (display and switch action in the PWA) waits for
E7 and is out of scope here. E1 (✅) supplies the `FleetStatusReport` that PR 6
extends. **E3 is 🔨 and landing concurrently** — see *Sequencing hazards*; E9's
implementation starts only after E3 is ✅.

### Real current state (verified on `origin/master` @ `ecfbb321`)

- **Launch path.** `agent.start` → `App::start_agent`
  (`src/app/agents.rs:145`) types
  `interactive_agent_executable(kind) + args` into the pane's **already
  running interactive shell** through
  `crate::platform::interactive_shell_command(&argv, &shell_name)`
  (`src/platform/linux.rs:117`, `macos.rs:59`, `windows.rs:620`,
  `fallback.rs:152`) and `encode_api_submission`. No process is spawned, no
  env is applied; the agent inherits the pane shell's environment. The pane
  must be at its prompt (`available_shell_name` → `agent_pane_busy`
  otherwise). `AgentStartParams` (`src/api/schema/agents.rs:48`) is
  `{name, kind, pane_id, args, timeout_ms}` — **no env field**, and the
  server owns `argv[0]`, so a client cannot prefix the typed command. The
  shell names herdr accepts as "at a prompt" are
  `is_pane_shell_process_name` (`src/platform/mod.rs:328`): `sh bash dash zsh
  fish ksh mksh csh tcsh elvish xonsh nu pwsh powershell cmd`.
- **Session id for resume.** `persisted_session_from_launch_args`
  (`src/agent_resume.rs:72`) is **Codex-only**. Claude's session id arrives
  through the hook herdr installs into `<CLAUDE_CONFIG_DIR>/hooks/
  herdr-agent-state.sh` + a `SessionStart` entry in
  `<CLAUDE_CONFIG_DIR>/settings.json` (`src/integration/targets.rs:121`
  `install_claude()`, `src/integration/claude_settings.rs`); the hook sends
  `pane.report_agent_session {source:"herdr:claude", agent:"claude",
  agent_session_id, agent_session_path, session_start_source}` using
  `HERDR_PANE_ID`/`HERDR_SOCKET_PATH` from the pane env. The id is held in
  `TerminalState.persisted_agent_session` (`src/terminal/state.rs:130`) and
  exposed as `AgentInfo.agent_session: Option<AgentSessionInfo {source,
  agent, kind, value}>` on `agent.get`/`agent.list`. `agent_resume::plan`
  (`:136`) already emits `["claude","--resume",<id>]` for
  `("herdr:claude","claude",Id)`. **`install_claude()` reads the ambient
  `CLAUDE_CONFIG_DIR`** (`claude_dir()`, `src/integration/env.rs:58`), so a
  per-profile hook install is `herdr integration install claude` run with
  that env — no code change.
- **Per-agent metadata already exists end to end on stock servers.**
  `pane.report_metadata` (`PaneReportMetadataParams`,
  `src/api/schema/panes.rs:479`: `source`, `agent`, `applies_to_source`,
  `state_labels`, `tokens: HashMap<String, Option<String>>`, `ttl_ms`
  optional 1..86 400 000, `seq`) stores tokens on
  `TerminalState.metadata_tokens`; limits in `src/app/api_helpers.rs:202-208`
  (32 keys/resource, key ≤ 32 chars, value ≤ 80 chars, source grammar
  `[A-Za-z0-9:._-]{1,80}`). They surface as `AgentInfo.tokens` /
  `state_labels`, on the wire as `ClientShellAgent.tokens` /
  `state_labels` (`src/protocol/wire.rs:1061`, read-only), are filterable in
  agent views (`AgentViewField::Token`), and the sidebar renders any token
  through `AgentSidebarToken::Custom` (`$name` in `[ui.sidebar.agents]`
  rows, `src/ui/sidebar/tokens.rs:89`). `state_labels.<status>` replaces the
  status word in the row (`src/client/shell/agent_sidebar.rs:277-282`).
  Metadata reported with `applies_to_source = "herdr:claude"` is cleared when
  the Claude process exits (`src/terminal/state.rs:505-520` retain logic).
  Tokens are in-memory only: `PaneSnapshot` (`src/persist/snapshot.rs:98`)
  has no tokens field, so they do not survive a server restart.
- **Restore relaunch is server-side and env-less.** After a server restart,
  `start_pending_agent_resume` (`src/app/agent_resume.rs:203`) spawns a fresh
  shell with `pane_launch_env(ws_idx, pane_id, Vec::new())` and types
  `claude --resume <id>`: a resumed agent comes back under the **default**
  profile. Changing that is a server change (out of scope); E9 documents it
  and offers `switch-account` to move it back.
- **Process environment is readable locally.** `pane.process_info`
  (`PaneProcessInfo {shell_pid, foreground_process_group_id, processes:
  [{pid, name, argv0, argv, cmdline}]}`, `src/api/schema/panes.rs:570`) gives
  the pids; on Linux `/proc/<pid>/environ` is readable for the user's own
  processes. There is no such helper in `src/platform/` today.
- **Detection.** `src/detect/manifests/claude.toml` (`version =
  "2026.08.31.1"`, `min_engine_version = 2`, 16 rules) has no usage-limit
  rule. Manifest rules are `deny_unknown_fields` with states exactly
  `idle|working|blocked|unknown` — **no `reason`/label on a match**; the
  matched rule id is only visible through `agent.explain`
  (`explain_to_json_value`, `src/detect/manifest.rs:830`: `matched_rule.id`)
  and `herdr agent explain <target> --json` / `--file PATH --agent LABEL
  --json` (offline evaluation of a captured screen). Precedence is local
  override (`<config>/agent-detection/claude.toml`) > cached remote >
  bundled; `herdr server reload-agent-manifests` reloads.
  `scripts/agent_detection_manifest_check.py` requires
  `distribution/agent-detection/claude.toml` to be byte-identical to the
  bundled manifest when versions are equal and never lower.
- **Config.** `Config` (`src/config/model.rs:307`, `#[serde(default)]`) ends
  with `fleet: FleetConfig` (`:323`) and `gateway: GatewayConfig` (`:324`,
  E3). `src/config/io.rs`: `KNOWN_TOP_LEVEL_CONFIG_KEYS` (`:7`),
  `load_live_section::<T: DeserializeOwned>` (`:536`, works for an array
  value), `unknown_top_level_section_diagnostic` (`:410`, renders arrays of
  tables as `[[key]]`). `Config::collect_diagnostics` (`src/config.rs:120`)
  chains `fleet`/`gateway` diagnostics. `DEFAULT_CONFIG` is
  `src/main.rs:67` with the `[fleet]` block at `:406-418` and `[gateway]` at
  `:420-434`, guarded by `default_config_<section>_block_parses_without_
  diagnostics` / `_is_commented_out` tests (`:895-1050`).
  `scripts/config_reference_check.py:42` `SKIPPED_SUBTREES = ("keys.command",
  "fleet", "gateway")` must list every fork root key.
- **CLI.** Dispatch is a hand `match` in `src/cli.rs:97-136` (`"fleet" =>
  fleet::run_fleet_command`, `"machine" => …`); modules declared at
  `:25-42`; `send_request(&Request)` (`:779`) and `print_response` (`:755`)
  are the shared client helpers (`ApiClient::local()` over
  `crate::api::socket_path()`). `src/cli/spec.rs` is clap **for help and
  completions only** (`fleet_command()` `:143`, `agent_command()` `:345`,
  `start` at `:454-479`, `json_flag()` `:973`, `env_option()` `:981`);
  `every_spec_subcommand_renders_short_and_long_help` covers new
  subcommands automatically if they have `.about()`. `src/main.rs` keeps a
  usage printer (`:612-782`) and an unknown-command allowlist (`:774-793`).
  `src/cli/machine.rs` is the model for a `list|add|remove|…` command with
  `--json` rows; `src/cli/agent.rs:289` `agent_start` is the manual flag loop
  E9 extends. No `agent.stop`/`agent.restart` exists; `agent.prompt`,
  `agent.send_keys`, `agent.wait --until`, `pane.send_text` (raw bytes, no
  bracketed paste — a trailing `\r` submits), `pane.process_info` do.
- **TUI.** The authoritative UI is the client shell (`src/client/shell/`).
  **There is no agent-start dialog anywhere** — agents are started by typing
  in a pane or through the API. Overlays are `ClientShellOverlay` variants
  (`src/client/shell/state.rs:630`) rendered by
  `overlays.rs::render_client_overlay` (`:32`) with shared chrome (`panel`,
  `popup`, `button`, `row`); `ClientWorktreeOpenOverlay` (`state.rs:531`,
  `worktree_overlays.rs:115`, `overlay_input.rs:468`) is the filterable-list
  modal to copy; `ClientConfirmCloseOverlay` the confirm modal. The pane
  context menu is `context_menu.rs` (`items()` `:5-79`,
  `open_pane_context_menu` `:141`, `activate_pane_context_action`) with
  `ClientContextMenuAction` (`state.rs:569`) and a `Pane {pane_id,
  workspace_id, …}` target carrying no agent facts. The sidebar agent row is
  `agent_sidebar.rs::agent_rows` (`:237-309`). The client reads
  `config.toml` itself (`config.rs::reload_client_config`), so `[[accounts]]`
  is readable client-side without the server.
- **Fleet report.** `src/fleet/report.rs::AgentReport` (`:172`) and
  `src/fleet/state.rs::MergedAgent` (`:255`, built by
  `HostState::merged_agent` `:234`) copy `name/title/agent/display_agent/
  agent_status/state_change_seq/focused` and **drop `tokens` and
  `state_labels`**. `FleetChange` (`state.rs:281`) has no metadata variant.
  E3's `/api/fleet` serializes `FleetStatusReport` verbatim
  (`FleetHandle::report`, `src/gateway/fleet.rs:243`), so a report field
  appears on the phone path with zero gateway changes.
- **Tests/tooling.** `tests/cli/harness.rs` has `spawn_herdr_with_config(
  config_home, runtime_dir, socket_path, path_override, config_toml)`,
  `run_cli_json`, `spawn_named_server`; `tests/cli/agents.rs:32-60` already
  builds a **fake `pi` stub on a PATH override** and asserts on what it
  received — the exact pattern for a fake `claude`. Fork integration tests
  are `tests/fork_<topic>.rs`; `tests/support/fleet_lab.rs::Lab` drives
  `scripts/fork/fleet-lab.sh` (PATH inherited; marker panes are **not** at a
  prompt, so tests create their own pane). `Cargo.toml` has no
  `[dev-dependencies]`, no `tempfile`, no shell-quoting crate (E9 needs
  none: quoting reuses `crate::platform` helpers).
- **Nothing of E9 exists:** no `src/accounts/`, `src/cli/account.rs`,
  `[[accounts]]`, `docs/fork/accounts.md`, `scripts/fork/accounts-lab.sh`,
  `tests/fork_accounts.rs`, and zero references to `.claude.json`,
  `.credentials.json` or `oauthAccount` in `src/`.

### Locked decisions

- **(a) Profile layout — separate `CLAUDE_CONFIG_DIR` per account sharing
  transcripts via symlinks** *(roadmap default)*. `herdr account add` creates
  `<config_dir>` (`0700`) and seeds it from the source profile (the default
  one, or `--from <name>`): **symlinked** (shared) `projects/`, `todos/`,
  `skills/`, `plugins/`, `commands/`, `agents/`, `CLAUDE.md`,
  `history.jsonl` (each only if present in the source); **copied**
  `settings.json` (then the herdr hook is installed into the new directory by
  running `herdr integration install claude` as a child process with
  `CLAUDE_CONFIG_DIR=<config_dir>`), and `.claude.json` with identity keys
  scrubbed (`oauthAccount` plus any top-level key whose name contains
  `apikey`, `token`, `credential` or `secret`, case-insensitive) so
  onboarding, project trust and MCP settings carry over; **never touched**
  `.credentials.json` (the new profile is logged out until
  `herdr account login`), `statsig/`, `shell-snapshots/`, `debug/`, `cache/`,
  `ide/` and anything not listed (Claude recreates them). The list lives in
  one place (`src/accounts/layout.rs`) and is printed by `herdr account add
  --dry-run`. *Must be verified against the installed Claude Code* (see
  *What must be verified live later*).
- **(b) How the env reaches the agent — client-side, stock servers
  unchanged** *(roadmap default)*, as a **two-step launch**: the client first
  types an environment assignment into the pane shell in that shell's syntax
  (`export CLAUDE_CONFIG_DIR='…'` for `sh bash dash zsh ksh mksh`, `set -gx`
  for `fish`, `setenv` for `csh tcsh`, `$env:CLAUDE_CONFIG_DIR = '…'` for
  `pwsh powershell`, `$env.CLAUDE_CONFIG_DIR = '…'` for `nu`, `set-env` for
  `elvish`, `$CLAUDE_CONFIG_DIR = '…'` for `xonsh`, `set "…"` for `cmd`) via
  `pane.send_text` with a trailing `\r`, then calls the stock `agent.start`,
  whose typed `claude …` inherits it. The shell is identified from
  `pane.process_info` (the process whose pid is `shell_pid`, checked with
  `crate::platform::is_pane_shell_process_name`); an unrecognised shell is a
  **hard error**, never a silent launch under the wrong account. The
  fallback the roadmap allows (an optional `agent.start` field) is **not**
  planned: an old server would ignore it and report success — exactly the
  failure the endpoint contract forbids — so it would have to be a new
  advertised method, which E9 does not need.
- **(c) Scope — Claude only in v1** *(roadmap default)*. `agent = "claude"`
  is the only accepted value; the config schema and `AccountAgent` enum are
  append-only so `codex` (`CODEX_HOME`) etc. can follow.
- **(d) Limit handling — detect and suggest, never auto-switch** *(roadmap
  default)*. The `usage_limit` rule marks the agent `blocked`; the CLI/TUI
  surface the hint on demand and `herdr account watch` (opt-in, PR 10)
  labels it in the sidebar. Nothing switches without an explicit confirmed
  command.
- **(e) Where the account fact lives — metadata tokens on the server, no
  client ledger** *(auto default)*. The launch/switch driver reports
  `tokens.account = <name>` and `tokens.account_state ∈ {ok, unverified,
  mismatch, limited, logged_out}` with `source = "fork:accounts"`,
  `applies_to_source = "herdr:claude"`, no TTL. Truth is re-established by
  reading the Claude process environment where the OS allows (Linux
  `/proc/<pid>/environ`; other platforms report `unverified`). A persisted
  client-side ledger keyed by session id was considered and rejected: after
  a server restart the resumed agent runs under the default profile, so a
  ledger would confidently show the wrong account.
- **(f) Default resolution** *(auto default)*: `--account <name>` → the
  profile marked default → if exactly one profile exists, that one →
  otherwise no profile (launch unchanged, no tokens). `--account none` opts
  out explicitly. When at least one profile is configured, a Claude launch
  through `herdr agent start` or the TUI applies the resolved default;
  typing `claude` by hand in a pane is untouched. Workspace/host-level
  defaults from the roadmap's chain are a follow-up (`[accounts.defaults]`
  is reserved and rejected with a diagnostic).
- **(g) Profile storage — `[[accounts]]` in `config.toml` (declarative) plus
  a CLI-managed `<config>/accounts/profiles.toml`** *(auto default)*. herdr
  never rewrites `config.toml`; `herdr account add|remove|default` edit
  `profiles.toml` (`default = "<name>"` top-level key + `[[profiles]]`).
  Merge: config profiles first, then store profiles; a name defined in both
  is a diagnostic and the config entry wins; the store's `default` beats
  `default = true` flags; more than one `default = true` is a diagnostic
  (first wins). `--print-config` on `add` prints the `[[accounts]]` snippet
  instead of writing the store.
- **(h) Names and limits** *(auto default)*: profile names match
  `[A-Za-z0-9._-]{1,32}` (fits the 80-char token value and the sidebar);
  `config_dir` is tilde-expanded with
  `crate::integration::env::expand_tilde_path` and must be absolute after
  expansion; two profiles may not share a directory; the default profile's
  directory may be `~/.claude` (the ambient one).
- **(i) Switch protocol** *(auto default)*: preflight (Claude agent with a
  known `agent_session` of source `herdr:claude`, status not `working`
  unless `--interrupt`, target profile exists and is logged in, target ≠
  current unless `--force`) → confirmation (TTY prompt, `--yes` for
  automation; non-TTY without `--yes` exits 2) → `Escape` if blocked, then
  `agent.prompt "/exit"` → wait until the pane's foreground is the shell
  (default 20 s, never kill) → two-step relaunch with the same agent name and
  `--resume <id>` → wait for the hook to report the same session id → probe
  → report tokens → JSON result. Any failure after exit leaves the pane at a
  shell with a clear message and the export line still applied, so a manual
  `claude --resume <id>` works.
- **(j) `herdr account login <name>`** types the export line and
  `claude auth login` into a pane at its prompt (`--pane <id>`, default the
  current pane); it does not start a managed agent. If the installed Claude
  Code has no `auth login` subcommand, the documented fallback is `claude`
  then `/login` (verify live).
- **(k) Limit-rule evidence** *(auto default)*: the usage-limit screen cannot
  be captured live in `--auto` (it needs a rate-limited account). PR 9 ships
  the rule from a **fixture captured from a screenshot**
  (`tests/fixtures/fork/claude-usage-limit.txt`), validated offline with
  `herdr agent explain --file … --agent claude --json` and live with the fake
  stub printing the fixture; the plan marks exactly what to re-verify with a
  real limited account.
- **(l) Validation stand-ins**: `scripts/fork/accounts-lab.sh` (PR 1) boots
  one named session under a throwaway `XDG_CONFIG_HOME` with a fake `claude`
  (`scripts/fork/fake-claude.sh`) first on `PATH` and two seeded profile
  directories. The stub prints `CLAUDE_CONFIG_DIR=<dir>` and its argv,
  writes `<dir>/last-launch.json`, reports its session id to herdr exactly
  like the real hook (`herdr pane report-agent-session … --source
  herdr:claude --agent claude --agent-session-id <id> --session-start-source
  startup|resume`), shows a `❯ ` prompt, exits on `/exit`, honours
  `--resume <id>` (prints `resumed <id>`, reports `source=resume`),
  implements `auth login` (writes a fake `.credentials.json` `0600` and an
  `oauthAccount` into `.claude.json`) and prints the usage-limit fixture when
  `FAKE_CLAUDE_LIMIT=1`. Nothing in E9 ever touches the user's `~/.claude`,
  `~/.claude.json` or `~/.config/herdr`.

### Sequencing hazards

- **E3 is landing concurrently.** Its remaining PRs edit these upstream
  files: `Cargo.toml` (PR 1, already merged — no further `Cargo.toml` change
  is expected from E3), `src/main.rs`, `src/cli.rs`, `src/cli/spec.rs`,
  `src/config/model.rs`, `src/config/io.rs`, `tests/support/mod.rs` (plus
  `build.rs`). E9's implementation **starts only after E3 is ✅** on
  `master`; every E9 branch is created from that `origin/master`. E9 plans
  **no `Cargo.toml`/`Cargo.lock` change** (no new dependency is needed) and
  keeps every upstream-file edit adjacent-line additive: new `mod`/`match`
  arms directly after the `fleet`/`gateway` ones, new `Config` field after
  `gateway`, new `DEFAULT_CONFIG` block after `[gateway]`, new
  `KNOWN_TOP_LEVEL_CONFIG_KEYS` entry in alphabetical position, new
  `SKIPPED_SUBTREES` entry appended.
- **Upstream files touched, and by which PR only:** `src/main.rs` (`mod
  accounts;`, usage lines, allowlist, `DEFAULT_CONFIG` block + two tests),
  `src/cli.rs` (`mod account;` + one arm), `src/config/model.rs` (one
  field), `src/config/io.rs` (key + section load + diagnostics),
  `src/config.rs` (one `.chain`), `scripts/config_reference_check.py` (one
  tuple entry) — **PR 1**. `src/cli/spec.rs`: PR 1 (`account_command()`
  with `list`), PR 2 and PR 3 (subcommands inside `account_command()`), PR 4
  (`--account` on `agent start`), PR 5 (`switch-account` under
  `agent_command()`). `src/cli/agent.rs`: PR 4 (`--account` arm + hand-off),
  PR 5 (`switch-account` arm). `src/platform/mod.rs`: PR 4 (one appended
  `process_env_var`). `src/detect/manifests/claude.toml` +
  `distribution/agent-detection/claude.toml`: PR 9. `src/client/shell/
  {state,context_menu,overlays,overlay_input}.rs`: PR 7 then PR 8. Never:
  `src/protocol/**`, `src/server/**`, `src/app/**`, `src/api/**`,
  `src/persist/**`, `src/integration/**`, `src/agent_resume.rs`,
  `tests/fixtures/endpoint-*.json`, `docs/next/**`.
- **Fork-owned collisions:** `src/cli/account.rs` is created by PR 1 and
  edited by PRs 2, 3, 9, 10 — 2 → 3 → 9 → 10 are dependency-sequenced.
  `src/accounts/client.rs` is created by PR 4 and edited by PR 5 (switch
  driver) and PR 10 — sequenced. `src/accounts/mod.rs` gains one `pub mod`
  line per PR; keep them alphabetical so concurrent PRs (W2: 2, 4, 6; W3: 3,
  5, 7) merge cleanly, and the later PR in a wave rebases before its gate.
  `src/fleet/{state,report}.rs` are edited by PR 6 only.
- **`src/cli/spec.rs` in W2 (PRs 2 and 4) and W3 (PRs 3 and 5)** are edited
  in different functions (`account_command()` vs `agent_command()`); the
  later PR rebases onto `master` before its gate.
- **Two Rust builds at once** is the memory hazard the gate lock exists for
  (another epic's agents may be building). Never bypass
  `scripts/fork/gate.sh`; commits run under
  `flock /tmp/herdr-fork-gates/gate.lock` because the pre-commit hook runs
  `just lint`.
- **Real Claude is never used by validation.** Every PR validates with the
  fake stub; the items under *What must be verified live later* are run by a
  human against a real installation after the epic.

## Status legend

✅ merged · 🔨 in progress · ⬜ not started · ⛔ blocked

## PR map

| # | Title | Group | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | feat(accounts): account profiles config, pure resolution, herdr account list and the accounts lab | A · Foundations | — | ✅ |
| 2 | feat(accounts): herdr account add, remove and default with seeded profile directories | A · Foundations | 1 | ✅ |
| 3 | feat(accounts): herdr account status and login | B · CLI | 2 | ✅ |
| 4 | feat(accounts): launch claude under a profile with herdr agent start --account | B · CLI | 1 | ✅ |
| 5 | feat(accounts): switch a running claude agent to another profile keeping its session | B · CLI | 4 | ⬜ |
| 6 | feat(fleet): fleet report and change stream carry agent metadata tokens | C · Fleet | 1 | ⬜ |
| 7 | feat(accounts): tui account picker to start claude in a pane | D · TUI | 4 | ⬜ |
| 8 | feat(accounts): tui switch-account action with confirmation | D · TUI | 5, 7 | ⬜ |
| 9 | feat(detect): claude usage-limit rule and account limit hints | E · Limits | 3, 5 | ⬜ |
| 10 | feat(accounts): herdr account watch labels usage-limited agents | E · Limits | 9 | ⬜ |
| 11 | docs(accounts): accounts guide, adr, readme and roadmap drift | F · Docs | 2, 6, 8, 10 | ⬜ |

**Wave preview:** W1 `[1]` → W2 `[2, 4, 6]` → W3 `[3, 5, 7]` → W4 `[8, 9]`
→ W5 `[10]` → W6 `[11]`. Critical path 1 → 2 → 3 → 9 → 10 → 11 (and 1 → 4 →
5 → 9). At most two Rust gates run concurrently in a wave (the gate lock
serialises them).

**Model assignment:** tasks run on `opus`. The **review agent must run on
`fable`** for **PR 2** (copies `.claude.json`, must never copy or print
credentials), **PR 4** (anything that could launch an agent under the wrong
account: shell detection, quoting, verification), **PR 5** (could lose a
conversation on switch: exit/resume protocol), **PR 7** and **PR 8** (the TUI
halves of the same two hazards, plus routing the action to the right pane and
endpoint). PRs 1, 3, 6, 9, 10, 11 review on `opus`.

## Verification (the gate — every PR)

```bash
bash scripts/fork/gate.sh <worktree>        # runs `just ci` under the machine-wide lock
echo "EXIT=$?"                              # read the EXIT= line; nothing else decides green/red
```

`just ci` = `cargo fmt --check` + `cargo clippy --all-targets -D warnings` +
`cargo nextest run` + the python maintenance tests (including
`scripts.test_config_reference_check` and
`scripts.test_agent_detection_manifest_check`) + the bun suites. Never pipe
the wrapper into `tail`/`head`/`grep`. Use `just test-one <filter>` while
iterating; the gate before commit is the full wrapper. PRs that touch feature
gated code (`src/cli/spec.rs`, `src/main.rs`) also run
`cargo clippy --all-targets --no-default-features -- -D warnings` (E3's
`--no-default-features` build must keep compiling; E9 adds nothing behind
the `gateway` feature).

Real-server validation is **mandatory** for every PR: build with
`cargo build`, boot the accounts lab (`scripts/fork/accounts-lab.sh up`,
which sets an isolated `XDG_CONFIG_HOME`, clears `HERDR_SOCKET_PATH` /
`HERDR_CLIENT_SOCKET_PATH`, and puts the fake `claude` first on `PATH`), run
the exact commands listed per PR, capture the JSON/text evidence in the PR
description, and tear down (`accounts-lab.sh down`). Never point a command
at the user's live server or `~/.claude`.

CI gotchas: Zig 0.15.2 is needed for libghostty-vt (`scripts/fork/
dev-setup.sh`); `scripts/config_reference_check.py` fails `just ci` unless
`"accounts"` is in `SKIPPED_SUBTREES` (PR 1); `scripts/
agent_detection_manifest_check.py` fails if the bundled and published
`claude.toml` diverge at the same version (PR 9); the pre-commit hook runs
`just lint`, so commit under the gate lock.

## Cross-cutting constraints (all PRs)

- **Servers stay stock.** No change under `src/server/`, `src/app/`,
  `src/api/`, `src/protocol/`, `src/persist/`, `src/integration/`. Every
  account operation is a client composed of existing stock methods
  (`pane.process_info`, `pane.send_text`, `agent.start`, `agent.get`,
  `agent.list`, `agent.prompt`, `agent.send_keys`, `agent.explain`,
  `pane.report_metadata`, `events.subscribe`). A LAN host running upstream
  herdr works with every E9 command.
- **Never the wrong account silently.** The launch driver refuses unknown
  shells, refuses profiles whose directory is missing, and reports
  `account_state = mismatch` (and exits non-zero) when the probe reads a
  different `CLAUDE_CONFIG_DIR` than intended. `unverified` is shown as such;
  nothing is displayed as verified without evidence.
- **Read is safe, control is explicit.** `list`, `status`, `watch` never
  send input or write tokens except `watch`'s clearly documented labels;
  `switch-account` and the TUI switch always confirm; nothing auto-switches;
  `login` never runs without a named profile and a pane.
- **Secrets.** `.credentials.json` is never read, copied, printed or logged —
  only its existence and mode. `oauthAccount` is read for email/plan display
  only; tokens inside `.claude.json` are scrubbed on seed. Profile
  directories are `0700`; `profiles.toml` contains paths and names only.
  Evidence in PRs redacts emails.
- **State is separated from runtime; pure modules.** `src/accounts/
  {config,profile,store,layout,launch,switch,limit,tokens}.rs` are sync and
  free of sockets, `tokio`, `ratatui` and `crate::client`; an architecture
  test in `src/accounts/mod.rs` mirrors `src/fleet/mod.rs::PURE_MODULES`.
  Only `client.rs` (CLI driver) and the `src/client/shell/` overlays touch
  I/O; the switch protocol is a pure state machine driven by observations.
- **Minimal upstream wiring**, listed exhaustively in *Sequencing hazards*;
  every edit is a handful of adjacent lines and lands in exactly one PR.
- **Multiplicative-perf discipline.** Nothing E9 adds runs per render or per
  pane: the sidebar badge is the existing `$token` path; `agent.explain` is
  called only on demand (status, switch preflight) or on a status-change
  event (`watch`), never polled; the probe reads one `/proc` file per launch.
- **Runtime/client boundary.** Server-visible facts use neutral names
  (`account`, `account_state` tokens; `tokens`/`state_labels` on the fleet
  report); no `sidebar`/`row`/`badge` names cross into the API or the fleet
  report.
- **Code conventions.** No `unwrap()`/`expect()` in production code;
  `tracing` for logs; `#[allow]` only with a reason; the single
  platform-specific function lives in `src/platform/mod.rs` behind
  `#[cfg(target_os = "linux")]` with a stub for other targets; no new
  dependency. Lowercase conventional commits with the environment's trailers;
  branches `feat/e9-pr<N>-<slug>` from `origin/master`; squash-merge with
  `--delete-branch`; never edit `docs/next/**`, `skills/herdr/SKILL.md`,
  root `README.md`/`CHANGELOG.md`.

## Per-PR detail

### PR 1 — feat(accounts): account profiles config, pure resolution, herdr account list and the accounts lab · deps: —

**Goal.** The declarative and CLI-managed profile sources, their merge and
validation as pure data, the token vocabulary every later PR reports, a
read-only `herdr account list`, the `--default-config` reference, and the
local validation fixture (fake `claude` + lab script) used by every PR.

**Files**

- `src/accounts/mod.rs` (new): `pub mod config; pub mod launch; pub mod
  layout; pub mod profile; pub mod store; pub mod tokens;` + the
  `PURE_MODULES` architecture test (forbid `tokio`, `ratatui`,
  `interprocess`, `crate::ipc`, `crate::remote`, `crate::client`,
  `crate::api::client` in the pure files).
- `src/accounts/launch.rs` (new, pure half): `ShellFamily::from_process_
  name(&str) -> Option<ShellFamily>` (`Posix | Fish | Csh | PowerShell | Nu
  | Elvish | Xonsh | Cmd`, built on `crate::platform::is_pane_shell_process_
  name`) and `env_assignment_line(family, var, value) -> Result<String,
  LaunchError>` per decision (b): golden strings per family, single-quote
  escaping, refusal of control characters (and `"`/`%` for `cmd`), a leading
  space so `HISTCONTROL=ignorespace` / `HIST_IGNORE_SPACE` shells skip it.
  PR 3 (`login`) and PR 4 (launch) both consume it.
- `src/accounts/config.rs` (new): `AccountProfileConfig { name: String,
  agent: String (default "claude"), config_dir: String, default: bool }`
  (`#[serde(default)]`, `Deserialize + Debug + Clone + PartialEq`),
  `pub fn diagnostics(profiles: &[AccountProfileConfig]) -> Vec<String>`
  (unknown agent, bad name, empty dir, duplicate name, duplicate dir,
  multiple defaults, reserved `[accounts.defaults]`). Everything is a
  diagnostic, never a parse failure (fleet convention).
- `src/accounts/store.rs` (new): `AccountsStore { default: Option<String>,
  profiles: Vec<AccountProfileConfig> }`, `store_path() ->
  <config>/accounts/profiles.toml` (`crate::config::config_dir()`),
  `load() -> io::Result<AccountsStore>` (missing file = empty; parse errors
  are diagnostics), `save(&self)` (PR 2 uses it; write `0700` dir, atomic
  rename, `#[serde(deny_unknown_fields)]` on the file schema with a
  `version = 1` key).
- `src/accounts/profile.rs` (new, pure): `AccountAgent { Claude }` (append
  only), `AccountProfile { name, agent, config_dir: PathBuf (expanded),
  default: bool, origin: ProfileOrigin { Config, Store } }`, `Profiles {
  profiles: Vec<AccountProfile> }` with `resolve(config: &[
  AccountProfileConfig], store: &AccountsStore, home: &Path) ->
  (Profiles, Vec<String>)`, `get(name)`, `default_profile()`,
  `choose(explicit: Option<&str>) -> Result<Choice, ChoiceError>` where
  `Choice::{Profile(&AccountProfile), None}` implements decision (f),
  `validate_name`, `Profiles::test_new()`.
- `src/accounts/layout.rs` (new; read-only half): the share/copy/private
  lists as `const` slices, `ProfileInspection { dir_exists, logged_in
  (credentials file present), credentials_mode_ok (0600 on unix), identity:
  Option<AccountIdentity { email, organization, plan }>, hook_installed,
  broken_links: Vec<PathBuf> }`, `inspect(profile) -> ProfileInspection`,
  `pub fn identity_from_claude_json(text: &str) -> Option<AccountIdentity>`
  (lenient: `oauthAccount.emailAddress|email`, `organizationName`,
  `subscriptionType|plan`; never reads `.credentials.json` contents).
- `src/accounts/tokens.rs` (new): `METADATA_SOURCE = "fork:accounts"`,
  `APPLIES_TO_SOURCE = "herdr:claude"`, `ACCOUNT_TOKEN = "account"`,
  `ACCOUNT_STATE_TOKEN = "account_state"`, `AccountState { Ok, Unverified,
  Mismatch, Limited, LoggedOut }` with `as_str`/`parse` (unknown → `None`).
- `src/cli/account.rs` (new): `ACCOUNT_USAGE`, `run_account_command(args)`
  matching `list [--json] | help|--help|-h | _ => usage exit 2`; rows
  `{name, agent, config_dir, default, origin, dir_exists, logged_in,
  hook_installed}` as tab-separated text or `--json` (model:
  `src/cli/machine.rs`). Exit 0 report, 1 when diagnostics exist, 2 usage.
- `scripts/fork/fake-claude.sh` (new, `sh`, the stub of decision (l)) and
  `scripts/fork/accounts-lab.sh up|down|status|env` (new): `$ROOT =
  ${HERDR_ACCOUNTS_LAB_ROOT:-/tmp/herdr-accounts-lab}`; `up` builds `bin/`
  with the stub as `claude`, seeds `profiles/perso` and `profiles/work`
  (each with a fake `.claude.json` identity and a `0600`
  `.credentials.json`, `projects/` shared by symlink from `work` to
  `perso`), writes `$ROOT/xdg/herdr-dev/config.toml` with `onboarding =
  false` and the two `[[accounts]]` (perso default), starts
  `herdr --session accounts-lab server` under `env -u HERDR_SOCKET_PATH -u
  HERDR_CLIENT_SOCKET_PATH XDG_CONFIG_HOME=$ROOT/xdg XDG_RUNTIME_DIR=…
  PATH=$ROOT/bin:$PATH`, creates one workspace with a pane at a shell prompt
  and prints the socket paths; `env` exports `HERDR_ACCOUNTS_LAB_ROOT`,
  `XDG_CONFIG_HOME`, `HERDR_ACCOUNTS_LAB_SESSION`, `HERDR_ACCOUNTS_LAB_PANE`,
  `HERDR_ACCOUNTS_LAB_PROFILE_PERSO/WORK`, `PATH`; `down` stops the server
  and removes `$ROOT`. Reuse `scripts/fork/fleet-lab.sh`'s pid/runtime-dir
  hygiene and marker file.
- `tests/support/accounts_lab.rs` (new) + `pub mod accounts_lab;` in
  `tests/support/mod.rs` *(upstream file — one line, after E3's
  `pub mod gateway;`)*: `Lab::new/up/down/herdr(args)` mirroring
  `tests/support/fleet_lab.rs`.
- `tests/fork_accounts.rs` (new): `account list --json` against the lab
  shows two profiles, `perso` default, both `dir_exists`, `logged_in`.
- *(upstream file — minimal wiring)* `src/main.rs`: `mod accounts;` after
  `mod gateway;`; usage lines `herdr account list|add|status|login|default
  …` after the `gateway` line; `"account"` in the unknown-command allowlist
  after `"gateway"`; a commented `[[accounts]]` block in `DEFAULT_CONFIG`
  directly after `[gateway]` (roadmap TOML verbatim, every line `#`) plus
  `default_config_accounts_block_parses_without_diagnostics` /
  `_is_commented_out` tests copied from the fleet pair (the block slicer
  must handle a `[[accounts]]` header: slice from `"\n[[accounts]]\n"` to the
  next `\n[` and uncomment `key = value` and `[[accounts]]` lines).
- *(upstream file — minimal wiring)* `src/config/model.rs`: `pub accounts:
  Vec<crate::accounts::config::AccountProfileConfig>,` after `gateway`.
  `src/config/io.rs`: `"accounts"` in `KNOWN_TOP_LEVEL_CONFIG_KEYS`
  (alphabetical), a `load_live_section(table, "accounts", "accounts
  config", …, |section| config.accounts = section)` block, and
  `diagnostics.extend(crate::accounts::config::diagnostics(&config.
  accounts))`. `src/config.rs`: `.chain(crate::accounts::config::
  diagnostics(&self.accounts))`. `src/cli.rs`: `mod account;` and
  `"account" => account::run_account_command(&args[2..])?,` after the
  `gateway` arm. `src/cli/spec.rs`: `account_command()` with `list`
  (`json_flag()`) inserted after `gateway_command()` in `command()`.
  `scripts/config_reference_check.py`: `"accounts"` appended to
  `SKIPPED_SUBTREES` with the fork rationale comment.

**Shapes/approach.** `[[accounts]]` is a top-level array of tables, which
`load_live_section::<Vec<_>>` deserialises directly and
`unknown_top_level_section_diagnostic` already renders as `[[accounts]]`.
`Profiles::resolve` is the only place the two sources meet; the CLI, the
launch driver (PR 4), the TUI (PR 7) and `status` (PR 3) call
`crate::accounts::profile::load_profiles(&Config) -> (Profiles,
Vec<String>)` (a thin wrapper that reads the store) and never the raw config.
The fake stub is one POSIX `sh` script with a `case "$1"` for `auth`,
`--version` and the interactive default; it reports its session through the
`herdr` binary named by `HERDR_BIN` (exported by the lab) so the test binary's
`CARGO_BIN_EXE_herdr` is used.

**Tests.** Unit: diagnostics for every invalid shape; merge precedence and
default precedence (config vs store); `choose` for explicit/default/single/
none; `identity_from_claude_json` on a real-shaped sample and on garbage;
name grammar; tilde expansion; `PURE_MODULES` guard; `DEFAULT_CONFIG` block
tests. Integration: `tests/fork_accounts.rs::account_list_reports_lab_
profiles`; `tests/cli/surface.rs` gets an `(&["account","--help"], …)` case.

**Real-server validation.** `cargo build`; `bash scripts/fork/accounts-lab.sh
up`; `eval "$(bash scripts/fork/accounts-lab.sh env)"`; `herdr --session
accounts-lab account list --json` → two rows, `perso` default, both
`logged_in: true`, `hook_installed: false` (the stub profiles have no hook
yet); `herdr --default-config | sed -n '/^\[\[accounts\]\]/,/^\[/p'` shows the
commented block; `herdr --session accounts-lab config` (or the reload path)
reports no diagnostics; a deliberately duplicated name in the lab config
produces exactly one diagnostic and exit 1; `accounts-lab.sh down`.

**Downstream.** Field names (`name`, `agent`, `config_dir`, `default`), the
store schema (`version`, `default`, `[[profiles]]`), the token constants
(`METADATA_SOURCE = "fork:accounts"`, `APPLIES_TO_SOURCE = "herdr:claude"`,
`AGENT_LABEL = "claude"`, `ACCOUNT_TOKEN = "account"`,
`ACCOUNT_STATE_TOKEN = "account_state"`) and the `AccountState` strings
(`ok | unverified | mismatch | limited | logged_out`, with
`AccountState::parse` returning `None` for anything else) are the user-facing
contract: add, never rename. `Profiles` (`get`, `iter`, `names`,
`default_profile`, `choose`, `Choice::{Profile, None}`,
`ChoiceError::{Unknown, InvalidName}`) and `ProfileInspection` are what PRs
2–5, 7–10 consume, always through
`crate::accounts::profile::load_profiles(&Config) -> (Profiles, Vec<String>)`.
The lab's `env` names — `HERDR_ACCOUNTS_LAB_ROOT`, `XDG_CONFIG_HOME`,
`HERDR_ACCOUNTS_LAB_SESSION`, `HERDR_ACCOUNTS_LAB_PANE`,
`HERDR_ACCOUNTS_LAB_PROFILE_PERSO` / `_WORK` / `_AMBIENT`,
`HERDR_ACCOUNTS_LAB_CLIENT_SOCKET`, `HERDR_ACCOUNTS_LAB_API_SOCKET`,
`HERDR_BIN`, `PATH` — are the fixture the E2E validation relies on.

**As built (PR 1, merged).** The tree had moved three merges past the
`ecfbb321` the *Real current state* section was verified at (all of E3, the
fleet reconnect fix, and the upstream sync to v0.9.0 — the binary is
`0.9.0-fork`). Every path in the section above still exists; the corrections
below are what later PRs must code against, and they win over the prose.

- **`Config.accounts` is `crate::accounts::config::AccountsSection`, not
  `Vec<AccountProfileConfig>`.** `Config::load` deserialises the whole file in
  one pass, so a wrong-shaped `accounts` key would have dropped the user's
  *entire* config back to defaults. `AccountsSection` has a hand-written
  `Deserialize` that is total over TOML: an array of tables is the profiles, a
  table (`[accounts.defaults]`) and any scalar are an empty section plus a
  diagnostic. Read the entries with `config.accounts.as_slice()`;
  `crate::accounts::config::diagnostics(&AccountsSection)` covers both the
  section verdict and the per-entry problems, and `section_diagnostic(&…)`
  returns just the section verdict (what `load_profiles` chains, so the
  per-entry ones are not reported twice).
- **`layout::inspect` takes options:** `inspect(&AccountProfile,
  InspectOptions) -> ProfileInspection`, with `InspectOptions::health()` (the
  cheap checks) and `InspectOptions::with_identity()` (also parses
  `.claude.json`, which is megabytes on a real installation).
  `inspect_dir(&Path, InspectOptions)` is the same thing addressed by
  directory. `account list` uses `health()`; PR 3's `status` wants
  `with_identity()`.
- **`store::load()` returns `(AccountsStore, Vec<String>)`**, not
  `io::Result`: a missing file is an empty store with no diagnostic, and a
  read/parse/version problem is a diagnostic, never a failure. The file
  schema is `deny_unknown_fields` with `version = 1` (defaulted, so a
  hand-written file without it still loads). `store::save(&AccountsStore)`
  ships unused behind a narrow `#[allow(dead_code)]`; PR 2 removes it by
  adding the caller. `parse`/`render` are the pure halves.
- **`validate_name` lives in `config.rs`** and is re-exported from
  `profile.rs`. It additionally refuses `.`, `..` and a leading `-` (the name
  becomes a directory component in PR 2 and a CLI argument everywhere).
- **Directories are compared and stored lexically normalized**
  (`config::dir_key`): `~/.claude`, `~/.claude/` and `~/./.claude` are one
  directory, so two profiles cannot share one credentials directory by
  spelling it differently. `AccountProfile.config_dir` is the normalized form.
- **`launch::env_assignment_line` refuses `!` for `Csh` only.** csh and tcsh
  run history substitution before quote processing, so `setenv X '/p/a!b'`
  fails with "Event not found", the assignment silently never happens, and the
  agent would launch under the ambient account. Refusals per family are
  covered by `no_family_can_be_talked_into_a_second_command`, which runs 11
  adversarial values across all eight families — extend it, do not replace it.
- **No `src/integration/**` edit.** `crate::integration::env` is a private
  module, so `profile.rs` mirrors its `home_dir()` and repeats the
  `CLAUDE_CONFIG_DIR` string (`AccountAgent::config_dir_env_var`) rather than
  opening that module up.
- **`mod accounts;` is alphabetical in `src/main.rs`** (before
  `mod agent_resume;`), not after `mod gateway;`.
- **The `DEFAULT_CONFIG` `[[accounts]]` sample is *fully* commented, including
  its header, and sits at the end of the file after `[advanced]`** — not
  directly after `[gateway]`. An uncommented `[[accounts]]` header with every
  key commented out is one nameless profile, which would report two
  diagnostics on a fresh config; and a commented block between `[gateway]` and
  `[experimental]` breaks `uncommented_default_gateway_block()` (it would
  uncomment `name = …` twice into `[gateway]`). The slicer anchors on
  `"\n# [[accounts]]\n"`.
- **`scripts/config_reference_check.py` consults `SKIPPED_SUBTREES` before it
  resolves a field's type.** `AccountsSection` lives outside the checker's
  `src/config` model root, so the old order treated `accounts` as a leaf key
  the website reference had to enumerate.
- **The lab pins `CLAUDE_CONFIG_DIR`.** `accounts-lab.sh` seeds a third,
  deliberately logged-out profile `ambient` and points `CLAUDE_CONFIG_DIR` at
  it for everything it starts (exported as
  `HERDR_ACCOUNTS_LAB_PROFILE_AMBIENT`; `tests/support/accounts_lab.rs::Lab::
  herdr` does the same), so a launch that applied no profile still lands
  inside the lab. `fake-claude.sh` has **no** `$HOME/.claude` fallback: with
  no `CLAUDE_CONFIG_DIR` it writes nothing, and it exits 3 if pointed at the
  real `~/.claude`.
- `tests/support/mod.rs` gained `pub mod accounts_lab;` **before** (not after)
  `pub mod fleet_lab;` — the list is alphabetical. E3's `pub mod gateway;`
  was already there.

### PR 2 — feat(accounts): herdr account add, remove and default with seeded profile directories · deps: 1

**Goal.** Create a new profile directory seeded from an existing one per
decision (a), record it in `profiles.toml`, and manage the default — without
ever touching credentials.

**Files**

- `src/accounts/layout.rs` (write half; PR 1 shipped the read half plus the
  `SHARED_ENTRIES` / `COPIED_ENTRIES` / `PRIVATE_ENTRIES` /
  `SCRUBBED_IDENTITY_KEYS` / `SCRUBBED_KEY_SUBSTRINGS` constants this PR
  applies — drop their `#[allow(dead_code)]` markers as you add the callers):
  `SeedPlan { links: Vec<(src,
  dst)>, copies: Vec<(src, dst)>, scrub: Vec<PathBuf>, skipped: Vec<String>
  }`, `plan_seed(source_dir, target_dir) -> io::Result<SeedPlan>` (pure over
  a directory listing), `apply_seed(&SeedPlan) -> io::Result<SeedReport>`
  (create `0700` dir; refuse a non-empty target unless `--force`; symlinks
  with `std::os::unix::fs::symlink` behind `#[cfg(unix)]` and directory
  junction-free copy fallback on windows), `scrub_claude_json(text) ->
  Result<String, String>` (pure, `serde_json::Value`, removes identity keys,
  preserves everything else).
- `src/accounts/store.rs`: `AccountsStore::{add, remove, set_default}`
  returning `Result<(), String>`; `save` is already atomic (PR 1) and only
  needs its `#[allow(dead_code)]` removed once this PR calls it.
- `src/cli/account.rs`: `add <name> [--config-dir <path>] [--from
  <profile>] [--dry-run] [--force] [--print-config] [--no-hook] [--json]`,
  `remove <name> [--delete-dir]` (refuses when the profile came from
  `config.toml` — tell the user to edit it; `--delete-dir` only removes the
  directory after confirming it is not the ambient `~/.claude` and not
  shared), `default <name>`.
- `src/cli/spec.rs` *(upstream file — minimal wiring)*: the three
  subcommands inside `account_command()`.
- `tests/fork_accounts.rs`: `account_add_seeds_shared_transcripts_and_
  private_identity`, `account_remove_refuses_config_profiles`,
  `account_default_prefers_store`.

**Shapes/approach.** `add` resolves the source (`--from`, else the default
profile, else `~/.claude` if it exists) → `plan_seed` → prints the plan under
`--dry-run` → `apply_seed` → hook install by running the current executable
(`std::env::current_exe()`) with `["integration","install","claude"]` and
`CLAUDE_CONFIG_DIR=<target>` (skipped with `--no-hook`; a failure is a
warning naming the command to rerun, not a rollback) → `store.add` →
prints the row (`--json`) and, unless `--print-config`, the reminder to run
`herdr account login <name>`. Default `config_dir` is `~/.claude-<name>`.
The subprocess is also how validation proves the hook lands in the right
directory.

**Tests.** Unit: `plan_seed` over a synthetic source tree (present/absent
entries, existing target, symlink cycles), `scrub_claude_json` keeps
`projects`/`mcpServers`/`hasCompletedOnboarding` and drops `oauthAccount`
and `*ApiKey*`, `store.add/remove/set_default` invariants (unknown name,
duplicate, config-origin refusal). Integration: the lab test above asserts
`work/projects` is a symlink to `perso/projects`, `work/.claude.json`
exists without `oauthAccount`, `work/.credentials.json` does **not** exist,
`work/hooks/herdr-agent-state.sh` exists and `work/settings.json` has the
`SessionStart` hook (the lab exports `HERDR_BIN` so the child process is the
test binary).

**Real-server validation.** Lab up; every `account add` passes
`--config-dir "$HERDR_ACCOUNTS_LAB_ROOT/profiles/<name>"` — the default
`config_dir` is `~/.claude-<name>` in the *developer's* home and the lab does
not override `HOME`. `herdr --session accounts-lab account add third
--config-dir …/profiles/third --dry-run` prints the plan and writes nothing;
the same without `--dry-run` then `ls -la …/profiles/third` shows the
links/copies, no `.credentials.json`, `hooks/herdr-agent-state.sh` present;
`grep SessionStart …/third/settings.json`; `account list --json` shows
`third` with `origin: "store"`, `logged_in: false`; `account default third`
then `list` shows the new default; `account remove third --delete-dir`
cleans up; `account remove perso` (config origin) exits 1 with the message.
Verify `stat -c %a` of the new dir is `700`, that the source profile's
`.credentials.json` mtime is unchanged, and that the three crafted-source
refusals fire (a `--config-dir` spelled through `..` or a symlink, a
`.claude.json` symlinked at `.credentials.json`, a `projects` link pointing
at a config directory).

**Downstream.** `hook_installed` from PR 1's `inspect(profile,
InspectOptions::health())` is what PR 4's preflight warns on ("session ids will not be reported; switching will not
work") — it must never block a launch. The seed list is documentation
source for PR 11.

**As built (PR 2, merged).** Everything the *As built (PR 1)* corrections say
still holds; the additions below are what PRs 3, 4 and 11 must code against.

- **`SeedPlan` carries named entries, not tuples.** `links` and `copies` are
  `Vec<SeedEntry { entry: String, source: PathBuf, target: PathBuf }>` so the
  `--dry-run` printout and the `--json` output can name the entry; `scrub` is
  `Vec<PathBuf>` (the subset of `copies` whose identity keys are removed) and
  `skipped: Vec<String>` carries a reason per entry. `apply_seed(&SeedPlan,
  force: bool) -> io::Result<SeedReport>`, and `SeedReport { created, linked,
  copied, scrubbed, warnings }`.
- **`plan_seed` refuses more than a missing source.** Same directory as the
  source (after `config::dir_key`), a target nested inside the source or vice
  versa, and a source that is not a directory are all errors. A `SHARED_ENTRIES`
  entry that is itself a symlink in the source is `canonicalize`d and the new
  profile links to the *final* target, so a profile seeded from a seeded
  profile never chains.
- **`guard_no_credentials` runs twice** — once when the plan is built and again
  before it is applied — over the source *and* target of every link and copy
  and over `scrub`. A plan naming `.credentials.json` on any of them is an
  error, not a skip. `PRIVATE_ENTRIES` still reach `skipped`, which is how
  `--dry-run` shows the user that credentials are deliberately left behind.
- **Non-destructive by construction.** An entry that already exists in the
  target is left exactly as it is and reported as a warning; copied files are
  written with `create_new` at `0600` on unix and the directory is `0700`;
  `--force` only relaxes the refusal to seed into a non-empty directory and
  **never** relaxes the refusal to seed into one that already holds
  `.credentials.json`. A `.claude.json` this build cannot scrub (not JSON, not
  an object, too large) is **not copied at all** — a warning, never a
  passthrough.
- **`scrub_claude_json` is top-level only**, by decision (a): a nested MCP
  server's own credentials are the user's, not the account's, and dropping
  them would break the settings the copy exists to carry over. `is_scrubbed_key`
  is the shared predicate (`SCRUBBED_IDENTITY_KEYS` case-insensitively, plus
  any key containing a `SCRUBBED_KEY_SUBSTRINGS` needle).
- **The seed source falls back through `CLAUDE_CONFIG_DIR`.** `--from`, else
  the default profile when its directory exists, else the *ambient* Claude
  directory — `CLAUDE_CONFIG_DIR` when set, otherwise `~/.claude` — mirroring
  the private `crate::integration::env::claude_dir`. Nothing is guessed: when
  none of the three exists, `add` fails and names `--from`.
- **The default `config_dir` is `~/.claude-<name>` in the *user's* home**, so
  the plan's original validation recipe (`account add third` then `ls
  $HERDR_ACCOUNTS_LAB_ROOT/profiles/third`) does not work: every lab command
  and every test passes `--config-dir "$HERDR_ACCOUNTS_LAB_ROOT/profiles/…"`.
  The lab does not override `HOME`.
- **The store is never overwritten with a store herdr could not read.**
  `store::load()` degrades a bad file to an empty store, so every writer goes
  through `cli::account::store_for_write`, which refuses when `load` reported
  any diagnostic. `AccountsStore::remove` clears `default` when it named the
  removed profile; `AccountsStore::set_default` accepts a `[[accounts]]` name
  (that is how `herdr account default` chooses without rewriting
  `config.toml`) and the CLI checks the name resolves in the merged view first.
- **`--delete-dir` refuses** a symlink, a non-directory, a filesystem root, a
  home directory, the ambient Claude directory, and a directory another profile
  also uses. The last branch is unreachable through the merged view — `resolve`
  already drops a second profile with the same `dir_key` — and is kept as
  defence in depth for a future caller.
- **The hook is installed by subprocess**: `std::env::current_exe()` with
  `["integration","install","claude"]` and `CLAUDE_CONFIG_DIR=<target>`, output
  captured so it cannot corrupt `--json` stdout. A failure is a warning naming
  the command to rerun; `--no-hook` warns too, because a profile without the
  hook can be launched but never switched.
- **Flags:** only `add` takes `--json`; `remove` and `default` print one line.
  `add` exits 0 (warnings on stderr), 1 for an operation failure, 2 for usage.
- **`profile::home_dir` and `profile::expand` are now `pub`** so the CLI
  expands `--config-dir` exactly the way `resolve` will read it back. Windows
  sharing uses `symlink_dir`/`symlink_file` with no junction fallback: a link
  that cannot be made is a warning and the profile simply starts without that
  entry.

**Hardening found during PR 2's review** (all fixed in the same PR; PRs 3–5
and 7–10 inherit the helpers):

- `config::dir_key` is **lexical**: it keeps `..` and cannot see a symlink, so
  `--config-dir <dir>/../work` and `--config-dir <symlink>/work` both passed
  every same-directory check and would have registered a second "account" on
  one login. `layout::resolved_key(&Path)` (longest existing prefix
  canonicalized, remainder appended, `.`/`..` collapsed) is the companion key;
  **every check that must not be walked around compares both spellings.** Reuse
  it, do not re-derive it.
- `plan_seed` resolves a `COPIED_ENTRIES` symlink *before* classifying it: a
  `.claude.json` symlinked at `.credentials.json` would otherwise have been
  read and copied under an innocent name, and its `claudeAiOauth` key matches
  none of `SCRUBBED_KEY_SUBSTRINGS`.
- `guard_no_credentials` also refuses a **link source** that holds a
  credentials file (existence only) or that contains either profile directory,
  so a `projects -> .` in a crafted source cannot put one account's login
  inside another profile.
- `may_delete_dir`'s home and ambient checks are **containment** checks over
  both spellings, because `remove_dir_all` resolves symlinked ancestors that
  the lexical path never revealed.
- Known and accepted: two concurrent `herdr account add` runs can lose one
  store entry (load–modify–save, no lock file). Both directories are still
  created.

### PR 3 — feat(accounts): herdr account status and login · deps: 2

**Goal.** Show each profile's health and identity (never tokens), which
agents are running on it, and let the user log a profile in from a pane.

**Files**

- `src/accounts/status.rs` (new, pure assembly; call
  `layout::inspect(profile, InspectOptions::with_identity())` — the default
  `health()` deliberately does not parse `.claude.json`): `AccountStatus { profile,
  inspection, agents: Vec<AgentOnAccount { pane_id, name, agent_status,
  account_state }> }`, `assemble(profiles, inspections, agent_infos) ->
  Vec<AccountStatus>` (agents matched by `tokens.account`), `render_text`.
- `src/cli/account.rs`: `status [<name>] [--json]` (server optional: when
  `agent.list` fails with server-not-running the agents column is `-` and
  exit stays 0 — read is safe and offline), `login <name> [--pane <id>]`.
- `src/cli/spec.rs` *(upstream file — minimal wiring)*: two subcommands.
- `tests/fork_accounts.rs`: `account_status_reports_identity_without_
  secrets`, `account_login_types_into_the_pane`.

**Shapes/approach.** `login` = resolve the pane (`--pane` or `pane.current`),
`pane.process_info` → shell name → `ShellFamily::from_process_name` →
`accounts::launch::env_assignment_line` (PR 1; it returns `Err` for an
unrecognised shell *and* for a directory that shell cannot quote — surface
both, never type a fallback) → `pane.send_text(line +
"\r")` → `pane.send_text("claude auth login\r")` → prints "follow the login
in pane <id>; run `herdr account status <name>` when done". An unrecognised
shell or a pane not at its prompt is an error; nothing is typed. `status` shows
`email` and `plan` from `identity`, `logged_in`, `hook_installed`,
`broken_links`, and the agents list; `--json` mirrors it.

**Tests.** Unit: `assemble` with agents carrying/not carrying tokens and an
unknown `account_state` string; `render_text` golden. Integration: after the
stub's `auth login` the lab profile shows the fake email; the pane's
`pane.read` contains `fake-claude: auth login` and the stub wrote
`.credentials.json` with mode `600`.

**Real-server validation.** Lab up; `account status --json` shows both
profiles with `identity.email` (redact in evidence), `agents: []`; `account
login work --pane $HERDR_ACCOUNTS_LAB_PANE`; `herdr … pane read
$HERDR_ACCOUNTS_LAB_PANE` shows the export line and the stub's login output;
`account status work` now `logged_in: true`. Stop the server and rerun
`account status` → exit 0 with `agents: -`.

**Downstream.** `AccountStatus` JSON is what PR 9 extends with `limit`
information and PR 11 documents. It is a flat object, not the nested
`{profile, inspection, agents}` the plan sketched (see *As built*), so PR 9's
keys go alongside `hook_installed` and `agents`.

**As built (PR 3, merged).** The *As built* notes for PRs 1, 2 and 4 all still
hold; the corrections below are what PRs 9, 10 and 11 must code against.

- **`assemble` takes the name filter, and the row shape is flat.** The
  signature is `assemble(&Profiles, only: Option<&str>, inspect: impl
  Fn(&AccountProfile) -> ProfileInspection, agents: Option<&[AgentFact]>) ->
  Vec<AccountStatus>`. `only` narrows *before* `inspect` runs, because
  `InspectOptions::with_identity()` parses a whole `.claude.json` and
  `status <name>` must not pay for the profiles it does not print; the default
  flag is still decided against the full merged view, so a one-profile report
  says what the full one would. `AccountStatus` is not the planned
  `{profile, inspection, agents}` nesting but a **flat superset of `account
  list`'s row** — `name, agent, config_dir, default, origin, dir_exists,
  logged_in` plus `credentials_mode_ok, identity, hook_installed,
  broken_links, agents` — so a reader who learned `list` already knows
  `status`, and PR 9 adds its limit keys to the same flat object.
- **`agents` is `null` when unknown and `[]` when empty.** They are different
  answers — "no server answered" versus "the server has none" — and collapsing
  them would let a stopped server read as an idle account. The text report
  renders them `- (no herdr server answered)` and `none`.
- **Inspection is a closure, not a parallel slice.** The plan's
  `assemble(profiles, inspections, agent_infos)` would have paired two vectors
  by index; passing `|profile| layout::inspect(profile, …)` keeps the module
  pure, keeps the filter cheap, and cannot mis-pair.
- **`AgentFact` is the input type, not `AgentInfo`.** `status.rs` reduces
  `crate::api::schema::AgentInfo` to `{pane_id, name, agent_status, account,
  account_state}` through `AgentFact::from_agent_info`, so the assembly and its
  tests need no server and a schema change cannot reshape the report silently.
  `account_state` is carried **verbatim**, with `account_state_known: false`
  when `AccountState::parse` does not recognise it: a newer herdr may report a
  state this build has never heard of, and dropping it would hide an agent's
  state entirely.
- **An agent on a profile that is gone is surfaced, not dropped.**
  `status::agents_on_unknown_profiles` is reported on stderr as a warning
  (exit code unchanged) — it is the one thing a per-profile report would
  otherwise hide.
- **`status` never fails because of the server.** Server-not-running,
  protocol-mismatch, an API error and an unreadable agent list all degrade to
  `agents: null` plus a `note:` on stderr (silent for the protocol guard, which
  prints its own message). Exit stays 0; only configuration diagnostics make it
  1, exactly like `list`. Usage errors are 2.
- **`login` does not touch `src/accounts/client.rs`.** PR 5 owns that file, and
  `AppliedLine`'s drop notice is worded for an agent launch, so `login` has its
  own three small runtime helpers in `src/cli/account.rs` (`pane_process_info`,
  `send_line`, `wait_for_prompt`) over `crate::cli::send_request`. They reuse
  the tested pure pieces — `launch::pane_shell_at_prompt` and
  `launch::env_assignment_line` — so the refusal rules are the launcher's, not
  a second copy.
- **`login` refuses a missing profile directory.** Letting `claude auth login`
  create one would produce an unseeded, world-readable directory that no other
  herdr command knows how to repair; the message names `herdr account add`.
  It also refuses an unknown profile, a pane that is not idle at its shell
  prompt, and a shell `env_assignment_line` cannot quote — in every case
  nothing is typed and stdout stays empty.
- **The lab's two profiles both ship logged in**, so the plan's recipe
  (`account login work` then `status work` now `logged_in: true`) proves
  nothing. Validation and `tests/fork_accounts.rs::account_login_types_the_
  profile_into_the_pane` instead create a third, logged-out profile with
  `herdr account add <name> --config-dir "$HERDR_ACCOUNTS_LAB_ROOT/profiles/
  <name>" --no-hook` and watch `logged_in` flip `false → true` with a `0600`
  credentials file and the stub's identity appearing.
- **`main.rs` gained two usage lines** (`account status`, `account login`).
  The plan reserved `src/main.rs` for PR 1; two adjacent `println!` lines in the
  usage block are the whole edit and collide with nothing.
- The `#[allow(dead_code)]` on `layout::InspectOptions::with_identity` is gone —
  `status` is its caller. `layout::CREDENTIALS_MODE` still has no production
  caller and keeps its marker.

**Hardening found during PR 3's review** (all fixed in the same PR):

- **`login`'s second line had no gate.** The launch driver hands its second
  line to `agent.start`, which refuses a busy pane server-side; `login` submits
  `claude auth login` with a raw `pane.send_text` that nothing stands in front
  of. A pane that had picked up a foreground job between the two sends would
  have received the command as *stdin*. `wait_for_prompt` timing out is now a
  refusal (exit 1) that names the exported variable and the command to run by
  hand; the budget grew to 5 s because a slow prompt command is the ordinary
  reason to miss it.
- **A lost `pane.send_text` reply is not "nothing was typed".**
  `SendLineError { detail, may_have_landed }` draws the same distinction
  `client::apply_env` does: a server rejection wrote nothing to the pty, a
  transport failure may have delivered the line and lost only the reply, and
  the second case prints a note naming the pane and directory.
- The "already has credentials, completing this login replaces them" and "no
  session hook" warnings are printed **before** the typing, where the user can
  still act on them.
- The login command is an exhaustive `match profile.agent`, so a second
  `AccountAgent` is a compile error rather than a `claude auth login` typed at
  a profile that is not Claude's. `login` also renders
  `LaunchError::PaneNotAtPrompt` itself, because the shared message ends by
  offering `--account none`, which belongs to `herdr agent start`.
- **`identity_from_claude_json` bounds what it will print.** PR 3 is the first
  caller of `InspectOptions::with_identity()`, so it is the first to print a
  file herdr does not own into a terminal. `is_printable_identity` caps a field
  at 120 characters (length first, so the scan stays bounded) and refuses bidi
  and zero-width formatting characters as well as control ones — a 10 MB
  `subscriptionType` used to be echoed verbatim, and a U+202E address renders
  as one thing while it reads as another. A refused field falls through to the
  next key name and then to "unknown"; it is never cleaned up and shown.
- Continuation lines in the agents column align with the first agent (the join
  is 15 spaces, matching `field`'s two-space + twelve-wide-label + one-space
  gutter).

### PR 4 — feat(accounts): launch claude under a profile with herdr agent start --account · deps: 1

**Goal.** The two-step launch of decision (b) with verification and the
account token, exposed as `herdr agent start --account <name>` and applied
by default per decision (f).

**Files**

- `src/accounts/launch.rs` (created in PR 1 with `ShellFamily`,
  `env_assignment_line` and `LaunchError`, all behind a module-level
  `#![allow(dead_code)]` this PR should delete once it has callers; this PR
  adds): `LaunchPlan { pane_id, name, kind:
  "claude", args, profile, line }`, `plan_launch(profiles, choice,
  pane_shell, …) -> Result<LaunchPlan, LaunchError>` (profile directory must
  exist; hook missing is a warning carried on the plan, not an error).
- `src/accounts/client.rs` (new, runtime): `AccountsClient<'a> { api:
  &'a ApiClient }` with `shell_of(pane) -> Result<String>` (`pane.
  process_info`), `apply_env(pane, line)` (`pane.send_text`), `start(plan)
  -> AgentInfo` (`agent.start` with the existing `agent_pane_busy` retry
  from `src/cli/agent.rs` factored into a shared helper in this file),
  `verify(agent_info, expected_dir) -> AccountState` (`pane.process_info`
  → the `claude` process pid → `crate::platform::process_env_var(pid,
  "CLAUDE_CONFIG_DIR")`), `report(pane, name, state)` (`pane.report_
  metadata` with the PR 1 constants, `agent: "claude"`, no TTL),
  `launch_with_account(...) -> Result<LaunchOutcome, LaunchError>` running
  the sequence and returning `{agent, account, account_state, line}`.
- `src/platform/mod.rs` *(upstream file — one appended function)*:
  `pub(crate) fn process_env_var(pid: u32, name: &str) -> Option<String>`;
  `#[cfg(target_os = "linux")]` reads `/proc/<pid>/environ` (NUL-separated,
  first match, bounded read via the existing `read_limited_reader`), other
  targets return `None`. Unit test on a fake environ blob (pure parse
  helper `parse_environ_blob` next to it).
- `src/cli/agent.rs` *(upstream file — minimal wiring)*: `"--account"`
  arm in the flag loop (value `<name>` or `none`); after parsing, when the
  kind is `claude` and `crate::accounts::profile::load_profiles` yields a
  choice, hand off to `crate::accounts::client::launch_with_account` and
  print its JSON (`{agent, argv, account, account_state}` — the stock
  response plus two keys); otherwise the existing path unchanged.
  `src/cli/spec.rs`: `--account <NAME>` option on `start` with help text.
- `tests/fork_accounts.rs`: `agent_start_with_account_exports_the_profile_
  dir`, `agent_start_uses_the_default_profile`, `agent_start_account_none_
  skips_profiles`, `agent_start_refuses_unknown_shell` (a pane whose
  foreground is `sh -c 'while :; do sleep 1; done'` is busy; a pane running
  a shell named `weirdsh` — a copied `sh` binary — is refused with the
  message).

**Shapes/approach.** Sequence: profiles → choice → `pane.process_info` →
family → line → `pane.send_text` → (150 ms) → `agent.start` → wait for
`interactive_ready` (existing `wait_for_named_agent`) → verify → report →
print. If verification says `mismatch`, the token is still reported (so the
sidebar shows it) and the command exits 1 with the actual directory. If the
profile has no hook (`inspect.hook_installed == false`) print a one-line
warning (session id will not be reported). When no profile is chosen the
command is byte-for-byte the stock behaviour.

**Tests.** Unit: every shell family's line (golden strings, quoting of `'`
and spaces, refusal of `\n`), `plan_launch` error paths, `parse_environ_
blob`, `AccountState` transitions in `verify` (env matches / differs /
unreadable). Integration: the lab stub writes `last-launch.json` with
`CLAUDE_CONFIG_DIR` and argv; the test asserts it equals the `work`
directory, `agent.get` shows `tokens.account == "work"` and `account_state
== "ok"` (Linux) and `agent_session.value` set (the stub reported it).

**Real-server validation.** Lab up; `herdr --session accounts-lab agent
start a1 --kind claude --pane $HERDR_ACCOUNTS_LAB_PANE --account work` →
JSON with `account: "work"`, `account_state: "ok"`; `herdr … agent get a1`
shows the tokens and `agent_session`; `herdr … pane read $PANE` shows
`CLAUDE_CONFIG_DIR=…/profiles/work` printed by the stub; `cat
$HERDR_ACCOUNTS_LAB_PROFILE_WORK/last-launch.json`; `cat /proc/$(pgrep -f
'profiles/work' | head -1)/environ | tr '\0' '\n' | grep CLAUDE_CONFIG_DIR`
as independent evidence. Second pane: start without `--account` → `perso`
(default). Third: `--account none` → no tokens. Attach the TUI in a PTY
harness (`tests/cli/harness.rs` style) with `[ui.sidebar.agents.rows_by_
agent] claude = [["state_icon","workspace"],["agent","$account"]]` and
capture the row showing `work`.

**Downstream.** `launch_with_account` is the only launcher; PR 5's relaunch
and PR 7's TUI call it, never `agent.start` directly. `process_env_var`
returning `None` means `unverified`, never `ok`. The JSON keys `account`,
`account_state` on the `agent start` response are contract.

**As built (PR 4, merged).** The sequence and the exit codes are as
specified; the corrections below are the shapes PR 5, PR 7 and PR 8 must code
against, and they win over the prose above.

- **`crate::platform::process_env_var` answers three ways, not two.**
  `ProcessEnvVar::{Unreadable, Unset, Set(String)}` replaces the planned
  `Option<String>`, because "the environment was read and the variable is not
  there" is *evidence* (the assignment never reached the launched job, so the
  agent is on Claude's own default directory) while "could not read it" is the
  absence of evidence. The plan's *Downstream* line — "`process_env_var`
  returning `None` means `unverified`" — therefore holds only for
  `Unreadable`. A value that is not UTF-8 is `Unreadable`, and so is an empty
  or oversized `/proc` blob.
- **`verify` grades through the pure `client::grade(&[ProcessEnvVar], &str)`.**
  A reading naming a different directory is `mismatch` whatever the others
  say; otherwise a reading naming the expected directory is `ok`; otherwise
  *any* environment read without the variable is `mismatch` with no directory
  (`LaunchOutcome::mismatch_detail` renders that as "no CLAUDE_CONFIG_DIR at
  all"); only when nothing at all could be read is it `unverified`. The pane
  shell's own pid is always excluded — its `/proc` environment is its
  exec-time one and never shows the line just typed. A shell that swallows the
  line (a `read` builtin, a continuation prompt) passes every gate herdr has,
  so this is the check that catches it; `tests/fork_accounts.rs::agent_start_
  reports_a_mismatch_when_the_shell_swallows_the_environment_line` drives that
  through a real server.
- **`apply_env` consumes the plan and returns `client::AppliedLine`**, a guard
  that owns the fact that the pane's shell now exports the variable.
  `AppliedLine::finish()` grades and reports; dropping it any other way prints
  a note naming the pane, the directory and the profile, because `agent.start`
  can fail from a dozen places after the line has landed and the export
  outlives all of them. Callers keep the guard alive until the agent is ready.
- **`--account none` is the stock launch, which means it does not undo a
  previous one.** Nothing is typed and no token is reported, so a pane whose
  shell still exports `CLAUDE_CONFIG_DIR` from an earlier `--account` start
  launches under that directory with no account claimed. The `agent start`
  after-help says so.
- The probe reads `/proc` on the machine the CLI runs on. That is sound
  because `crate::cli::send_request` only ever speaks to the local API socket,
  so the pids `pane.process_info` returns are local pids; a hand-forwarded
  remote API socket would break the assumption and is not supported.
- `pane_shell_at_prompt` lives in `launch.rs` (pure) and is deliberately as
  strict as the server's own `available_pane_shell_from_job`: the foreground
  process group must *be* the shell and hold nothing else. It additionally
  accepts `argv[0]` when the process name is not a shell, matching
  `src/cli/agent.rs::process_info_shows_shell_initialization`.
- **`PaneProcessInfo.foreground_processes`, not `processes`.** The field the
  plan named does not exist; the shape is `{pane_id, shell_pid,
  foreground_process_group_id, tty, foreground_processes: [{pid, name, argv0,
  argv, cmdline, cwd}]}` (`src/api/schema/panes.rs`). While an agent runs the
  pane's `shell_pid` is **not** in that list — the foreground job is the
  agent's own process group.
- **The `agent_pane_busy` retry was not factored out of `src/cli/agent.rs`,
  and there is no `AccountsClient` struct.** Duplicating that retry loop into
  `accounts::client` would have made a second, drifting copy of the subtlest
  part of `agent.start`, so `agent_start` keeps the stock loop verbatim and
  the account steps bracket it: `prepare_account_launch` (resolve → refuse →
  type) before it, `AppliedLine::finish` (verify → report) after it.
  `client.rs` never calls `agent.start`. Its entry points are free functions
  over `crate::cli::send_request` rather than methods on a struct holding an
  `ApiClient`, so an account command reports an incompatible or absent server
  through the same protocol guard as every other `herdr` subcommand.
- **Directories are compared through `config::dir_key`**, on both sides, so a
  trailing slash in a `config_dir` or in the process environment is not a
  mismatch.
- **`agent_start_refuses_unknown_shell` became two tests.** herdr offers no
  way to start a pane under a renamed shell binary, so the planned `weirdsh`
  case is a unit test over a synthetic `PaneProcessInfo`
  (`a_shell_herdr_cannot_write_an_assignment_for_is_refused_by_name`), and the
  integration test covers what a lab can really produce: a pane whose
  foreground is `sleep`
  (`agent_start_refuses_a_busy_pane_before_typing_anything`), asserting that
  nothing was typed and nothing was launched.
- **The csh refusal PR 1 shipped is reachable from the launch path** and is
  covered by `a_csh_pane_refuses_a_directory_it_cannot_quote_rather_than_
  launching`: a tcsh pane with a `!` in the profile directory fails the plan
  instead of launching under the ambient account.
- **`src/main.rs` was not touched.** `agent start` already has its usage line
  and `--account` is a flag, so no usage or allowlist change was needed; the
  only upstream edits are `src/cli/agent.rs`, `src/cli/spec.rs`
  (`agent_command()` only) and the appended `src/platform/mod.rs` function.

### PR 5 — feat(accounts): switch a running claude agent to another profile keeping its session · deps: 4

**Goal.** Decision (i) as a pure state machine plus the CLI
`herdr agent switch-account <pane|name> <account> [--yes] [--interrupt]
[--force] [--timeout MS] [--json]`.

**Files**

- `src/accounts/switch.rs` (new, pure): `SwitchMachine::new(SwitchInput {
  agent: AgentInfo-shaped struct, target, options, now })`, `Phase::{
  Preflight, Confirm, Interrupt, Exit { deadline }, AwaitShell { deadline },
  Relaunch, AwaitSession { deadline }, Verify, Record, Done(SwitchResult),
  Failed(SwitchError) }`, `fn next(&mut self, obs: Observation) -> Action`
  where `Observation::{Agent(Option<AgentInfoLite>), Foreground(Option<
  ForegroundKind>), Confirmed(bool), Launched(Result<LaunchOutcome,_>),
  Tick(now)}` and `Action::{AskConfirm(String), SendKeys(Vec<String>),
  Prompt(String), PollAgent, PollForeground, Launch(LaunchRequest), Report(
  Tokens), Finish(Result<SwitchResult, SwitchError>)}`; `SwitchResult {
  pane_id, name, from: Option<String>, to, session_id, account_state }`.
- `src/accounts/client.rs`: `switch_account(...)` driving the machine with
  the real API (`agent.get`, `agent.send_keys ["Escape"]`, `agent.prompt
  "/exit"`, `pane.process_info` polling at 250 ms, `launch_with_account` with
  `args = ["--resume", id]` and the same name, `agent.get` until
  `agent_session.value == id`).
- `src/cli/agent.rs` *(upstream file — minimal wiring)*: `"switch-account"
  => crate::cli::account::agent_switch_account(&args[1..])` arm; the
  implementation lives in `src/cli/account.rs`. `src/cli/spec.rs`: the
  subcommand under `agent_command()` with `.about()` and `.after_help`
  describing the protocol and that it never kills the agent.
- `tests/fork_accounts.rs`: `switch_account_resumes_the_same_session_under_
  the_new_profile`, `switch_account_refuses_working_agent_without_
  interrupt`, `switch_account_requires_yes_when_not_a_tty`, `switch_account_
  leaves_shell_usable_when_relaunch_fails` (target dir removed between
  preflight and relaunch).

**Shapes/approach.** The confirmation text names pane, agent, from → to and
the session id. Exit uses `agent.prompt` (which submits) with `/exit`; if
the agent is `blocked` an `Escape` goes first (300 ms). `AwaitShell`
succeeds when `pane.process_info.processes` contains only the shell (pid ==
`shell_pid`, or the foreground group equals the shell's). On timeout the
machine fails with `AgentStillRunning` and **sends nothing else**. The
relaunch reuses the agent name (herdr clears the managed name on process
exit — `clear_agent_name` is reached from the exit path; if a live test
shows a `duplicate_name` error, fall back to `<name>` + `agent.rename` after
launch and record that in *As built*). `AwaitSession` accepts the hook's
`resume` report with the same id; the stub emits it. `from` comes from the
current `tokens.account` (may be `None`).

**Tests.** Unit: a scripted-observation harness walking every phase to
`Done`; each failure path (`no session`, `working without --interrupt`,
`target missing`, `target logged out`, `exit timeout`, `session mismatch
after resume`, `launch error`) ends in `Failed` with no further `Action`
except `Finish`; timers are injected `now` values. Integration: the stub
records `--resume <id>` and the second `last-launch.json` shows the `work`
dir; `agent.get` shows the same `agent_session.value` before and after and
`tokens.account` flipped.

**Real-server validation.** Lab up; start `a1` on `perso`; `herdr --session
accounts-lab agent switch-account a1 work --yes --json` → `SwitchResult` with
the same `session_id`, `account_state: "ok"`; `pane read` shows `/exit`,
the new export line and `resumed <id>`; `agent get a1` tokens `work`. Repeat
with the stub in `FAKE_CLAUDE_BUSY=1` mode (never returns to the prompt) →
the command fails after the timeout with `AgentStillRunning`, the stub is
still running, nothing was killed. Run once through a real PTY (`script -q`)
without `--yes` to see the prompt and decline it (`n`) → nothing sent.

**Downstream.** `SwitchMachine` is what PR 8's TUI action drives on a
background thread with the same `Observation`/`Action` types; PR 9 adds a
preflight hint (`limit`) through `SwitchInput.limit: Option<UsageLimit>`
without changing phases.

### PR 6 — feat(fleet): fleet report and change stream carry agent metadata tokens · deps: 1

**Goal.** `herdr fleet status --json` (and therefore E3's `/api/fleet` and
`/api/events`) show `tokens` and `state_labels` per agent, so the account is
visible fleet-wide and later on the phone.

**Files**

- `src/fleet/state.rs` (fork): `MergedAgent.tokens: BTreeMap<String,
  String>`, `MergedAgent.state_labels: BTreeMap<String, String>` (appended,
  `#[serde(default)]`), copied in `HostState::merged_agent`; a new
  `FleetChange::AgentMetadata { pane: FleetPaneRef, tokens, state_labels }`
  (appended variant) emitted by `apply` when an existing agent's maps
  differ; `assert_invariants_for_test` unchanged.
- `src/fleet/report.rs` (fork): `AgentReport.tokens`, `.state_labels`
  (appended; `#[serde(default)]` so older JSON still decodes), `From<&
  MergedAgent>` copies them; `render_text` adds an `account` column only
  when any agent carries `tokens.account` (pure function `account_column`).
- `docs/fork/fleet-core.md`: field table rows for the two maps and the new
  change kind; note that E4's reducer must treat unknown `kind`s as no-ops
  (the contract already says so).
- `tests/cli/fleet.rs`: extend `fleet_status_json_merges_two_named_
  sessions` (or add a sibling) to report a token with `herdr pane
  report-metadata … --token account=work` on one session and assert it in
  the merged JSON.

**Shapes/approach.** Wire `ClientShellAgent.tokens: Vec<(String, String)>`
→ `BTreeMap` (sorted, deterministic JSON). `apply` compares maps only for
agents present in both snapshots (O(agents)); no per-frame work.
`FLEET_STATUS_SCHEMA` stays `v1` (additive).

**Tests.** Unit: round-trip with tokens; a report without the new keys
decodes (`default`); `apply` emits `AgentMetadata` exactly once per change
and not when unchanged; `render_text` golden with and without the column.
Integration as above.

**Real-server validation.** `fleet-lab.sh up 2`; on `lab-1` create a pane
at a prompt, start the fake `claude` under the accounts lab's `PATH` (or
simply `herdr --session lab-1 pane report-metadata <pane> --source
fork:accounts --agent claude --applies-to-source herdr:claude --token
account=work`); `herdr fleet status --json | jq '.agents[].tokens'` shows
`{"account":"work"}`; `herdr fleet status --watch` prints an
`agent_metadata` line when the token changes. If E3's gateway is merged,
`curl -H "Authorization: Bearer $(cat …/gateway/read.token)"
http://127.0.0.1:7788/api/fleet | jq '.agents[0].tokens'` confirms the
phone path.

**Downstream.** E4's fleet reducer reads `agents[].tokens.account`; E7's
phone switch action will call the same stock methods PR 5 uses, routed per
host.

### PR 7 — feat(accounts): tui account picker to start claude in a pane · deps: 4

**Goal.** From the pane context menu, start Claude in this pane under a
chosen profile, following the existing modal language.

**Files**

- `src/client/shell/state.rs` *(upstream file — minimal wiring)*:
  `ClientShellOverlay::AccountPicker(ClientAccountPickerOverlay)` +
  `ClientShellOverlayKind::AccountPicker` + the `kind()` arm;
  `ClientContextMenuAction::StartClaudeAs`; `ClientContextMenuTarget::Pane`
  gains `agent_kind: Option<String>` and `account: Option<String>` (filled
  from `snapshot.agents` in `open_pane_context_menu`).
- `src/client/shell/account_overlay.rs` (new, fork): `ClientAccountPicker
  Overlay { pane_id, mode: PickerMode::Start, entries: Vec<AccountEntry {
  name, plan, logged_in, is_default }>, selected, query, job: Option<
  AccountJob>, error }` modelled on `ClientWorktreeOpenOverlay`;
  `render_account_picker` using `overlays.rs::{panel, popup, row, button}`;
  `route_account_picker_key` (↑/↓, type-to-filter, Enter, Esc);
  `AccountJob` = `std::thread::JoinHandle` + `mpsc::Receiver<JobEvent>`
  polled on the shell's existing tick; the thread runs
  `crate::accounts::client::launch_with_account` against
  `ApiClient::local()`.
- `src/client/shell/context_menu.rs` *(upstream file — minimal wiring)*:
  one item `Start Claude as account…` in the `Pane` arm, shown only when the
  pane has no agent, the client config has ≥ 1 profile, and the active
  endpoint is the local server; `activate_pane_context_action` arm opening
  the overlay. `overlays.rs::render_client_overlay` and
  `overlay_input.rs::route_overlay_key`: one arm each delegating to the fork
  file.
- `src/client/shell/config.rs`: `ClientShellConfig.accounts: Vec<
  AccountProfileConfig>` copied from the loaded config (the client already
  reloads `config.toml`).

**Shapes/approach.** Agent name for the launch: `claude`, then `claude-2`…
unique among `snapshot.agents[].name`. Progress lines in the modal
(`exporting profile…`, `starting claude…`, `verifying…`); success closes the
modal and the sidebar row shows `$account` if the user configured the
token; failure stays open with the message and a `Close` button. The modal
never sends input itself — the driver does, over the API, exactly as the
CLI. Endpoint gating reuses the multi-machine client's notion of the active
endpoint (`endpoint_sidebar.rs`); remote endpoints hide the item (v1 is
local-host only).

**Tests.** Unit: picker filtering/selection, name generation, `JobEvent`
folding into overlay state (pure), the context-menu item visibility matrix
(`agent present`, `no profiles`, `remote endpoint`). Integration: a PTY
harness test (`tests/cli/harness.rs` pattern, as `tests/multi_client.rs`
attaches) that opens the context menu on the lab pane with a right-click
mouse event, picks `work`, and asserts `agent.get` tokens afterwards.

**Real-server validation.** Lab up; attach `herdr --session accounts-lab`
in a PTY harness; right-click the pane → `Start Claude as account…` →
choose `work` → the stub prints `CLAUDE_CONFIG_DIR=…/work`; `agent list
--json` shows `tokens.account == "work"`; capture the modal frames as
evidence. Repeat with an empty `[[accounts]]` config: the item is absent.

**Downstream.** PR 8 reuses `ClientAccountPickerOverlay` with `PickerMode::
Switch { agent_name, current }` and `AccountJob`.

### PR 8 — feat(accounts): tui switch-account action with confirmation · deps: 5, 7

**Goal.** `Switch account…` on a Claude agent's pane: picker → explicit
confirm modal → the PR 5 machine on a background thread with progress → the
sidebar reflects the new account.

**Files**

- `src/client/shell/account_overlay.rs`: `PickerMode::Switch`, a
  `ClientAccountSwitchConfirm { pane_id, name, from, to, session_id,
  interrupt: bool }` step rendered like `ClientConfirmCloseOverlay` (`y`/
  Enter confirms, `n`/Esc cancels, `i` toggles interrupt when the agent is
  working), then the job phase showing the machine's phase names.
- `src/client/shell/state.rs`, `context_menu.rs` *(upstream files — minimal
  wiring)*: `ClientContextMenuAction::SwitchAccount`; item visible when the
  pane's agent is `claude`, ≥ 2 profiles exist, and the endpoint is local.
- `src/accounts/client.rs`: `switch_account_with_progress(…, on_event:
  impl FnMut(SwitchEvent))` (shared by CLI `--verbose` and the TUI).

**Shapes/approach.** The confirm step is mandatory (no config to skip it).
The job thread owns its own `ApiClient`; the overlay only folds events. If
the user closes the overlay while a job runs, the job continues to a
terminal state (never leaves an agent half-switched) and the outcome lands
as a notification (`notifications.rs`).

**Tests.** Unit: the confirm/interrupt keymap, event folding, visibility
matrix. Integration: PTY harness drives menu → picker → confirm → asserts
the stub's `resumed <id>` and the flipped token.

**Real-server validation.** Lab up, start `a1` on `perso` via the CLI;
attach the TUI; right-click → `Switch account…` → `work` → confirm; watch the
progress lines; `agent get a1` shows the same `agent_session.value` and
`tokens.account == "work"`. Decline once and prove nothing was typed (`pane
read` unchanged).

**Downstream.** None beyond documentation (PR 11).

### PR 9 — feat(detect): claude usage-limit rule and account limit hints · deps: 3, 5

**Goal.** Recognise Claude Code's usage-limit screen as `blocked` with rule
id `usage_limit`, extract the reset time when shown, and surface a "switch
account" hint in `account status` and the switch preflight — best-effort,
fixture-driven, with exact live follow-ups recorded.

*(PR 3 as built: `AccountStatus` is one **flat** object per profile — the
`account list` row plus `credentials_mode_ok`, `identity`, `hook_installed`,
`broken_links`, `agents` — not the `{profile, inspection, agents}` nesting the
prose above sketched, and `agents` is `null` when no server answered. The
`limit` keys go on `AgentOnAccount` alongside `account_state`, which is carried
verbatim with `account_state_known`. `jq '.[] | .agents[]? | .limit'` — the
`?` matters, because `.agents` can be `null`.)*

**Files**

- `tests/fixtures/fork/claude-usage-limit.txt` (new): the bottom-buffer text
  of the limit screen transcribed from a screenshot (documented source and
  date in a sibling `README.md`), plus `claude-usage-limit-negative.txt`
  (a normal idle prompt mentioning "limit" in user text) to prove the rule
  does not fire on incidental text.
- `src/detect/manifests/claude.toml` *(upstream file — one appended rule,
  version bump)*: rule `usage_limit`, `state = "blocked"`, `priority = 985`
  (above `live_blocked_form` so it wins while both are visible),
  `region = "bottom_non_empty_lines(12)"`, `visible_blocker = true`, gates:
  `all = [{ any = [{ line_regex = ['(?i)^\s*.*usage limit reached'] },
  { line_regex = ['(?i)you.ve (hit|reached) your (usage )?limit'] }] }]`
  with `any = [{ contains = ["resets"] }, { contains = ["upgrade"] },
  { contains = ["/login"] }, { contains = ["switch"] }]` and `not` gates for
  the transcript viewer and permission prompts — **the exact strings are
  finalised from the fixture**, encoded as explicit AND/OR gates per
  `AGENTS.md`, never whole-pane text. `version` → `2026.09.06.1`,
  `updated_at` updated. `distribution/agent-detection/claude.toml` copied
  byte-identical (the check script requires it; the fork does not publish
  the catalog).
- `src/accounts/limit.rs` (new, pure): `UsageLimit { reset_text:
  Option<String> }`, `classify(explain_json: &serde_json::Value, screen:
  &str) -> Option<UsageLimit>` (`matched_rule.id == "usage_limit"` +
  `reset_regex` over the detection read), `hint(&UsageLimit, profiles) ->
  String` ("usage limit on <account>; switch with `herdr agent
  switch-account <pane> <other>`").
- `src/cli/account.rs`: `status` calls `agent.explain` **only for agents
  whose status is `blocked`** and adds `limit: {reset_text}`; `switch-
  account` preflight prints the hint (from `agent.explain` on the source
  agent) and, with `--json`, includes `limit`.
- `src/accounts/switch.rs`: `SwitchInput.limit: Option<UsageLimit>` (text
  only).
- `scripts/fork/fake-claude.sh`: `FAKE_CLAUDE_LIMIT=1` prints the fixture
  then idles.

**Shapes/approach.** Evidence loop per `AGENTS.md`: with the stub printing
the fixture, `herdr agent read <pane> --source detection --format text`
shows what the detector sees; iterate the rule through the override path
(`$XDG_CONFIG_HOME/herdr-dev/agent-detection/claude.toml` inside the lab —
never the user's `~/.config/herdr`) and `herdr --session accounts-lab
server reload-agent-manifests`; then move the final rule into the bundled
manifest and delete the lab override. Check `src/detect/manifest_update.rs`
for how cached remote manifests shadow the bundled one (precedence: override
> cached remote > bundled): if a remote catalog with an equal or higher
version can shadow the fork's rule, record the mitigation in *As built* and
in `docs/fork/accounts.md` (either the fork's bundled version strictly
above upstream's, or a documented `herdr` setting that disables remote
manifest updates).

**Tests.** Unit: `manifest.rs` tests load the bundled manifest and assert
the fixture matches `usage_limit` with `blocked` and the negative fixture
does not (offline through `explain_for_label`); `classify` on both; reset
extraction on `resets 3pm`, `resets at 14:00`, absent. Maintenance:
`python3 scripts/agent_detection_manifest_check.py --require-all-published`
passes. Integration: the lab stub in limit mode → `herdr agent explain
<pane> --json` shows `matched_rule.id == "usage_limit"`, `agent list` shows
`blocked`, `account status --json` shows `limit`.

**Real-server validation.** Lab up with `FAKE_CLAUDE_LIMIT=1`; start `a1`;
`herdr … agent read a1 --source detection --format text` (attach to the PR
as evidence); `herdr … agent explain a1 --json | jq .matched_rule`; `herdr …
account status --json | jq '.[] | .agents[]? | .limit'`; `herdr … agent
switch-account a1 work --yes --json` shows `limit` in preflight output.
Negative: without the env the rule does not match (`explain` → the idle
rule).

**Downstream.** PR 10 reuses `classify`. The fixture is the only evidence
until a real limited account is available (see the live checklist).

### PR 10 — feat(accounts): herdr account watch labels usage-limited agents · deps: 9

**Goal.** An opt-in, event-driven helper that turns the limit into a sidebar
hint on stock servers: `herdr account watch [--json]` subscribes to
`pane.agent_status_changed`, runs `agent.explain` once per transition to
`blocked` on a Claude agent, and reports `state_labels.blocked = "usage
limit"` plus `tokens.account_state = limited` (cleared back to `ok` on the
next non-blocked transition). It never switches.

**Files**

- `src/accounts/watch.rs` (new, pure): `WatchState` folding
  `PaneAgentStatusChangedEvent`-shaped inputs into `WatchAction::{Explain(
  pane), Label(pane, UsageLimit), Clear(pane), Nothing}` with per-pane
  debounce (no second explain within 5 s) and a bound on tracked panes.
- `src/accounts/client.rs`: `watch(...)` driving `events.subscribe` (the
  streaming request path used by `herdr events`/`agent wait`; reuse
  `ApiClient`'s subscription helper) and the actions; prints one line per
  label change; exits 0 on Ctrl-C and 1 when the server goes away.
- `src/cli/account.rs`, `src/cli/spec.rs`: the subcommand.
- `docs/fork/accounts.md` snippet: `[ui.sidebar.agents.rows_by_agent]
  claude = [["state_icon","workspace","tab"],["agent","$account",
  "$account_state"]]` and the `state_text` swap the label produces.

**Tests.** Unit: fold sequences (blocked → explain → label; blocked again
within 5 s → nothing; idle → clear; unknown agent kind → nothing).
Integration: the lab in limit mode with `watch` running in the background;
`agent list --json` shows `state_labels.blocked == "usage limit"` and
`account_state == "limited"` within 5 s; switching the stub out of limit
mode (send `Enter` to the stub, which then prints a normal prompt) clears
it.

**Real-server validation.** As the integration test, by hand, plus the TUI
attached to show the row text `usage limit` and `$account_state` `limited`;
`Ctrl-C` on `watch` exits 0 and leaves labels as they are (documented).

**Downstream.** E6 (push) can reuse `WatchState` semantics for
"blocked by limit" notifications.

### PR 11 — docs(accounts): accounts guide, adr, readme and roadmap drift · deps: 2, 6, 8, 10

**Goal.** User docs and the decision record.

**Files**

- `docs/fork/accounts.md` (new): concepts, `[[accounts]]` reference,
  `profiles.toml`, the share/copy/private list (from `layout.rs`, kept in
  sync by a doc-contract test that greps the constants), every `herdr
  account …` and `herdr agent … --account/switch-account` command with exit
  codes and JSON shapes, the sidebar token snippet, the switch protocol and
  its guarantees, limitations (restore relaunches under the default
  profile; remote endpoints hide the TUI actions; `unverified` on macOS;
  cached remote manifests), the live-verification checklist for real Claude
  Code, and the fake-stub lab.
- `docs/fork/decisions/0003-claude-accounts-as-profile-dirs.md` (new ADR):
  decisions (a), (b), (e), (g) with the rejected alternatives (shared dir
  with swapped credentials; `agent.start` env field; client ledger).
- `docs/fork/README.md`: an *Accounts* section linking the guide;
  `docs/fork/ROADMAP.md`: E9 drift corrections only (e.g. the two-step
  launch, tokens instead of client-local metadata, `fork:accounts` source),
  never scope changes.
- `tests/fork_accounts.rs`: `docs_list_the_seed_layout` contract test.

**Real-server validation.** Walk `docs/fork/accounts.md` top to bottom
against the lab: every command in the guide runs as written and produces
the documented shape (paste the transcript into the PR).

**Downstream.** E7 links here for the phone-side switch; E4 reads the
token names.

## Critical files referenced (reuse, don't reinvent)

- `src/app/agents.rs:145` `start_agent` and `src/platform/mod.rs:297`
  `interactive_unix_shell_command` / `:319` `quote_powershell_arg` / `:328`
  `is_pane_shell_process_name` — the launch and shell-name facts E9 builds
  on (read, never modify `agents.rs`).
- `src/api/schema/agents.rs` `AgentStartParams`, `AgentInfo` (`tokens`,
  `state_labels`, `agent_session`), `src/api/schema/panes.rs`
  `PaneReportMetadataParams`, `PaneProcessInfo`, `PaneSendTextParams`;
  `src/app/api_helpers.rs:202-208` metadata limits and source grammar.
- `src/agent_resume.rs` (`plan`, `AgentSessionRef`, `is_official_agent_
  source`) and `src/integration/assets/claude/herdr-agent-state.sh` — the
  session-id path the stub imitates; `src/integration/env.rs`
  (`CLAUDE_CONFIG_DIR_ENV_VAR`, `expand_tilde_path`, `claude_dir`);
  `src/integration/targets.rs:121` `install_claude` (run as a child process
  with the profile env).
- `src/terminal/state.rs:505-520` metadata retention on exit
  (`applies_to_source`), `:2059` `clear_agent_name`.
- `src/detect/manifests/claude.toml`, `src/detect/manifest.rs` (rule schema,
  `explain_to_json_value`, override path, reload),
  `scripts/agent_detection_manifest_check.py`, `AGENTS.md` *Agent Detection
  Updates*.
- `src/config/model.rs:307-324`, `src/config/io.rs:7,536,410`,
  `src/config.rs:120`, `src/main.rs:67,406-434,895-1050`,
  `scripts/config_reference_check.py:42` — the config wiring pattern
  (fleet/gateway precedent).
- `src/cli.rs:97-136,755,779`, `src/cli/machine.rs`, `src/cli/fleet.rs`,
  `src/cli/agent.rs:289`, `src/cli/spec.rs:143,345,454,973` — CLI patterns.
- `src/client/shell/{state.rs:531-609,630, context_menu.rs, overlays.rs,
  overlay_input.rs, worktree_overlays.rs, agent_sidebar.rs:237-309,
  config.rs}` and `src/ui/sidebar/tokens.rs:89` (`Custom` token) — TUI
  patterns.
- `src/fleet/state.rs:234-319`, `src/fleet/report.rs:172-213`,
  `src/fleet/mod.rs::PURE_MODULES` — fleet report contract and the
  architecture-guard pattern to copy into `src/accounts/mod.rs`.
- `tests/cli/harness.rs`, `tests/cli/agents.rs:32-60` (fake-agent stub),
  `tests/support/fleet_lab.rs`, `tests/fork_gateway.rs` (fork test shape),
  `scripts/fork/fleet-lab.sh`, `scripts/fork/gate.sh`.
- Binding rules: `AGENTS.md` Universal Project Rules (state/runtime
  separation, platform isolation, multiplicative perf, runtime/client
  boundary, stable endpoint contract), Testing, Code Conventions, Agent
  Detection Updates; `.claude/rules/fork.md` (branches, gate, real-server
  validation, safety); roadmap principles 1, 4–7.

## End-to-end epic validation

Run by `implement-epic` after PRs 1–11 are ✅ and merged, from the root
checkout on `master` (`git -C <root> pull --ff-only`), with
`bash scripts/fork/gate.sh <root>` → `EXIT=0` first, then `cargo build`.

1. **Lab.** `bash scripts/fork/accounts-lab.sh up`; `eval "$(bash
   scripts/fork/accounts-lab.sh env)"`. `herdr --session accounts-lab
   account list --json` → `perso` (default) and `work`; `account status
   --json` → identities present, no secrets in the output (grep the JSON for
   `token`/`credential` → nothing).
2. **Add.** `account add third --json`; the directory shows the documented
   links/copies, no `.credentials.json`, hook installed; `account login
   third --pane $HERDR_ACCOUNTS_LAB_PANE` → `status third` `logged_in`.
3. **Start.** Two panes at a prompt: `agent start a1 --kind claude --pane
   <p1> --account work` and `agent start a2 --kind claude --pane <p2>` (default
   `perso`). `agent list --json` → `tokens.account` `work`/`perso`,
   `account_state` `ok` (Linux), `agent_session` set on both;
   `last-launch.json` in each profile names the right directory.
4. **See it.** Attach the TUI in the PTY harness with the documented
   `rows_by_agent` snippet → both rows show their account; `herdr fleet
   status --json` (with `[fleet]` pointing at `kind = "local", session =
   "accounts-lab"`) → `agents[].tokens.account` for both; if the E3 gateway
   is merged, `GET /api/fleet` shows the same.
5. **Switch.** `agent switch-account a1 perso --yes --json` → same
   `session_id`, `account_state ok`; the pane shows `/exit`, the new export
   line, `resumed <id>`; `agent get a1` → `tokens.account == "perso"`. Then
   switch `a2` from the TUI (context menu → picker → confirm) and assert the
   same through the API. A declined confirmation sends nothing.
6. **Limits.** Restart the stub in limit mode for `a1` (`FAKE_CLAUDE_LIMIT=1
   agent start …`); `agent explain a1 --json` → `usage_limit`; `account
   status --json` → `limit`; `account watch` running → `state_labels.blocked
   == "usage limit"` and `account_state == "limited"` within 5 s; the sidebar
   row reads `usage limit`; the switch preflight prints the hint. Nothing
   switched by itself (the token stays until the explicit command).
7. **Failure is local and safe.** Remove `profiles/work` and run
   `switch-account a2 work --yes` → exits 1 in preflight, nothing sent
   (`pane read` unchanged). Point a pane's shell at an unsupported name →
   `agent start --account work` refuses with the message and starts nothing.
8. **Teardown.** `accounts-lab.sh down`; `ls /tmp/herdr-accounts-lab` gone;
   `~/.claude`, `~/.claude.json`, `~/.config/herdr` untouched (compare
   `stat` mtimes captured before step 1).

**What must be verified live later** (a human with real Claude Code and a
rate-limited account; record results in `docs/fork/accounts.md` and the
plan's *As built* notes):

- The seed list of decision (a) against the installed Claude Code version:
  which of `projects/ todos/ skills/ plugins/ commands/ agents/ CLAUDE.md
  history.jsonl` exist and are safe to share; whether Claude Code rewrites
  `settings.json`/`.claude.json` atomically (which would replace a symlink —
  the plan copies them for that reason) and whether `--resume` finds shared
  transcripts through the `projects/` symlink.
- The `oauthAccount` key names used for email/plan display.
- `claude auth login` exists in the installed version (else the `/login`
  fallback) and creates `.credentials.json` in `CLAUDE_CONFIG_DIR`. **PR 3
  shipped the `auth login` spelling and validated it only against the stub**,
  which implements it: `herdr account login <name>` types
  `export CLAUDE_CONFIG_DIR='<dir>'` and then `claude auth login`. A human must
  run it once against a real installation and check three things — that the
  subcommand exists at all, that it writes `.credentials.json` (mode `0600`)
  into the exported directory and not into `~/.claude`, and that
  `herdr account status <name>` then shows that account's `oauthAccount` email.
  If `auth login` is gone, the fallback is `claude` then `/login` typed inside
  the session, which is a different shape (an interactive agent, not a command
  that exits): `login` would then type only the export line and tell the user
  to run `claude` and `/login`, or hand off to `herdr agent start --account`.
  Record which of the two the installed version needs before PR 11 documents
  it.
- `/exit` from `agent.prompt` exits cleanly from idle, blocked (after
  `Escape`) and the usage-limit screen; the resumed session reports
  `session_start_source = "resume"` with the same id through the hook.
- The `usage_limit` rule against the real screen (capture with `herdr agent
  read <pane> --source detection --format text` and `--format ansi`), the
  reset-time wording, and that permission prompts never match it.
- Whether a cached remote manifest shadows the fork's bundled rule on an
  installation that has run `herdr server update-agent-manifests`.
- macOS: the probe returns `unverified` (expected) and the export line works
  in the default `zsh`; Windows: the `pwsh` line and the cmd refusal.
