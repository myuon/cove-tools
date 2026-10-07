# cove-tools

Small web apps written in [Cove](https://github.com/myuon/cove), and the host
that runs them on one machine.

- **`crates/cove-host`** — a self-hosted function host: loads `apps/<name>/`,
  checks, prepares and compiles each app once, and runs every request in its
  own Cove isolate on a shared worker pool (issue #1).
- **`apps/`** — the sample apps: `hello` (pure), `crunch` (CPU-heavy),
  `slow` (waits on a timer that parks the run), `notes` (the persistent
  key-value store) and `proxy` (allowlisted outbound HTTP); and the real
  apps: [**`webhooks`**](apps/webhooks/README.md), the webhook lab (#2),
  [**`ledger`**](apps/ledger/README.md), the bench ledger (#3), and
  [**`algo`**](apps/algo/README.md), the algorithm playground (#4); and
  [**`admin`**](apps/admin/README.md), the admin UI (#17), the one app that
  may change the others' configuration.

The host is Rust; the apps are Cove. Cove is a git dependency pinned to one
commit (`rev` in the workspace `Cargo.toml`), and a change the compiler or
the runtime needs goes to [myuon/cove](https://github.com/myuon/cove) first.

## Building and running

```console
$ cargo build --profile checked
$ ./target/checked/cove-host serve --apps apps
cove-host: loading apps from apps
  crunch     v1-397ddca0  requires [-]  granted [-]  ok: 227 fn on native, checked in 19.3 ms, prepared in 1.9 ms
  hello      v1-05b655c0  requires [-]  granted [-]  ok: 234 fn on native, checked in 11.6 ms, prepared in 2.4 ms
  notes      v1-cf6980a0  requires [kv, log]  granted [kv, log]  ok: 226 fn on native, checked in 3.7 ms, prepared in 2.5 ms
  proxy      v1-f26c6f57  requires [fetch, log]  granted [fetch, log]  ok: 227 fn on native, checked in 21.1 ms, prepared in 2.1 ms
  slow       v1-2914903a  requires [log, timer]  granted [log, timer]  ok: 220 fn on native, checked in 12.8 ms, prepared in 1.6 ms

listening on http://127.0.0.1:8080 — 16 worker thread(s), a run yields after 2.0 ms while others wait, a fresh isolate per request, apps served round robin
  stats: curl -s http://127.0.0.1:8080/_host/stats   ops page: http://127.0.0.1:8080/_host/ui
  admin: http://127.0.0.1:8081 (token in data/admin.token); update with `cove-host update <app>`
```

(`--profile checked` is release with debug assertions and overflow checks
back on; `--release` works as well. The tests use the same profile — see
[Tests](#tests).)

`serve` takes `--data DIR` (where apps keep their state, one directory per
app; default `./data`), `--addr` (default `127.0.0.1:8080`), `--workers N` (threads
that run Cove; default one per hardware thread), `--io-threads N` (HTTP and
pending host work; default 2), `--slice MS` (default 2; `0` never asks a run
to yield), `--max-connections N` and `--max-in-flight N` (default 10,000
each), `--backend auto|vm|native` (default `auto`), `--quiet` (no `log`
lines on stdout), `--admin ADDR` (the admin listener for updates, default
`127.0.0.1:8081`) or `--no-admin`, `--ops-listener public|admin` (which
listener serves `/_host/`; default `public`), `--public-origin URL` and
`--trust-proxy` (what apps are told of the client's scheme and host, behind
a reverse proxy), and `--shutdown-grace SECONDS` (default 10). Those last
four are for [deploying](#deploying). `cove-host --version` names the Cove
commit the binary was built against.

### The sample apps

```console
$ curl -i 'http://127.0.0.1:8080/hello/?name=Cove'
HTTP/1.1 200 OK
content-type: text/plain; charset=utf-8
content-length: 21

Hello, Cove! (GET /)
$ curl -s -X POST --data 'hi there' http://127.0.0.1:8080/hello/echo
hi there
$ curl -s 'http://127.0.0.1:8080/crunch/?n=100000'
9592 primes up to 100000, the largest 99991
$ curl -s 'http://127.0.0.1:8080/slow/?ms=200&times=2'
waited 2 x 200 ms
```

`slow` prints `[slow] info: GET / waited 2 x 200 ms` on the server's
standard output: that is the `log` module. Each `timer.sleep` parked the run;
no worker was held while it waited.

A bug in an app is one failed request, answered with the runtime's own
diagnostic:

```console
$ curl -i 'http://127.0.0.1:8080/hello/spin'
HTTP/1.1 500 Internal Server Error
content-type: text/plain; charset=utf-8
content-length: 363

error[cove::runtime]: execution stopped: fuel budget of 2000000 exhausted
  --> hello/hello.cove:46:9
   |
46 |   while turns >= 0 {
   |         ^^^^^^^^^^
  --> hello/hello.cove:10:12
   |
10 |     return spin()
   |            ^^^^^^ called from here
  rule: ADR 0001: CPU, time, concurrency, and host-call limits are runtime controls, not termination proofs.
$ curl -i 'http://127.0.0.1:8080/slow/?ms=5000'
HTTP/1.1 504 Gateway Timeout
content-type: text/plain; charset=utf-8
content-length: 266

error[cove::runtime]: execution stopped: wall-clock deadline of 3s exceeded
  --> slow/slow.cove:22:5
   |
22 |     timer.sleep(ms)
   |     ^^^^^^^^^^^^^^^
  rule: ADR 0001: CPU, time, concurrency, and host-call limits are runtime controls, not termination proofs.
```

The second one was parked when its deadline passed: the host cancelled it
(`ParkedVm::cancel`, Cove ADR 0082) rather than wait the five seconds.

### State and outbound calls

`notes` keeps one note per path in its own store, which survives a restart:

```console
$ curl -s -X PUT --data 'buy milk' http://127.0.0.1:8080/notes/todo
stored todo
$ curl -s -X PUT --data 'x' http://127.0.0.1:8080/notes/zeta
stored zeta
$ curl -s http://127.0.0.1:8080/notes/todo
buy milk
$ curl -s 'http://127.0.0.1:8080/notes/?order=desc&limit=1'
zeta
$ ls data/
notes
```

`proxy` forwards the request — method, body, `content-type` and `x-*`
headers — to `?url=`, which has to be on its allowlist (here it may reach
this host itself on port 8080, and `https://example.com`):

```console
$ curl -s 'http://127.0.0.1:8080/proxy/?url=http://127.0.0.1:8080/hello/?name=proxy'
Hello, proxy! (GET /)
$ curl -s -X POST -H 'x-test: yes' --data 'posted' 'http://127.0.0.1:8080/proxy/?url=http://127.0.0.1:8080/hello/echo'
posted
$ curl -s 'http://127.0.0.1:8080/proxy/?url=http://example.org/'
`http://example.org:80` is not on app `proxy`'s fetch allowlist (http://127.0.0.1:8080, http://localhost:8080, https://example.com:443)
```

The last was refused before anything was sent, and the run never parked.

A client that goes away cancels its request: `curl -m 0.3
'http://127.0.0.1:8080/slow/?ms=2000'` gives up after 0.3 s, and the parked
run is cancelled then rather than two seconds later (`errors.cancelled` in
the stats).

`GET /_host/apps/<app>/logs?n=200` answers an app's recent log lines — its
`log.*` and the host's own lines about it (a failed request, a run that
would not yield, an update), the last 1,000 kept in memory. They are also
written to `<data>/<app>/log.txt`, rotated to `log.1.txt` past 1 MiB (one
old file kept), by a writer thread of their own, so no worker or I/O thread
waits on a file:

```console
$ curl -s http://127.0.0.1:8080/_host/apps/notes/logs
1791168727.534 info: stored todo (8 bytes)
1791168727.547 info: stored zeta (1 bytes)
```

### Stats and the operations page

`GET /` lists the apps. The operations views are read-only:

| path | what |
| --- | --- |
| `GET /_host/ui` | one HTML page (no script, refreshes every 5 s): every app's version, state, tier, counters, queues, parks, yields, declined and overdue yields, worker time, instructions, live versions, errors, cancellations, rejections, KV usage against its quota and fetch counts; the last 20 errors and 10 log lines of each app. Every value from an app is HTML-escaped |
| `GET /_host/stats` | the same counters as JSON, with server totals |
| `GET /_host/apps/<app>` | one app as JSON: the above, its `limits`, every version it has had (`version`, `loaded_unix_s`, `current`, `alive`, `program_alive`), its last 50 errors (`unix_ms`, `kind`, `status`, `version`, `message`) and, on the native tier, `native`: how many functions have machine code (`compiled` of `reachable`) and each one left on the encoded tier (`refusals`: `function`, `reason`, the `instruction` the code generator stopped at, and `at`/`source`, where the source wrote it) |
| `GET /_host/apps/<app>/logs?n=200` | its recent log lines, as text |

They are read-only and unauthenticated, and by default on the public
listener. Behind a reverse proxy start the host with `--ops-listener admin`:
then the public listener answers 404 for everything under `/_host/`, and the
views are served on the admin listener instead — without its token, as they
change nothing, and only to a request whose `Host` is a loopback name
(`localhost`, `127.0.0.1`, `[::1]`), so that a web page cannot reach them by
rebinding its own name to 127.0.0.1. Reach them over SSH:
`ssh -L 8791:127.0.0.1:8791 <server>`, then `http://localhost:8791/_host/ui`.

`/_host/stats`, per app: `state`
(`ready`, `refused` with the reason, `disabled`, or `removed`), `version`, `tier`, `required` and `granted`,
`hosts` (its `[route] hosts`), `overridden` (whether the admin app changed its configuration),
`served`, `ok`, `errors` by kind, `rejected` by reason (`disabled` among them), `in_flight`,
`queued`, `parked`, `parks`, `yields`, `yield_requests`, `yields_declined`,
`overdue_yields`, `blocking_host_calls`, `instructions`, `fuel`, `worker_ms`,
`heap_peak_words`, `fetch` (`calls`, `refused`, `errors`), `kv` (`keys`,
`bytes`, `max_keys`, `max_bytes`, for an app granted `kv`), `updates`,
`updates_refused`, `versions_alive` and `programs_alive`; and the server's `connections`,
`rejected_connections`, `not_found`, `admitted` and `totals`.

### Checking and testing an app

`cove check` and `cove test` cannot see the host's modules (`web`, `log`,
`timer`), so the host ships its own — the embedder-side answer to cove#151.
`check` runs exactly the admission `serve` runs at load, and exits non-zero if
any app would be refused:

```console
$ ./target/checked/cove-host check --apps apps
crunch     requires [-]  granted [-]  ok
hello      requires [-]  granted [-]  ok
slow       requires [log, timer]  granted [log, timer]  ok
checked 3 app(s) against the host's schemas (web, log, timer); 0 warning(s), 0 refused
$ ./target/checked/cove-host check --apps crates/cove-host/tests/apps
error[cove_host::spawn]: `spawner.handle` can spawn a task, which cove-host does not run
 --> spawner/spawner.cove:6:19
  |
6 |     let doubled = tasks.spawn { 21 * 2 }
  |                   ^^^^^^^^^^^^^^^^^^^^^^
  rule: A spawned task runs outside the host's worker pool and keeps its parent from parking or yielding; cove-host refuses an app that can spawn.
greedy     requires [log]  granted [-]  REFUSED: `greedy.handle` requires `log`, which app.toml does not grant
spawner    requires [-]  granted [-]  REFUSED: `spawner.handle` can spawn a task, which cove-host does not run
checked 2 app(s) against the host's schemas (web, log, timer); 0 warning(s), 2 refused
$ echo $?
1
```

`test` runs every `test fn` of each app the way `cove test` does — through
Cove's own `cove_runtime::testing::TestRun`, so a failure is reported by the
same rules — with the app's grant (not the test's derived one), its limits,
and the host's modules. A lowering that refuses an entry, in `check`, at load
or for a test, is printed with each refusal's location:

```console
$ ./target/checked/cove-host test --apps apps
ok    crunch     crunch.aSizeOutOfRangeIsRefused
ok    crunch     crunch.countsThePrimesUpToN
ok    hello      hello.answersABodyOfTheSizeAsked
ok    hello      hello.greetsTheWorldWhenNobodyIsNamed
ok    hello      hello.greetsWhoeverTheQueryNames
ran 5 test(s), 5 passed
```

Both take app names to narrow them (`cove-host check hello`), and `test`
takes `--filter`.

**Secrets.** Neither runs with a host, so a secret taken from the host's
store (`{ store = "…" }`, [Secrets set at run time](#secrets-set-at-run-time))
is looked up in the store of the data directory `--data` names
(`<data>/_host/secrets`, read only); without `--data` there is no store.
`check` refuses an app whose secret cannot be resolved, as `serve` would.
`test` does not: a secret only gates `auth.check` and a `[fetch.headers]`
header, which a test should not reach for real, so a secret it cannot
resolve — store, environment variable or file — is given the value
`cove-host-test-placeholder-<name>`, and the run says which on stderr:

```console
$ ./target/checked/cove-host test --apps crates/cove-host/tests/apps keyed
note: `keyed` runs its tests with a placeholder for secret(s) `key`: not set here (see `cove-host test --help`)
ok    keyed      keyed.aWrongTokenIsNotTheSecret
ran 1 test(s), 1 passed
```

## Updating an app

```console
$ $EDITOR apps/hello/hello.cove                 # Hello → Hi
$ ./target/checked/cove-host update hello
{
  "app": "hello",
  "previous": "v1-05b655c0",
  "version": "v2-11744df1"
}
$ curl -si http://127.0.0.1:8080/hello/ | grep -i -e x-cove -e hi
x-cove-app-version: v2-11744df1
Hi, world! (GET /)
$ echo 'fn broken( {' >> apps/hello/hello.cove
$ ./target/checked/cove-host update hello; echo "exit $?"
cove-host: 422 Unprocessable Entity
update of `hello` refused; still serving v2-11744df1:
does not parse:
error[cove::parse::unexpected_token]: expected identifier, found `{`
  --> hello/hello.cove:51:12
   |
51 | fn broken( {
   |            ^

exit 1
$ curl -s http://127.0.0.1:8080/hello/
Hi, world! (GET /)
```

`cove-host update <app>` asks the running host to load `<apps>/<app>` again.
The new version is parsed, checked, admitted (capabilities, `spawn`, config),
lowered, prepared and compiled on a blocking thread — not on a worker, not on
the I/O runtime — while the current version keeps serving. **Only if all of
that succeeds** is the app's route switched, in one step; otherwise the
current version stays and the command prints the diagnostics and exits 1
(`updates_refused` in the stats, a line in the app's log).

- **Requests in flight finish on the version they were admitted to** —
  queued, running, yielded or parked: a request holds its version (and so
  its prepared program and registry) until it answers. Requests admitted
  after the switch get the new one.
- **The old version is dropped when its last request ends.** Its
  `PreparedProgram` goes with it; `/_host/apps/<app>` lists each version with
  `alive` and `program_alive`, and `versions_alive` / `programs_alive` count
  them.
- **Every response says which version answered**: `x-cove-app-version:
  v<n>-<hash>`, the hash being of the app's files, so two loads of the same
  code have the same hash. The id is in the stats, the logs and every
  recent error too.
- The app's counters, log, queue and KV store belong to the app, not the
  version: they carry over. During an update both versions share the one
  store (and its quota accounting); the new version's quotas apply from the
  switch.
- A name the host does not serve yet is **added** by `update`; `cove-host
  remove <app>` stops routing to an app (in-flight requests finish; its data
  stays, and `update` brings it back as its next version).
- One update runs at a time.

**The admin listener.** Updates go to a second listener, `--admin`
(default `127.0.0.1:8081`), never to the public one, so a reverse proxy that
forwards the public port cannot reach them, and the admin port can stay on
localhost whatever the public one is. Every admin request needs
`Authorization: Bearer <token>`; the host writes a random 256-bit token to
`<data>/admin.token` (mode 0600) the first time it starts without one, and
`cove-host update` / `remove` read it from there (`--token-file`, `--admin`
to point them elsewhere). Anything without the token is 401 and changes
nothing. The endpoints are `POST /apps/<app>/update`,
`POST /apps/<app>/deploy` and `/rollback` (see [Deploying an
app](#deploying-an-app)), `DELETE /apps/<app>`,
`POST /apps/<app>/enable`, `/disable` and `/reset` (see [Administering apps
at run time](#administering-apps-at-run-time)), `GET /secrets`,
`PUT /secrets/<name>` and `DELETE /secrets/<name>` (see [Secrets set at run
time](#secrets-set-at-run-time)), `GET /apps` (the stats) and
`GET /changes?n=50` (the change history). There is no file watcher: an update is an
explicit act, which is what makes a refused one a report rather than a
silent non-event.

## Deploying an app

`update` reloads a directory that is already in the host's apps directory.
An app that lives somewhere else — its own repository, like
[myuon/ai-daily](https://github.com/myuon/ai-daily)'s `apps/aidaily` — is
**deployed**: `cove-host deploy` packs it and sends it to the admin listener,
which checks it and only then writes it into the apps directory and updates
to it.

```console
$ ./target/checked/cove-host deploy ../elsewhere/hello     # with a README.md beside it
cove-host: not part of the app, left out: README.md
cove-host: deploying `hello`: 3 file(s), 2430 bytes
{
  "app": "hello",
  "previous": null,
  "version": "v1-05b655c0"
}
$ sed -i 's/Hello,/Hi,/' ../elsewhere/hello/hello.cove
$ ./target/checked/cove-host deploy ../elsewhere/hello 2>/dev/null | grep version
  "version": "v2-11744df1"
$ ./target/checked/cove-host rollback hello | grep version
  "version": "v3-05b655c0"
$ echo 'fn broken( {' >> ../elsewhere/hello/hello.cove
$ ./target/checked/cove-host deploy ../elsewhere/hello; echo "exit $?"
cove-host: not part of the app, left out: README.md
cove-host: deploying `hello`: 3 file(s), 2440 bytes
cove-host: 422 Unprocessable Entity
deploy of `hello` refused; still serving v3-05b655c0:
does not parse:
error[cove::parse::unexpected_token]: expected identifier, found `{`
  --> hello/hello.cove:51:12
   |
51 | fn broken( {
   |            ^

exit 1
```

- **What is sent** is the app's `app.toml` and its `.cove` files, at any
  depth — nothing else (a README, samples, a `.git`; anything hidden is
  skipped). A symbolic link, a path outside the directory, and more than
  16 MiB or 4096 files are refused before anything is sent, and again by
  the host. The name is the directory's (`--name` to choose another); it is
  the app's route and its main module's name, so `aidaily/aidaily.cove`.
- **The check is the running host's**: the app is loaded as its next
  version with the host's env (its `[secrets]`, `[access]`), the admin's
  changes to its grant, and the hostnames other apps claim — exactly as an
  update loads it. **If it does not load, nothing changes**: no file is
  written, the current version keeps serving, and the diagnostics are
  printed (exit 1). An app that needs a secret the host does not have is
  refused here, saying which: set it first — in the admin app's Secrets
  page or with `cove-host secret set` for a `{ store = ... }` secret
  ([Secrets set at run time](#secrets-set-at-run-time)), or in
  `~/cove-tools/env` and a restart for an `{ env = ... }` one. A `[secrets]`
  value `{ file = ... }` is not sent: keep secrets in the store or the env.
- **If it loads**, it is written to `<apps>/.deploy/<app>`, the current
  `<apps>/<app>` is moved to `<apps>/.previous/<app>` (replacing the one kept
  there), the new copy is renamed into place, and the host updates to it:
  requests in flight finish on the old version, as with `update`.
- `cove-host rollback <app>` swaps `<apps>/<app>` with the kept copy — after
  loading it, so a kept version that no longer checks is refused and nothing
  moves — and updates to it. A second rollback undoes the first.
- Both are admin requests (`POST /apps/<app>/deploy` with the archive as the
  body, `POST /apps/<app>/rollback`), behind the same token as `update`, and
  both go in the change history.

**To whisky, over ssh.** The admin listener stays on the server's
localhost; the archive goes over ssh's stdin to the `cove-host` installed
there, which sends it to the listener with the token it can read. No tunnel,
no token on the laptop:

```console
$ tar -C apps/aidaily -c . | ssh whisky \
    '~/cove-tools/current/cove-host deploy - --name aidaily --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
$ ssh whisky '~/cove-tools/current/cove-host rollback aidaily --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
```

`deploy -` reads a tar archive (any `tar`'s, macOS's included: its `._`
files are hidden and skipped) and needs `--name`. With a tunnel
(`ssh -L 8791:127.0.0.1:8791 whisky`) and a copy of the token,
`cove-host deploy apps/aidaily --admin 127.0.0.1:8791 --token-file <copy>`
works from the laptop too.

`cove-host deploy <dir> --into <apps>` does the same with no running host,
checking as `cove-host check` does with the current environment (and, with
`--data <data>`, that data directory's secret store); the host loads it at
its next start. `deploy/install.sh` installs the bundled apps this way, and
checks every installed app with `--data` before it switches.

## Administering apps at run time

Beyond updating code, an app's **configuration** can be changed on the
running host — enabled or disabled, its grant, its fetch allowlist, its
limits — by one app, the admin app (`apps/admin`, issue #17), through a host
module of its own, and on the machine through the admin listener. No other
app can: the capability is `admin`, and **only the app named `admin` may be
granted it**. An `app.toml` of any other app that grants it — or a change
that would — refuses that app at load, and `cove-host check` says so. The
grant is a line in the startup banner and in `/_host/stats` like every other
(`admin  requires [admin, auth, …]  granted [admin, auth, …]`): the one app
that can change the others is visible as such.

The module is `host` (the admin app's own main module is `admin`, and Cove
refuses a package module that shadows a host module):

| operation | answers |
| --- | --- |
| `host.apps()` | `Array<host.App>`: every app — `state` (`serving`, `disabled`, `refused`, `removed`) and `reason`, `version`, `tier`, `entry`, `hosts`, `required` and `granted` (and `grantAdded`/`grantRemoved`, what the admin changed), `fetchAllow`, `limits` (`host.Limits`: `fuel`, `maxHostCalls`, `deadlineMs`, `maxHeapWords`, `maxInFlight`, `maxQueued`, `maxRequestBytes`, `maxResponseBytes`) and `limitsChanged`, `isAdmin`, its counters (`served`, `ok`, `errors`, `rejected`, `inFlight`, `queued`, `kvKeys`, `kvBytes`) and its ten newest `recentErrors` |
| `host.capabilities()` | `Array<String>`: what a grant may name |
| `host.history(limit)` | `Array<host.Change>`: `atMs`, `who`, `app`, `action`, `detail`, `outcome`, newest first |
| `host.setEnabled(app, enabled, who)` | `Result<String, Error>` |
| `host.configure(app, settings, who)` | `Result<String, Error>`: `settings` is `host.Settings { grant, fetchAllow, limits }`, the whole of what they are to be |
| `host.reset(app, who)` | `Result<String, Error>`: back to `app.toml` |
| `host.secrets()` | `Array<host.Secret>`: every secret stored or used — `name`, `set`, `updatedMs` (0 when unset), `apps` that use it — never a value |
| `host.setSecret(name, value, who)` | `Result<String, Error>`: stores it and reloads the apps that use it; the message is what each reload came to |
| `host.deleteSecret(name, force, who)` | `Result<String, Error>`: refused while an app uses it unless `force` |

The five that change something park the run while the host works (an app
is reloaded on a blocking thread, as an update is), so they hold no worker.

**A change is a re-check, not a revocation.** Cove decides capabilities
before a program runs, so there is nothing to take away from a run in
progress: a change loads the app again with the change applied — parsed,
checked, admitted, lowered, prepared, compiled — and routes to that version
the way `cove-host update` does. Requests already admitted finish on the
version they were admitted to.

| the reload | the change | the app |
| --- | --- | --- |
| loads | kept | serves the new version |
| is refused because its entry requires a capability the change took away | kept | **refused**: answers 503 with the reason; every other app serves |
| anything else: a limit out of range (fuel, deadline or in-flight below 1, a negative number, a heap above the runtime's), an allowlist entry that does not parse, a capability the host does not have, `admin` for another app | **not kept** | as before; the answer is the reason |

Taking a capability away from an app that needs it is how an app is stopped
from using it, so that is applied; a change that is merely wrong is not.
Granting it back loads the app again, with its store as it was.

**Disabling** stops routing to an app and nothing else: its version stays
loaded, and its queue, counters, log and store stay. A request to it is
answered **503** `app … is disabled by the administrator` (no `Retry-After`;
counted as `rejected.disabled`) — 503 rather than 404 so that an app that is
switched off is not mistaken for a mistyped URL; what was already admitted
finishes. Enabling routes to it again.

**The admin app cannot disable itself, take `admin` away from itself, or
make any change that would leave it refused** — those are refused, through
it. The admin listener can do all three, which is the point of it:

```console
$ ./target/checked/cove-host disable notes    # or enable, reset
`notes` disabled: not routed to; in-flight requests finish
$ ./target/checked/cove-host reset admin      # drop the admin's changes to the admin app
```

**Where the changes are kept.** Not in `app.toml`: a deploy replaces an
app's directory with what was sent (`install.sh --with-bundled-apps` too).
They are kept in the data directory, which no release and no deploy
touches:

- `<data>/_host/overrides.json`: per app, `enabled`, the capabilities and
  allowlist entries added and removed, and the limits set — each relative to
  `app.toml`, so a later release whose `app.toml` grants something new still
  grants it unless that very capability was removed. A change back to what
  the file says leaves no entry. It is applied every time the app loads (at
  start, on `update`, on a change), so it survives a restart and a release.
  It is plain JSON and the last way out: delete an app's entry, or the file,
  and restart. A file that does not read stops the host from starting —
  running without it would quietly re-enable and re-grant.
- `<data>/_host/changes.jsonl`: the history — who (what the admin app says
  of its user, or `admin listener`), when, which app, what was asked and
  what came of it, refused attempts included — one JSON line each, appended.
- `<data>/_host/secrets`: the secret store, below.

### Secrets set at run time

An API key used to mean ssh, an edit of `~/cove-tools/env` and a restart.
A secret can instead live in the host's **secret store** and be set,
replaced and deleted on the running host — from the admin app's Secrets
page, the admin listener, or `cove-host secret`. An `app.toml` takes a
secret from it with `store`, beside `env`, `file` and `value`:

```toml
[secrets]
gemini = { store = "gemini" }

[fetch]
allow = ["https://generativelanguage.googleapis.com"]

[fetch.headers."https://generativelanguage.googleapis.com"]
x-goog-api-key = { secret = "gemini" }
```

- **Where it is kept**: `<data>/_host/secrets`, one JSON file (`{ "secrets":
  { "<name>": { "value", "updated_ms" } } }`), mode 0600, written whole
  through a temporary file that is synced and renamed (and the directory
  synced), so a crash leaves the old file or the new one. The data directory
  is the one thing the service may write besides the apps
  (`ReadWritePaths`), and no release or deploy touches it. A file that does
  not read stops the host from starting, saying so without quoting it.
- **A secret that is not set refuses the app** that takes it, at load, with
  the usual diagnostic naming it — `secret `gemini`: the host's secret
  store has no `gemini` (set it on the admin app's Secrets page, or with
  `cove-host secret set gemini`)` — which is the reason the admin app shows
  for the app, and the Secrets page lists the name as unset and used by it.
  A name is a letter or digit, then letters, digits, `_`, `-` or `.`, at
  most 64; a value is not empty and at most 16 KiB.
- **Setting or replacing one reloads every app that uses it** — those
  routed to whose `app.toml` names it in a `store` — through the same
  versioned update as `cove-host update`: requests in flight finish on the
  old value, the new version has the new one. The answer says what each
  reload came to. **The value is stored first and kept whatever the reloads
  do**: an app that does not load (its code broken on disk, say) keeps
  serving what it served, and picks the value up at its next load.
- **Deleting one an app uses is refused** (409) unless forced; forced, the
  apps that use it are reloaded without it and are refused (503), so the
  value is no longer used anywhere.
- **Write-only.** No endpoint, page or command returns a value: the listings
  are names, whether each is set, when, and which apps use it. Nothing
  logs one, and the stats, the diagnostics and the change history (which
  records who, the secret's name and the action — app `–`) do not hold
  one. The admin app handles the value once, in the request that sets it,
  in a password field that is never filled back in.

| through | list | set or replace | delete |
| --- | --- | --- | --- |
| the admin app (`/secrets`) | the table | the row's password field, or *Set a secret* | the row's *Delete*, with *confirm* ticked (and *force* for one in use) |
| the admin listener | `GET /secrets` | `PUT /secrets/<name>`, the value as the body | `DELETE /secrets/<name>[?force=1]` |
| `cove-host secret` | `list` | `set <name>`, the value on stdin | `delete <name> [--force]` |

The admin app's forms are POSTs under its usual protections: an identity
(`auth.identity`: Cloudflare Access, or its token where Access is off), and
a POST from another site refused (`Sec-Fetch-Site`, `Origin`). There is no
script on its pages, so the confirmation is a required checkbox, checked
again by the app. `cove-host secret` takes `deploy`'s `--admin` and
`--token-file`; `set` reads one line without echo from a terminal, or all of
stdin otherwise, dropping one trailing newline:

```console
$ ssh whisky '~/cove-tools/current/cove-host secret list --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
$ ssh -t whisky '~/cove-tools/current/cove-host secret set gemini --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
value for secret `gemini` (not echoed):
```

### Routing by hostname

An app may be reached by a hostname of its own instead of by its prefix:

```toml
[route]
hosts = ["covtools-admin.ramda.io", "admin.localhost"]
```

A request whose `Host` (without its port) is one of them reaches the app with
its whole path — `/`, `/apps/notes`, even `/_host/...` — and the app is **not**
reachable as `/<app>/` on any other hostname: `https://covtools.ramda.io/admin/`
is a 404. That is what lets a hostname carry an access policy of its own
(Cloudflare Access, in the deployment) without another hostname's policy
being a way round it. A hostname reaches one app; a second app claiming it
is refused. `x-forwarded-prefix` is empty for such an app.

**Each routed hostname is its own origin.** Under `--public-origin
https://covtools.ramda.io`, a request routed by `covtools-admin.ramda.io` is
told `x-forwarded-proto: https` and `host: covtools-admin.ramda.io` (the
public origin's scheme, and its port if it has one) — the name the route
matched, never one the client chose — so an app's same-origin check compares
against the origin its own pages were served from.

## Deploying

`deploy/` runs the host as a systemd service on one Linux machine, behind a
TLS-terminating proxy — written for the owner's home server (Ubuntu 24.04,
x86-64), a Cloudflare Tunnel to `https://covtools.ramda.io`, and Cloudflare
Access in front:

| file | what |
| --- | --- |
| [`deploy/cove-tools.service`](deploy/cove-tools.service) | the system unit: `User=ioijoi`, public listener `127.0.0.1:8790`, admin `127.0.0.1:8791`, two workers, CPU and memory caps, hardening (no `MemoryDenyWriteExecute`: the native tier maps machine code; writable: `data/`, and `apps/` for `cove-host deploy`) |
| [`deploy/env.example`](deploy/env.example) | the apps' secrets (`WEBHOOKS_ADMIN_TOKEN`, `LEDGER_TOKEN`, `ADMIN_UI_TOKEN`) and the Cloudflare Access settings (`ACCESS_TEAM_DOMAIN`, `COVTOOLS_ACCESS_AUD`, `COVTOOLS_ADMIN_ACCESS_AUD`, `ACCESS_ALLOWED_EMAILS`), as `~/cove-tools/env`; `install.sh` appends a key a release adds — a secret fresh, a setting as written there — and leaves the others |
| [`deploy/install.sh`](deploy/install.sh) | as the service's user, no sudo: downloads a release, verifies its sha256, unpacks it into `~/cove-tools/releases/<version>/`, checks every app installed in `~/cove-tools/apps` with the new binary (and refuses to switch if one would be refused), deploys the bundled apps it is asked for (`--with-bundled-apps`), points `~/cove-tools/current` at it, and prints the one `sudo` command. It never removes or replaces an app it was not asked to |
| [`deploy/backup.sh`](deploy/backup.sh) | SQLite online backups of every app's `kv.sqlite3`, kept 14 days; a user crontab line is in the file |
| [`deploy/cloudflare.md`](deploy/cloudflare.md) | the tunnel's public hostname and the Access applications, with the paths left open to outside callers |

A release is a tag: pushing `v<version>` (the workspace's version) runs
[`release.yml`](.github/workflows/release.yml), which builds `cove-host` on
Ubuntu 24.04 with the native tier, and publishes
`cove-host-<version>-x86_64-linux.tar.gz` (the binary, `apps/`, `deploy/`,
this README), its `.sha256`, and `install.sh`. CI installs the same tarball
into a scratch home and runs the unit's own command line against it
(`deploy/smoke.sh`).

On the server, as the service's user:

```console
$ curl -fsSLO https://github.com/myuon/cove-tools/releases/download/v0.3.1/install.sh
$ bash install.sh --with-bundled-apps "webhooks ledger algo admin" v0.3.1
...
first time: install the unit and start the service (needs sudo, once):

  sudo install -m644 /home/ioijoi/cove-tools/current/deploy/cove-tools.service /etc/systemd/system/cove-tools.service && sudo systemctl daemon-reload && sudo systemctl enable --now cove-tools
```

**The platform and the apps are installed separately.** A release is the
platform — the binary, the unit, `deploy/` — and installing one never
removes or replaces an app: the apps are whatever was deployed into
`~/cove-tools/apps`, from this repository or another ([Deploying an
app](#deploying-an-app)). An upgrade is `bash install.sh v<new>`, then
`sudo systemctl restart cove-tools` (the script says which, and when the
unit changed). Before it switches, it checks every installed app with the
new binary against `~/cove-tools/env`, and if one would be refused it stops
and switches nothing. This repository's apps — the webhook lab, the ledger,
the algorithm playground and the admin UI — are deployed only when asked:
`--with-bundled-apps "webhooks ledger algo admin"` deploys those of the
release, each checked and its previous version kept, exactly as `cove-host
deploy` would; a first install wants it, and an upgrade that should also
update them passes it again. The sample apps are never installed. The
service stops on SIGTERM by answering new requests 503 and waiting up to
`--shutdown-grace` for those in flight.

Upgrading from a release before this split: its unit made `apps/`
read-only to the service, so install the new unit when the script says it
changed, or `cove-host deploy` is refused with a permission error.

What the deployment relies on from the host:

- **`--ops-listener admin`**: `/_host/` is 404 on the public listener; the
  operations views are on the admin listener, which is never in the tunnel.
- **`[route] hosts` in the admin app's `app.toml`**: the admin UI is
  `https://covtools-admin.ramda.io`, a second public hostname on the same
  tunnel and port with an Access application of its own; it is not
  reachable under `covtools.ramda.io` ([Routing by hostname](#routing-by-hostname)).
  Its changes live in `data/_host/`, which a release does not touch. No flag
  and no unit change was needed for it.
- **Cloudflare Access, verified by the host** (`[access]` in the admin
  app's and the webhook lab's `app.toml`, the values in `~/cove-tools/env`):
  the admin UI and the webhook lab's admin pages know their user from the
  Access token, and refuse a request that did not come through Access
  ([Cloudflare Access](#cloudflare-access-who-is-asking),
  [deploy/cloudflare.md](deploy/cloudflare.md) §6). No flag and no unit
  change: `EnvironmentFile=` already reads the env.
- **`--public-origin https://covtools.ramda.io`**: cloudflared reaches the
  host as plain HTTP on localhost, but the browser's `Origin` is the public
  one. The host tells every app `x-forwarded-proto: https` and
  `host: covtools.ramda.io`, so the webhook lab's and the ledger's
  cross-site refusals compare against the right origin and the receive URLs
  the lab shows are the public ones. Without it a client cannot claim
  `https` (the host sets `x-forwarded-proto: http` itself); `--trust-proxy`
  believes a proxy's `X-Forwarded-Proto` and `X-Forwarded-Host` instead, for
  a proxy that serves several names. Nothing in the host or the apps uses the
  client's address; cloudflared's `cf-connecting-ip` reaches the apps as a
  header (the webhook lab stores it with each request).

## Writing an app

An app is a directory under `apps/` holding an `app.toml`. The `.cove` files
directly in it are the module named after the directory; each subdirectory
holding `.cove` files is a module of that name, which the app's files may
`use`. Nothing outside the directory is visible: every app is a package of
its own with the standard library.

The entry is `handle(request: web.Request) -> web.Response` (`<app>.handle`
unless `entry` says otherwise), over two types the host declares:

```cove
use web

export fn handle(request: web.Request) -> web.Response {
  // request.method, request.path, request.query, request.headers, request.body
  web.Response(status: 200, headers: Map.of(), body: "hello\n")
}
```

- `path` is the path below the app's prefix: `/hello/a/b` reaches `hello`
  with `/a/b` (always starting with `/`).
- `query` is `Map<String, String>`, decoded; for a repeated key the last
  wins.
- `headers` is `Map<String, String>`, names lowercased, a repeated header's
  values joined with `, `.
- `body` is a `String`; a body that is not UTF-8 is answered 400 before the
  app runs.
- The response's `status` must be 100–599 and its headers sendable; a
  `content-type` of `text/plain; charset=utf-8` is added when it sets none,
  and `content-length`, `transfer-encoding`, `connection`, `keep-alive` and
  `upgrade` are the HTTP layer's and dropped.

Building a `web.Response` needs no capability (cove#579). The other modules
do, and an app may use only what `app.toml` grants:

| module | capability | operations |
| --- | --- | --- |
| `web` | — | the `Request` and `Response` types |
| `log` | `log` | `info`, `warn`, `error` (`String`): a line on stdout, `[app] level: line` |
| `timer` | `timer` | `sleep(millis: Int)`: parks the run for that long (at most 60 s; `0` parks and comes straight back) |
| `kv` | `kv` | `get(key) -> Option<String>`, `put(key, value) -> Result<Unit, Error>`, `delete(key) -> Bool`, `increment(key, by) -> Result<Int, Error>` (atomic), `list(prefix, after, limit) -> Array<kv.Entry>`, `listDesc(prefix, before, limit) -> Array<kv.Entry>` |
| `fetch` | `fetch` | `get(url)` and `request(method, url, headers: Map<String, String>, body)`, each `-> Result<fetch.Response, Error>`; the body is decoded from `gzip`, `deflate` or `br` |
| `time` | `time` | `nowMillis() -> Int`: the wall clock, milliseconds since the Unix epoch; `nowMicros() -> Int`: microseconds, strictly increasing across the process |
| `random` | `random` | `hex(bytes: Int) -> String`: 1–64 random bytes from the operating system, as hex |
| `auth` | `auth` | `check(secret: String, authorization: String) -> Bool`: whether an `Authorization` header (`Bearer <s>`, or `Basic` with `<s>` as the password) presents the app's secret `secret`. Constant-time; the secret itself never reaches the app. `identity(headers: Map<String, String>) -> Result<auth.Identity, Error>`: who is asking — `{ email, via }`, a Cloudflare Access user the host verified (`via` `"access"`) or the `[access] token` secret (`via` `"token"`, `email` empty); see [Cloudflare Access](#cloudflare-access-who-is-asking). `usesAccess() -> Bool`: whether Access is on for the app, so whether a refusal should prompt for a token |
| `host` | `admin` | the admin app's view of the host, and its changes: see [Administering apps at run time](#administering-apps-at-run-time). Only the app `admin` may be granted it |

Every request also carries `x-forwarded-prefix`: where the host mounted the
app (`/webhooks`), so it can write links to itself. The host sets it,
replacing whatever the client sent.

### `kv`: the app's persistent store

Each app has its own store, `<data>/<app>/kv.sqlite3`, opened only for an app
granted `kv`. One database per app is the namespace: an app's `kv` holds its
own connection and nothing else, so no key it can write reaches another
app's data.

- `list(prefix, after, limit)` answers the entries (`kv.Entry { key, value
  }`) whose key starts with `prefix` and is greater than `after` (`""` for the
  first page), ascending, at most `limit` (1–1000); `listDesc(prefix, before,
  limit)` the same descending, keys less than `before`. The last key of a page
  is where the next one starts, so an app that writes `event:<zero-padded
  time>` lists its newest events with `kv.listDesc("event:", "", 50)` — the
  shape the webhook lab's history (#2) and the benchmark ledger's runs by
  prefix (#3) need.
- A `put` past a quota is the app's `Err`, naming the quota, and the store is
  unchanged: `max_key_bytes`, `max_value_bytes`, `max_keys`, `max_bytes` (keys
  and values summed) under `[kv]`.

**Why SQLite**: it is the boring choice — one file per app, a format that
outlives this program, crash-safe by design — and `rusqlite`'s `bundled`
feature builds it in, with no system library. It runs in WAL mode with
`synchronous = NORMAL`: a committed write survives the host process
crashing; a power loss may lose the last transactions, never the file.

**Why every call answers at once, on the worker**: a call into a local
SQLite is cheaper than parking. Measured on the macOS x86-64 development
machine, `--profile checked`: a `put` 34 µs, a `get` 5 µs, a list of 50 entries
250 µs (`cargo t --lib -- --ignored kv_call_cost --nocapture`), against **15
µs** for a park and resume end to end (`cargo t --test host -- --ignored
park_cost --nocapture`: a request of sixteen zero-length sleeps against one).
Parking a `get` would triple its cost, and parking a `put` would only move
its write to another thread. The call runs on the worker, never on the
async runtime's threads, so HTTP is not blocked behind a write; a worker held
34 µs is far inside a 2 ms slice.

### `fetch`: outbound HTTP to an allowlist

`[fetch] allow` in `app.toml` lists where the app may call:
`scheme://host` (the scheme's port), `scheme://host:port`, or
`scheme://host:*` (any port), `http` or `https`. The `fetch` capability says
an app may call out at all; the allowlist says where, and the host holds
every call to it at the boundary:

- a URL off the list is answered `Err` **at once, before anything is
  sent**, without parking (counted in `fetch.refused`);
- redirects are not followed — a 3xx is answered as the response it is — so
  an allowed host cannot send the request elsewhere;
- hosts match by name as written; what a name resolves to is the system
  resolver's, so this is no defence against DNS rebinding.

**An API key the app never sees.** An app cannot read a secret (`auth.check`
only compares against one), so a key for an authenticated API is given to the
host instead, which adds it as a header when the request is sent:

```toml
[secrets]
openai = { env = "OPENAI_API_KEY" }
gemini = { env = "GEMINI_API_KEY" }

[fetch]
allow = ["https://api.openai.com", "https://generativelanguage.googleapis.com"]

[fetch.headers."https://api.openai.com"]
authorization = { secret = "openai", prefix = "Bearer " }

[fetch.headers."https://generativelanguage.googleapis.com"]
x-goog-api-key = { secret = "gemini" }
```

- Each `[fetch.headers."<origin>"]` key is a header name; its value names a
  `[secrets]` entry, with an optional literal `prefix` sent before it
  (empty by default).
- The app is refused at load, saying why and never quoting the value, if
  the origin is not a bare origin (`scheme://host[:port]`: no path, no
  `:*`), is not an entry of `[fetch] allow` (compared as parsed, so
  `https://api.openai.com` and `https://api.openai.com:443` are the same), if
  the secret is not in `[secrets]`, the header name is not one, or the prefix
  and secret cannot be a header value.
- The header is added to a request whose URL has **exactly that origin** —
  scheme, host and effective port — and to no other. It is added after the
  app's headers and **replaces** an app-supplied header of the same name
  (names compare case-insensitively).
- The value never reaches the app: not in a `fetch.Response`, an `Err`, a
  log line, the stats or the admin app's view of the app. Redirects are not
  followed, so a redirect cannot carry it to another origin.
- The admin app's allowlist changes cannot point it at a new origin: the
  origin is checked against `[fetch] allow` as `app.toml` writes it, and only
  `app.toml` can bind a header. Removing the origin from the allowlist at run
  time leaves the app loaded, and requests there are refused like any other
  off the list.

What the host does not do is defend the value against the app's own code:
apps share one process (see [Security](#security)). This keeps the key out of
the app's hands as a string, in its logs and in its answers.

A fetch parks the run. It is a future on the I/O runtime (reqwest over
hyper) raced against the run's deadline and against the client going away;
whichever ends first drops the future, which closes the outbound
connection, so the upstream sees the request abandoned. Each fetch is also
bounded by `[fetch] timeout` (default 10 s), `max_request_bytes` (its body,
default 1 MiB) and `max_response_bytes` (default 4 MiB). Any response is
`Ok`, whatever its status; an `Err` is a fetch that got none.

**Compressed responses are decoded.** A request goes out with
`accept-encoding: gzip, deflate, br` unless the app set its own
`accept-encoding`, which is sent as written. A response with
`content-encoding: gzip`, `deflate` or `br` is decoded whatever the request
asked for — some servers compress regardless — and the app sees the decoded
body, without the `content-encoding` and `content-length` headers, which
describe the encoded one. Any other encoding is handed over as it came,
header included.

- `max_response_bytes` bounds the **decoded** body. Decoding streams and
  stops as soon as the limit is passed, so a compression bomb (a megabyte
  that inflates to a gigabyte) is the usual too-large `Err`, refused without
  being inflated.
- A body that does not decode — corrupt, truncated, or not in the encoding
  its header names — is an `Err` (`fetch could not decode the response: …`),
  never garbage text.

**https** is supported, with rustls and the Mozilla root set
(`webpki-roots`), so there is no dependency on the system's OpenSSL. Inbound
TLS stays the reverse proxy's job, but outbound calls have no proxy to do it
for them.

### `app.toml`

```toml
entry = "hello.handle"          # optional; `<app>.handle` by default
grant = ["log", "timer"]        # the capabilities granted; none by default

[limits]                        # every key optional; the defaults are shown
fuel = 50000000                 # per request
max_host_calls = 1000           # per request
deadline = "10s"                # per request, parked time included ("ms" or "s")
max_call_depth = 512            # per request (the runtime's own default if absent)
max_heap_words = 4194304        # per request; see "Runtime constraints"
max_in_flight = 64              # this app's runs started and not answered
max_queued = 256                # this app's requests waiting to start
max_request_bytes = 1048576     # request body
max_response_bytes = 4194304    # response body

[kv]                            # the app's store; see `kv` above
max_key_bytes = 1024
max_value_bytes = 1048576
max_keys = 100000
max_bytes = 67108864            # keys and values, summed

[fetch]                         # outbound HTTP; see `fetch` above
allow = []                      # e.g. ["https://api.github.com", "http://127.0.0.1:*"]
timeout = "10s"                 # one fetch, connect to last byte
max_request_bytes = 1048576
max_response_bytes = 4194304

# A `[secrets]` value the host sends as a header to one origin of `allow`;
# see `fetch` above. The prefix is optional.
# [fetch.headers."https://api.openai.com"]
# authorization = { secret = "openai", prefix = "Bearer " }

[route]                         # see "Routing by hostname"
hosts = []                      # e.g. ["admin.example"]: reached by these only

[secrets]                       # what `auth.check` compares against, or
                                # `[fetch.headers]` sends; one of:
admin = { env = "APP_ADMIN_TOKEN" }   # an environment variable of the host
# admin = { store = "admin" }         # the host's secret store, set at run time
# admin = { file = "admin.secret" }   # a file, relative to the app's directory
# admin = { value = "..." }           # literal, for tests

[access]                        # `auth.identity`; see "Cloudflare Access"
team = { env = "ACCESS_TEAM_DOMAIN" }       # `<team>.cloudflareaccess.com`
aud = { env = "APP_ACCESS_AUD" }            # the Access application's AUD tag(s)
emails = { env = "ACCESS_ALLOWED_EMAILS" }  # optional: only these users
token = "admin"                 # a `[secrets]` name: the token way in
fallback = "none"               # or "token": the token as well with Access on
```

A secret that cannot be resolved (the variable unset, the file missing, the
store without it) refuses the app, saying which; its value is never printed.
`store` is for secrets only: an `[access]` setting cannot come from it. An `[access]`
value is a setting, not a secret: an unset (or empty) `team` or `aud` turns
Access off for the app, which the host logs when it loads it.

### Cloudflare Access: who is asking

An app behind [Cloudflare Access](deploy/cloudflare.md) asks the host who
its user is with `auth.identity(request.headers)`, instead of a login of its
own (issue #23). Access adds the user's token — a JWT signed by the team's
keys — to every request it lets through, as `Cf-Access-Jwt-Assertion` (and
the `CF_Authorization` cookie, read when the header is missing). The host
verifies it: the RS256 signature by a key of the team's JWKS
(`https://<team>/cdn-cgi/access/certs`, chosen by `kid`), `iss` the team,
`aud` one of the app's `[access] aud`, `exp` and `nbf` with a minute's
leeway, an `email` claim (so an Access service token is not an identity),
and `[access] emails` if set. The answer is `Ok(auth.Identity { email, via:
"access" })`, or `Err` saying what was wrong.

The keys are fetched with the first token and kept; a token naming a key not
held fetches them again (Cloudflare rotates them), at most once a second, and
keys an hour old are refreshed when next needed. If the keys cannot be
fetched, every token they would verify is refused and the failure is logged:
it fails closed. A fetch parks the run; a token whose key is held is answered
at once.

`[access] token` names a `[secrets]` entry presented as `Authorization:
Bearer <secret>` or a Basic password (`via: "token"`, no email). With Access
off for the app — a local run, where `team` or `aud` is unset — it is the way
in, and `auth.usesAccess()` is `false`, so the app can answer 401 with a
login prompt. With Access on it is accepted only under `fallback = "token"`;
the shipped apps say `"none"`, so a request that did not come through Access
— straight to `127.0.0.1:8790`, say — is refused whatever it presents, and
no browser prompt is shown (403). A script that needs in goes through Access
too, which takes an Access service token anyway, and a service token has no
email: the admin listener is the way for a script on the machine.

The apps only call `auth.identity`; nothing else of a request is affected,
so a path an app does not guard — the webhook lab's receive URLs — needs no
token and fetches no keys.

An unknown key is refused, so a misspelt limit is never silently not
applied. A config that does not read, an app that does not check (warnings
included), an entry that requires a capability the app is not granted, or
code that can `spawn` refuses **that app**: it is answered 503 with the
reason, and every other app starts.

### Limits and what they answer

| condition | status |
| --- | --- |
| no app by that name | 404 |
| the app was refused at load | 503 with the reason, no `Retry-After` |
| the app is disabled by the administrator | 503, no `Retry-After` |
| request body over `max_request_bytes` (by `Content-Length`, or as a chunked body arrives) | 413 |
| request body not UTF-8 | 400 |
| the app already has `max_queued` requests waiting | **429**, `Retry-After: 1` |
| the server already has `--max-in-flight` requests admitted | **503**, `Retry-After: 1` |
| `--max-connections` connections open | 503, `Retry-After: 1`, connection closed |
| the request waited in the queue longer than the app's deadline | 503, `Retry-After: 1`, not run |
| fuel, host calls, call depth spent | 500, the runtime's diagnostic |
| deadline passed, running or parked | 504, the runtime's diagnostic |
| other runtime error (an assertion, an overflow, out of memory) | 500, the runtime's diagnostic |
| the run's heap needs more than `max_heap_words` | 500 while it allocates, `x-cove-stop: heap` |
| response body over `max_response_bytes`, or not a valid response | 500, saying which |
| the client went away before the answer | the run is cancelled (queued, running, yielded or parked) and counted as `errors.cancelled`; 499 in the app's log |
| a `kv.put` past a quota, a `fetch` off the allowlist or past its limits | not a status: the app's `Err` to answer as it likes |

**Every answer of a run says what the run cost**, in headers the host adds:
`x-cove-run-fuel` and `x-cove-run-instructions` (the runtime's counts),
`x-cove-run-yields`, `x-cove-run-yields-declined`, `x-cove-run-parks`,
`x-cove-run-worker-us` (time on a worker, every slice summed) and
`x-cove-run-wall-us` (admission to answer). **A run a limit stopped also
says which**: `x-cove-stop` is the error kind's name — `fuel`, `deadline`,
`host_calls`, `call_depth`, `heap`, `queue_timeout`, `response_too_large`,
`cancelled`, `runtime`, … — so a page that runs a request with `fetch` can
say why without reading the diagnostic (`apps/algo` does). An app cannot
read its own meter during a run, which is why this is a header and not a
host call.

429 and 503 are deliberately different: 429 is *this app* at its own limit
(its neighbours are unaffected, and the client of that app should back off);
503 is the whole host. Every one is counted under the app in
`/_host/stats` (`rejected` and `errors`).

## How requests are scheduled

```text
  HTTP (hyper on tokio) ──admit──▶ ┌ app A: starts ░░░ runs ░ ┐
           ▲                       │ app B: starts ░   runs   │ ──take, round robin──▶ workers × N
           │ oneshot               └ app C: …                 ┘                          │
           │                               ▲   ▲                                         │
           │                               │   └── continue ◀── yielded at a safepoint ──┤
           │                               └────── resume ◀── host future on tokio ◀─────┤ parked
           └────────────────────────────────────────────────── answered ─────────────────┘
```

- **The HTTP front is hyper 1.x on tokio.** It owns the protocol —
  keep-alive, pipelining, chunked bodies, header size and read timeouts — and
  the sockets, so an idle connection costs a task and no thread, and waiting
  on thousands of them is epoll/kqueue rather than the edge sample's `poll(2)`
  over every socket (cove#590). It is the established choice for a Rust HTTP
  server, and it is a library rather than a framework: routing, limits and
  the stats are this crate's, a few hundred lines.
- **No Cove code runs on tokio's threads.** A request is admitted into its
  app's queue and the connection's task awaits a oneshot. A dedicated pool of
  worker threads (`--workers`) takes jobs and, for each request, builds a
  fresh `OwnedVm` over the app's shared `PreparedProgram` (a few
  microseconds; nothing of one request is visible to the next).
- **A run that waits holds no worker.** A host call that answers pending
  (`timer.sleep`) hands the scheduler a future; the run becomes a `ParkedVm`
  and the future runs on tokio, raced against the run's deadline. Its answer
  puts a resume job on the app's queue for whichever worker is free; a
  deadline that comes first cancels the run (504) and drops the future, which
  is how outstanding work will be withdrawn once there is outbound HTTP.
- **A long run is sliced.** Each worker publishes the run it is running; a
  monitor raises the run's `YieldRequest` once it has held its worker for a
  slice (`--slice`, 2 ms) *and* something is waiting for a worker. The run
  gives its worker up at its next safepoint — inside compiled code too (Cove
  ADR 0084, 0085) — and goes to the back of its app's queue as a
  `YieldedVm`, which any worker continues.
- **Apps are served round robin.** Each app has its own queue: runs to resume
  or continue, and requests waiting to start. A worker takes one job from the
  next app in turn that has work it may run; within an app, turns alternate
  between its started runs and its new requests, so a long run of an app
  does not hold back that app's own new requests either. A job is at most one slice of a run while others
  wait, so an app with hundreds of requests queued gets one turn in N like
  every other busy app: its backlog costs itself, not its neighbours. An app
  at `max_in_flight` has its starts held back (its started runs still
  continue); at `max_queued` new requests are rejected.

Turns are equal, not CPU: a turn that answers in 50 µs and one that runs its
full slice count the same. That is enough to stop starvation, which is what
this base needs; weighting turns by worker time (deficit round robin over
`worker_ms`) fits the same queues if measurements show it is wanted.

## Runtime constraints, made explicit

What the runtime cannot do yet is handled by refusing it or by counting it,
never by trusting it:

- **`spawn` is refused at load.** A spawned task is a thread outside the
  worker pool and its accounting; while one is alive its parent can neither
  park nor yield (ADR 0080 §2, 0084 §3), and it has no native tier (ADR
  0085). An app whose entry the checker says can reach a task `scope`
  (`FnEntry::can_spawn`, ADR 0088) is refused before it is prepared, with a
  diagnostic at the `spawn` (`cove_host::spawn`, above) — or at the entry
  when there is none to point at. That fact is a lower bound for a
  capability-open entry, so such an entry in a package where some function
  opens a scope is decided by its lowered program instead. Every run also
  carries `max_tasks = 0` as the runtime's backstop.
- **A run that cannot yield is surfaced.** Below an encoded function that
  compiled code called, inside a host call, or beside a task, a run declines a
  yield request and keeps its worker (ADR 0085's open items). The monitor
  counts a run still holding its worker well after it was asked (four slices,
  at least 20 ms) as `overdue_yields` and logs it; the runtime's own
  `yields_declined` is summed per app; and the run's **deadline still ends
  it**, since the deadline is checked at every safepoint whether or not the
  run can yield. `/_host/apps/<app>`'s `native.refusals` names the functions
  left on the encoded tier, with the instruction and the source line: a
  function that makes a host call, or writes a lambda, is one — and an
  algorithm called from such a function, by compiled code, cannot yield at
  all. The algorithm playground met exactly that ([its README](apps/algo/README.md#yields-on-the-native-tier)).
- **A host call that cannot park blocks, and is counted.** In the same
  places a call cannot park; `timer.sleep` then sleeps on the worker
  (`blocking_host_calls`), bounded by its 60 s maximum. None of the sample
  apps reaches this.
- **`max_heap_words` is the run's heap capacity.** Each isolate is built
  with `OwnedVm::with_heap_words` (ADR 0088), so a run that needs more fails
  the allocation where it makes it ("this run has no memory left", 500,
  `errors.heap`) rather than after it answers. Without it a run gets the
  runtime's default, 4 Mi words (32 MiB). It bounds Cove's heap, not what a
  host module holds outside it; above `u32::MAX` words, the most the runtime
  addresses, it is refused. A run's heap and declined yields are read at its
  every park and yield as well as its answer (`heap_peak_words`,
  `yields_declined`).
- **The native tier is used where it exists.** `--backend auto` compiles each
  app with `PreparedProgram::with_native` once at load on Unix x86-64 and
  says once, on stderr, that it falls back to the encoded VM elsewhere;
  `--backend vm` forces the VM, `--backend native` refuses apps where there is
  no native tier. `tier` in the stats says which each app got.
- **A client that disconnects cancels its run.** hyper drops the request's
  future when the connection closes, and a guard on it raises the request's
  cancellation: the runtime's `Cancellation` in the run's budget stops a
  running run at its next safepoint and a yielded one when it is continued;
  `Cancellation::on_cancel`, registered on the flag the parked run's meter
  hands back, wakes a parked run's wait, which cancels the run and drops its
  pending work (a fetch's connection with it); a request still queued is
  dropped when a worker reaches it. A run that cannot yield stops at its next
  safepoint all the same, since cancellation is checked there.

## Security

Same-process isolation between apps is a fault and resource boundary for code
you run yourself: each request is its own isolate with its own heap and
budget, an app can reach only the host modules it is granted, and one app's
bug, overload or budget overrun ends that app's requests and nobody else's.
**It is not a security guarantee against untrusted or malicious code**: the
apps share one process, one address space and one native code generator, and
nothing here has been reviewed as a sandbox.

An app behind Cloudflare Access need not trust that only the proxy reaches
the host: with `[access]`, the host verifies the Access token itself
(`auth.identity`), so a request straight to the host's port has no identity.

There is no TLS. Put the host behind a reverse proxy (Caddy, nginx,
cloudflared) that terminates TLS and forwards to `--addr`, which defaults to
localhost, and tell the host where it is reached (`--public-origin`) so that
the apps' same-origin checks and the URLs they write use the public origin:
see [Deploying](#deploying).

## Performance

`bench/perf.sh` runs `cove-host-load` (a small load generator in this crate)
against a host with four workers — the configuration of the Cove repository's
edge/Go comparison (`examples/edge/compare/README.md`, cove #589/#593) —
using the same method: open loop with latency measured from each request's
**intended** start (no coordinated omission), and closed loop for capacity.
[`bench/README.md`](bench/README.md) has the conditions, the commands, the
full tables and the comparison; in short, on the same machine
(i7-10700K, macOS, native tier):

| | cove-host | edge (native where measured) | Go |
| --- | ---: | ---: | ---: |
| `hello` capacity, 64 in flight | 110,671 req/s | 84,235 (VM) | 122,725 |
| `crunch n=20000` capacity, 16 in flight | 4,809 req/s | 2,783 | 3,720 |
| `cpu-io` mix capacity, 256 in flight | 1,831 req/s | 1,044 | 1,364 |
| `hello` p99 inside the mix at 990 req/s | 2.3 ms | 13.5 | — |

The HTTP stacks differ (hyper on tokio here; the edge sample's own std
HTTP/1.1 with a `poll(2)` idle thread; Go's `net/http`), the load generators
differ, and the edge `crunch` row predates the native tier's 32-bit division
(cove #596), which this host's Cove revision has. So these say *no
regression* rather than *faster than Go*. The check that matters for the
scheduler: **`hello`'s p99 inside the CPU+I/O mix stays at 2.3–3.0 ms from
330 to 1,434 req/s** (78% of the mix's capacity), against 2.6 ms for `hello`
alone; past saturation, the 2 ms slice keeps it at 95 ms against 139 ms
without one.

Beside a real CPU-heavy app — the algorithm playground's matching runs
holding all four workers — `hello`'s p99 is 4.7 ms on the native tier and
4.7 ms on the VM, against 2.0 ms alone (`bench/algo.sh`,
[`apps/algo/README.md`](apps/algo/README.md#responsiveness)).

```console
$ cargo build --profile checked
$ sh bench/perf.sh 3 > bench/results/perf-$(date +%F).txt
$ SLICE=0 ONLY=mix sh bench/perf.sh 3 > bench/results/perf-$(date +%F)-noslice.txt
$ python3 bench/summarize.py bench/results/perf-*.txt
```

## Issue #17's completion criteria

| criterion | where it is shown |
| --- | --- |
| an app enabled and disabled from the browser; a disabled app does not answer; enabled again, its KV data is there | `admin.rs::the_pages_enable_disable_configure_and_reset`, `::a_disabled_app_answers_503_finishes_what_it_had_and_keeps_its_data` (a parked request finishes; the store survives) |
| a change of grant or budget is applied only when it checks; a wrong one keeps the current config and says why | `admin.rs::a_change_that_is_wrong_is_refused_with_the_reason_and_changes_nothing` (eight kinds), `::the_pages_enable_disable_configure_and_reset` (the form's problems and the host's reason, on the page) |
| an app whose needed capability is taken away is refused, and the others answer | `admin.rs::taking_away_a_needed_capability_refuses_that_app_and_no_other` |
| `admin` cannot be granted to any app but the admin app | `admin.rs::only_the_admin_app_may_be_granted_admin` (at load, by `check`, and by a change); the admin app cannot disable itself or drop it: `::the_admin_app_cannot_disable_itself_or_drop_admin_but_the_listener_can` |
| changes survive a restart; unauthenticated and cross-site operations are refused | `admin.rs::changes_survive_a_restart_and_a_release_that_replaces_the_apps`, `::the_admin_app_needs_its_secret`, `::a_cross_site_form_changes_nothing`; the history: `::the_history_says_who_when_and_what`; escaping: `::the_pages_escape_what_they_show`; the hostname: `::the_admin_app_is_reached_by_its_hostname_only`, `::an_app_with_a_hostname_is_reached_by_it_and_by_nothing_else` |
| tests, and the Cloudflare Access steps | [Tests](#tests); [deploy/cloudflare.md](deploy/cloudflare.md) §4, [apps/admin/README.md](apps/admin/README.md) |

## Issue #1's completion criteria

| criterion | where it is shown |
| --- | --- |
| two or more independent Cove apps run at once | `host.rs::two_or_more_apps_are_served_concurrently`; the walkthrough above serves five |
| per-app KV isolation, and persistence across a restart | `services.rs::an_apps_keys_are_its_own`, `::the_store_survives_a_restart`; manual: the `notes` walkthrough, restart, `GET /notes/todo` |
| a light app keeps answering beside a CPU-heavy and an I/O app | `host.rs::hello_answers_while_crunch_saturates_the_workers_and_slow_is_parked`; measured: [Performance](#performance), `hello`'s p99 in the mix |
| budget overrun, overload and an invalid update stop no other app | `host.rs::a_budget_overrun_ends_that_request_only`, `::overload_is_rejected_explicitly_and_other_apps_still_answer`, `updates.rs::a_failed_update_keeps_the_current_version_and_says_why` (another app answering throughout four kinds of refused update) |
| requests on the old version complete during an update | `updates.rs::in_flight_requests_finish_on_the_old_version_and_new_ones_get_the_new` (parked, running, yielded and queued, each answered by v1; the next by v2; v1 and its program dropped after) |
| tests, and procedures for starting, updating and checking performance | [Tests](#tests); [Building and running](#building-and-running), [Updating an app](#updating-an-app), [Performance](#performance) |

## Tests

```console
$ cargo t     # = cargo test --workspace --profile checked
```

The tests run Cove programs, so they run optimised (`--profile checked`, as
in Cove's own repository); a bare `cargo test` works, more slowly. The
integration tests (`crates/cove-host/tests/host.rs`, `services.rs` and
`updates.rs`, `admin.rs`, `access.rs`, `secrets.rs`, and one per real app: `webhooks.rs`, `ledger.rs`, `algo.rs`) start the host
in-process on a free port and ask it over TCP. None asserts a duration: where
a test needs the host in some state it waits for the host's own stats to say
so, and what it asserts is counted.

- two or more apps answer concurrently;
- `hello` answers while `crunch` holds every worker and `slow` is parked,
  and the crunches yielded and the slow runs parked at every sleep without
  blocking a worker;
- an app past its `max_queued` gets 429 with `Retry-After` while another app
  answers; the server past `--max-in-flight` gets 503; past
  `--max-connections`, 503;
- fuel, deadline (while parked) and host-call overruns end only that request;
- an over-reaching app and a spawning app are refused at load, the others
  serve, and `check` agrees;
- request (declared and chunked) and response size limits;
- with one app flooding its queue on a single worker, another app's five
  requests are all served while the flood still has requests queued;
- on x86-64 Unix, the apps run on the native tier and a compiled run still
  yields;
- two apps' stores are separate (and separate files); a store survives
  stopping the host and starting another over the same data directory;
  quotas answer the app's `Err`; listing pages both ways;
- `fetch` reaches an allowed local upstream with GET and with POST carrying
  a header and a body, exactly as the upstream records them; a target off
  the allowlist is refused with the upstream seeing no connection at all;
- a `[fetch.headers]` secret reaches its origin, with its prefix, replacing
  the app's header of the same name; another allowed origin gets the app's
  header and not the secret; and the value is in none of the app's answers,
  a failed fetch's `Err`, its logs, its admin view or the stats;
- a fetch is abandoned — the upstream sees its connection closed — at the
  run's deadline, and when the client goes away;
- a client going away cancels a spinning run that is running or yielded, one
  still queued (without running it), and a parked one, each counted;
- an update while requests are parked, running, yielded and queued: each of
  those answers with the old version's body and `x-cove-app-version`, a
  request after the switch with the new one's, and once they have drained
  the old version and its `PreparedProgram` are gone;
- an update refused for a parse error, an ungranted capability, a `spawn` or
  a config error keeps the current version serving, answers 422 with the
  reason, and another app answers throughout;
- the admin listener answers 401 without the token (missing, wrong, empty)
  and changes nothing, and the public listener has no update route;
- an update adds an app, `remove` removes one, and an update brings it back;
- deploying (`deploying.rs`, and `deploy.rs`'s unit tests): an archive is
  refused for a symbolic link, a path that leaves the directory (`..`, an
  absolute path), a hard link, a pipe, a duplicate or its size, and packs
  `app.toml` and `.cove` files only; a deploy adds an app, and replaces one
  while a request parked on the old version finishes on it; a deploy that
  does not parse, that needs an ungranted capability or that needs a secret
  the env lacks is refused with its reason and changes no file; rollback
  restores the kept version and a second undoes it; both need the token;
  `cove-host deploy` from a directory and from stdin, and `--into`;
  `deploy/smoke.sh` (CI, Linux) checks `install.sh` leaves an app that is
  not the release's alone and refuses to switch while one does not check;
- through the `host` module: the list shows every app's state, grant and
  limits; a disabled app answers 503 while one of its requests in flight
  finishes, and enabled again has its store; taking a needed capability
  away refuses that app alone, and granting it back restores it; eight kinds
  of wrong change are refused with their reasons and change nothing; `admin`
  granted to another app is refused at load and by `check`; the admin app
  cannot disable itself or drop `admin`, and the admin listener can; the
  changes survive a restart and a release that replaces `apps/`; the history
  records who, when and what; an app with a hostname is reached by it alone,
  as that hostname's origin, and a hostname reaches one app;
- the secret store (`secrets.rs`, and `secrets.rs`'s and `config.rs`'s unit
  tests): the file survives reopening, is mode 0600, leaves no temporary
  file, and a write that fails leaves the old file and value; names and
  values are held to their rules and a broken file is refused, none of it
  quoting a value; an app whose stored secret is missing is refused naming
  it; setting it reloads the app, and `auth.check` and a `[fetch.headers]`
  header sent to a test upstream use the new value, replacing it the old
  value no longer works, and a restart finds it; a reload that fails keeps
  the value stored and the serving version; deleting a secret an app uses is
  409 unless forced, and forced refuses the app; the admin app sets,
  replaces and deletes from its page (a cross-site POST refused, a delete
  needing *confirm*, and *force* when used); `cove-host secret set|list|delete`;
  `check --data` and `test` without the secret (on a placeholder, said); and
  the value is in none of the listener's answers, the admin pages, the stats,
  the operations views, the logs, the change history or `overrides.json`;
- the operations page escapes markup an app logs, and shows errors and KV
  usage against the quota; an app's log reaches `<data>/<app>/log.txt`;
- Cloudflare Access (`access.rs`, against a JWKS the test serves with RSA
  keys it generates): a valid token gets into the admin UI (header or
  cookie) and the history records its verified email, not the unverified
  `Cf-Access-Authenticated-User-Email`; no token, a malformed one, a forged
  signature, another application's `aud`, another team's `iss`, an expired
  one, one not valid yet, one with no `exp`, one with no email (a service
  token) and an email not allowed are each refused with 403 and no prompt,
  and change nothing; the admin secret alone is refused while Access is on;
  a token signed by a rotated-in key fetches the keys again and gets in, the
  keys are then held, and an unknown key is refused with its fetches rate
  limited; with the JWKS down every token is refused and the failure logged,
  and back up the next one gets in; `fallback = "token"` lets the secret in,
  recorded as `token`; with Access off the secret and its Basic prompt are
  as before; the webhook lab's pages need the lab's own application's token
  while its receive URLs need nothing and fetch no keys.

`COVE_HOST_TEST_BACKEND=vm cargo t` runs the same suite on the encoded VM,
as CI does on its second pass.
