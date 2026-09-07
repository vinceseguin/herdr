# `herdr gateway` — the Herdr Fleet gateway

`herdr gateway` is a headless daemon that runs on **one** machine on your LAN,
connects to every herdr server the [fleet core](./fleet-core.md) knows about,
and serves that merged view over HTTP and WebSocket so a phone or a browser can
watch it. It is the fork's only network surface: phones cannot speak herdr's
unix-socket protocol and should not need an SSH key, so one gateway aggregates
the fleet and everything else talks to it.

```
 phone / browser ─── HTTP + WebSocket ──▶ herdr gateway ──▶ fleet core ──┬─▶ local session lab-1
                     (loopback or                                        ├─▶ local session lab-2
                      tailnet, token-gated)                              └─▶ ssh ──▶ workbox
```

Three properties hold everywhere in this document:

- **Servers are stock.** The gateway is an ordinary herdr *client*. Nothing on a
  host knows a gateway exists ([ADR 0001](./decisions/0001-servers-stay-stock-ssh-transport.md)).
- **The gateway is a passive reader.** It never becomes a host's foreground
  client, so watching a fleet does not resize anyone's panes. (Taking *control*
  of a pane is the one exception; see
  [client messages](#client-messages).)
- **Loopback first.** The default bind is `127.0.0.1:7788`; a non-loopback bind
  is refused unless you also say which browser origins may reach it.

Everything below is behind the `gateway` cargo feature, which fork builds enable
by default. `cargo build --no-default-features` produces an upstream-shaped
binary with no `gateway` command at all (`herdr gateway` then exits **2** with
`unknown command: gateway`).

---

## Quick start

```bash
# 1. tell the fleet which herdr servers to aggregate (see `[fleet]` below)
$EDITOR ~/.config/herdr/config.toml

# 2. run it (foreground; Ctrl-C stops it, as does SIGTERM on unix)
herdr gateway

# 3. from another shell: the tokens were generated on first run
curl -s -H "Authorization: Bearer $(cat ~/.config/herdr/gateway/read.token)" \
     http://127.0.0.1:7788/api/fleet | head -c 200

# 4. pair a phone (prints a one-time URL and a QR code)
herdr gateway pair
```

`herdr gateway status` tells you whether one is running, and
[the systemd unit](#running-it-under-systemd) keeps it running.

---

## Configuration

The gateway reads `config.toml` like the rest of herdr. Two sections matter:
`[fleet]` decides *which hosts* it aggregates, `[gateway]` decides *how it is
served*. Both live in the same file, and `herdr config check` reports a bad
value in either.

### `[gateway]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `bind` | string | `"127.0.0.1:7788"` | Listen address, `ADDRESS:PORT`. `--bind` overrides it for one run. |
| `allowed_origins` | array of string | `[]` | Browser origins (`scheme://host[:port]`) a browser may reach this gateway from. **A non-loopback bind needs at least one of this or `public_url`.** A loopback bind implicitly allows its own origins. |
| `public_url` | string | `""` | The origin pairing URLs and QR codes advertise — E5 sets it to the `https://<host>.<tailnet>.ts.net` origin. Implicitly allowed; an `https` scheme also marks device cookies `Secure`. |
| `auth_failure_limit` | integer | `5` | Failed authentications from one peer address before it is refused for the window. |
| `auth_failure_window_secs` | integer | `60` | Length of that window, in seconds. |
| `pairing_ttl_secs` | integer | `600` | How long a pairing URL stays valid; accepted range `30..=86400`. |

```toml
[gateway]
bind = "127.0.0.1:7788"
allowed_origins = ["https://gateway.tailnet-name.ts.net"]
public_url = "https://gateway.tailnet-name.ts.net"
auth_failure_limit = 5
auth_failure_window_secs = 60
pairing_ttl_secs = 600
```

**A `[gateway]` value of the right TOML type is never fatal** — a value that
fails validation becomes a diagnostic and the documented default is used
instead, because the same config can be valid under a different `--bind`. The
exact diagnostics:

```text
invalid gateway bind: gateway.bind = "…"; expected ADDRESS:PORT (for example 127.0.0.1:7788); using 127.0.0.1:7788
invalid gateway origin: gateway.allowed_origins[0] = "…"; <reason>; ignoring this origin
duplicate gateway origin: gateway.allowed_origins[1] = "…" repeats "…"
invalid gateway public url: gateway.public_url = "…"; <reason>; ignoring it
invalid gateway auth failure limit: gateway.auth_failure_limit = 0 would refuse every request; using 5
invalid gateway auth failure window: gateway.auth_failure_window_secs = 0 disables rate limiting; using 60
invalid gateway pairing ttl: gateway.pairing_ttl_secs = 10; expected 30..=86400; using 600
```

A value of the **wrong TOML type** is a different story, because the whole file
is deserialized in one pass: `bind = 7788` makes `herdr gateway` (and
`herdr config check`) print `config parse error: <error>; using defaults` and
fall back to defaults for **every** section, `[fleet]` included — not just
`[gateway]`. Fix the type before reading anything else the run says. (The
per-section `invalid gateway config: <error>; keeping current gateway settings`
wording belongs to the running server's live config reload, not to this path.)
An unknown key inside the section yields
`unknown config key gateway.<key>; ignoring key`. Note that
`auth_failure_limit = 0` and `auth_failure_window_secs = 0` fall back to the
defaults rather than *disabling* rate limiting: there is no way to turn the
limiter off from the config file.

**Origins are compared the way browsers serialize them** (RFC 6454 §6.1): the
scheme and the host both fold case (but `http` never matches `https`), and a
scheme's default port disappears —
so `HTTPS://Fleet.Example:443` matches the `https://fleet.example` a browser
actually sends, and `[0:0:0:0:0:0:0:1]` canonicalizes to `[::1]`. Port `0`, a
signed port, a non-ASCII host (use punycode), userinfo, a path, a query, a
fragment, `null` and any non-`http(s)` scheme are refused with a diagnostic.

### `[fleet]`

The gateway aggregates exactly the hosts `herdr fleet status` shows. The full
reference is [`fleet-core.md`](./fleet-core.md); the keys that matter here:

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `include_local` | bool | `true` | Include this machine's *default* session as the host `local`. |
| `include_machines` | bool | `false` | Also include every machine saved with `herdr machine add`, as an ssh host whose id is derived from its label. |
| `[[fleet.hosts]]` | array of tables | `[]` | Explicit hosts. |

```toml
[fleet]
include_local = false      # a dedicated gateway box usually has no local session worth showing
include_machines = true    # reuse `herdr machine add` instead of listing hosts twice

[[fleet.hosts]]
name = "lab-1"             # the host id; `lab-1/w1:p1` names a pane on it
kind = "local"             # "local" | "ssh"  (default "ssh")
session = "lab-1"          # required for kind = "local"

[[fleet.hosts]]
name = "workbox"
kind = "ssh"
target = "workbox"         # required for kind = "ssh"; an ssh destination
session = "agents"         # optional: a named session on that host
enabled = true             # default true; false reports the host `unavailable`, reason `host disabled in [fleet]`
```

`name = "local"` is reserved for `include_local`, and two hosts (or a machine
profile folding to the same id as a `[[fleet.hosts]]` entry) that claim one id
is a diagnostic naming both, not a silent win.

---

## Running it

```text
herdr gateway [--bind ADDR] [--config PATH]
herdr gateway pair [--control] [--ttl-secs N] [--label TEXT] [--no-qr] [--invert] [--json]
herdr gateway status [--json]
herdr gateway rotate-token <read|control>
```

```console
$ herdr gateway --bind 127.0.0.1:7788
listening on http://127.0.0.1:7788
```

`stdout` carries that one line and nothing else, so a supervisor can wait for
it. Everything else goes to **stderr**: the runtime's `tracing` events, plus a
handful of plain `eprintln!` lines for config diagnostics, a refused bind and a
token or device store the gateway will not touch — those last ones are *not*
governed by `HERDR_LOG`.

```text
2026-09-06T23:46:50.493006Z  INFO gateway: gateway listening listen=127.0.0.1:7788
```

Set `HERDR_LOG` to change the filter (default `herdr=info,gateway=info`); the
gateway's own events use the target `gateway`, so `HERDR_LOG=gateway=debug`
turns on per-request lines without the rest of herdr.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Clean stop (SIGINT/SIGTERM), a printed help, or a successful `pair`/`status`/`rotate-token`. |
| `1` | The gateway **refused** or could not serve: bad `[fleet]` config, a bind the policy rejects, an address already in use, a token or device file it does not trust, a rotation that did not change the file, no address to advertise in a pairing URL, or a serve error (`gateway stopped: …`). |
| `2` | Usage error (unknown option or subcommand, malformed `--bind`, `rotate-token` without a valid scope). |
| `3` | `herdr gateway status` when **no gateway is running**. |

### Where the files live

The token store follows `XDG_CONFIG_HOME`, **not** `--config`. In herdr the
config *directory* and the config *file path* are separate: `--config PATH` (and
`HERDR_CONFIG_PATH`) relocate only the file that is read, so a gateway started
with `--config /etc/herdr/gateway.toml` still keeps its tokens under
`~/.config/herdr/gateway/`. `pair`, `status` and `rotate-token` do not take
`--config` at all for the same reason — they find the store through
`XDG_CONFIG_HOME` and the running gateway's own marker file. (`pair` and
`status` do still *read* a config file for the base URL and the default TTL,
but only the default one or `HERDR_CONFIG_PATH`; `rotate-token` reads none.)

```text
<config>/gateway/                 0700, owned by you
├── read.token                    0600 — 64 hex characters, the `read` bearer token
├── control.token                 0600 — the `control` bearer token
├── devices.json                  0600 — paired devices (ids, scopes, labels, secret digests — never a secret)
├── gateway.json                  0600 — {pid, listen, started_unix} of the running gateway
└── pairings/                     0700 — one file per outstanding pairing code, created on first `pair`
```

Every write is atomic (a `0600` temp file in the same directory, `fsync`, then
rename), so a file is never briefly world-readable. A `<config>/gateway/`
directory **you own** with a wider mode is quietly tightened back to `0700` on
startup; anything the gateway cannot make safe by itself is a refusal (exit 1)
rather than a repair:

```text
cannot use the gateway token store: /…/gateway is owned by another user
cannot use the gateway token store: /…/gateway/read.token is readable by group or other; run chmod 600 on it or delete it to regenerate
cannot use the gateway token store: /…/gateway/read.token is not 64 hexadecimal characters; delete it to regenerate
cannot use the gateway token store: /…/read.token and /…/control.token hold the same token, so the read token would grant control; delete one of them to regenerate it
cannot read the paired devices: /…/gateway/devices.json is readable by group or other; run chmod 600 on it or delete it to regenerate
```

The token store is created and checked first, so a `<config>/gateway/` owned by
somebody else is always reported under `cannot use the gateway token store:`,
never under `cannot read the paired devices:`.

On Windows the mode checks are no-ops (as they are for herdr's own sockets) and
log at `debug`.

### The passive-reader guarantee

A herdr server sizes its panes for its **foreground** client. The gateway sends
`surface_active: false` in its handshake for every host, so a host keeps
rendering at its configured geometry while the gateway watches — attaching a
gateway does not resize anybody's panes, and detaching it does not resize them
back. Verified against the lab: a pane reporting `40 120` from `stty size` still
reports `40 120` with the gateway attached.

This covers the fleet connection and every `mode: "observe"` terminal. A
`mode: "control"` terminal is a real attach and *does* resize the pane's PTY to
the browser's `cols`/`rows`, exactly as `herdr terminal session control` would
— that is the point of control mode, not a leak in the guarantee.

**Residual:** a host running a herdr server older than upstream #3670 does not
understand `surface_active` and will treat any client as foreground. The fix is
to upgrade that host; the fork does not work around it.

### SSH hosts in daemon mode

An `ssh` host is reached by an `ssh` child running herdr's stdio bridge. Because
a gateway has no terminal to prompt at, its bridges are **noninteractive**:
`BatchMode=yes`, `NumberOfPasswordPrompts=0`, `StrictHostKeyChecking=yes`, and
the child's stderr is discarded rather than painted over your daemon log. An ssh
host that would have asked for a passphrase surfaces as
`connection.state == "unavailable"` with a reason in `/api/fleet`, never as a
prompt on the daemon's terminal.

Two consequences worth knowing:

- **The gateway's ssh forward sockets are scoped.** They are named
  `<tmpdir>/herdr-remote-<pid>-gateway-<host>-<target>-<session>.sock` so they
  never collide with the fleet connector's own
  `<tmpdir>/herdr-remote-<pid>-<host>-…` socket. `<tmpdir>` is the platform
  temp dir (`/tmp` on a stock Linux box), and when that readable name would
  exceed a unix socket's `sun_path` limit the runtime falls back to a short
  hashed `herdr-r-<pid>-gateway--<target>-<hash>.sock` form instead; Windows
  always uses the short form. They appear when the first terminal on that host
  opens and are removed on shutdown.
- **An ssh host serves one terminal stream at a time.** The bridge handles one
  connection inline, so a second terminal on the same ssh host waits 2 s for the
  first to release and is then refused with `terminal.error {code:"host_busy"}`
  and close `1013`. Local hosts have no such limit — three concurrent observers
  on one local pane all get frames. (This is a property of the ssh bridge in
  `src/remote/`, which E3 deliberately does not modify.)

Herdr's *discovery* probes (the `uname -s` and binary check that run before a
bridge starts) still use an interactive `ssh`, because they go through upstream
code the fork freezes for this epic. They pipe both stdout and stderr, so
nothing is painted on a daemon's terminal — but a host that cannot authenticate
without a prompt stalls **there**, before the noninteractive bridge is ever
started, and blocks that host's own supervisor thread until ssh gives up. Only
that one host is affected; the rest of the fleet keeps serving.

---

## Tokens, scopes and devices

Two independent tokens are generated on first run, 32 random bytes each printed
as 64 hex characters:

| Scope | File | May do |
| --- | --- | --- |
| `read` | `read.token` | `GET /api/fleet`, `GET /api/gateway`, `/api/events`, and **observe** terminals. |
| `control` | `control.token` | Everything `read` may do, plus **control** terminals: `terminal.input`, a real PTY `terminal.resize`, `terminal.scroll`, `takeover`. |

`control` implies `read`. A `read` credential can never send a byte into a pane:
the scope is checked when the terminal session is constructed (a
`(control mode, read scope)` session is not constructible at all) *and* re-checked
on **every** message, because a handshake-time check cannot police what a
long-lived socket does later. A control-capable credential that opened a session
in `mode: "observe"` also cannot type — the mode wins, so a phone watching
read-only cannot inject by accident.

Present a token as a bearer header:

```http
Authorization: Bearer <token>
```

The scheme is matched case-insensitively; a query-string token is ignored, and a
bad bearer is never "upgraded" by also sending a cookie. Comparison is SHA-256
plus a constant-time compare, always evaluated against both scopes' digests so
the timing says nothing about which one you nearly matched.

The alternative credential is a **device cookie**, minted by pairing:

```http
Cookie: herdr_gateway_device=<id>.<secret>
```

Devices are recorded in `devices.json` by id, scope, label and a digest of their
secret. The gateway updates a device's `last_seen` in memory and persists it at
most once a minute.

### Rotation, revocation, and no restart

```console
$ herdr gateway rotate-token read
rotated the read token; revoked 1 device and 3 pending pairing codes
the running gateway applies this on its next request; no restart is needed.
```

(The second line is printed only when a gateway is actually running.)

`rotate-token` revokes three things, in this order: the **token file**, then
that scope's **pending pairing codes**, then that scope's **devices**. The
pairing codes matter — a code still outstanding when you rotate would otherwise
mint a device carrying exactly the authority you just took back.

It really does take effect without a restart. Both stores remember the length,
mtime and (on unix) inode of the file they were read from, and re-`stat` it
before a comparison: two `stat`s on a request that presents a bearer token, one
on a request whose cookie is rejected and two on one that is accepted (the
`last_seen` touch re-checks), and a full re-read only when a stamp changed.
`rotate-token` runs in a *separate process*, so this is what makes the
running gateway stop honouring a revoked cookie — and stop writing the revoked
records back the next time it persists `last_seen`. The log says so:

```text
INFO gateway: reloaded the gateway token store after it changed on disk
INFO gateway: reloaded the paired devices after the file changed on disk
```

Live evidence, with the gateway untouched between the two lines:

```console
$ curl -s -b jar -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/api/fleet   # the revoked device's cookie
401
$ curl -s -H "Authorization: Bearer $(cat …/gateway/read.token)" -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/api/fleet
200
```

Two accepted behaviours to know about:

- **A store that cannot be *reloaded* fails open.** If the file becomes
  unreadable or corrupt while the gateway is running, the credentials already in
  memory stay in force and the gateway logs
  `WARN gateway: could not reload the gateway token store; the tokens already in memory stay in force`
  (or, for the device half,
  `could not reload the paired devices; the records already in memory stay in force`).
  Refusing every client because
  a file blipped is worse, and anyone who can corrupt that file already has
  write access to the gateway directory. Any *further* change is retried.
  A store that is bad at **startup** is still a hard refusal (exit 1).
- **On Windows a file stamp has no inode**, and a token file is always 64 bytes,
  so two rotations inside one mtime tick could be missed. Rotate once, or
  restart the gateway.
- **Rotating does not end a live session.** Scope is fixed at the terminal
  handshake, so a control session that is already open keeps its authority until
  it closes.

There is no revoke-*one*-device command yet; `rotate-token <scope>` revokes all
of that scope's devices at once.

---

## Pairing a phone

```console
$ herdr gateway pair
warning: this URL points at loopback, so only a browser on this machine can open it; set [gateway] public_url for a phone
Pair this device with Herdr Fleet (read scope, valid 10 min):
  http://127.0.0.1:7788/pair?code=<id>.<secret>
<a 31-line QR code of that URL>
If a scanner cannot read this code, re-run with --invert (dark terminals).
The link works once and then expires.
```

(The first line is the loopback warning below; it is absent once
`[gateway] public_url` is set.)

Open the URL (or scan the QR) on the phone. The gateway answers `303` with
`Location: /` and sets the device cookie, so the phone lands on the app already
authenticated and the secret never has to be typed.

| Flag | Effect |
| --- | --- |
| `--control` | Pair with the `control` scope instead of `read`. |
| `--ttl-secs N` | How long the link stays valid, `30..=86400` (default `pairing_ttl_secs`). |
| `--label TEXT` | Name the device in `herdr gateway status`. The label travels *with the code*, because the device record is created when the code is redeemed — on a machine you are not typing at. |
| `--no-qr` | Print only the sentence, the URL and the note (3 lines). |
| `--invert` | Invert the QR code. A QR code is dark modules on a light field and a terminal draws blocks in its **foreground** colour, so the default is right on a light terminal and unscannable on a dark one. |
| `--json` | `{"expires_unix":1788738…,"scope":"read"\|"control","url":"…"}` and nothing else — keys sorted, one line. |

**Which base URL the link uses**, in order:

1. `[gateway] public_url`, if set — this is what E5's `tailscale serve` origin
   goes in, and the only way a phone off this machine gets a working link.
2. Otherwise, the **running gateway's own bound address** from `gateway.json` —
   the only source that knows the port after `--bind 127.0.0.1:0`.
3. Otherwise `[gateway] bind`.

A wildcard address or port `0` with no gateway running is a refusal naming
`public_url`, never a URL that cannot work. A loopback base prints this warning
first, because only a browser on this machine can open the link:

```text
warning: this URL points at loopback, so only a browser on this machine can open it; set [gateway] public_url for a phone
```

### `GET /pair?code=<id>.<secret>`

| Outcome | Answer |
| --- | --- |
| Valid code | `303`, `Location: /`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, and `Set-Cookie: herdr_gateway_device=<id>.<secret>; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000` |
| Unknown, malformed, or already redeemed | `403 {"error":"pairing_invalid","message":"that pairing link is not valid"}` |
| The window passed | `403 {"error":"pairing_expired","message":"that pairing link has expired; ask for a new one"}` |
| No `?code=`, an empty one, or a query string that will not parse | `400 {"error":"pairing_required","message":"open the pairing link \`herdr gateway pair\` printed"}` |
| Peer over the failure limit | `429` with `Retry-After` |

`; Secure` is appended to the cookie when `[gateway] public_url` is `https`, or
when the request carried `X-Forwarded-Proto: https` **and** the peer is loopback
(which is how `tailscale serve` reaches it). `Location` is the literal `/`, so
the endpoint can never become an open redirect. `SameSite=Strict` is what stops
a cross-site request from carrying the cookie, and it is load-bearing: browsers
omit `Origin` on navigations and `<img>` loads, so the origin check alone would
not see them.

A code is single-use, and only success or expiry deletes it — a *wrong secret*
never deletes a valid code, and every failure except expiry collapses to
`pairing_invalid`, so guessing an id learns nothing. Because `consume` compares
the secret in constant time *before* it looks at the clock, only the holder of
the whole code can tell "expired" from "invalid". Bad and expired codes count
against the failure limiter; a request with no usable `?code=` at all does not.

One thing to expect: **a link previewer burns the link.** A one-time `GET` URL
pasted into a chat app that fetches previews will be redeemed by the previewer,
not by you. Type it, scan it, or use `--json` and open it directly.

---

## `herdr gateway status`

```console
$ herdr gateway status
gateway:  running (pid 246143)
listen:   127.0.0.1:7788
health:   ok
devices:  read 1, control 0
pairings: 3 pending
$ echo $?
0
```

```console
$ herdr gateway status --json
{"devices":{"control":0,"read":1},"healthy":true,"listen":"127.0.0.1:7788","pairings_pending":3,"pid":246143,"running":true,"schema":"herdr.gateway.status.v1"}
```

- `running` is "the pid recorded in `gateway.json` is alive" — that is how a
  marker left behind by a crash is refused rather than believed.
- `healthy` is a **real** `GET /health` against the recorded address (a
  wildcard bind is dialled on loopback). The 2 s bound is applied to the
  connect, to the write and to the read as a deadline, so a pathological
  network can stretch the whole probe to about twice that; it is not one
  end-to-end budget.
- Exit **3**, and `gateway:  not running`, when nothing is running. Every other
  successful run exits 0.

---

## The HTTP contract

Every JSON answer carries `Content-Type: application/json`,
`Cache-Control: no-store` and `X-Content-Type-Options: nosniff`. No `Server`
header is sent. The request body limit is 64 KiB.

| Route | Method | Auth | Answer |
| --- | --- | --- | --- |
| `/health` | `GET` | none | `200 {"ok":true}` — carries no fleet fact, so it is safe to expose to a supervisor. |
| `/api/gateway` | `GET` | `read` | What this gateway is and what the caller's credential is. |
| `/api/fleet` | `GET` | `read` | The `herdr.fleet.status.v1` report — the same JSON `herdr fleet status --json` prints. |
| `/api/events` | `GET` (upgrade) | `read` | WebSocket, [below](#websocket-apievents). |
| `/api/terminal/{host}/{pane}` | `GET` (upgrade) | `read` at the route | WebSocket, [below](#websocket-apiterminalhostpane). |
| `/pair` | `GET` | public, rate-limited | [Pairing](#get-paircodeidsecret). |
| everything else | `GET`/`HEAD` | public | The embedded web app. |

```console
$ curl -s -i http://127.0.0.1:7788/health
HTTP/1.1 200 OK
content-type: application/json
cache-control: no-store
x-content-type-options: nosniff
content-length: 11

{"ok":true}
```

```console
$ curl -s -H "Authorization: Bearer $READ" http://127.0.0.1:7788/api/gateway
{"client_version":"0.9.0-fork","features":["fleet","events","terminal","pairing"],
 "loopback":true,"public_url":"","schema":"herdr.gateway.info.v1","scope":"read","via":"bearer"}
```

`via` is `"bearer"` or `"device"`; a device principal also gets
`"device": {"id": "<64 hex>", "label": "<label>"}`, unless the record was
revoked between the credential check and the lookup, in which case the key is
simply absent. `features` is how a client
discovers what this gateway can do — names are only ever **appended**, so a
client must treat an unknown name as "ignore" and a missing name as "that
feature is not available here", never as an error.

### `GET /api/fleet`

The body is `FleetStatusReport`, schema `herdr.fleet.status.v1` — identical to
`herdr fleet status --json`, so one reader serves both. Real output from the
lab, two local hosts, abbreviated after the first host:

```json
{
  "schema": "herdr.fleet.status.v1",
  "client_version": "0.9.0-fork",
  "active_host": null,
  "hosts": [
    {
      "id": "lab-1",
      "kind": "local",
      "target": null,
      "session": "lab-1",
      "enabled": true,
      "connection": { "state": "connected", "server_version": "0.9.0-fork" },
      "boot_id": "245261-1788738396437057666",
      "revision": 1,
      "counts": { "blocked": 0, "working": 0, "done": 0, "idle": 0, "unknown": 0 },
      "workspaces": [
        { "ref": "lab-1/w1", "workspace_id": "w1", "label": "lab-1",
          "agent_status": "unknown", "focused": true }
      ]
    }
  ],
  "agents": [],
  "counts": { "blocked": 0, "working": 0, "done": 0, "idle": 0, "unknown": 0 }
}
```

- `active_host` is always `null` from a gateway: a gateway has no "current" host.
- `connection.state` is one of `connecting {attempt}`, `connected
  {server_version}`, `unavailable {reason, retry_in_ms}`, `incompatible
  {generation, reason}`, or `unknown` — a reader **must** treat an unrecognized
  state as `unknown` rather than failing.
- An unreachable host is **data**, not an error: `/api/fleet` still answers
  `200` with that host marked `unavailable`.
- Every `agents[]` entry carries `ref` (`host/w1:p1`), `host`, `pane_id`,
  `workspace_id`, `tab_id`, `workspace_label`, `name`, `title`, `agent`,
  `display_agent`, `agent_status`, `state_change_seq`, `fleet_change_seq` and
  `focused`. The list is merged across hosts, blocked first.

### Errors

Every failure is `{"error":"<snake_case_code>"}` plus an optional `"message"`
and, for `forbidden`, `"needed"`.

| Code | Status | When |
| --- | --- | --- |
| `unauthorized` | `401` | No credential, or one that did not verify. Also sends `WWW-Authenticate: Bearer realm="herdr gateway"`. |
| `forbidden` | `403` | The credential is valid but its scope is too narrow; `"needed"` names the scope. |
| `origin_not_allowed` | `403` | The `Origin` is not on the allowlist, there was more than one `Origin` header, it was not visible ASCII, or a device cookie arrived with no `Origin` on a request whose `Sec-Fetch-Site` is `cross-site` **or** `same-site`. |
| `too_many_requests` | `429` | The peer is over the failure limit. Sends `Retry-After` (never `0`). |
| `not_found` | `404` | An unknown `/api/…` path, or any path that looks like a file (its last segment has an extension) and is not in the embedded app. |
| `method_not_allowed` | `405` | A non-`GET`/`HEAD` method on the asset route, *after* a credential was accepted — see the note below. |
| `pairing_required` | `400` | `/pair` with no usable `?code=`. |
| `pairing_expired` / `pairing_invalid` | `403` | See [pairing](#get-paircodeidsecret). |
| `internal_error` | `500` | The gateway's fault, not the client's. |

A non-`GET`/`HEAD` method is never treated as public, so the credential check
runs first: `POST /` with no token is `401 unauthorized`, and only a request
that authenticates reaches the `405`. On `/api/…` and `/health` an
authenticated non-`GET` gets the router's own bare `405` — `Allow: GET,HEAD`,
an **empty** body, and none of the JSON envelope's headers.

```console
$ curl -s -D- -o/dev/null http://127.0.0.1:7788/api/fleet | grep -i www-authenticate
www-authenticate: Bearer realm="herdr gateway"
$ curl -s http://127.0.0.1:7788/api/fleet
{"error":"unauthorized"}
$ curl -s -H "Origin: https://evil.example" -H "Authorization: Bearer $READ" http://127.0.0.1:7788/api/fleet
{"error":"origin_not_allowed"}
$ curl -s -H "Authorization: Bearer $READ" http://127.0.0.1:7788/api/nope
{"error":"not_found"}
```

### The embedded web app

The app is compiled into the binary with `include_bytes!` over `web/dist`, so
there is **no filesystem lookup at request time** — a crafted path cannot escape
anything, because there is nothing to escape into. `/` and any extension-less
path serve `index.html` (SPA fallback); a path whose last segment *has* an
extension is looked up in the embedded table and is `404` when it is not there,
whether or not the extension is one the content-type table knows (an unknown
extension that *is* in the table is served `application/octet-stream`); a path
under `/api/` that reached the fallback answers JSON `not_found`,
never the HTML shell an API client could not tell from a real reply.
`index.html` is served `Cache-Control: no-cache, private`, other assets
`private, max-age=3600`.

```console
$ curl -s -o /dev/null -w '%{http_code} %{content_type}\n' http://127.0.0.1:7788/
200 text/html; charset=utf-8
$ curl -s -o /dev/null -w '%{http_code} %{content_type}\n' http://127.0.0.1:7788/nope.png
404 application/json
```

Until E4 lands, `web/dist/index.html` is a placeholder shell. Rebuilding it
re-runs `build.rs`, so a `cargo build` picks it up.

### Origin policy and CORS

`allowed_origins` is an **allowlist, not CORS**: the gateway sends no CORS
headers, so a page served from a *different* allowed origin still cannot call
`/api/*` (its preflight `OPTIONS` is a non-`GET` on an `/api/` path, which needs
a credential and gets `401`). The app must be same-origin — embedded, or behind
`public_url`.

A loopback bind implicitly allows its own origins — `http://127.0.0.1:<port>`,
`http://localhost:<port>`, `http://[::1]:<port>` and the bound address itself
(which matters for a bind like `127.0.0.53:7788`), each canonicalized, so a
bind on port 80 matches the portless `http://localhost` a browser sends. A
non-loopback bind gets none, so `allowed_origins` or `public_url` is the whole
allowlist. A request with **no** `Origin` header passes the origin step
entirely and is decided by its credential alone — only browsers send `Origin`,
and that is exactly why the device cookie is `SameSite=Strict` and why a cookie
with no `Origin` on a request whose `Sec-Fetch-Site` is `cross-site` or
`same-site` is refused.

The origin check runs **before** the public-path check, so a foreign `Origin`
is `403 origin_not_allowed` on `/health` and on a static asset too — only the
credential and rate-limit steps are skipped for public paths.

A non-loopback bind with neither is refused before anything is opened:

```console
$ herdr gateway --bind 0.0.0.0:7788
refusing to bind 0.0.0.0:7788: [gateway] allowed_origins is empty, so any web page
could drive this gateway. Set [gateway] allowed_origins (or public_url) to the origins
that may reach it, or bind a loopback address such as 127.0.0.1:7788.
$ echo $?
1
```

### Failure rate limiting

A peer address that presents `auth_failure_limit` (default 5) **invalid**
credentials inside `auth_failure_window_secs` (default 60) is refused with `429`
and a `Retry-After` until the oldest failure ages out.

Only a *presented* credential that fails counts. A request with **no**
credential does not, and neither does an origin refusal — a hostile page can
make your browser send requests to `127.0.0.1` but cannot read the answers, so
counting those would let any web site lock you out of your own gateway. A
**successful** authentication deliberately does not reset the window either: a
valid `read` token must not become an oracle for guessing the `control` one.

```console
$ for i in 1 2 3 4 5 6; do curl -s -o /dev/null -w '%{http_code} ' -H 'Authorization: Bearer nope' http://127.0.0.1:7788/api/fleet; done; echo
401 429 429 429 429 429
$ curl -s -D- -o/dev/null -H 'Authorization: Bearer nope' http://127.0.0.1:7788/api/fleet | grep -iE '^HTTP|retry-after'
HTTP/1.1 429 Too Many Requests
retry-after: 11
$ curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7788/health
200
```

(That transcript starts with four failures already inside the window, which is
why it blocks on the second request rather than the sixth. `/health` skips the
limiter, so it keeps answering — it does not skip the *origin* check.) While a
peer is blocked, even a valid token gets `429`.

At most 4096 peers are tracked; when the table is full an **unblocked** peer is
evicted before a blocked one, so a flood of fresh addresses has to earn 4096
blocks of its own before it can start clearing a victim's counter. That is a
cost, not a guarantee — an attacker with unlimited addresses never needs the
evicted one back.

---

## WebSocket: `/api/events`

`herdr.fleet.events.v1`. Needs the `read` scope; a missing credential is `401`
and a foreign origin `403` at the HTTP handshake, before any upgrade.

The stream is **self-contained**: it opens with a hello and a full report, so a
client never has to reconcile a `GET /api/fleet` taken at a different moment
with the deltas that arrived meanwhile. Subscribing and snapshotting happen
under one lock, so a change is either in that report **or** on the stream, never
in both and never in neither.

```console
$ python3 scripts/fork/ws-client.py ws://127.0.0.1:7788/api/events \
    -H "Authorization: Bearer $READ" --max-messages 2
text {"kind":"hello","schema":"herdr.fleet.events.v1","client_version":"0.9.0-fork","scope":"read"}
text {"kind":"fleet","report":{"schema":"herdr.fleet.status.v1","client_version":"0.9.0-fork","active_host":null,"hosts":[…]}}
```

Then one message per change, each a newline-free JSON object tagged by `kind`:

| `kind` | Fields | Meaning |
| --- | --- | --- |
| `hello` | `schema`, `client_version`, `scope` | First message. `scope` is the credential's proven scope. |
| `fleet` | `report` | A whole `herdr.fleet.status.v1` report. |
| `host_connection` | `host`, `connection` | A host changed state (the `connection` object of `/api/fleet`). |
| `snapshot` | `host`, `boot_id`, `revision` | That host re-sent its whole view; `boot_id` changing means it restarted. |
| `agent_added` | `agent` | An agent pane joined, or rejoined with fresh recency — treat it as an **upsert** keyed by `agent.pane`. |
| `agent_removed` | `pane` | `host/w1:p1`. |
| `agent_status` | `pane`, `from`, `to` | A status transition. |
| `active_host` | `host` | Not emitted by a gateway (it has no active host). |
| `resync` | — | You fell behind; a fresh `fleet` follows immediately. |

**`agent_added.agent` is not the `agents[]` shape.** The delta carries the
merged agent as the fleet model holds it: `pane`, `workspace` and `tab` are
single `host/id` strings where `/api/fleet`'s `agents[]` spells them out as
`ref` + `host` + `pane_id` + `workspace_id` + `tab_id`. The remaining nine
fields (`workspace_label`, `name`, `title`, `agent`, `display_agent`,
`agent_status`, `state_change_seq`, `fleet_change_seq`, `focused`) are spelled
the same. A client needs two decoders, or one that accepts both spellings.

**A reader must skip a `kind` it does not know** — new kinds are additive and
E6/E7 will add them.

Deltas are host-local: stopping `lab-2` produces
`{"kind":"host_connection","host":"lab-2","connection":{"state":"unavailable","reason":"…","retry_in_ms":1000}}`
and nothing that names `lab-1`.

Housekeeping: the inbound message cap is 4 KiB, the server pings every 30 s and
drops a socket after 2 unanswered pings, and anything a client *sends* is
ignored except as proof of life. When the gateway stops, the socket is closed
with **1001** and the reason `fleet runtime stopped` — a client should reconnect
with backoff, not report an error.

---

## WebSocket: `/api/terminal/{host}/{pane}`

`herdr.fleet.terminal.v1`. `{host}` is a fleet host id and `{pane}` a pane id on
it, so the URL for `lab-2/w1:p1` is
`/api/terminal/lab-2/w1:p1`. A pane id containing a `/` must be
percent-encoded — and is then still refused, because such a pane cannot form an
unambiguous reference. A malformed host or pane, an empty pane, or a pane
containing a control character is `404` **before** the upgrade; after the
upgrade every outcome is a `terminal.error` or a `terminal.closed` on the
socket, never a 5xx.

The route needs `read`; the **mode** decides what else is needed.

### Opening

The first client message must be `terminal.open`, within 10 s, and unknown
fields are rejected (a mistyped `mode` must not silently downgrade a control
session to observe):

```json
{"type":"terminal.open","mode":"observe","cols":80,"rows":24}
{"type":"terminal.open","mode":"control","cols":80,"rows":24,"takeover":false}
```

`cols` and `rows` must be `1..=1024` in each dimension. The gateway waits for
the host to confirm the attach before it answers `terminal.ready`, so a client
normally never draws a terminal it is about to lose. The wait is bounded at
10 s: a host that answers neither way inside that window gets its
`terminal.ready` anyway and the stream continues, so a very slow host can still
produce a ready-then-refused sequence.

```console
$ python3 scripts/fork/ws-client.py "ws://127.0.0.1:7788/api/terminal/lab-2/w1:p1" \
    -H "Authorization: Bearer $READ" \
    --send '{"type":"terminal.open","mode":"observe","cols":80,"rows":24}' --max-messages 2
text {"cols":80,"host":"lab-2","mode":"observe","pane":"w1:p1","ref":"lab-2/w1:p1","rows":24,"schema":"herdr.fleet.terminal.v1","type":"terminal.ready"}
binary 2233
```

### Binary frames

Every rendered frame is a **binary** message: a fixed 14-byte header, then the
host's already-diffed ANSI bytes verbatim. Fixed width on purpose — a browser
reads the header with one `DataView` and hands `frame.slice(14)` to xterm.js
with no per-frame JSON.

| Offset | Size | Field | Meaning |
| --- | --- | --- | --- |
| 0 | 1 | `kind` | `0x01` = terminal. **Ignore a binary message whose first byte you do not know.** |
| 1 | 8 | `seq` | `u64` little-endian, the host's frame sequence. |
| 9 | 2 | `width` | `u16` little-endian, columns. |
| 11 | 2 | `height` | `u16` little-endian, rows. |
| 13 | 1 | `full` | non-zero = full redraw, zero = incremental diff. |
| 14 | … | payload | ANSI bytes. |

The first frame of a session is always `full`. A real header from the lab:

```text
01 0100000000000000 5000 1800 01   →  kind 1, seq 1, 80×24, full
1b5b3f32303236681b5b3f32356c…      →  the payload, containing `herdr-fleet-lab:lab-2`
```

Frames mirror the server's own `MAX_FRAME_SIZE` (2 MiB). The gateway *buffers*
at most **two** frames per open terminal — the bridge from the host to the
socket uses a depth-2 channel, so a slow browser applies backpressure to the
server's render lane instead of making the gateway buffer. (Counting the frame
a blocked reader is holding and the one the socket task is writing, the true
ceiling is four; the point is that it is a small constant, not a queue.)

### Client messages

After `terminal.open` the vocabulary is herdr's own JSON terminal-control
vocabulary, parsed by the same function the CLI uses so the two cannot drift:

| Message | Fields | Observe | Control |
| --- | --- | --- | --- |
| `terminal.input` | `text` **or** `bytes` (base64) — at most one: both is an error, and neither sends an empty input | `forbidden` | forwarded to the PTY byte-exact |
| `terminal.resize` | `cols`, `rows` (`1..=1024`) | client-local viewport only | a **real PTY resize** |
| `terminal.scroll` | `direction`, `lines`, `source?`, `column?`, `row?`, `modifiers?` | `unsupported` | forwarded |
| `terminal.release` | — | closes the session | closes the session and releases the pane |

Client messages are capped at 64 KiB, and that cap is enforced by the WebSocket
layer: an oversize message fails the socket outright, so it produces a bare
close rather than a `terminal.error` you could read. A controller's
`terminal.resize` never
carries the browser's pixel cell geometry — `cell_width_px`/`cell_height_px` are
rebuilt as zeros, so a control client cannot change the pixel cell size the pane
reports to every *other* client of that host.

`terminal.scroll` is `unsupported` in observe mode because the host ignores
scrollback commands from a client that is not attached, and what it would move
for a controller is the *shared* scrollback, not a client-local viewport —
scroll in your own client instead.

### Server messages

These messages are built as JSON maps, so their keys come out **sorted** and
`type` is last, not first. (The `/api/fleet` report and the `/api/events`
envelopes are serialized from structs instead, and keep declaration order —
`kind` really is first there.)

```json
{"cols":80,"host":"lab-2","mode":"observe","pane":"w1:p1","ref":"lab-2/w1:p1","rows":24,"schema":"herdr.fleet.terminal.v1","type":"terminal.ready"}
{"code":"busy","message":"…","type":"terminal.error"}
{"reason":"released","type":"terminal.closed"}
```

`terminal.closed.reason` is `"released"` (your own `terminal.release`),
`"taken_over"` (another controller took the pane), `"the gateway is stopping"`,
a host-supplied reason string, or `null` for an ordinary end of stream. A
`terminal.closed` carries its own close code: **1000** for a release, a
takeover or an ordinary end of stream, and **1001** when the gateway is
stopping — that last one means *reconnect later*, not *something broke*.

### Error codes

| `code` | Condition | Close |
| --- | --- | --- |
| `bad_request` | No `terminal.open` within 10 s, a malformed open, geometry outside `1..=1024`, an unparseable or over-long command, or a binary message from the client. | `1002` at handshake; no close for a mid-session bad command |
| `forbidden` | The scope cannot hold the mode (`read` asking for `control`), or the mode forbids the message (an observer's `terminal.input`). | `1008` at handshake; no close for a later refusal |
| `host_unavailable` | The host is not configured, not connected, or the session could not be opened. | `1011` |
| `host_busy` | The **gateway's** single ssh transport slot for that host is taken. Retry later. | `1013` |
| `busy` | The **pane's** single attach slot on the host is held by another controller (retry with `takeover: true`), or the host has a read in progress on that pane and says to retry (a transient state; takeover is not the remedy — read the `message`). | `1013` |
| `pane_not_found` | The host is reachable but has no such pane. | `1000` |
| `unsupported` | An observer's `terminal.scroll`. | none — the session continues |
| `internal` | The gateway's fault, and also a host-side stream that failed mid-session — a frame over `MAX_FRAME_SIZE`, a framing error, or a read error. | `1011` |

`host_busy` and `busy` are different layers and a client should treat them
differently: `host_busy` means *wait*, `busy` usually means *offer "take
over"* — except for the read-in-progress wording, which means *retry*.

Real transcripts, all from the lab:

```console
# a read credential asking for control
text {"code":"forbidden","message":"terminal mode control needs the control scope","type":"terminal.error"}
close 1008

# an observer trying to type
text {"code":"forbidden","message":"an observe session cannot send input; open the terminal with mode \"control\"","type":"terminal.error"}

# an observer trying to scroll (the socket stays open)
text {"code":"unsupported","message":"the host ignores scrollback commands from an observer; scroll in the client instead","type":"terminal.error"}

# a host that is not in [fleet]
text {"code":"host_unavailable","message":"no fleet host named nosuch","type":"terminal.error"}
close 1011

# a pane the host does not have
text {"code":"pane_not_found","message":"terminal session observe failed: terminal target w9:p9 not found","type":"terminal.error"}
close 1000

# cols beyond the ceiling
text {"code":"bad_request","message":"terminal.open cols and rows must be at most 1024x1024","type":"terminal.error"}
close 1002
```

### Control and takeover

A pane has **one** attach slot on its host. A second controller that does not
ask for it is refused; one that asks takes it, and the previous owner is told
why.

```console
# controller A holds lab-1/w1:p1; controller B opens without takeover
text {"code":"busy","message":"terminal attach failed: terminal term_… already has an attached client; retry with --takeover","type":"terminal.error"}
close 1013

# controller B retries with {"takeover": true}
text {"…","mode":"control","type":"terminal.ready"}
binary 2233

# and controller A sees, on its own socket:
text {"reason":"taken_over","type":"terminal.closed"}
close 1000
```

Input reaches exactly one pane. A control session on `lab-1/w1:p1` that sends
`{"type":"terminal.input","text":"echo E3-DOC-CHECK\n"}` and releases leaves the
string in `lab-1`'s pane and nowhere else:

```console
$ herdr --session lab-1 pane read w1:p1 --source recent | grep -c E3-DOC-CHECK
1
$ herdr --session lab-2 pane read w1:p1 --source recent | grep -c E3-DOC-CHECK
0
```

Nothing in the gateway confirms a "destructive" action, because there is nothing
to confirm: `terminal.input` is exactly the bytes a console sends when you type.
Input bytes are never logged; an opened session logs `host`, `pane`, `mode`,
`takeover` and the credential kind only.

---

## `scripts/fork/ws-client.py`

A stdlib-only WebSocket client, used throughout this document. No dependencies,
so it runs anywhere `python3` does.

```bash
python3 scripts/fork/ws-client.py <ws-url> \
  [-H|--header 'Name: value']…  # extra request headers, e.g. Authorization
  [--send JSON]…           # text messages to send after connecting, in order
  [--send-stdin]           # send all of stdin as one text message
  [--max-messages N]       # stop after N printed messages
  [--timeout SECS]         # overall deadline; default 10, 0 waits forever
  [--binary hex|len]       # print binary messages as hex, or as a byte count (default)
```

It prints one line per message on stdout — `text …`, `binary …`,
`close <code>`, plus `handshake <status>` if the HTTP upgrade itself is refused
— which is what makes the transcripts above greppable. A close frame's reason
goes to stderr, and a failure path exits non-zero.

---

## Shutting down

`SIGINT` or `SIGTERM` stops the gateway with exit **0**. (On Windows only
Ctrl-C does: `SIGTERM` has no counterpart there, so a supervisor has to stop
the process another way.) In order: end every `/api/events` stream so an open
WebSocket does not hold graceful shutdown open, stop accepting and drain HTTP
for at most 5 s, then close the terminal sessions — each gets
`terminal.closed {"reason":"the gateway is stopping"}` and close **1001** —
drop their transports, stop the fleet connector, remove `gateway.json`, return.

Verified against the lab with a terminal stream and an events stream both open:
exit `0` in **27 ms**, `gateway.json` gone, both clients closed with `1001`
(`fleet runtime stopped` and `gateway stopping`), and every
`herdr-remote-<pid>-gateway-…` forward socket removed. A crash instead leaves
`gateway.json` behind, which is why `status` and `pair` check that the recorded
pid is alive.

---

## Running it under systemd

[`scripts/fork/systemd/herdr-gateway.service`](../../scripts/fork/systemd/herdr-gateway.service)
is a `systemd --user` unit. Everything the gateway needs is in `config.toml`, so
it sets no `Environment=` of its own.

```bash
mkdir -p ~/.config/systemd/user
cp scripts/fork/systemd/herdr-gateway.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now herdr-gateway
systemctl --user status herdr-gateway
journalctl --user -u herdr-gateway -f
```

`ExecStart` is `%h/.local/bin/herdr gateway`, so the unit is machine-independent
— install the binary there, or edit that one line. Run
`loginctl enable-linger $USER` if you want the gateway to survive your logging
out; without it a user service stops with your last session. Add
`Environment=HERDR_LOG=herdr=info,gateway=debug` for a verbose log.

E8 will install this unit for you.

---

## Troubleshooting

**`refusing to bind …: [gateway] allowed_origins is empty`** — you asked for a
non-loopback bind without saying who may reach it. Set `allowed_origins` (or
`public_url`), or bind loopback and put `tailscale serve` in front (E5).

**`cannot use the gateway token store: … is readable by group or other`** —
`chmod 600` the file, or delete it and let the gateway regenerate it. The
gateway refuses to run rather than quietly tighten a file you may have shared.

**`401` with a token you just copied** — check for a trailing newline
(`$(cat …/read.token)` strips it; a copy-paste may not), and check you are using
`read.token` for reads and `control.token` for control. After a `rotate-token`,
old tokens and that scope's device cookies are `401` immediately.

**`403 origin_not_allowed` from a browser** — the page's origin is not on the
allowlist. Remember the app must be **same-origin** with the API; the gateway
sends no CORS headers, so pointing a dev server at a remote gateway will not
work. Set `public_url` and serve the app from there.

**`429` and you are the only user** — the limiter is per **peer address**, and
behind `tailscale serve` every client arrives from `127.0.0.1`. Wait out
`Retry-After`, then find who is sending a bad credential. `X-Forwarded-For` is
deliberately not trusted (there is no proxy configuration to trust it against).

**`terminal.error {code:"busy"}`** — someone else (or another tab) controls that
pane. Reopen with `"takeover": true`; the other client gets
`terminal.closed {"reason":"taken_over"}` and should say so rather than
reconnecting in a loop.

**`terminal.error {code:"host_busy"}` on an ssh host** — that host's single
bridged stream is in use. Close the other terminal on it and retry; local hosts
never do this.

**A host is stuck `unavailable` or `connecting`** — read its `reason` in
`/api/fleet`. For an ssh host, the usual cause is that the key needs a
passphrase or the host key is unknown. The gateway's *bridges* are
`BatchMode=yes` and fail instead of prompting, but the discovery probe that
runs first is still interactive, so such a host can sit in `connecting` until
ssh gives up. Test the same target by hand with
`ssh -o BatchMode=yes <target> true`.

**Stale `herdr-remote-<pid>-gateway-…` sockets in the temp dir** — left by a
gateway that was `SIGKILL`ed. They are named by pid, so they never collide with
a new run; delete them at leisure.

**`herdr gateway status` says `not running` but a process exists** — the pid in
`gateway.json` is not alive, so the marker is stale (a crash), or two gateways
share one config directory and overwrote each other's marker. Run one gateway
per config directory.

---

## Accepted limitations

Recorded deliberately, so nobody rediscovers them as bugs:

- **No header-read or idle timeout.** A slowloris client can hold connections
  open. Closing it needs another direct dependency; the loopback default and
  E5's `tailscale serve` in front are the mitigation.
- **Behind a proxy, every peer is one address.** See the `429` note above.
- **`allowed_origins` is an allowlist, not CORS.** The app must be same-origin.
- **A failed store *reload* fails open**, keeping the credentials already in
  memory. A bad store at startup is still a refusal.
- **On Windows a file stamp has no inode**, so two rotations inside one mtime
  tick could be missed.
- **`pair`, `status` and `rotate-token` ignore `--config`.** The store follows
  `XDG_CONFIG_HOME`, `pair` prefers the running gateway's marker for the
  address, and the config file these commands read is the default one (or
  `HERDR_CONFIG_PATH`) — so a daemon started with `--config` could have a
  `public_url` they do not see.
- **A one-time pairing URL is burned by a link previewer.**
- **Rotating a token does not end a live control session**; scope is fixed at
  the terminal handshake.
- **A browser that vanishes during the ≤10 s attach wait** holds that pane's
  attach slot until the host answers or the timeout fires.
- **An ssh host serves one terminal stream at a time**, so a controller and an
  observer cannot share one ssh host today.
- **There is no revoke-one-device command**; `rotate-token <scope>` revokes all
  of that scope's devices.
- **A `[gateway]` value of the wrong TOML type resets every section**, because
  the config file is deserialized in one pass. The diagnostic says
  `config parse error: …; using defaults`.
- **`SIGTERM` is a unix-only stop.** On Windows the gateway waits on Ctrl-C.
- **An ssh host whose key needs a passphrase stalls in *discovery*,** which is
  still interactive upstream code, rather than failing fast the way the
  noninteractive bridge does.

---

## What comes next

- **E4** builds the phone app in `web/`, bootstrapping from `GET /api/fleet`,
  applying `/api/events` deltas, and rendering `/api/terminal/…` frames with
  xterm.js. It reads the binary header exactly as tabulated above.
- **E5** puts the gateway on a tailnet: `tailscale serve` terminates HTTPS on
  loopback, `public_url` becomes the `https://<host>.<tailnet>.ts.net` origin,
  and pairing URLs and `Secure` cookies follow from that one key.
- **E7** adds agent actions (`prompt`, `keys`, `start`, `rename`, `close`) as
  `POST` routes gated by the same `control` scope.

Every later `[gateway]` key and every new endpoint is documented **here**, in
the same pull request that adds it.
