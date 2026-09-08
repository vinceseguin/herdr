# ADR 0003 — A Claude account is a config directory; herdr applies it client-side and records it as server metadata

**Status:** accepted (2026-09-07). **Applies to:** E9, and the account halves
of E4 (phone display) and E7 (phone switch action).

## Context

Claude Code's usage limits are per account. A developer holding two Max
licences can only use the second one today by logging out and back in inside
Claude Code — a global, manual act that loses track of which agent is on which
account. E9's goal is to pick an account per agent at start, see it everywhere,
and move a running agent to another account **without losing its conversation**.

Three facts about herdr and Claude Code set the boundaries:

- **An account already is a directory.** Claude Code reads `CLAUDE_CONFIG_DIR`
  (`src/integration/env.rs`); that directory holds `.credentials.json` (the
  OAuth token) and `.claude.json` (`oauthAccount`, project trust, MCP servers).
  Nothing else identifies the account.
- **herdr does not spawn agents.** `App::start_agent` (`src/app/agents.rs`)
  *types* `claude …` into the pane's already-running interactive shell.
  `AgentStartParams` has no `env` field and the server owns `argv[0]`, so a
  client cannot prefix the command it asks for.
- **Servers stay stock**
  ([ADR 0001](./0001-servers-stay-stock-ssh-transport.md)). E9 may not change `src/server/`,
  `src/app/`, `src/api/`, `src/protocol/`, `src/persist/` or
  `src/integration/`; a LAN host running upstream herdr has to work with every
  account command.

The roadmap left four decisions open — (a) profile layout, (b) how the
environment reaches the agent, (c) scope, (d) limit handling — and the plan
locked eight more while building. This ADR records the four that shape the
architecture: **(a)** layout, **(b)** launch mechanism, **(e)** where the
account fact lives, and **(g)** profile storage.

## Options considered

### (a) What a profile is

1. **One Claude directory, credentials swapped in and out.** A switch is a file
   move. But `CLAUDE_CONFIG_DIR` is process-wide state read at start: swapping
   the file moves *every* running Claude on the machine to the new account at
   once, which is the opposite of per-agent accounts. It also means herdr
   handling credential files.
2. **A directory per account, fully independent.** Correct isolation, but
   `--resume <id>` could not find a conversation started under the other
   account: transcripts live in `<config dir>/projects/`, so the switch would
   lose the conversation it exists to keep.
3. **A directory per account, sharing transcripts and machine-wide state by
   symlink.** Isolated credentials, shared conversations.

### (b) How `CLAUDE_CONFIG_DIR` reaches the launched agent

1. **A new optional `env`/`account` field on `agent.start`.** Small and
   obvious — and forbidden by the stable client endpoint contract for exactly
   the reason it is tempting: an older server would *ignore* the field and
   report success, launching the agent under the wrong account while telling
   the client it worked. Making it safe means a new advertised method, i.e. a
   server change, i.e. every LAN host needs the fork.
2. **Prefix the typed command** (`CLAUDE_CONFIG_DIR=… claude …`). The server
   owns `argv[0]`; a client cannot do this without a server change either.
3. **Two steps, both stock.** Type an environment assignment into the pane's
   shell with `pane.send_text`, then call the stock `agent.start`, whose typed
   `claude` inherits it.

### (e) Where "this agent runs under account X" is recorded

1. **A client-side ledger** keyed by the Claude session id, persisted by the
   fork. But after a server restart `start_pending_agent_resume` relaunches the
   agent with `pane_launch_env(…, Vec::new())` — under the *default* profile —
   and no client is involved. The ledger would then confidently show the wrong
   account, which is worse than showing none.
2. **Server metadata tokens.** `pane.report_metadata` already exists on stock
   servers, is per pane, is scoped to the agent integration, is exposed on
   `agent.get`/`agent.list`, crosses the wire as `ClientShellAgent.tokens`, and
   the sidebar already renders any token as `$name`.

### (g) Where profiles are declared

1. **`config.toml` only.** Declarative and reviewable, but `herdr account add`
   would have to rewrite the user's config file — which herdr never does.
2. **A herdr-managed store only.** Writable, but not reviewable and not
   shareable with a dotfiles repo.
3. **Both, merged.**

## Decision

**(a) A profile is a `CLAUDE_CONFIG_DIR` of its own, seeded from an existing
one: transcripts and machine-wide state shared by symlink, settings and
identity copied and scrubbed, credentials never touched.** The three lists live
in one place, `src/accounts/layout.rs` (`SHARED_ENTRIES`, `COPIED_ENTRIES`,
`PRIVATE_ENTRIES`), and a doc-contract test keeps `docs/fork/accounts.md` in
step with them. `settings.json` and `.claude.json` are *copied*, not shared,
because Claude Code rewrites them atomically and would replace a symlink with a
regular file — silently rejoining the two profiles. `.claude.json` is copied
only after `oauthAccount` and any top-level key containing `apikey`, `token`,
`credential` or `secret` is removed. `.credentials.json` is never read, copied,
linked, printed or logged; a new profile is logged out until
`herdr account login`.

**(b) The launch is client-side and two-step, and an unknown shell is a hard
error.** The pane's shell is identified from `pane.process_info` and classified
by `ShellFamily::from_process_name`; the assignment is written in that shell's
own syntax (eight families, golden strings in `src/accounts/launch.rs`) with a
leading space so `HISTCONTROL=ignorespace` shells skip it, and a directory the
shell cannot quote safely is refused rather than approximated. Option 1 is
rejected on the endpoint contract; option 2 is impossible without a server
change.

**(e) The account is a metadata token on the server, and never claimed without
evidence.** The launcher and the switch driver report
`tokens.account = <profile>` and `tokens.account_state ∈ {ok, unverified,
mismatch, limited, logged_out}` with `source = "fork:accounts"`,
`applies_to_source = "herdr:claude"` and no TTL. `ok` requires reading the
launched process's own environment (`/proc/<pid>/environ` on Linux); a platform
that cannot read it reports `unverified`, and an environment naming a different
directory — or naming none — reports `mismatch` and exits non-zero.
`logged_out` is reserved vocabulary: nothing writes it today (a logged-out
profile is a launch warning and a switch refusal), and it is in the enum so a
reader handles it when a producer appears. `limited` is `herdr account watch`'s,
and the only one written with a lease. No client ledger exists.

**(g) `[[accounts]]` in `config.toml` and a CLI-managed
`<config>/accounts/profiles.toml`, merged.** herdr never rewrites
`config.toml`; `herdr account add|remove|default` write the store only. Config
profiles come first and win a name collision (with a diagnostic); the store's
`default` key beats a `default = true` flag. `herdr account add --print-config`
prints the `[[accounts]]` block instead of writing the store, for people who
keep their config in a dotfiles repo.

Two further locks follow from the roadmap and are recorded here because later
epics depend on them:

- **(c) Claude only in v1.** `agent = "claude"` is the only accepted value;
  `AccountAgent` and the config schema are append-only so `codex`
  (`CODEX_HOME`) can follow without a rename.
- **(d) Detect and suggest, never auto-switch.** The `usage_limit` detection
  rule marks the agent `blocked`; `herdr account status` prints a hint and the
  opt-in `herdr account watch` labels the sidebar. `WatchAction` has no variant
  that types, prompts, switches or kills — the decision is enforced by the
  type, not promised in prose. Every switch, CLI or TUI, is confirmed.

## Consequences

- **Stock servers work.** Every account operation composes existing methods —
  `pane.process_info`, `pane.send_text`, `agent.start`, `agent.get`,
  `agent.list`, `agent.prompt`, `agent.send_keys`, `agent.explain`,
  `pane.report_metadata`. No wire, protocol or endpoint change; `PROTOCOL_VERSION`
  is untouched and no endpoint fixture moved. A LAN host running upstream herdr
  accepts every one of them.
- **The fact travels for free.** Because the account is a metadata token,
  `herdr agent list`, the sidebar (`$account`, `$account_state`), the fleet
  report (E9 PR 6) and therefore the gateway's `/api/fleet` and `/api/events`
  (E3) all carry it with no further work. **E4 reads
  `agents[].tokens.account` and `agents[].tokens.account_state`; E7's phone
  switch action calls the same stock methods PR 5 uses, routed per host.**
  Those four strings — `account`, `account_state`, `fork:accounts`,
  `herdr:claude` — are contract: add, never rename.
- **Tokens are in-memory.** `PaneSnapshot` has no tokens field, so a server
  restart drops them; and restore relaunches a resumed agent under the
  *default* profile, env-less, because that path is server-side. herdr shows no
  account rather than a wrong one, and `herdr agent switch-account` is how a
  human puts it back.
- **`unverified` is a real answer on macOS and Windows.** The probe is
  `#[cfg(target_os = "linux")]` (`crate::platform::process_env_var`); elsewhere
  the launch is applied but not proven, and the surfaces say so rather than
  claiming `ok`.
- **A shell herdr cannot write an assignment for cannot start an agent under an
  account.** That is deliberate: the alternative to a hard error is a silent
  launch under the ambient account. `--account none` remains the stock,
  account-less launch.
- **Two spellings of a path are one directory.** Because a second profile
  pointing at the same directory would be a second "account" sharing one
  login, every check that guards a write — `herdr account add`'s clash check,
  the seed's containment guards, `remove --delete-dir` — compares both the
  lexical key (`config::dir_key`) and the resolved key (`layout::resolved_key`,
  longest existing prefix canonicalized). The config/store merge folds the
  spelling only: it runs before anything is written and must not stat the
  filesystem to answer. Reuse both keys; do not re-derive them.
- **The seed list is a bet on Claude Code's file layout** and is the one part
  of this ADR a human must confirm against a real installation — which entries
  exist, whether `--resume` finds transcripts through the `projects/` symlink,
  and whether `claude auth login` still exists. The checklist lives in
  [`../accounts.md`](../accounts.md).
- **New code is additive.** `src/accounts/` (twelve modules, ten of them pure
  and guarded by a `PURE_MODULES` architecture test), `src/cli/account.rs`, two
  fork files under `src/client/shell/`, and the process-environment reader
  appended to `src/platform/mod.rs` (`ProcessEnvVar`, `process_env_var` in its
  two `cfg` forms, `parse_environ_blob` and their tests). Upstream wiring is a
  handful of adjacent lines per file and is enumerated in the sync-policy table
  of [ADR 0002](./0002-adopt-upstream-multi-machine-client.md).
