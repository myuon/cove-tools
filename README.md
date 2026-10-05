# cove-tools

Small web apps written in [Cove](https://github.com/myuon/cove), and the host
that runs them on one machine.

- **`crates/cove-host`** — a self-hosted function host: loads `apps/<name>/`,
  checks, prepares and compiles each app once, and runs every request in its
  own Cove isolate on a shared worker pool (issue #1).
- **`apps/`** — the sample apps: `hello` (pure), `crunch` (CPU-heavy),
  `slow` (waits on a timer that parks the run), `notes` (the persistent
  key-value store) and `proxy` (allowlisted outbound HTTP); and the real
  apps: [**`webhooks`**](apps/webhooks/README.md), the webhook lab (#2). A
  benchmark ledger (#3) and an algorithm playground (#4) come next.

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
lines on stdout), and `--admin ADDR` (the admin listener for updates, default
`127.0.0.1:8081`) or `--no-admin`.

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
| `GET /_host/apps/<app>` | one app as JSON: the above, its `limits`, every version it has had (`version`, `loaded_unix_s`, `current`, `alive`, `program_alive`) and its last 50 errors (`unix_ms`, `kind`, `status`, `version`, `message`) |
| `GET /_host/apps/<app>/logs?n=200` | its recent log lines, as text |

They are on the public listener and unauthenticated, so a reverse proxy in
front of the host should not forward `/_host/` (an error message names
source lines).

`/_host/stats`, per app: `state`
(`ready`, `refused` with the reason, or `removed`), `version`, `tier`, `required` and `granted`,
`served`, `ok`, `errors` by kind, `rejected` by reason, `in_flight`,
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
spawner    requires [-]  granted [-]  REFUSED: can spawn a task:
checked 2 app(s) against the host's schemas (web, log, timer); 0 warning(s), 2 refused
$ echo $?
1
```

`test` runs every `test fn` of each app the way `cove test` does, with the
app's grant (not the test's derived one), its limits, and the host's modules:

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
nothing. The endpoints are `POST /apps/<app>/update`, `DELETE /apps/<app>`
and `GET /apps` (the stats). There is no file watcher: an update is an
explicit act, which is what makes a refused one a report rather than a
silent non-event.

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
| `fetch` | `fetch` | `get(url)` and `request(method, url, headers: Map<String, String>, body)`, each `-> Result<fetch.Response, Error>` |
| `time` | `time` | `nowMillis() -> Int`: the wall clock, milliseconds since the Unix epoch; `nowMicros() -> Int`: microseconds, strictly increasing across the process |
| `random` | `random` | `hex(bytes: Int) -> String`: 1–64 random bytes from the operating system, as hex |
| `auth` | `auth` | `check(secret: String, authorization: String) -> Bool`: whether an `Authorization` header (`Bearer <s>`, or `Basic` with `<s>` as the password) presents the app's secret `secret`. Constant-time; the secret itself never reaches the app |

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

A fetch parks the run. It is a future on the I/O runtime (reqwest over
hyper) raced against the run's deadline and against the client going away;
whichever ends first drops the future, which closes the outbound
connection, so the upstream sees the request abandoned. Each fetch is also
bounded by `[fetch] timeout` (default 10 s), `max_request_bytes` (its body,
default 1 MiB) and `max_response_bytes` (default 4 MiB). Any response is
`Ok`, whatever its status; an `Err` is a fetch that got none.

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

[secrets]                       # what `auth.check` compares against; one of:
admin = { env = "APP_ADMIN_TOKEN" }   # an environment variable of the host
# admin = { file = "admin.secret" }   # a file, relative to the app's directory
# admin = { value = "..." }           # literal, for tests
```

A secret that cannot be resolved (the variable unset, the file missing)
refuses the app, saying which; its value is never printed.

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
| request body over `max_request_bytes` (by `Content-Length`, or as a chunked body arrives) | 413 |
| request body not UTF-8 | 400 |
| the app already has `max_queued` requests waiting | **429**, `Retry-After: 1` |
| the server already has `--max-in-flight` requests admitted | **503**, `Retry-After: 1` |
| `--max-connections` connections open | 503, `Retry-After: 1`, connection closed |
| the request waited in the queue longer than the app's deadline | 503, `Retry-After: 1`, not run |
| fuel, host calls, call depth spent | 500, the runtime's diagnostic |
| deadline passed, running or parked | 504, the runtime's diagnostic |
| other runtime error (an assertion, an overflow, out of memory) | 500, the runtime's diagnostic |
| the run's heap above `max_heap_words` when it answered | 500 |
| response body over `max_response_bytes`, or not a valid response | 500, saying which |
| the client went away before the answer | the run is cancelled (queued, running, yielded or parked) and counted as `errors.cancelled`; 499 in the app's log |
| a `kv.put` past a quota, a `fetch` off the allowlist or past its limits | not a status: the app's `Err` to answer as it likes |

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
  0085). An app whose lowered entry contains a `spawn` is refused with a
  diagnostic at the `spawn` (`cove_host::spawn`, above), and every run also
  carries `max_tasks = 0` as the runtime's backstop.
- **A run that cannot yield is surfaced.** Below an encoded function that
  compiled code called, inside a host call, or beside a task, a run declines a
  yield request and keeps its worker (ADR 0085's open items). The monitor
  counts a run still holding its worker well after it was asked (four slices,
  at least 20 ms) as `overdue_yields` and logs it; the runtime's own
  `yields_declined` is summed per app; and the run's **deadline still ends
  it**, since the deadline is checked at every safepoint whether or not the
  run can yield.
- **A host call that cannot park blocks, and is counted.** In the same
  places a call cannot park; `timer.sleep` then sleeps on the worker
  (`blocking_host_calls`), bounded by its 60 s maximum. None of the sample
  apps reaches this.
- **The per-run heap is the runtime's fixed 32 MiB.** `OwnedVm::new` builds
  every run over the runtime's `DEFAULT_HEAP_WORDS` (4 Mi words) and offers no
  way to choose another, so that is the hard ceiling ("this run has no memory
  left", 500). `max_heap_words` is checked when a run answers — the one point
  the runtime exposes a run's heap — and replaces an over-limit answer with a
  500; it cannot stop a run while it allocates, and a value above the ceiling
  is refused.
- **The native tier is used where it exists.** `--backend auto` compiles each
  app with `PreparedProgram::with_native` once at load on Unix x86-64 and
  says once, on stderr, that it falls back to the encoded VM elsewhere;
  `--backend vm` forces the VM, `--backend native` refuses apps where there is
  no native tier. `tier` in the stats says which each app got.
- **A client that disconnects cancels its run.** hyper drops the request's
  future when the connection closes, and a guard on it raises the request's
  cancellation: the runtime's `Cancellation` in the run's budget stops a
  running run at its next safepoint and a yielded one when it is continued;
  a token wakes a parked run's wait, which cancels the run and drops its
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

There is no TLS. Put the host behind a reverse proxy (Caddy, nginx) that
terminates TLS and forwards to `--addr`, which defaults to localhost.

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

```console
$ cargo build --profile checked
$ sh bench/perf.sh 3 > bench/results/perf-$(date +%F).txt
$ SLICE=0 ONLY=mix sh bench/perf.sh 3 > bench/results/perf-$(date +%F)-noslice.txt
$ python3 bench/summarize.py bench/results/perf-*.txt
```

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
integration tests (`crates/cove-host/tests/host.rs`, `services.rs` and `updates.rs`) start the host
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
- the operations page escapes markup an app logs, and shows errors and KV
  usage against the quota; an app's log reaches `<data>/<app>/log.txt`.

`COVE_HOST_TEST_BACKEND=vm cargo t` runs the same suite on the encoded VM,
as CI does on its second pass.
