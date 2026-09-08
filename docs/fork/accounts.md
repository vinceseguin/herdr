# Claude account profiles

Usage limits are per Claude account. If you hold two licences, herdr can run
one agent under each, show you which agent is on which account, and move a
running agent to the other account **without losing its conversation**.

This is epic E9 of the fork. The architecture and the rejected alternatives are
in [ADR 0003](./decisions/0003-claude-accounts-as-profile-dirs.md); this page is
the user guide.

- [What an account is](#what-an-account-is)
- [Quick start](#quick-start)
- [Declaring profiles](#declaring-profiles)
- [The profile directory](#the-profile-directory)
- [`herdr account` reference](#herdr-account-reference)
- [Starting an agent under an account](#starting-an-agent-under-an-account)
- [Seeing which account an agent uses](#seeing-which-account-an-agent-uses)
- [Switching a running agent](#switching-a-running-agent)
- [In the TUI](#in-the-tui)
- [Usage limits](#usage-limits)
- [Limitations and known gaps](#limitations-and-known-gaps)
- [What a human must still verify against real Claude Code](#what-a-human-must-still-verify-against-real-claude-code)
- [The accounts lab](#the-accounts-lab)

## What an account is

An account is **one Claude config directory** — the directory Claude Code reads
from `CLAUDE_CONFIG_DIR`. It holds `.credentials.json` (that account's OAuth
token) and `.claude.json` (`oauthAccount`, project trust, MCP servers). Nothing
else identifies the account.

A *profile* is herdr's name for one of those directories plus a short name you
use on the command line. Profiles are **per host**: credentials never leave the
machine the agent runs on, and herdr never reads, copies, prints or logs a
credentials file — only whether it exists and what mode it has.

herdr does not spawn agents; it types into a pane's shell. So starting Claude
under a profile is two steps, both of which work against a **stock, upstream
herdr server**:

1. type ` export CLAUDE_CONFIG_DIR='<dir>'` into the pane's shell
   (`pane.send_text`), in that shell's own syntax;
2. call the ordinary `agent.start`, whose typed `claude` inherits it.

No server, protocol or endpoint change is involved, so a LAN machine running
upstream herdr accepts every command on this page.

## Quick start

This runs against your own Claude directory and your own herdr server. To try
the same commands without touching either, use [the accounts lab](#the-accounts-lab).

```bash
# 1. declare the account you already use as `perso` — a [[accounts]] block in
#    config.toml, see below — and add a second one
herdr account add work --config-dir ~/.claude-work
herdr account login work                 # types `claude auth login` into a pane
herdr account status                     # who each profile is logged in as

# 2. start agents under a chosen account
herdr agent start a1 --kind claude --pane w1:p1 --account work
herdr agent start a2 --kind claude --pane w1:p2   # the default profile

# 3. see it
herdr agent list | jq '.result.agents[] | {name, tokens}'   # always JSON
herdr fleet status                       # an ACCOUNT column appears

# 4. move a running agent, keeping its conversation
herdr agent switch-account a1 perso
```

## Declaring profiles

There are two sources and they are merged.

**`config.toml`** — declarative, reviewable, safe to keep in a dotfiles repo.
herdr never rewrites this file.

```toml
[[accounts]]
name = "perso"              # display name and id: letters, digits . _ - (max 32)
agent = "claude"            # only "claude" in v1
config_dir = "~/.claude"    # this profile's CLAUDE_CONFIG_DIR
default = true              # used when an agent starts without a choice

[[accounts]]
name = "work"
agent = "claude"
config_dir = "~/.claude-work"
```

`herdr --default-config` prints the same block, commented out.

**`<config>/accounts/profiles.toml`** — written by `herdr account add`,
`remove` and `default`. You normally never edit it:

```toml
version = 1
default = "perso"

[[profiles]]
name = "third"
agent = "claude"
config_dir = "/home/you/.claude-third"
default = false
```

### Merge and validation rules

- Config profiles come first, then store profiles. A name defined in both is a
  **diagnostic** and the `config.toml` entry wins.
- The store's `default = "<name>"` key beats a `default = true` flag in
  `config.toml`, and may name a `[[accounts]]` profile — that is how
  `herdr account default` chooses without rewriting your config.
- More than one `default = true` is a diagnostic; the first wins.
- Names must match `[A-Za-z0-9._-]{1,32}` and may not be `.`, `..`, `none`
  (reserved, because `--account none` opts out) or start with `-` (the name
  becomes a directory component and a CLI argument).
- `config_dir` is tilde-expanded and must be absolute afterwards. Two profiles
  may not share a directory. The merge folds the *spelling*, so `~/.claude`,
  `~/.claude/` and `~/./.claude` are one directory and the second entry is a
  diagnostic; seeing through a **symlink** costs a filesystem call, so that is
  done where it protects a write — `herdr account add`'s clash check and the
  seed's own guards — not in the merge.
- `[accounts.defaults]` is **reserved** and rejected with a diagnostic:
  per-workspace and per-host defaults are a follow-up.
- Nothing here is a parse failure. A malformed `accounts` key produces a
  diagnostic and an empty section; it never drops the rest of your config.

### Which profile is used when you do not say

1. `--account <name>` on the command line;
2. the profile marked default;
3. if exactly one profile exists, that one;
4. otherwise none — the launch is byte-for-byte a stock herdr launch and no
   account is claimed.

`--account none` opts out explicitly. Typing `claude` by hand in a pane is
never touched by any of this.

## The profile directory

`herdr account add` seeds a new profile from an existing one. Three lists
decide what happens to each entry; they live in `src/accounts/layout.rs` and a
test keeps this table in step with them.

| Entry | Treatment | Why |
| --- | --- | --- |
| `projects` | **shared** (symlink) | transcripts — `--resume` must find a conversation started under the other account |
| `todos` | shared | machine-wide state, not account state |
| `skills` | shared | |
| `plugins` | shared | |
| `commands` | shared | |
| `agents` | shared | |
| `CLAUDE.md` | shared | |
| `history.jsonl` | shared | |
| `settings.json` | **copied** | Claude Code rewrites it atomically, which would replace a symlink with a regular file and silently rejoin the two profiles |
| `.claude.json` | **copied, scrubbed** | carries onboarding, project trust and MCP settings — worth having; carries identity — must not be shared |
| `.credentials.json` | **never touched** | the new profile is logged out until `herdr account login` |
| `statsig` | never touched | Claude recreates it |
| `shell-snapshots` | never touched | |
| `debug` | never touched | |
| `cache` | never touched | |
| `ide` | never touched | |

Anything not listed is left for Claude Code to recreate.

**Scrubbing.** The copied `.claude.json` loses its top-level `oauthAccount`
key and any top-level key whose name contains `apikey`, `token`, `credential`
or `secret` (case-insensitive). Scrubbing is top-level only on purpose: a
nested MCP server's own credentials are yours, not the account's, and dropping
them would break the settings the copy exists to carry over. A `.claude.json`
herdr cannot scrub — not JSON, not an object, too large — is **not copied at
all**; you get a warning, never a passthrough.

**Safety rails.** The seed refuses a source that is not a directory, the same
directory as the target, a target nested in the source or vice versa, a
symlinked target, a shared entry that is itself a credentials directory or
contains another profile, and any plan that names a credentials file on either
side of a link or a copy. Existing entries in the target are left exactly as
they are and reported as warnings. On unix the directory is created `0700` and
copied files `0600`; Windows gets the inherited permissions. `--force` only
relaxes the refusal to seed into a non-empty directory; it never relaxes the
refusal to seed into one that already holds `.credentials.json`.

**The hook.** After seeding, `herdr account add` runs
`herdr integration install claude` as a child process with
`CLAUDE_CONFIG_DIR` set to the new directory, so the profile installs herdr's
`SessionStart` hook (`herdr-agent-state.sh` — `herdr-agent-state.ps1` on
Windows — under the profile's `hooks` directory, plus an entry in
`settings.json`). Without that hook Claude never reports its session id — the
profile can still be launched, but `herdr agent switch-account` cannot keep the
conversation. A failed install is a warning naming the command to rerun, not a
rollback; `--no-hook` skips it and warns for the same reason.

## `herdr account` reference

```
herdr account list [--json]
herdr account add <name> [--config-dir <path>] [--from <profile>]
                         [--dry-run] [--force] [--print-config]
                         [--no-hook] [--json]
herdr account remove <name> [--delete-dir]
herdr account default <name>
herdr account status [<name>] [--json]
herdr account login <name> [--pane <id>]
herdr account watch [--interval <ms>] [--once] [--json] [--keep-labels]
```

**Exit codes** are the same everywhere: `0` a report or a completed operation,
`1` a read-only report that found configuration diagnostics or an operation
that failed, `2` a usage error. A completed operation that degraded — an entry
the platform could not share, a hook that did not install — still exits `0` and
says so on stderr, because the profile it just created is real and usable.

`list`, `add`, `remove` and `default` contact **no server**. `status`, `login`
and `watch` talk to the local herdr server.

### `list`

```console
$ herdr account list
perso	claude	/tmp/herdr-acct-pr11/profiles/perso	default	config	dir	logged-in	no-hook
work	claude	/tmp/herdr-acct-pr11/profiles/work	-	config	dir	logged-in	no-hook
```

```console
$ herdr account list --json
[
  {
    "name": "perso",
    "agent": "claude",
    "config_dir": "/tmp/herdr-acct-pr11/profiles/perso",
    "default": true,
    "origin": "config",
    "dir_exists": true,
    "logged_in": true,
    "hook_installed": false
  },
  …
]
```

`origin` is `config` or `store` — plus `unregistered` in `add`'s own output,
for a profile that was created but deliberately not written to the store
(`--print-config`). `logged_in` means a credentials file is present — its
contents are never read.

### `add`

`--dry-run` prints the plan and writes nothing:

```console
$ herdr account add third --config-dir /tmp/herdr-acct-pr11/profiles/third --dry-run
would create /tmp/herdr-acct-pr11/profiles/third (0700), seeded from /tmp/herdr-acct-pr11/profiles/perso
  link  projects -> /tmp/herdr-acct-pr11/profiles/perso/projects
  copy  .claude.json (identity keys removed)
  skip  todos: absent from the source profile
  …
  skip  .credentials.json: private to each account; never copied
```

Without `--dry-run`, `--json` reports what happened:

```console
$ herdr account add third --config-dir /tmp/herdr-acct-pr11/profiles/third --json
{
  "profile": { "name": "third", "agent": "claude", "config_dir": "…/third",
               "default": false, "origin": "store", "dir_exists": true,
               "logged_in": false, "hook_installed": true },
  "seeded_from": "/tmp/herdr-acct-pr11/profiles/perso",
  "dry_run": false,
  "created": true,
  "linked":   ["/tmp/herdr-acct-pr11/profiles/third/projects"],
  "copied":   ["/tmp/herdr-acct-pr11/profiles/third/.claude.json"],
  "scrubbed": ["/tmp/herdr-acct-pr11/profiles/third/.claude.json"],
  "hook_installed": true,
  "stored": true,
  "warnings": []
}
```

The default `--config-dir` is **`~/.claude-<name>` in your home directory**, so
scripts and tests that want a profile somewhere else must pass `--config-dir`
explicitly. The seed source is `--from`, else the default profile when its
directory exists, else the ambient Claude directory (`CLAUDE_CONFIG_DIR` when
set, otherwise `~/.claude`); when none of the three exists, `add` fails and
names `--from`.

`--print-config` prints the `[[accounts]]` block for you to paste into
`config.toml` instead of writing the store. It is not a dry run — the
directory is still created, seeded and hooked — and the block is a text-mode
courtesy: `--print-config --json` writes no store entry and prints no block.

### `remove` and `default`

```console
$ herdr account remove perso
account profile "perso" is declared as [[accounts]] in config.toml; remove the block there instead
$ echo $?
1
```

`--delete-dir` also deletes the directory, refusing a symlink, a
non-directory, a filesystem root, a home directory, the ambient Claude
directory, and a directory another profile also uses or lives inside. Each of
those comparisons is made on both the lexical and the resolved path.

```console
$ herdr account default third
default account profile is now third
```

`default` accepts a `[[accounts]]` name too — the choice is recorded in the
store, never in your `config.toml`.

### `status`

```console
$ herdr account status
perso (config, default)
  dir          /tmp/herdr-acct-pr11/profiles/perso
  identity     perso@example.test · Example Org · max
  logged in    yes
  hook         missing (session ids are not reported; switching accounts will not work)
  agents       none
```

`--json` prints an array with one object per profile, each a flat superset of
`list`'s row:

```json
[{
  "name": "work", "agent": "claude", "config_dir": "…/work",
  "default": false, "origin": "config",
  "dir_exists": true, "logged_in": true, "credentials_mode_ok": true,
  "identity": { "email": "work@example.test", "organization": "Example Org", "plan": "max" },
  "hook_installed": false,
  "broken_links": [],
  "agents": [
    { "pane_id": "w1:p2", "name": "a1", "agent_status": "idle",
      "account_state": "ok", "account_state_known": true }
  ]
}]
```

- `identity` comes from `oauthAccount` in `.claude.json` — the same thing
  Claude Code shows in its own status line. Each field is capped at 120
  characters and refused if it carries control, bidi or zero-width characters:
  it is not herdr's file. The key is omitted entirely when nothing readable is
  there, as is `credentials_mode_ok` off unix or with no credentials file.
- `agents` is `null` when **no server answered** and `[]` when the server has
  none. They are different answers, so the distinction survives into JSON; the
  text report renders them `- (no herdr server answered)` and `none`.
  `jq '.[] | .agents[]? | …'` — the `?` matters.
- `account_state` is carried **verbatim**, with `account_state_known: false`
  when this build does not recognise the value. A newer herdr may report a
  state this one has never heard of; dropping it would hide the agent's state.
- `status` **never fails because of the server**. No server or an API error
  degrade to `agents: null` plus a `note:` on stderr; a protocol mismatch
  degrades the same way but says nothing extra, because the shared mismatch
  guard has already printed it. All three exit 0. Configuration diagnostics —
  or a profile name no profile defines — make it 1.
- An agent claiming a profile that no longer exists is reported as a warning
  on stderr rather than silently dropped.

### `login`

```console
$ herdr account login third --pane w1:p1
typed into pane w1:p1:
  export CLAUDE_CONFIG_DIR='/tmp/herdr-acct-pr11/profiles/third'
  claude auth login
Follow the login in pane w1:p1, then run `herdr account status third`.
```

`login` types; it does not complete the login for you, and it never starts a
managed agent. It refuses — typing **nothing** — when the profile is unknown,
its directory does not exist (run `herdr account add` first), the pane is not
idle at its own shell prompt, or the shell is one herdr cannot write an
assignment for. Between the two lines it waits (up to 5 s) for the prompt to
come back; if it does not, the login command is not typed either and the
message names the exported variable and the command to run by hand.

Warnings about an existing credentials file ("completing this login replaces
them") and about a missing hook are printed **before** anything is typed.

### `watch`

See [Usage limits](#usage-limits).

## Starting an agent under an account

```
herdr agent start <name> --kind claude --pane <pane> --account <profile>
```

The response is the stock `agent_started` result plus two keys:

```console
$ herdr agent start a1 --kind claude --pane w1:p2 --account work
{"id":"cli:agent:start","result":{"account":"work","account_state":"ok","agent":{…},"argv":["claude"],"type":"agent_started"}}
```

### `account_state`: how much evidence there is

| Value | Meaning |
| --- | --- |
| `ok` | the launched process's own environment was read and names the profile's directory |
| `unverified` | the launch applied the profile, but no environment could be read — every non-Linux platform, and Linux when the pane's process list is not readable |
| `mismatch` | an environment was read and names a **different** directory, or none at all |
| `limited` | the agent is blocked on this account's usage limit (written by `herdr account watch`) |
| `logged_out` | reserved: a profile with no credentials file. Nothing writes it today — a logged-out profile is a launch *warning* and a switch *refusal* instead — but readers must handle it |

Nothing is reported `ok` without evidence. The probe reads
`/proc/<pid>/environ` for the launched job's processes, excluding the pane
shell's own pid — a shell's `/proc` environment is its exec-time one and never
shows the line just typed, so including it would hide exactly the failure the
probe is for. A `mismatch` still writes the token (so every surface shows it)
and exits **1**, naming the directory it found — or saying the process has no
`CLAUDE_CONFIG_DIR` at all, which is the other way to earn a `mismatch`.

### The shells herdr can write an assignment for

| Family | Shells | Line |
| --- | --- | --- |
| posix | `sh bash dash zsh ksh mksh` | ` export CLAUDE_CONFIG_DIR='/p/work'` |
| fish | `fish` | ` set -gx CLAUDE_CONFIG_DIR '/p/work'` |
| csh | `csh tcsh` | ` setenv CLAUDE_CONFIG_DIR '/p/work'` |
| powershell | `pwsh powershell` | ` $env:CLAUDE_CONFIG_DIR = '/p/work'` |
| nu | `nu` | ` $env.CLAUDE_CONFIG_DIR = '/p/work'` |
| elvish | `elvish` | ` set-env CLAUDE_CONFIG_DIR '/p/work'` |
| xonsh | `xonsh` | ` $CLAUDE_CONFIG_DIR = '/p/work'` |
| cmd | `cmd` | ` set "CLAUDE_CONFIG_DIR=/p/work"` |

Every line begins with a space so shells configured with
`HISTCONTROL=ignorespace` / `HIST_IGNORE_SPACE` keep it out of history.

An **unrecognised shell is a hard error**: the alternative is a silent launch
under the ambient account. So is a directory the shell cannot quote safely —
a `'` in csh, nu or xonsh, a `\` in xonsh, a `"`/`%`/`!`/`^`/`&`/`<`/`>`/`|`
in cmd, and `!` in csh or tcsh (which history-expands it *inside* single
quotes, so the assignment would silently never happen). An empty value, any
control character and a path that is not UTF-8 are refused for every family.

### Refusals

```console
$ herdr agent start bad --kind claude --pane w1:p3 --account nope
unknown account profile "nope"; configured profiles: perso, work, third
$ echo $?
2
```

The launcher also refuses a profile whose directory is missing, and a pane that
is not idle at its own shell prompt — in every case before a byte is typed. A
profile with no hook installed is a **warning**, never a refusal.

### `--account none`

`--account none` is the stock launch, which means it does **not undo** a
previous one: nothing is typed and no token is reported, so a pane whose shell
still exports `CLAUDE_CONFIG_DIR` from an earlier `--account` start launches
under *that* directory with no account claimed. This is stated in
`herdr agent start --help`.

## Seeing which account an agent uses

The account is a **metadata token on the server**, reported by the launcher and
the switch driver with `source = "fork:accounts"`,
`applies_to_source = "herdr:claude"` and no TTL:

```console
$ herdr agent get a1
… "tokens": { "account": "work", "account_state": "ok" } …
```

Because it is an ordinary token it travels everywhere for free:

**The sidebar.** Any token renders as `$name`. Put this in `config.toml`:

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["agent", "$account", "$account_state"]]
```

**`herdr fleet status`** grows an `ACCOUNT` column when any agent reports one:

```console
$ herdr fleet status
AGENT      STATUS   WORKSPACE     NAME  ACCOUNT
lab/w1:p2  blocked  accounts-lab  a1    work
```

```console
$ herdr fleet status --json | jq '.agents[] | {host, name, tokens, state_labels}'
{ "host": "lab", "name": "a1",
  "tokens": { "account": "work" },
  "state_labels": {} }
```

`--watch --json` emits a change of kind `agent_metadata` when a token changes
(plain `--watch` prints it as `agent * <pane> account=work`). A metadata edit
deliberately does **not** advance `fleet_change_seq`: recency means "the agent's
state advanced", and a launcher writing a token must not re-order the list.

**The gateway (E3).** `/api/fleet` serializes the same report and `/api/events`
forwards the same change kinds, so the phone path carries `tokens.account`
with zero gateway changes. A reducer must skip a `kind` it does not know.

## Switching a running agent

```
herdr agent switch-account <target> <account> [-y|--yes] [--interrupt]
                                              [--force] [--timeout MS] [--json]
```

`<target>` is a pane id or a managed agent name.

**The protocol.** Preflight → confirmation → *recheck* → `Escape` (when the
agent is blocked, or working under `--interrupt`) and a 300 ms settle →
submit `/exit` → wait for the pane's own shell to come back → apply the new
profile's export line and start `claude --resume <id>` under the same agent
name → wait for the hook to report the **same** session id → probe the
environment → write the tokens.

**The agent is never killed.** On a timeout waiting for the exit, nothing
further is sent and the agent keeps running with its conversation.

```console
$ herdr agent switch-account a1 perso --yes --json
{"account_state":"ok","from":"work","limit":null,"name":"a1","pane_id":"w1:p2","session_id":"fake-…","to":"perso"}

$ herdr agent switch-account a1 work --yes
switched a1 in pane w1:p2 from perso to work (ok), same session fake-…
```

The pane shows the whole protocol:

```
❯ /exit
fake-claude: exiting

…/work ❯  export CLAUDE_CONFIG_DIR='/tmp/herdr-acct-pr11/profiles/perso'
…/work ❯ claude --resume fake-3787453-1788839212
CLAUDE_CONFIG_DIR=/tmp/herdr-acct-pr11/profiles/perso
resumed fake-3787453-1788839212
```

**Exit codes are the contract that matters when this fails.**

| Code | Meaning |
| --- | --- |
| `0` | the agent runs under the new profile with the same session id |
| `2` | **refused before a byte reached the pane** — the agent is exactly as it was |
| `1` | the protocol had started; the message says what the pane holds |

A graded `mismatch` is a *result*, not a refusal: the token is written so every
surface shows it, and the command exits 1.

**The refusals, in order** (all exit 2, nothing sent). The CLI checks itself
first — a `--timeout` outside 1..600 000, the literal account `none`, and an
account name no profile defines — and none of those even reads the agent.
Then the preflight reads it once: nothing managed at the target → not a Claude
agent → no managed name (`agent.start` needs one; give it one with
`herdr agent rename <pane> <name>`) → no reported session id → a session from
another integration → a session id herdr's own resume planner will not turn
into argv → the target profile's directory is missing → the target is logged
out (unless `--force`) → the agent already claims that account (unless
`--force`) → the agent is `working` without `--interrupt`.

```console
$ herdr agent switch-account a3 third --yes
error: account profile "third" points at /tmp/…/profiles/third, which does not exist; nothing was sent
$ echo $?
2
```

**Guarantees.**

- The pane is **pinned** at preflight (pane id + terminal id) and every key,
  prompt, poll and launch afterwards addresses it. A pane whose identity
  changed while the confirmation was on screen is refused (`AgentChanged`)
  with nothing sent; one that changes after Claude has exited fails with
  `PaneReplaced` instead of relaunching into a stranger's shell.
- The agent is read **once more after the confirmation** (`Recheck`): a human
  can take minutes over `[y/N]`, and an agent that was idle then could be
  mid-tool-call now. A changed name, terminal or session id, or a now-working
  agent without `--interrupt`, is refused with nothing sent.
- `AwaitShell` needs **two** independent facts: the Claude process is gone from
  the pane's foreground, *and* the server reports `agent_not_found` for the
  pane (it has released the terminal). Waiting on only the first would
  relaunch into a pane the server still calls busy — after the export line had
  already been typed. (A pane whose terminal id the server will not report is
  simply not ready yet, and one that reports a *different* terminal id is the
  `PaneReplaced` above.)
- A resume that comes back with a **different** session id fails immediately
  with `SessionMismatch`: the resume did not take, and the original
  conversation is still on disk.
- Every failure after the relaunch still grades and records the relaunched
  agent — it really is running under the new profile — and says so as a
  warning. The one exception is `PaneReplaced`: there is nothing of ours left
  in that pane to grade, and claiming an account for a shell that runs nothing
  would be worse than claiming none. Every failure that leaves the pane at a
  shell names `claude --resume <id>` so you can finish by hand.
- With no terminal to ask and no `--yes`, the answer is "there is nobody to
  ask", never an assumed yes.
- `--timeout` (default 20 000 ms, max 600 000) is the budget for **each**
  waiting stage. It is also given to the relaunch's readiness wait when the
  server would accept it — over 3 s and at most 300 s, the server's own bounds
  — so at the default one flag governs the whole command; outside that window
  the relaunch keeps the server's default and only herdr's own waits move.

## In the TUI

Right-click a pane. The context menu carries at most one of two items:

- **`Start Claude as account...`** — when the pane has **no** agent, at least
  one profile is configured, and the active endpoint is the local server. It
  opens a picker (arrow keys or the wheel, `/` to focus the filter and then
  type, Enter, Esc) showing each profile's health — `default · no hook`, the
  worst problem only — and launches through exactly the same code path as the
  CLI, with progress lines in the modal.
- **`Switch Claude account...`** — when the pane's agent is `claude` and at
  least **two** profiles are configured. Picker → a mandatory red confirm
  panel carrying the protocol's own wording verbatim → the same switch machine
  on a background thread.

Both items are hidden while the client is showing a **remote endpoint** — a
machine added with `herdr machine add` — because v1 drives the account from the
machine the agent runs on.

Details worth knowing:

- The picker shows health only (`dir_exists`, `logged_in`, `hook_installed`,
  `is_default`) — stat calls. It deliberately does not parse `.claude.json` for
  the account's email, which is megabytes on a real installation, while you
  hold a mouse button down. `herdr account status` is where identity is shown.
- A single click on a row submits it, and that row is preflighted again at that
  moment: a picker left open while the pane, the agent's name or the profile
  changed underneath it refuses rather than acting on what it drew.
- `i` on the picker screen toggles "interrupt if working". It is a picker knob,
  not a confirm knob, because a working agent is refused *before* the question
  is ever asked. (While the filter has focus, `i` types an `i`.)
- The confirmation ignores a *yes* for the first 500 ms so a key repeat or the
  second half of a double-click cannot answer it before you have read it. A
  *no* is never delayed.
- A refusal that reached **nothing** leaves the picker armed for another row;
  anything that reached the pane settles the modal. The modal has one status
  line and a failure wins it, so what you read there is the failure itself; the
  warnings and the `claude --resume <id>` recovery hint that came with it are
  in the CLI's report, and `herdr agent get <pane>` is how you see where the
  pane actually ended up. A success that carried a warning — a graded
  `mismatch`, say — also keeps the modal open; only a clean one closes itself.
- The modal cannot be dismissed while a job runs, and a worker that has been
  silent for 90 s is detached with a message saying the launch may still
  finish. That budget is suspended while a question is on screen — a worker
  waiting on a *person* is not stalled — and its clock restarts when the
  question is answered.
- `--force` is never used from the TUI: overriding a logged-out target or a
  no-op switch is a deliberate command-line act.

## Usage limits

`src/detect/manifests/claude.toml` carries a `usage_limit` rule (priority 985,
region `after_last_horizontal_rule`, state `blocked`). It reads the live status
area **below** Claude's prompt box, not the bottom N lines of the buffer: a
notice that has scrolled above the box is history, not state — and reading it
as state left a freshly *relaunched* agent looking limited, which broke the
switch this epic exists for.

`herdr agent explain <pane> --json` shows what matched:

```console
$ herdr agent explain a1 --json | jq '{matched_rule, manifest_source, manifest_version, cached_remote_version}'
{
  "matched_rule": { "id": "usage_limit", "priority": 985,
                    "region": "after_last_horizontal_rule", "state": "blocked" },
  "manifest_source": "bundled",
  "manifest_version": "2026.09.07.1",
  "cached_remote_version": null
}
```

**`herdr account status` surfaces it.** It asks the detector only about a
*blocked Claude agent* — never one explain per pane — and prints the hint on
stderr so `--json` stays a clean document:

```console
$ herdr account status work
…
$ herdr account status work 2>&1 >/dev/null
a1: usage limit on account "work" (resets 3pm); switch with `herdr agent switch-account w1:p2 perso`
```

```console
$ herdr account status work --json | jq '.[].agents[]? | {name, agent_status, limit}'
{ "name": "a1", "agent_status": "blocked", "limit": { "reset_text": "3pm" } }
```

An **absent** `limit` key means "nobody asked", never "not limited".

**`herdr agent switch-account`** carries the same fact into its confirmation
text ("herdr sees a usage limit on `<account>`, which resets `<time>`." — the
account named is the one the agent currently claims, and the reset clause is
dropped when the screen showed no reset time) and into `--json` as a `limit`
key. It also has a special exit path for limits: a
blocked agent makes the stock server refuse `agent.prompt`, and `Escape`
cannot clear a usage limit, so the one command that gets you *out* of a limit
would have refused every limited agent. When — and only when — the refusal code
is `agent_blocked` **and** a usage limit was seen, `/exit` is typed with
`pane.send_text` instead, once, after the pinned pane identity is
re-established.

### `herdr account watch`

An **opt-in** helper that keeps the limit visible on stock servers.

```console
$ herdr account watch --once --json
{"account":"work","event":"limited","name":"a1","pane_id":"w1:p2","reset_text":"3pm"}

$ herdr account watch --once
limited a1 (w1:p2) account=work resets 3pm
```

While it runs, a limited agent carries:

```json
{
  "tokens":       { "account": "work", "account_state": "limited" },
  "state_labels": { "blocked": "usage limit" }
}
```

`state_labels.blocked` replaces the status word in the sidebar row, so with the
`rows_by_agent` snippet above the row reads `usage limit` instead of `blocked`.

**It polls `agent.list`; it does not subscribe to status events.** That
subscription is per pane on a stock server, so covering every Claude agent
would need one subscription per pane and a reconnect whenever a pane appears —
and widening it is a server change E9 does not make. One `agent.list` per
interval costs the same whatever the pane count, and it re-reads the labels the
server *actually holds*, which is how it repairs a server restart instead of
missing it forever.

**It never types, prompts, switches or kills.** Its action enum has no such
variant — decision (d) is enforced by the type, not promised in prose.

**Every label is a lease.** This is the part to understand before relying on
the badge:

- `ttl_ms = 4 × --interval`, floored at 15 s (`--interval` defaults to 5 000,
  so the default lease is 20 s), renewed at half the lease. A watcher that is
  killed, crashes or loses its machine therefore leaves a `limited` badge for
  **seconds**, not for the session.
- When the lease expires the server drops **both** the label and the
  `account_state` token. Expiry **removes** `account_state`; it does not
  restore what the launcher wrote. A limited agent whose watcher died shows
  `account` with **no** `account_state` until something writes one — the
  honest answer, since nothing is left running to say otherwise. A watcher that
  is still alive when the limit clears *does* put back the state it replaced,
  and puts it back without a lease; it will never put back `limited` itself,
  which would turn an expiring badge into a permanent one.
- `Ctrl-C` / `SIGTERM` clears every label it still holds — the ones on panes
  the last poll still saw, and only if the server is reachable right then;
  otherwise the lease is what clears them — and exits 0. A second interrupt
  exits 130 immediately.
- `--keep-labels` skips that clear. So does `--once`: a one-shot pass has no
  lifetime for a label to belong to, so it leaves what it found to the lease
  rather than being an expensive no-op.
- A closed stdout ends the watcher by `SIGPIPE` on unix like every other herdr
  CLI, and the leases cover that too.

Observed in the lab with the default interval, after **killing** the watcher
while it held a label (a running watcher renews at half the lease and the badge
never drops; where in that cycle the kill lands is what decides whether the
badge outlives it by 10 s or by the full 20 s):

```text
t=5s   tokens={'account':'work','account_state':'limited'} labels={'blocked':'usage limit'}
t=10s  tokens={'account':'work','account_state':'limited'} labels={'blocked':'usage limit'}
t=15s  tokens={'account':'work'}                           labels=None
```

**Budgets.** `--interval` is refused outside 500..300 000 ms — a refusal, not a
clamp, so a mistyped `10` does not silently become 500. `agent.explain` is
asked at most once per 5 s per pane, and after three looks in one blocked
episode that produced no label — a failed explain spends the budget too, since
what is being rationed is round trips — it drops to one look per 60 s, so an
agent parked on a permission prompt costs one round trip a minute. Leaving
`blocked` resets the episode, so the next one gets the full budget.

**Reconnect.** A server that is not running is not a failure: one `note:` on
stderr, capped exponential backoff (500 ms → 30 s), one `note:` on recovery,
and the next successful poll reconciles what the watcher believes against what
the server holds. Only a protocol mismatch is fatal (exit 1); `--once` against
no server exits 1.

## Limitations and known gaps

- **A restored agent comes back under the default profile.** After a server
  restart, herdr's own restore path relaunches `claude --resume <id>` from the
  server, with no environment — that code is server-side and E9 does not touch
  it. Metadata tokens are in-memory only (`PaneSnapshot` has no tokens field),
  so the account is *forgotten* rather than shown wrongly. Put it back with
  `herdr agent switch-account`.
- **`unverified` on macOS and Windows.** The environment probe is Linux-only
  (`/proc/<pid>/environ`). Elsewhere the profile is applied but not proven, and
  every surface says `unverified` rather than claiming `ok`.
- **The TUI actions are local-endpoint only.** While the client is showing an
  SSH endpoint from the machine sidebar, both context-menu items are hidden.
  Accounts on another host are driven by running the CLI *on* that host —
  `ssh <host> herdr account …`, or inside a `herdr --remote <host>` attach,
  where the client is itself running there and the menu items come back.
  (`herdr --remote` is an attach and refuses to carry a subcommand, so
  `herdr --remote <host> account list` is a usage error.) The phone half is E7.
- **A cached remote agent-detection manifest can shadow the `usage_limit`
  rule.** herdr prefers a cached remote `claude.toml` whose version is
  **greater than or equal to** the bundled one. An installation that has run
  `herdr server update-agent-manifests` against upstream's catalog would
  therefore lose the fork's rule the moment upstream publishes a `claude.toml`
  at or above `2026.09.07.1`. Two mitigations:
  1. today the fork's bundled version is dated ahead of the catalog upstream
     publishes (`2026.09.04.1` against the fork's `2026.09.07.1`), so a cached
     copy of it is ignored as older;
  2. the durable fix is to stop the background fetch entirely:
     ```toml
     [update]
     manifest_check = false
     ```
  `herdr agent explain <pane> --json` reports `manifest_source` and
  `cached_remote_version`, which is how you check which one is in force.
- **The `usage_limit` rule is best-effort until verified against a real
  rate-limited account.** It ships from a *reconstructed* fixture, not a live
  capture: it is proven **not** to fire on healthy screens, and it is **not**
  proven to fire on the real one. The provenance table and the exact checks are
  in [`tests/fixtures/fork/README.md`](../../tests/fixtures/fork/README.md);
  the summary is below.
- **The one residual false positive, stated exactly.** With **no horizontal
  rule anywhere on the screen** the region falls back to the whole screen, so a
  screen with no rule, no `⏺`/`⎿` transcript markers and both of the rule's
  gates present still matches. No real Claude Code screen has that shape —
  Claude draws either the rules around its prompt area or its transcript
  markers — and every one of those conditions must hold at once.
- **Two concurrent `herdr account add` runs can lose one store entry** (the
  store is load–modify–save with no lock file). Both directories are still
  created and neither is damaged; declare the profile that lost its entry
  yourself — a `[[accounts]]` block naming its `name` and `config_dir` is all a
  profile is — since a second `add` into a directory that has since been logged
  into is refused.
- **Windows sharing is best-effort.** Symlinks are attempted with
  `symlink_dir`/`symlink_file` and no junction fallback; a link that cannot be
  made is a warning and the profile simply starts without that entry.

## What a human must still verify against real Claude Code

Everything on this page was validated against a **fake `claude` stub**, never
the real thing. These are the items that need a real installation — and, for
the last group, a really rate-limited account. Record the results here and in
the plan's *As built* notes.

**The seed list**, against the installed Claude Code version: which of
`projects`, `todos`, `skills`, `plugins`, `commands`, `agents`, `CLAUDE.md`,
`history.jsonl` exist and are safe to share; whether Claude Code rewrites
`settings.json` / `.claude.json` atomically (the reason they are copied); and
whether `--resume` finds shared transcripts through the `projects` symlink.
Also the exact `oauthAccount` key names used for email, organization and plan.

**`claude auth login` exists.** herdr ships the `auth login` spelling and has
validated it only against the stub, which implements it. Run once:

```
herdr account add live --config-dir ~/.claude-live
herdr account login live --pane <pane>          # <pane> is a real pane id
herdr account status live --json
```

and check three things: the subcommand exists at all; it writes
`.credentials.json` (mode `0600`) into the **exported** directory and not into
`~/.claude`; and `herdr account status live` then shows that account's
`oauthAccount` email. If `auth login` is gone, the fallback is `claude` then
`/login` typed inside the session — a different shape (an interactive agent,
not a command that exits), and `herdr account login` would then type only the
export line and hand off to `herdr agent start --account`.

**The exit half of the switch.**

```
herdr agent switch-account <agent> <other-account>
```

`/exit` submitted through `agent.prompt` must exit cleanly from an idle screen
and from a blocked one after `Escape`; the pane must return to its own shell
prompt inside the 20 s default budget (time it, and raise `--timeout` /
`DEFAULT_TIMEOUT_MS` if a real transcript flush is slower); `herdr agent get
<pane>` must then report `agent_not_found`. Confirm that an agent in the middle
of a tool call is **not** asked to exit without `--interrupt`, and that
`Escape` interrupts rather than kills.

**The resume half.** `claude --resume <id>` under the new profile must report
the *same* session id through the hook with
`session_start_source = "resume"`, and herdr must then read the new profile's
directory out of the relaunched process's environment (`account_state: "ok"`).
Also check the case only a fake can stage: a `--resume` of an id the new
profile cannot see (transcripts **not** shared) starts a *new* conversation —
herdr must report `SessionMismatch`, name the original id, and the original
transcript must still be on disk.

**The `usage_limit` rule against the real screen.** This is the one `AGENTS.md`
"screen detection is evidence-based" requirement E9 could not meet locally.

```
herdr agent read <pane> --source detection --format text   # replaces the fixture
herdr agent read <pane> --source detection --format ansi
herdr agent explain <pane> --json | jq .matched_rule
```

(Substitute the pane id before running any of those: pasted verbatim, `<pane>`
is a shell redirection, not a placeholder.)

In order of risk: **(1) placement** — the rule reads
`after_last_horizontal_rule`, the status area below the live prompt box, and
accepts the notice at the start of a line or as a `·`/`∙`/`|`-separated footer
segment; a notice that lives only in the transcript above the box will **not**
match and the region must be widened. **(2) wording** — `<N>-hour limit
reached`, `Weekly limit reached`, `Claude usage limit reached`, and the second
gate, which is a reset clause (`resets 3pm`, `resets at 14:00`, `Your limit
will reset at …`) **or** a line starting `/upgrade`.
**(3)** that no permission prompt, MCP dialog or `/upgrade` menu is ever
reported as `usage_limit` while the account is healthy — and, with the account
really limited, that a permission prompt raised on top of it is still reported
as the permission prompt. **(4)** that the transcript-marker veto (`⏺`, `⎿`)
does not swallow the real footer.

**`switch-account` from a real limit screen.** Confirm that the typed `/exit`
fallback exits cleanly from the limit screen, that the pane returns to its own
shell prompt inside the budget, and that the resumed session comes back with
the same id. If the real screen swallows a typed `/exit`, the fallback needs an
`Escape` in front of it or a different key sequence.

**Remote manifest shadowing.** On an installation that has run
`herdr server update-agent-manifests`, confirm `herdr agent explain <pane>
--json` reports `manifest_source: "bundled"`, and that `[update]
manifest_check = false` keeps it that way after upstream publishes a newer
`claude.toml`.

**Other platforms.** macOS: the probe returns `unverified` (expected) and the
export line works in the default `zsh`. Windows: the `pwsh` line, and the `cmd`
refusal.

## The accounts lab

`scripts/fork/accounts-lab.sh` boots one isolated herdr server with two seeded
profiles and a fake `claude` first on `PATH`, so every command on this page can
be exercised without touching your own Claude installation.

```bash
cargo build
bash scripts/fork/accounts-lab.sh up
eval "$(bash scripts/fork/accounts-lab.sh env)"
export XDG_RUNTIME_DIR="$HERDR_ACCOUNTS_LAB_ROOT/runtime"
unset HERDR_SOCKET_PATH HERDR_CLIENT_SOCKET_PATH HERDR_ENV HERDR_SESSION \
      HERDR_CONFIG_PATH

"$HERDR_BIN" --session accounts-lab account list --json
…
bash scripts/fork/accounts-lab.sh down
```

Spell the binary out. `$HERDR_BIN` is the debug build you just made; a bare
`herdr` is whatever is first on your `PATH` — very likely your own installed
herdr, talking to your own server. The two `unset`s matter for the same
reason: the script drops those variables for *its own* calls, but `env` does
not drop them from your shell, so a lab command typed inside a herdr pane
would otherwise be routed back to the pane's server.

Do this in a throwaway terminal. The `eval` and the `export` last for that
shell only, but while they last its `PATH` starts with the lab's `bin`, so a
bare `claude` there is the stub, not your real Claude Code — and
`XDG_RUNTIME_DIR` points at the lab, which is exactly why the script does not
export it for you.

`env` exports `HERDR_ACCOUNTS_LAB_ROOT`, `XDG_CONFIG_HOME`,
`HERDR_ACCOUNTS_LAB_SESSION`, `HERDR_ACCOUNTS_LAB_PANE`,
`HERDR_ACCOUNTS_LAB_PROFILE_PERSO` / `_WORK` / `_AMBIENT`,
`HERDR_ACCOUNTS_LAB_CLIENT_SOCKET`, `HERDR_ACCOUNTS_LAB_API_SOCKET`,
`HERDR_BIN` and `PATH` — the pane and the two socket paths only while the lab
is actually up. It deliberately does **not** export
`XDG_RUNTIME_DIR` — eval-ing that into an interactive shell would hijack your
Wayland/D-Bus/PipeWire sockets — so set it yourself as above when you drive the
lab by hand. `HERDR_ACCOUNTS_LAB_ROOT` picks a different root, which is how two
labs run at once (keep it short: `up` refuses before it creates anything if the
client socket path it would derive reaches 104 bytes).

**Isolation, by construction.** Every herdr call carries `--session
accounts-lab`, the lab's own XDG directories, and drops `HERDR_SOCKET_PATH`,
`HERDR_CLIENT_SOCKET_PATH`, `HERDR_ENV`, `HERDR_SESSION` and
`HERDR_CONFIG_PATH`. `CLAUDE_CONFIG_DIR` is pinned to a third, deliberately
logged-out profile (`ambient`), so an agent launched *without* a profile still
lands inside the lab. `scripts/fork/fake-claude.sh` has **no**
`$HOME/.claude` fallback: with no `CLAUDE_CONFIG_DIR` it writes nothing, and it
exits 3 if pointed at the real `~/.claude`.

**Because every `add` in the lab must pass `--config-dir`:** the default is
`~/.claude-<name>` in the *developer's* home, and the lab does not override
`HOME`.

The stub's knobs, used by the tests and useful by hand: `FAKE_CLAUDE_BUSY=1`
(refuses `/exit`), `FAKE_CLAUDE_NO_SESSION=1` (reports no session id, like a
profile with no hook), `FAKE_CLAUDE_RESUME={ok,new,fail}`,
`FAKE_CLAUDE_LIMIT=1` (starts on the limit screen), and the inputs `/work`,
`/limit`, `/redraw` and `/exit`. `/redraw` reprints the prompt box and is the
only way to drive a limit back *out* of `after_last_horizontal_rule` — the
notice never scrolls itself away, `Escape` cannot clear it, and `agent.prompt`
is refused on a blocked agent.
