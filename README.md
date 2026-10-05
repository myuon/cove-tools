# cove-tools

Small web apps written in [Cove](https://github.com/myuon/cove), and the host
that runs them on one machine.

- **`crates/cove-host`** — a self-hosted function host: loads `apps/<name>/`,
  checks, prepares and compiles each app once, and runs every request in its
  own Cove isolate on a shared worker pool (issue #1).
- **`apps/`** — the sample apps: `hello` (pure), `crunch` (CPU-heavy) and
  `slow` (waits on a timer that parks the run). The real apps — a webhook lab
  (#2), a benchmark ledger (#3) and an algorithm playground (#4) — come next.

The host is Rust; the apps are Cove. Cove is a git dependency pinned to one
commit (`rev` in the workspace `Cargo.toml`), and a change the compiler or
the runtime needs goes to [myuon/cove](https://github.com/myuon/cove) first.

## Building and running

```console
$ cargo build --profile checked
$ ./target/checked/cove-host serve --apps apps
cove-host: loading apps from apps
  crunch     requires [-]  granted [-]  ok: 227 fn on native, checked in 17.9 ms, prepared in 1.9 ms
  hello      requires [-]  granted [-]  ok: 234 fn on native, checked in 13.1 ms, prepared in 3.4 ms
  slow       requires [log, timer]  granted [log, timer]  ok: 220 fn on native, checked in 16.7 ms, prepared in 1.6 ms

listening on http://127.0.0.1:8080 — 16 worker thread(s), a run yields after 2.0 ms while others wait, a fresh isolate per request, apps served round robin
  stats: curl -s http://127.0.0.1:8080/_host/stats
```

(`--profile checked` is release with debug assertions and overflow checks
back on; `--release` works as well. The tests use the same profile — see
[Tests](#tests).)

`serve` takes `--addr` (default `127.0.0.1:8080`), `--workers N` (threads
that run Cove; default one per hardware thread), `--io-threads N` (HTTP and
pending host work; default 2), `--slice MS` (default 2; `0` never asks a run
to yield), `--max-connections N` and `--max-in-flight N` (default 10,000
each), `--backend auto|vm|native` (default `auto`) and `--quiet` (no `log`
lines).

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

`GET /` lists the apps; `GET /_host/stats` is JSON — per app: `state`
(`ready` or `refused`, with the reason), `tier`, `required` and `granted`,
`served`, `ok`, `errors` by kind, `rejected` by reason, `in_flight`,
`queued`, `parked`, `parks`, `yields`, `yield_requests`, `yields_declined`,
`overdue_yields`, `blocking_host_calls`, `instructions`, `fuel`, `worker_ms`
and `heap_peak_words`; and the server's `connections`,
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
| `timer` | `timer` | `sleep(millis: Int)`: parks the run for that long (at most 60 s) |

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
```

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
  next app in turn that has work it may run, finishing an app's started runs
  before starting more. A job is at most one slice of a run while others
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
- **A client that disconnects is not yet noticed.** Its run finishes and the
  answer is dropped. (Cancellation on disconnect is the next pull request's.)

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

## Tests

```console
$ cargo t     # = cargo test --workspace --profile checked
```

The tests run Cove programs, so they run optimised (`--profile checked`, as
in Cove's own repository); a bare `cargo test` works, more slowly. The
integration tests (`crates/cove-host/tests/host.rs`) start the host
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
  yields.

`COVE_HOST_TEST_BACKEND=vm cargo t` runs the same suite on the encoded VM,
as CI does on its second pass.
