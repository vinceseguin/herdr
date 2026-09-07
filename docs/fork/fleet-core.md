# Fleet core

Epic **E1** gives one herdr client process connections to several herdr
servers at once — this machine's default session, other named sessions on this
machine, and machines reached over SSH — and merges what they report into a
single, host-qualified view. `herdr fleet status` is the first surface built on
it; the gateway (E3) is the next. (The fork's own fleet TUI, E2, was
retired in favour of upstream's multi-machine client — see
[ADR 0002](./decisions/0002-adopt-upstream-multi-machine-client.md);
`herdr machine …` is the console.)

Nothing on the other side changes. A fleet host runs a **stock** herdr server
and speaks the frozen generation-1 client endpoint protocol; the fork adds no
message, no field and no server code (see
[ADR 0001](./decisions/0001-servers-stay-stock-ssh-transport.md)).

- [Configuring `[fleet]`](#configuring-fleet)
- [Host-qualified ids](#host-qualified-ids)
- [`herdr fleet status`](#herdr-fleet-status)
- [Connection states](#connection-states)
- [SSH hosts](#ssh-hosts)
- [Reconnecting](#reconnecting)
- [What the fleet never does](#what-the-fleet-never-does)
- [For developers](#for-developers)

## Configuring `[fleet]`

The fleet is configured in the normal `config.toml`
(`$XDG_CONFIG_HOME/herdr/config.toml`, or `herdr-dev/` for a debug build).
`herdr --default-config` prints the commented reference block:

```toml
[fleet]
# Hosts aggregated by `herdr fleet status` and the fork's gateway.
# This machine's default session is always host "local" unless disabled.
# include_local = true
# Also aggregate every machine saved by `herdr machine add` as an ssh host.
# include_machines = false
#
# [[fleet.hosts]]
# name = "workbox"        # display name and id prefix (workbox/w1:p1)
# kind = "ssh"            # "ssh" | "local"
# target = "workbox"      # ssh destination (alias, user@host, ssh://host:2222)
# session = "agents"      # optional named session on that host; required for kind = "local"
# enabled = true
```

### Keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `fleet.include_local` | bool | `true` | Add this machine's **default** session as the host `local`, first in the list. |
| `fleet.include_machines` | bool | `false` | Also add every machine saved by `herdr machine add` as an ssh host, after the `[[fleet.hosts]]` entries. See [Saved machines as hosts](#saved-machines-as-hosts). |
| `fleet.hosts[].name` | string | — (required) | Host id and display name. Session-name rules: `[A-Za-z0-9._-]`, 1–64 bytes, never `/`, never `.`/`..`. It is the `host` half of every `host/w1:p1` id. |
| `fleet.hosts[].kind` | `"ssh"` \| `"local"` | `"ssh"` | How the fleet reaches the host. |
| `fleet.hosts[].target` | string | — | SSH destination: an alias from your `~/.ssh/config`, `user@host`, or `ssh://host:2222`. **Required** for `kind = "ssh"`, and rejected for `kind = "local"`. |
| `fleet.hosts[].session` | string | — | Named herdr session on that host. **Required** for `kind = "local"`; optional for `kind = "ssh"`, where omitting it means the host's default session. |
| `fleet.hosts[].enabled` | bool | `true` | `false` keeps the host in the config and in the report, but the fleet never opens it. |

`[[fleet.hosts]]` entries keep their file order in every list; the implicit
`local` host, when enabled, always comes first.

A `kind = "local"` host is a named session on **this** machine. That is not
only a test fixture: it is how [the fleet lab](./README.md#fleet-lab) builds a
fleet of N hosts without N machines, and how the integration tests run.

```toml
[fleet]
include_local = true

[[fleet.hosts]]
name = "workbox"
kind = "ssh"
target = "workbox"
session = "agents"

[[fleet.hosts]]
name = "scratch"
kind = "local"
session = "scratch"
enabled = false
```

### The reserved `local` host

`local` is the id of this machine's default session. While
`include_local = true`, a `[[fleet.hosts]]` entry named `local` is a
configuration error — set `include_local = false` if you want to configure that
host yourself, or rename yours.

### Saved machines as hosts

`herdr machine add <ssh-target> --label <label>` installs Herdr on a machine,
starts its server and saves the profile in the client's endpoint catalog
(`$XDG_STATE_HOME/herdr/client/endpoints.json`, `herdr-dev/` for a debug
build). Those machines are exactly the fleet's precondition — a reachable herdr
server — so `include_machines = true` adopts every one of them as an ssh host
instead of making you copy each target into `[[fleet.hosts]]`:

```toml
[fleet]
include_local = false
include_machines = true

[[fleet.hosts]]
name = "scratch"
kind = "local"
session = "scratch"
```

```console
$ herdr machine list
0123…  Lab SSH  lab.example  lab-1  enabled

$ herdr fleet status
HOST     KIND   STATE
scratch  local  connected
lab-ssh  ssh    connected
```

The switch lives in `[fleet]`, not `[gateway]`, so `herdr fleet status` and the
gateway always report the same fleet. It is **off by default**: an existing
config keeps exactly the hosts it names.

**Host id.** A saved machine has a free-form label; a fleet host id is the
`host` half of every `host/w1:p1` reference. The derivation is frozen:

1. the label verbatim when it is already a valid host name (`workbox` →
   `workbox`, case included — `Workbox` stays `Workbox`);
2. otherwise the label lowercased with every run of characters outside
   `[A-Za-z0-9._-]` folded to a single `-`, leading and trailing runs dropped
   (`My Laptop (home)` → `my-laptop-home`, `Lab SSH` → `lab-ssh`).

The machine's ssh target, explicit session and enabled flag are carried
through unchanged, so a disabled machine is a disabled host (`unavailable:
host disabled`) exactly like `[[fleet.hosts]] enabled = false`. `kind` is
always `ssh`; the report shape (`herdr.fleet.status.v1`) gains no field, so
nothing downstream can tell a machine host from a configured one.

**Diagnostics.** Machines are validated with the same all-or-nothing rule as
`[[fleet.hosts]]`, because a label that quietly re-points a host id would send
a terminal to the wrong machine:

- the label derives no valid host name (`///`, or a fold longer than 64 bytes)
- the derived id is the reserved `local`
- the derived id is already a `[[fleet.hosts]]` name
- two machines derive the same id (one diagnostic naming both)
- the saved ssh target or session is malformed

Each diagnostic names the machine's label and profile id and the command that
fixes it — `herdr machine rename <profile-id> --label <name>` for an id
problem, `herdr machine remove <profile-id>` for a malformed target or session
— and, like every `[fleet]` problem, means no host resolves at all:

```console
$ herdr fleet status
duplicate saved machine host name: machine "Scratch" (0123456789abcdef0123456789abcdef) derives the fleet host id "scratch", which is already a [fleet] host name; rename it with: herdr machine rename 0123456789abcdef0123456789abcdef --label <name>; ignoring [fleet] hosts
$ echo $?
1
```

An unreadable catalog is one diagnostic (`saved machines unavailable: …`); a
**missing** catalog is not an error at all — `include_machines = true` before
the first `herdr machine add` simply adds nothing.

> There is no `[[fleet.machines]]` id override. A machine you cannot rename is
> the escape hatch's only use case, and it does not exist yet; rename the
> machine or add it to `[[fleet.hosts]]` by hand instead.

### Validation

`[fleet]` is validated without touching the network or the filesystem, and every
problem is reported as a diagnostic through the normal config path: the list
`herdr config check` prints, the TUI's diagnostic banner, and the diagnostics a
live `herdr server reload-config` produces. The rules:

- `name` is missing, malformed, too long, or contains `/`
- `name` is `local` while `include_local = true` (reserved)
- two hosts share a `name`
- `kind = "ssh"` without a `target`
- a `target` that is empty, starts with `-` (it would be read as an ssh flag),
  or contains whitespace or control characters (an ssh destination is one argv
  element)
- `kind = "local"` without a `session`
- `kind = "local"` **with** a `target` — refused rather than ignored, because
  silently dropping it would attach you to a same-named local session instead of
  the machine you named
- a `session` that is not a valid session name

Validation is all-or-nothing: **any** diagnostic, including one on a host with
`enabled = false`, means no host specs are resolved. `herdr fleet status` then
prints the diagnostics on stderr and exits **1** rather than reporting a partial
fleet, so a typo in one host cannot quietly drop it from the list.

With `include_machines = true`, the saved-machine problems listed under
[Saved machines as hosts](#saved-machines-as-hosts) join that all-or-nothing
rule with one difference: they need the endpoint catalog on disk, so they are
reported when the fleet is resolved — by `herdr fleet status` (stderr, exit 1)
and by the gateway at startup — not by `herdr config check`, the TUI banner or
`reload-config`, which validate `[fleet]` without touching the filesystem.

```console
$ herdr config check
config: issues found
reserved fleet host name: fleet.hosts[0].name = "local" names this machine's default session; set fleet.include_local = false or rename the host; ignoring [fleet] hosts
missing fleet host session: fleet.hosts[0].session is required for kind = "local"; ignoring [fleet] hosts

$ herdr fleet status
reserved fleet host name: fleet.hosts[0].name = "local" names this machine's default session; set fleet.include_local = false or rename the host; ignoring [fleet] hosts
missing fleet host session: fleet.hosts[0].session is required for kind = "local"; ignoring [fleet] hosts
$ echo $?
1
```

> `[fleet]` is deliberately absent from upstream's generated config reference
> (`scripts/config_reference_check.py` skips the whole subtree, because fork
> rules forbid editing `docs/next/**` and the check cannot model an array of
> tables). **This page is the reference for every `[fleet]` key.** Adding a key
> without documenting it here leaves it undocumented everywhere.

## Host-qualified ids

Server-side ids (`w1`, `w1:t1`, `w1:p1`) are only unique **within one server**.
The fleet prefixes them with the host id:

| Kind | Form | Example |
| --- | --- | --- |
| Workspace | `host/w1` | `workbox/w1` |
| Tab | `host/w1:t1` | `workbox/w1:t1` |
| Pane / agent | `host/w1:p1` | `workbox/w1:p1` |

Host ids can never contain `/`, so the first `/` splits the reference. That is
the string form in the CLI, in JSON (`ref` fields), and in the gateway's URLs
later; the typed forms are `FleetWorkspaceRef`, `FleetTabRef` and
`FleetPaneRef`, which serialize as those strings.

Two per-host counters are **not** comparable across hosts:

- `boot_id` / `revision` are one server's snapshot identity. `revision` is
  counted per *client connection* and restarts at 1 for each one, so it can go
  **backwards** on an unchanged `boot_id` when a host reconnects. Treat the
  first snapshot after a `host_connection … connected` as the new baseline;
  never drop it for carrying a lower number than the one you hold.
- `state_change_seq` restarts at every server boot.

For cross-host recency the fleet assigns its own monotonic `fleet_change_seq`
whenever an agent first appears, or its host reports that it moved — a higher
`state_change_seq`, or a different status. Sort by that, never by
`state_change_seq`.

## `herdr fleet status`

```console
$ herdr fleet help
Inspect the configured fleet of herdr hosts

usage: herdr fleet status [--json] [--timeout-ms MS] [--watch]

Options:
  --json              Print the fleet status report as JSON
  --timeout-ms MS     Wait at most MS for hosts to answer (default 5000)
  --watch             Keep running and print one line per change
```

(`herdr fleet --help` and `herdr fleet status --help` are answered by herdr's
shared clap spec instead: the first lists the `status` subcommand, the second
the same three flags.)

| Flag | Meaning |
| --- | --- |
| `--json` | Print the report as pretty JSON instead of the table. |
| `--timeout-ms MS` | How long to wait for hosts that have not answered yet. Default `5000`, maximum `600000` (a typo cannot hang a script). Accepts `--timeout-ms 30000` and `--timeout-ms=30000`. |
| `--watch` | Print the settled report, then one line per change until you stop it. |

Exit codes: **0** whenever a report was produced (an unreachable host is data,
not a failure), **1** for an invalid `[fleet]` section, **2** for a usage error.

The command settles early: it returns as soon as every host is either connected
*with* a snapshot or has failed, so the timeout only bounds hosts that are
genuinely slow. It is read-only — it never sends input to a host, and it sets no
active host, so `active_host` is `null` and every host stays at the inactive
surface size.

### Text

The examples on this page are real output from a five-host fleet built with the
[fleet lab](./README.md#fleet-lab) and the [SSH lab](./README.md#ssh-lab):
`local` (this machine's default session — not running in the lab), `lab-ssh`
(the lab reached over ssh), `lab-2` (a named local session), `nowhere` (an ssh
target nothing listens on) and `spare` (a host with `enabled = false`).

```console
$ herdr fleet status --timeout-ms 30000
client 0.9.0-fork  active host: none

HOST     KIND   STATE        VERSION     BLOCKED  WORKING  DONE  IDLE  UNKNOWN
local    local  unavailable  -           0        0        0     0     0
lab-ssh  ssh    connected    0.9.0-fork  0        0        0     0     0
lab-2    local  connected    0.9.0-fork  0        0        0     0     0
nowhere  ssh    unavailable  -           0        0        0     0     0
spare    local  unavailable  -           0        0        0     0     0
  ! local: no herdr server for session default at /tmp/herdr-fleet-lab/xdg/herdr-dev/herdr-client.sock
  ! nowhere: remote platform detection failed: ssh: connect to host 127.0.0.1 port 1: Connection refused
  ! spare: host disabled in [fleet]

no agents
```

The lab's panes run a marker command, not an agent, so the merged list is empty
and every roll-up is zero. With agents on a host the last block is a second
table — this is the golden rendering the unit test in `src/fleet/report.rs`
pins:

```text
HOST     KIND   STATE        VERSION     BLOCKED  WORKING  DONE  IDLE  UNKNOWN
local    local  connected    0.9.0-fork  1        0        0     0     0
workbox  local  unavailable  -           0        0        0     0     0
  ! workbox: connection refused

AGENT        STATUS   WORKSPACE  NAME
local/w1:p1  blocked  repo       reviewer
```


Hosts that are not connected get a `!` line with the reason underneath the
table, and the agent table is replaced by `no agents` when the merged list is
empty.

The agent table grows one more column, `ACCOUNT`, when — and only when — at
least one merged agent carries an `account` metadata token, so a fleet that
does not use account profiles renders exactly the four columns
above. An agent with no such token shows `-` in it:

```text
AGENT        STATUS   WORKSPACE  NAME      ACCOUNT
local/w1:p1  blocked  repo       reviewer  work
lab-2/w1:p1  idle     lab-2      builder   -
```

### JSON

`--json` prints one `herdr.fleet.status.v1` document. It is an **additive**
contract: fields are added, never renamed, and a reader must tolerate an
unknown `connection.state` or `agent_status` (both decode to `unknown`).

```console
$ herdr fleet status --timeout-ms 30000 --json
{
  "schema": "herdr.fleet.status.v1",
  "client_version": "0.9.0-fork",
  "active_host": null,
  "hosts": [
    {
      "id": "local",
      "kind": "local",
      "target": null,
      "session": null,
      "enabled": true,
      "connection": {
        "state": "unavailable",
        "reason": "no herdr server for session default at /tmp/herdr-fleet-lab/xdg/herdr-dev/herdr-client.sock",
        "retry_in_ms": 1000
      },
      "boot_id": null,
      "revision": null,
      "counts": { "blocked": 0, "working": 0, "done": 0, "idle": 0, "unknown": 0 },
      "workspaces": []
    },
    {
      "id": "lab-ssh",
      "kind": "ssh",
      "target": "herdr-ssh-lab",
      "session": "lab-1",
      "enabled": true,
      "connection": {
        "state": "connected",
        "server_version": "0.9.0-fork"
      },
      "boot_id": "1613778-1788644429866237637",
      "revision": 1,
      "counts": { "blocked": 0, "working": 0, "done": 0, "idle": 0, "unknown": 0 },
      "workspaces": [
        {
          "ref": "lab-ssh/w1",
          "workspace_id": "w1",
          "label": "lab-1",
          "agent_status": "unknown",
          "focused": true
        }
      ]
    },

    ... lab-2 (connected, local), nowhere (unavailable, ssh) ...

    {
      "id": "spare",
      "kind": "local",
      "target": null,
      "session": "lab-1",
      "enabled": false,
      "connection": {
        "state": "unavailable",
        "reason": "host disabled in [fleet]",
        "retry_in_ms": null
      },
      "boot_id": null,
      "revision": null,
      "counts": { "blocked": 0, "working": 0, "done": 0, "idle": 0, "unknown": 0 },
      "workspaces": []
    }
  ],
  "agents": [],
  "counts": { "blocked": 0, "working": 0, "done": 0, "idle": 0, "unknown": 0 }
}
```

(The `counts` objects are printed one field per line; they are folded here, and
two hosts are elided, for length. Nothing else is edited.)


| Field | Notes |
| --- | --- |
| `schema` | Always `herdr.fleet.status.v1`. |
| `client_version` | Version of the client that produced the report (`0.9.0-fork`). |
| `active_host` | `null` for `herdr fleet status`; a long-lived consumer that installs one host's surfaces sets it. |
| `hosts[]` | Every configured host, in config order, `local` first. |
| `hosts[].kind` | `"local"` or `"ssh"`; treat an unknown value as a transport you cannot use. |
| `hosts[].connection` | Tagged by `state`; see [Connection states](#connection-states). |
| `hosts[].boot_id` / `revision` | Identity of the last snapshot, kept while the host is unavailable. |
| `hosts[].workspaces[]` | The host's workspaces from that snapshot, with `ref = host/w1`. Kept while the host is down so a client can dim rather than blank it. |
| `agents[]` | The **merged** agent list: blocked → working → done → idle → unknown, then most recently changed first, then host order, then pane id. |
| `agents[].fleet_change_seq` | Fleet-wide recency, comparable across hosts. |
| `agents[].state_change_seq` | The host's own counter — only comparable within one host boot. |
| `agents[].tokens` | The agent's metadata tokens on its host (`pane.report_metadata`), as a `{name: value}` object sorted by name. Absent keys mean the host reported none; the object is omitted entirely by a client older than this field, so read it as empty when missing. `account` is the profile an agent was launched under and `account_state` how well that is known (`herdr account`); any other integration's tokens ride the same field. |
| `agents[].state_labels` | Per-status label overrides on its host, `{status: label}` sorted by status. A client that renders a status word may substitute the label for the matching status; one that does not may ignore the whole object. |
| `counts` | Totals across every contributing host (`blocked`, `working`, `done`, `idle`, `unknown`). |

Only a **connected host with a snapshot** contributes agents and counts. When a
host drops, its agents leave the merged list and its roll-up zeroes; its
`workspaces[]` stay so the host does not blank out.

### `--watch`

`--watch` prints the settled report first (a table, or the JSON document with
`--json`), then one line per change. With `--json` those lines are NDJSON
`FleetChange` objects tagged by `kind`; without it, one human line each.

Taking the SSH lab down and bringing it back up while a watch runs — the
whole reconnect, host-local, with `lab-2` and `local` untouched by it:

```console
$ herdr fleet status --timeout-ms 30000 --watch --json
{ ... the settled report ... }
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"unavailable","reason":"host closed the connection","retry_in_ms":1000}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"connecting","attempt":2}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"unavailable","reason":"remote bridge failed: ssh bridge exited with exit status: 255","retry_in_ms":2000}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"connecting","attempt":3}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"unavailable","reason":"remote platform detection failed: ssh: Could not resolve hostname herdr-ssh-lab: Temporary failure in name resolution","retry_in_ms":4000}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"connecting","attempt":4}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"unavailable","reason":"remote platform detection failed: ssh: Could not resolve hostname herdr-ssh-lab: Temporary failure in name resolution","retry_in_ms":8000}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"connecting","attempt":5}}
{"kind":"host_connection","host":"lab-ssh","connection":{"state":"connected","server_version":"0.9.0-fork"}}
{"kind":"snapshot","host":"lab-ssh","boot_id":"1613778-1788644429866237637","revision":1}
```

The same run without `--json`:

```console
$ herdr fleet status --timeout-ms 30000 --watch
... the settled table ...
host lab-ssh unavailable: host closed the connection
host lab-ssh connecting
host lab-ssh unavailable: remote bridge failed: ssh bridge exited with exit status: 255
host lab-ssh connecting
host lab-ssh unavailable: remote platform detection failed: ssh: Could not resolve hostname herdr-ssh-lab: Temporary failure in name resolution
host lab-ssh connecting
host lab-ssh connected
snapshot lab-ssh boot 1613778-1788644429866237637 revision 1
```


| `kind` | Line | Meaning |
| --- | --- | --- |
| `host_connection` | `host <id> <state>[: reason]` | The host's connection state changed. Carries the connection in the same JSON vocabulary as the report — but **never** the advertised method list; read `methods` from the report or the state. |
| `snapshot` | `snapshot <id> boot <boot_id> revision <n>` | That host's structure changed. |
| `agent_added` | `agent + <ref> <status> <workspace>` | An agent joined the merged list, or rejoined it. Treat it as an upsert keyed by `ref`: a reconnecting host re-announces every agent it still has. |
| `agent_removed` | `agent - <ref>` | An agent left, or its host stopped contributing. |
| `agent_status` | `agent ~ <ref> <from> -> <to>` | Status transition. |
| `agent_metadata` | `agent * <ref> <name>=<value>… <status>:<label>…`, or `agent * <ref> cleared` | An agent already in the merged list changed its `tokens` or `state_labels`. Both maps are the **whole** metadata after the change, never a patch: a token that disappeared is simply absent, and both empty means everything was cleared. An agent that has just joined carries its metadata on its `agent_added` instead, so this kind never duplicates one. |
| `active_host` | `active host <id>` / `active host none` | The active host changed (a consumer called `set_active`). |

New kinds are appended to this vocabulary as the fork grows, so **a reader must
skip a `kind` it does not know** rather than fail on it — a reducer treats it as
a no-op and keeps the state it has. The same rule holds for a new field inside a
kind it does know.

Every line is flushed as it is written, so `herdr fleet status --watch --json |
jq` works live. A closed pipe (`| head`) ends the watch: like every herdr CLI
command it restores SIGPIPE's default disposition while it writes, so on unix
the process is killed by SIGPIPE (status 141), exactly as `yes | head` is — the
pipeline's own status is the reader's.

`--watch` also ends by itself when no host can ever change again — that happens
when every host's supervisor has stopped: no host is enabled at all (an empty
`[fleet]` with `include_local = false`, or every host `enabled = false`), or no
transport could be built for any of them.

## Connection states

| State | JSON | Meaning |
| --- | --- | --- |
| Connecting | `{"state":"connecting","attempt":n}` | An attempt is in progress. `attempt` is the 1-based number of the attempt in progress and does **not** reset on success, so a host that reconnected once reports `attempt: 2`. |
| Connected | `{"state":"connected","server_version":"…"}` | Handshake done. Agents and counts only appear once its first snapshot has arrived. |
| Unavailable | `{"state":"unavailable","reason":"…","retry_in_ms":n}` | This host is down, for the stated reason; the fleet will retry in `retry_in_ms`. Other hosts are unaffected. |
| Incompatible | `{"state":"incompatible","generation":n,"reason":"…"}` | The host answered, but not with endpoint generation 1 (or not with the four v1 codecs). It is retried at the backoff ceiling, because a host can be updated while the fleet runs. |
| Unknown | `{"state":"…"}` anything else | Forward-compatibility fallback for a report written by a newer client. Read it as "cannot tell", i.e. unusable. |

Reasons are operator-facing sentences, not errnos. Some you will see:

- `no herdr server for session default at /…/herdr-client.sock` — that session
  is not running (or the socket moved).
- `host disabled in [fleet]` — `enabled = false`; no connection was attempted,
  and `retry_in_ms` is `null`.
- ``no herdr with endpoint generation 1 on host; run `herdr --remote workbox` once to install it``
- `remote platform detection failed: ssh: connect to host 127.0.0.1 port 1: Connection refused`
- `remote bridge failed: ssh bridge exited with exit status: 255`
- `host closed the connection`

## SSH hosts

An SSH host is reached exactly the way `herdr --remote` reaches one: discover
the herdr binary on that machine, bind a private local socket, and run
`ssh -T <target> "exec <herdr> [--session <name>] remote-client-bridge"` for
each connection, piping the endpoint protocol over its stdio. E1 widened that
bridge in `src/remote/attach.rs` to `pub(crate)` and added the fleet-side
adapter in `src/fleet/transport/ssh.rs`; `herdr --remote` itself is unchanged.

### Install herdr on the host once, yourself

**The fleet never installs anything.** It only *discovers* an existing herdr on
the host (`command -v herdr` through your login shell, then the usual
directories) and checks that it reports endpoint generation 1. A host without
one is reported unavailable with the command you need to run:

```text
no herdr with endpoint generation 1 on host; run `herdr --remote workbox` once to install it
```

`herdr --remote <target>` is the interactive path that may prompt, upload a
binary, or hand a running server off. Run it once per host, by hand; after that
the fleet finds what it left behind. `HERDR_REMOTE_BINARY` is honoured by
`--remote` and deliberately ignored by the fleet.

### `[remote].manage_ssh_config` applies here too

Fleet SSH hosts use the same `[remote]` setting as `herdr --remote`:

```toml
[remote]
manage_ssh_config = true   # the default
```

With it on, each host gets herdr's managed ssh config (`-F`) — your
`~/.ssh/config` and `/etc/ssh/ssh_config` are `Include`d, keepalives are added,
and connections share a private `ControlMaster` socket, so a reconnect does not
pay a fresh TCP+auth handshake. Set it to `false` to run plain `ssh` with your
configuration untouched; the fleet still works, just without the keepalives and
the shared master.

### Reconnecting over SSH takes a couple of attempts

The bridge is a *listener*: a connection succeeds as soon as the local socket
accepts, before the `ssh` child has necessarily done anything. So an `ssh` child
that dies is invisible to the attempt that started it and surfaces as the next
read's EOF. The fleet reports the chain as it unwinds — one step per attempt,
each naming what failed:

```text
host lab-ssh unavailable: host closed the connection
host lab-ssh connecting
host lab-ssh unavailable: remote bridge failed: ssh bridge exited with exit status: 255
host lab-ssh connecting
host lab-ssh unavailable: remote platform detection failed: ssh: Could not resolve hostname herdr-ssh-lab: Temporary failure in name resolution
host lab-ssh connecting
host lab-ssh connected
```

That is expected, not a bug: a bridge failure retires the ssh session, the
discovery cache and the bridge, so the *next* attempt re-probes the host over
ssh and reports the real error. Backoff runs 1 s → 2 s → 4 s → … → 30 s
throughout.

Discovery is cached per host and retired by failure, never by age, so a
reconnect after the *remote server* restarted costs no extra ssh round-trip.

### The forward socket, and what a signal leaves behind

Each ssh host binds its own private forward socket next to `herdr --remote`'s,
in your temp directory, named per **process and host id**:

```text
/tmp/herdr-remote-<pid>-<host>-<target>-<session>.sock
```

The host-id scope is what lets two fleet hosts point at the same target and
session without sharing a socket. A name that would not fit the platform's
socket-path limit falls back to a hashed short form
(`herdr-r-<pid>-<host>-<target>-<hash>.sock`), which the scope enters the same
way. `--remote`'s own unscoped names, readable and hashed, are byte-identical to
what they were before E1.

A clean exit (`FleetConnector::shutdown`, or dropping the transport) unlinks the
socket and closes the control master with `ssh -O exit`. A process **killed by a
signal** runs no destructors: the socket file is left behind (harmless — the
name carries the pid, so nothing collides) and the `ControlPersist` master stays
up until it times out. A long-running consumer (the gateway, E3) must call
`shutdown` on exit.

### Interactive vs. noninteractive ssh

`FleetConnectorOptions::ssh_noninteractive` picks the ssh policy for every
bridged connection, and `transport_for` hands it to `SshTransport`.

- **`false` (the default, `for_config`)** — `bridge_connection` spawns `ssh`
  with `stderr` **inherited** and no forced options, exactly as
  `herdr --remote` does. In a CLI that is what you want: ssh's own errors reach
  your terminal and its auth flow can prompt.
- **`true` (`for_daemon`)** — `BatchMode=yes`, `NumberOfPasswordPrompts=0`,
  `StrictHostKeyChecking=yes`, a bounded connect and keepalives, with the ssh
  child's `stderr` on `/dev/null`. This is what a gateway or any other daemon
  must use: it has no terminal to prompt on, and an inherited ssh warning would
  otherwise go to the daemon's own stderr.

A full-screen consumer is in the same position as a daemon — an ssh warning on
the inherited stderr is painted straight over the TUI — so it uses
`ssh_noninteractive: true` as well.

The flag covers the **bridged** ssh children, which is where the endpoint stream
and the inherited stderr live. It does **not** cover the short discovery
commands an ssh transport runs first (`uname -s`, the remote-binary probe):
those use `remote::attach`'s interactive `RemoteSsh`, which pipes stdout *and*
stderr — so nothing is painted on a daemon's terminal — but does not force
`BatchMode`. A host that cannot authenticate without a prompt therefore blocks
its own supervisor thread there, which is the same host-local outcome as any
other hung ssh probe: no other host, reader or request is affected, and
`shutdown` detaches the thread. Closing it needs a noninteractive constructor
in `src/remote/`, which E3 leaves untouched on purpose.

## Reconnecting

Every host has its own supervisor thread and its own backoff, and every failure
is host-local. One host being unreachable, incompatible, or misbehaving never
affects another host and never ends the process.

- Backoff: **1 s → 2 s → 4 s → … → 30 s**, capped; reset on a successful
  handshake.
- An `Unavailable` host keeps its last snapshot (so a client can dim it) but
  stops contributing agents and counts.
- An `Incompatible` host is retried at the ceiling — it may be updated while the
  fleet runs.
- A host whose *transport* cannot be built at all is reported once and its
  supervisor stops; retrying could not change which transports the binary has.
- Reconnect covers the whole chain for ssh hosts: control master, discovery,
  bridge, socket.

## What the fleet never does

By design (plan decision (f), ADR 0001), the fleet connector:

- never changes a server, the wire protocol, or the endpoint contract — hosts
  may run upstream herdr;
- never installs, uploads, stops or hands off a herdr on any host, and never
  prompts;
- never reads `HERDR_REMOTE_BINARY`;
- never sends input: `herdr fleet status` is read-only, and nothing in E1 sends
  `ClientShellPaneInput` on its own;
- never turns one host's failure into a fleet-wide failure, and never calls
  `std::process::exit` or panics because of what a host said.

It also never changes a host's pane geometry — as of E3. `HandshakeParams` has
a `surface_active` flag, written straight into the generation-1
`endpoint.hello.v1`:

- `HandshakeParams::read_only` (`herdr fleet status`, the gateway) sends
  **`surface_active: false`**. Upstream #3670 taught the server to leave such an
  endpoint out of foreground selection: it is never made the host's foreground
  client, its `ClientResize` is ignored, and it claims no shell-tab geometry —
  while snapshots keep flowing. A read-only consumer is therefore genuinely
  passive, and may hold its connections for as long as it likes.
- `HandshakeParams::for_client` (a full-screen console) sends
  **`surface_active: true`**, because whichever host is active really is
  rendering into that terminal.

One residual: a host running a herdr **older than #3670** has no
`surface_active` field to read (the hello has no `deny_unknown_fields`, so it
ignores it) and still treats every connecting client shell as its foreground
client, whose surface size is the host's effective pane geometry. The fleet
handshakes inactive hosts at herdr's own default headless geometry
(`DEFAULT_HEADLESS_COLS` × `DEFAULT_HEADLESS_ROWS`), so the common case — a
headless server running agents — is still a no-op resize there; a pre-#3670
host that already has an attached client, or a `[server]`
`headless_cols`/`headless_rows` of its own, is resized while the fleet client is
connected and restored when it disconnects. **Against such a host, hold fleet
connections only while something is reading them.** This is documented, not
worked around: the fix is on the host, which needs no fork code.

## For developers

### Module map — `src/fleet/`

| Module | Purity | What it owns |
| --- | --- | --- |
| `hosts.rs` | pure | `HostId` (`HostId::LOCAL` = `"local"`), `HostKind`, `HostSpec`, `resolve_hosts` — the only place `[fleet]` becomes specs. |
| `machines.rs` | pure | `MachineProfile`, `machine_host_id`, `machine_host_specs` — the frozen label → host-id derivation and its diagnostics. |
| `hosts_source.rs` | runtime | `hosts_for_config(&Config)` — `resolve_hosts` plus the saved machines when `include_machines` is on. The **only** module under `src/fleet/` that names `crate::client`. |
| `refs.rs` | pure | `FleetPaneRef`, `FleetTabRef`, `FleetWorkspaceRef`; `Display`/`FromStr`/serde as `host/w1:p1`. |
| `state.rs` | pure | `FleetState`, `HostState`, `HostConnection`, `HostEvent`, `MergedAgent`, `AgentRollup`, `FleetChange`, `Backoff`. |
| `report.rs` | pure | `FleetStatusReport`, `HostReport`, `ConnectionReport`, `WorkspaceReport`, `AgentReport`, `render_text`. |
| `transport/` | runtime | `HostTransport`, `transport_for`, `LocalTransport`, `SshTransport`. |
| `handshake.rs` | runtime | The generation-1 hello/welcome, `HandshakeParams`, `LOCAL_HANDSHAKE_READ_TIMEOUT` (5 s), `REMOTE_HANDSHAKE_READ_TIMEOUT` (60 s). |
| `endpoint_lane.rs` | runtime | One host's request/response correlation, chunk reassembly, 60 s expiry. |
| `connector.rs` | runtime | `FleetConnector`, `FleetEvent`, `HostCommand`, `FleetConnectorOptions`, `INACTIVE_SURFACE`; one supervisor thread per host. |
| `oneshot.rs` | runtime | `FleetSession`, `collect_status`, `watch` — driving all of it from a blocking caller. |

Every consumer resolves its host list through `hosts_source::hosts_for_config`,
never `hosts::resolve_hosts` directly, so `herdr fleet status` and the gateway
can never disagree about which machines are in the fleet.

The five pure modules import only `std`, `serde`, each other, `[fleet]`'s
config types, `crate::api::schema::AgentStatus` and `crate::protocol` data
types. A unit test in `src/fleet/mod.rs` fails the build if any of them grows a
`use` of `tokio`, `ratatui`, `interprocess`, `crate::ipc`, `crate::remote` or
`crate::client`. Keep it that way: `FleetState` must stay testable without a
socket.

Pure-state testing follows upstream's idiom: `FleetState::test_new()`,
`FleetState::test_with_adversarial_identity_state()` and
`FleetState::assert_invariants_for_test()`.

### Driving the connector with an active host

```text
FleetConnector::start(specs, options)
  → set_active(Some(host)) with the real ClientSurfaceSize
  → consume FleetEvent::Surface / SurfacePatch for the active host
      into ClientShellState::set_pane_surface / apply_pane_surface_patch
  → route HostCommand::PaneInput / Resize / Focus / Endpoint to the active host
  → shutdown() on exit
```

Switching hosts is one `set_active` call; the first frame afterwards is a full
`PaneSurface`. Frames from inactive hosts are dropped in the reader thread
before anything is allocated into the event channel, so an idle host in the
sidebar costs no presentation work.

Known gaps a consumer with an active host owns:

- `FleetConnectorOptions.active_surface` is fixed at `start`, so a *reconnect*
  of the active host re-handshakes with the size the connector was built with,
  not the last size sent through `HostCommand::Resize`. Add a setter (or have
  the connector remember the last active resize) when a consumer needs it.
- The ssh child's stderr is inherited unless `ssh_noninteractive` is set (see
  above) — a daemon or a TUI must set it.
- On unix, `SshStdioBridge::drop` joins its accept thread, and each bridged
  connection waits for its `ssh` child without killing it, so **dropping an ssh
  transport while a bridged stream is still open blocks until that child
  exits**. `SshTransport::connect`'s error path and its session retirement
  inherit this. The supervisor only ever reconnects after the previous stream
  died, so it does not bite today; a caller that drops a transport while holding
  a stream must know.

### Driving the connector (E3)

```text
FleetConnector::start(specs, FleetConnectorOptions::for_daemon(config))
  → set_active(None)                    // no active host, no geometry claimed
  → take_events()                       // the merged receiver, owned by one task
  → apply every FleetEvent::Host to FleetState::apply
  → broadcast the returned FleetChange values over /api/events
  → serve FleetStatusReport (herdr.fleet.status.v1) on /api/fleet
```

`FleetConnectorOptions::for_daemon` is the constructor a headless consumer
wants: everything `for_config` decides, plus `ssh_noninteractive: true`. Its
handshake is `read_only`, so every host sees `surface_active: false`.

`take_events()` hands the receiver to the caller by value, which is what a
`select!` loop needs while it still calls `send` on the connector. The caller
then owns closing it: **drop the receiver before calling
`FleetConnector::shutdown`**, or a supervisor parked on a full channel can only
exit once the shutdown wait has elapsed. `src/gateway/fleet.rs` does exactly
that — stop the fold task, then join the supervisors on a blocking thread.

Terminal streaming uses the per-host observe/control path, not this connector's
surfaces. `FleetStatusReport` and `FleetChange` are append-only: new fields are
optional, new enum variants need an `Unknown` fallback (or a skip) on the
reading side. `FleetChange::HostConnection` does not carry the advertised
`methods` list — read it from `FleetState::host` or the report, so a
method-gated action fails closed.

### Ordering

`FleetState::merged_agents()` is the one sanctioned fleet ordering:
`(status_rank, Reverse(fleet_change_seq), host_index, pane_id)` with
`status_rank` **blocked 0 < working 1 < done 2 < idle 3 < unknown 4**. It is
cached and recomputed only when a snapshot or a connection changes — never per
render.

It is deliberately different from upstream's per-host sidebar priority
(`status_priority`, which puts done above working). A *per-host* view should
keep upstream's order so it looks like upstream's sidebar; every *fleet*-wide
list uses `merged_agents()`.

### Testing locally

Both labs live in [`docs/fork/README.md`](./README.md): the
[fleet lab](./README.md#fleet-lab) gives you N local hosts, and the
[SSH lab](./README.md#ssh-lab) makes one of them reachable as a real ssh host,
with no root and no contact with your `~/.ssh`.

To exercise `include_machines` against the SSH lab, write the catalog under the
lab's own `XDG_STATE_HOME` so your real
`~/.local/state/herdr*/client/endpoints.json` is never touched:

```bash
bash scripts/fork/fleet-lab.sh up 2 && eval "$(bash scripts/fork/fleet-lab.sh env)"
bash scripts/fork/ssh-lab.sh up     && eval "$(bash scripts/fork/ssh-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"

export XDG_STATE_HOME="$HERDR_FLEET_LAB_ROOT/state"
mkdir -p "$XDG_STATE_HOME/herdr-dev/client"
ID=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
printf '{"version":1,"selected_profile":null,"ssh":[{"id":"%s","label":"Lab SSH","target":"herdr-ssh-lab","session":"lab-1","enabled":true}]}\n' "$ID" \
  > "$XDG_STATE_HOME/herdr-dev/client/endpoints.json"
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

HOME=$HERDR_SSH_LAB_HOME $H fleet status     # lab-2 (local) + lab-ssh (ssh, from "Lab SSH")
bash scripts/fork/ssh-lab.sh down; bash scripts/fork/fleet-lab.sh down
```
