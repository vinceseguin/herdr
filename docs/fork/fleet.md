# The Fleet console

Epic **E2**. `herdr fleet` opens one full-screen console over **every host in
your `[fleet]` config at once**: each host's workspaces and agents live in the
sidebar with live status, and one of them — the *active host* — fills the pane
area and receives everything you type.

It is upstream's client shell with N server connections behind it instead of
one. Nothing on the other side changes: a fleet host runs a **stock** herdr
server and speaks the frozen generation-1 endpoint protocol (see
[ADR 0001](./decisions/0001-servers-stay-stock-ssh-transport.md)).

Configuration — every `[fleet]` key, host kinds, the `host/w1:p1` id form,
connection states and SSH host setup — is [`fleet-core.md`](./fleet-core.md).
This page is the console.

- [Launching](#launching)
- [Reading the sidebar](#reading-the-sidebar)
- [Switching hosts](#switching-hosts)
- [What goes to which host](#what-goes-to-which-host)
- [Notifications](#notifications)
- [When a host drops](#when-a-host-drops)
- [Limits](#limits)
- [Walkthrough against the labs](#walkthrough-against-the-labs)
- [For developers](#for-developers)

## Launching

```bash
herdr fleet          # the console
herdr --fleet        # the same thing, as a flag
```

Both open the console over the hosts in `[fleet]`. `herdr fleet status` is
unchanged and still prints the read-only report; `herdr fleet --help` prints
the command's own help.

`--session <name>` keeps its ordinary global meaning — it names *this*
process's session, which is what the implicit `local` host points at:

```bash
herdr --session work fleet     # `local` is this machine's "work" session
```

The console needs at least one **enabled** host:

```console
$ herdr fleet
herdr: no fleet hosts enabled in [fleet]; see docs/fork/fleet-core.md
$ echo $?
1
```

An invalid `[fleet]` section reports each diagnostic the same way and exits 1.
Neither is a *host* failure — a host that is unreachable is data, drawn dimmed
in the sidebar, and never a reason to refuse the console. That is deliberate:
the terminal is set up **before** any host handshake, so the console opens at
once and hosts arrive as they connect.

`--fleet` cannot be combined with `--remote`; that is rejected with exit 2.

## Reading the sidebar

The sidebar keeps upstream's two sections — spaces above, agents below — and
each grows one **host group** per configured host, in config order with `local`
first. Four hosts, three local and one over SSH, with the console on `lab-ssh`:

```
 spaces

▾ lab-1 · no agents
  · lab-1
▾ lab-2 · no agents
  · lab-2
▾ lab-3 · no agents
  · lab-3
▾ lab-ssh · no agents
 · lab-1

 agents           grouped

▾ lab-1 · no agents
▾ lab-2 · no agents
▾ lab-3 · no agents
▾ lab-ssh · no agents
```

- The header is `▾`/`▸` (expanded/collapsed), the host name, then a `·`-joined
  detail: the non-zero agent counts (`2 blocked · 1 working`), or `no agents`,
  or `loading` for a connected host whose first snapshot has not arrived, or
  the connection summary (`connecting`, `reconnecting (attempt 2)`,
  `unavailable`, `incompatible`).
- The **active host's** header line is highlighted, and its rows are the
  ordinary client sidebar — worktree grouping, tokens, tab counts, every hit
  target you have today. (That is why `lab-ssh`'s workspace row above sits at a
  different indent from the other three: it is upstream's row, not a fleet
  one.) Other hosts contribute one line per workspace and one per agent, with
  the same status glyphs and colours.
- A host that is not connected is **dimmed** (unless it is the active host,
  whose header line stays highlighted), and its reason is a second dimmed line
  under the header. It keeps its last snapshot's rows, so you can still see
  what was running there. At the sidebar's minimum 25 columns that reason line
  is what runs out of room — sidebar text is cut hard, with no `…` — so the
  [pane-area notice](#when-a-host-drops) is the surface that carries host,
  state and reason in full.
- A host you are switching to shows `…` at the right of its header.
- **Click the first cell of a header** (`▾`/`▸`) to collapse or expand that
  group; click anywhere else on the header to make it active.

Agent rows inside a group follow upstream's own agent-panel order, including
your `[ui] agent_panel_sort` preference — a host group looks like today's
sidebar. Fleet-*wide* ordering (blocked first across machines) is a different
question, answered by `herdr fleet status` and the gateway, not by this
sidebar.

## Switching hosts

Three ways, all landing in the same place:

| | |
| --- | --- |
| Click a host header | that host becomes active |
| Click another host's workspace or agent row | that host becomes active **and** the row is focused there |
| `prefix+shift+f` | the host picker |

The picker lists every configured host with its state, version, agent counts
and transport:

```
 Fleet                                     3 hosts
──────────────────────────────────────────────────
 1 ● lab-1  connected 0.8.2-fork  no agents  local
 2   lab-2  connected 0.8.2-fork  no agents  local
 3   lab-3  connected 0.8.2-fork  no agents  local
 move j/k · jump 1-9 · switch enter · close esc
```

`↑`/`↓` or `j`/`k` move, `home`/`end` jump to the ends, `1`-`9` select the
n-th row, `enter` switches, `esc` closes, a click on a row switches, a click
outside closes. Pressing the binding again does *not* close it — an open
overlay swallows keys before prefix resolution, exactly as the navigator's
does. A digit **selects but does not switch** — the choice is confirmed with
`enter`, so a mistyped digit cannot move your next keystrokes to
another machine. A host that is down is selectable (you get its
[notice](#when-a-host-drops) instead of a pane); a host with `enabled = false`
is listed but inert. The active host is marked `●`.

The binding is `[fleet.keys] host_picker`, default `prefix+shift+f` — see
[`[fleet.keys]`](./fleet-core.md#fleet-keys) for the syntax and the precedence
rule. It is `prefix+shift+f` ("**F**leet") and **not** `prefix+shift+h`, which
is upstream's default `keys.swap_pane_left`.

A switch is deliberately abrupt: the old host's surface, hit map and pending
requests are dropped the moment the target changes, so the console is briefly
unclickable and the pane area says `switching to <host>…` until the new host's
first full frame arrives. That is the safe direction — a click resolved against
the wrong host would act on another machine.

## What goes to which host

Everything you do reaches the **active host**, and only the active host:

| | |
| --- | --- |
| Keystrokes, paste, mouse in the pane area | active host |
| Terminal resize | active host (inactive hosts stay at herdr's headless geometry) |
| Focus, scroll, copy mode, endpoint commands (rename, close, worktrees, …) | active host |
| Sidebar clicks on **another** host's rows | a host switch, then the focus request on the new host |

There is one write path in the console and it names a host explicitly; an event
whose host is not the active one is dropped before it can reach the shell. A
message can therefore only ever reach the host whose ids it carries — including
when a click and the keystrokes after it arrive in the same input batch.

Inactive hosts receive nothing you did not ask for. The only thing the console
sends them on its own is the resize that follows activation and deactivation.

## Notifications

A notification from any host is shown with its host in front of the title —
**every** host, the active one included:

```
┌─────────────────────────────────┐
│● [lab-2] codex needs attention  │
│  lab-2 · 1                      │
└─────────────────────────────────┘
```

`prefix+o` (and a click on the toast) opens the notification's target. When it
belongs to another host, the console switches to that host and focuses the pane
there once its projection arrives.

Two things are easy to trip over:

- **Toasts are off by default.** `[ui.toast] delivery` is `off` out of the box,
  so a fresh install shows nothing. Set `delivery = "herdr"` (in-app),
  `"terminal"` or `"system"` to see them.
- **Sound is unchanged.** It follows `[ui.sound]` exactly as in a single-host
  client; the fleet path does not gate it separately.

A host switch **clears the notifications on screen** — they are part of the
per-host state the switch resets. They are still delivered while you are on
another host; they are just not carried across the switch.

Notification validity is answered by the *sender's* host: a "still blocked?"
check for a host you are not looking at reads that host's own agent statuses,
never the active host's. Every server starts at `w1`/`w1:p1`, so an unqualified
id would otherwise be validated — or focused — on the wrong machine.

## When a host drops

A host failure is host-local. Its group goes dimmed with the reason, its rows
stay, and the rest of the console keeps working — you can read other hosts,
switch to them and type in them.

When the **active** host drops, the pane area is replaced by a one-line notice
instead of leaving a stale frame on screen:

```
 lab-1 · reconnecting · server is shutting down             ▾ lab-1 · unavailable
                                                              no herdr server for ses…
```

The forms are `<host> · connecting`, `<host> · reconnecting (attempt N)`,
`<host> · reconnecting · <reason>`, `<host> · unavailable · <reason>`,
`<host> · incompatible · <reason>` and `switching to <host>…`. `reconnecting`
is used for a host that had been connected before, `connecting` for a first
attempt.

While the notice is up:

- **Input is discarded, not queued.** Keystrokes, pastes and pane mouse events
  aimed at the pane area or a pane popup are dropped, and so are the two places
  input can sit waiting (the copy-mode replay queue and an in-flight mouse
  gesture). Input leases are left alone: they only ever produce a release for a
  key the host may already have seen the press of. Nothing
  you typed at a dead host is replayed into it when it comes back — or into
  whatever host is active by then.
- The pane area is unclickable, but the sidebar, the picker and the overlays
  stay live, so you can switch to a host that is up.
- The console does not exit, and never exits because of a host.

When the host comes back it repaints at the **current** terminal size — the
geometry the console announces to a host is the live one, so a reconnect
re-handshakes at the size the window has now, not the size it had at launch.

One cosmetic split worth knowing, visible in the dump above: the sidebar header
says `unavailable` where the pane-area notice says `reconnecting`. The header's
vocabulary is the connection state shared with `herdr fleet status` and the
picker; the notice's is what the console is doing about it. There is no
`retry in N s` countdown — a number that only updates when a change arrives
would be stale on screen.

## Limits

Fleet v1 is deliberately narrow:

- **No mixed-host layouts.** One tab never holds panes from two machines. The
  pane area is one host's; the sidebar is what aggregates.
- **No kitty graphics and no direct graphics.** The console handshakes each
  host without them, so images fall back the way they do on a terminal that
  lacks support.
- **Local keybindings only.** Your own keymap applies on every host;
  `--remote-keybindings server` has no fleet equivalent, and
  `HERDR_REMOTE_KEYBINDINGS` is ignored with a warning.
- **Run the console while you are using it, not as a daemon.** A connecting
  client becomes each host's *foreground* client, and the foreground client's
  surface is that host's effective pane geometry. Inactive hosts are held at
  herdr's default headless size, which makes the common case (a headless server
  running agents) a no-op — but a host that already has an attached client, or
  its own `[server] headless_cols`/`headless_rows`, is resized while the
  console is connected and restored when it quits.
- **SSH stderr goes to the herdr log, not to your screen.** herdr's ssh child
  inherits stderr, which in a CLI is what you want and in a full-screen console
  would paint over the TUI, so the console redirects fd 2 into
  `herdr-client.log` while it runs (unix) and restores it on exit — including
  from the panic hook. An ssh problem reaches you as a host *reason* in the
  sidebar; the raw text is in the log.

Quitting is `prefix+q` (upstream's `keys.detach`), as everywhere else — it
leaves every host's server running. Every exit path — quit, Ctrl-C, a
terminal hangup, a panic — shuts the connector down, so no ssh forward socket
or control master is left behind.

## Walkthrough against the labs

The [fleet lab](./README.md#fleet-lab) gives you N independent herdr servers on
this machine and the [SSH lab](./README.md#ssh-lab) makes one of them reachable
as a real ssh host. `scripts/fork/tui-drive.py` drives the console through a
real PTY. Everything below runs against throwaway sessions under the lab's own
`XDG_CONFIG_HOME`; it cannot reach your own herdr.

### Boot three hosts

```bash
cargo build
bash scripts/fork/fleet-lab.sh up 3
eval "$(bash scripts/fork/fleet-lab.sh env)"
H="env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV target/debug/herdr"
```

Point `[fleet]` at them. `include_local = false` matters: it keeps the console
away from your default session.

```bash
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

[[fleet.hosts]]
name = "lab-3"
kind = "local"
session = "lab-3"
EOF
$H fleet status
```

```console
client 0.8.2-fork  active host: none

HOST   KIND   STATE      VERSION     BLOCKED  WORKING  DONE  IDLE  UNKNOWN
lab-1  local  connected  0.8.2-fork  0        0        0     0     0
lab-2  local  connected  0.8.2-fork  0        0        0     0     0
lab-3  local  connected  0.8.2-fork  0        0        0     0     0

no agents
```

### Reading a `--dump`

Three things about `tui-drive.py` will otherwise waste an afternoon:

- **`--redraw` before `--dump`.** A TUI writes frame *diffs*, so without a
  forced full repaint (resize away, forget the text so far, resize back) a
  dump is a pile of partial updates rather than a screen. `--redraw` is an
  ordered step; `--dump` is not — it prints everything captured since the last
  `--redraw`, once the child has exited.
- **The dump is stripped text in draw order, not a rectangle.** The console
  draws region by region, so pane and sidebar text interleave. The sidebar is
  the right-hand 25 columns, which is enough to pull it back out — redirect the
  run's stdout to `dump.txt` first:

  ```bash
  sed 's/│/\n/g' dump.txt | sed -E 's/^.*(.{25})$/\1/' | sed 's/ *$//'
  ```

  Every sidebar excerpt on this page came out of that pipeline.
- **One `--expect` per frame, and only on text a frame writes contiguously.**
  An expectation matches only output that arrives *after* its step starts, so
  two expectations on the same frame always fail the second. And a sidebar
  label whose state word changed arrives as a cell diff, not as the whole
  string — expect something the pane area prints, or `--redraw` first.

### Open the console

```bash
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 30 \
  --expect 'herdr-fleet-lab:lab-1' --redraw --dump --keys '\x02q' \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
     HERDR_DISABLE_SOUND=1 target/debug/herdr fleet
```

```
 spaces

▾ lab-1 · no agents
  · lab-1
▾ lab-2 · no agents
  · lab-2
▾ lab-3 · no agents
  · lab-3
```

(The lab's marker panes are plain shells, not agents — hence `no agents` on a
host whose pane is very much alive.)

### Type into the active host, and prove it landed there

```bash
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 30 \
  --expect 'herdr-fleet-lab:lab-1' --keys 'echo from-console\r' \
  --expect 'from-console' --keys '\x02q' \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
     HERDR_DISABLE_SOUND=1 target/debug/herdr fleet
$H --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | grep -c from-console   # 1
$H --session lab-2 pane read "$HERDR_FLEET_LAB_PANE_2" --source recent | grep -c from-console   # 0
```

### Switch with the picker

```bash
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 30 \
  --expect 'herdr-fleet-lab:lab-1' --keys '\x02F' --expect 'connected' \
  --keys '3\r' --expect 'herdr-fleet-lab:lab-3' --keys 'echo via-picker\r' \
  --keys '\x02q' \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
     HERDR_DISABLE_SOUND=1 target/debug/herdr fleet
$H --session lab-3 pane read "$HERDR_FLEET_LAB_PANE_3" --source recent | grep -c via-picker   # 1
$H --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | grep -c via-picker   # 0
```

`\x02F` is `prefix` + `shift+f`. Adding `--redraw --dump` while the picker is
open is what produced the picker block
[above](#switching-hosts).

### Make a notification arrive from another host

Toasts are off by default, and the notification has to be triggered while the
console is running — so this one is two shells. Give the second one the same
`eval "$(bash scripts/fork/fleet-lab.sh env)"` and `H=…` as the first, or
`$HERDR_FLEET_LAB_PANE_2` will be empty.

```bash
printf '\n[ui.toast]\ndelivery = "herdr"\n' >> "$XDG_CONFIG_HOME/herdr-dev/config.toml"
```

In the first shell, open the console and wait for it:

```bash
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 45 \
  --expect 'herdr-fleet-lab:lab-1' --expect 'needs attention' --redraw --dump \
  --keys '\x02o' --expect 'herdr-fleet-lab:lab-2' --keys '\x02q' \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
     HERDR_DISABLE_SOUND=1 target/debug/herdr fleet
```

In the second, once it is up, push `lab-2`'s agent into `blocked`.
`pane report-agent` is the server's own semantic path: a transition into
`blocked` becomes a `NeedsAttention` notification carrying a workspace, tab and
pane id.

```bash
$H --session lab-2 pane report-agent "$HERDR_FLEET_LAB_PANE_2" \
  --source fleet-doc --agent codex --state blocked
```

The console — sitting on `lab-1` — draws the toast and updates `lab-2`'s group,
and `prefix+o` moves there:

```
┌─────────────────────────────────┐
│● [lab-2] codex needs attention  │
│  lab-2 · 1                      │
└─────────────────────────────────┘

▾ lab-2 · 1 blocked
  ● lab-2

switching to lab-2…
```

### Take a host down under the console

Same shape: the console waits in one shell, you stop a server in the other.

```bash
$H --session lab-1 server stop
```

```
 lab-1 · reconnecting · server is shutting down             ▾ lab-1 · unavailable
                                                              no herdr server for ses…
```

Typing at that notice reaches nothing: `pane read` on the other hosts shows no
trace of it, and lab-1 never sees it either when it returns. Start the server
again (`$H --session lab-1 server &`) and the console picks it up within a few
seconds; `$H fleet status` from the second shell is the easiest confirmation
that it is `connected` again.

### The same over SSH

```bash
bash scripts/fork/ssh-lab.sh up
eval "$(bash scripts/fork/ssh-lab.sh env)"
```

Add an ssh host pointing at lab-1's session:

```toml
[[fleet.hosts]]
name = "lab-ssh"
kind = "ssh"
target = "herdr-ssh-lab"
session = "lab-1"
```

and run the console with `HOME=$HERDR_SSH_LAB_HOME`, which is what makes the
alias resolvable without herdr reading your real `~/.ssh`:

```bash
python3 scripts/fork/tui-drive.py --cols 120 --rows 40 --timeout 60 \
  --expect 'herdr-fleet-lab:lab-1' --keys '\x02F' --expect 'lab-ssh' \
  --keys '4\r' --expect 'herdr-fleet-lab:lab-1' --keys 'echo over-ssh\r' \
  --expect 'over-ssh' --keys '\x02q' \
  -- env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_ENV \
     HOME=$HERDR_SSH_LAB_HOME HERDR_DISABLE_SOUND=1 target/debug/herdr fleet
$H --session lab-1 pane read "$HERDR_FLEET_LAB_PANE_1" --source recent | grep -c over-ssh   # 1
```

To exercise a real ssh **drop and recovery** (two shells again — the console
runs in the first), do not use `ssh-lab.sh down`: it deletes the lab root,
`HOME` included, so an already-running console's ssh then fails with
`Could not resolve hostname herdr-ssh-lab` for the rest of the run and can
never come back. Stop the lab's sshd in place instead — and take its
per-connection children with it, or the console's established session survives
the listener and nothing drops:

```bash
sshd_pid="$(cat "$HERDR_SSH_LAB_ROOT/sshd.pid")"
pkill -P "$sshd_pid"        # the per-connection sshd: this is the console's link
kill "$sshd_pid"            # the listener
```

```
 lab-ssh · reconnecting · host closed the connection        ▾ lab-ssh · unavailable
```

Bring it back with the lab's own config — the root, `HOME` and the ssh config
are all still there, so `ssh-lab.sh status` recognises the new listener as the
lab's own:

```bash
/usr/sbin/sshd -f "$HERDR_SSH_LAB_ROOT/sshd_config" -E "$HERDR_SSH_LAB_ROOT/sshd.log"
```

(`ssh-lab.sh` looks for `/usr/sbin/sshd`, then `/usr/bin/sshd`, then `sshd` on
`PATH`, and `HERDR_SSH_LAB_SSHD` overrides all three — use the same binary it
started.)

The console reconnects over ssh on its own and repaints at the current size:

```
▾ lab-ssh · no agents
 · lab-1
```

### Tear down

```bash
bash scripts/fork/ssh-lab.sh down     # ssh lab first: fleet-lab down deletes its root
bash scripts/fork/fleet-lab.sh down
```

Then, from a shell that has **not** eval'd the lab env, confirm nothing of
yours was touched:

```bash
herdr session list      # no lab-*
```

## For developers

The console is `run_client_loop` — upstream's one event loop — with its write
side widened:

```text
ClientLink { link: ServerLink, fleet_events: Option<Receiver<FleetEvent>>,
             fleet: Option<FleetClientState> }

ServerLink::Single(LocalStream)     the ordinary client
ServerLink::Fleet(FleetLink)        connector.send(&active, HostCommand::…)
```

Every write goes through `ServerLink`, and every inbound `FleetEvent` is
*translated* into the existing `ServerMessage` arms after a host check — so no
rendering or input arm is duplicated and a future upstream fix to the loop
still applies. `FleetState` lives in the loop; the shell sees only a pure,
pre-rendered `FleetSidebarModel`, rebuilt on change and never per frame.

| Module | What it owns |
| --- | --- |
| `src/client/link.rs` | `ClientLink`, `ServerLink`, `FleetLink`, `LinkWriteError` |
| `src/client/fleet.rs` | `run_fleet`, `FleetClientState`, event translation, `switch_host`, the stderr redirect |
| `src/client/shell/fleet.rs` | `FleetShellState`, the pane-area notice, `input_allowed`, fleet actions |
| `src/client/shell/fleet_sidebar.rs` | host groups in both sidebar sections |
| `src/client/shell/fleet_overlay.rs` | the host picker |
| `src/fleet/sidebar.rs` | the pure row model (`FleetSidebarModel`, `host_picker_rows`) |

`src/fleet/` itself — connector, state, transports — is
[`fleet-core.md`](./fleet-core.md#for-developers).

Testing: `src/client/shell/tests/fleet*.rs` are the shell-state tests (no
PTYs), `tests/fork_fleet_tui.rs` drives real lab servers through a PTY with
`tests/support/fleet_tui.rs`, and `just bench-fleet-scale` profiles the sidebar
at 1 vs 5 hosts × 15 agents.
