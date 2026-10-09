# The performance check

Is cove-host slower than the server it grew out of? The Cove repository's
`examples/edge` was measured against the same service in Go
(`examples/edge/compare/README.md`, cove #589 for the VM and #593 for the
native tier). This directory runs the same kinds of load against cove-host,
on the same machine, so that a regression in the host's HTTP front, its
scheduler or its isolates shows up as a number.

```console
$ cargo build --profile checked
$ ulimit -n 10240
$ sh bench/perf.sh 3 > bench/results/perf-$(date +%F).txt                   # about 3 minutes
$ SLICE=0 ONLY=mix sh bench/perf.sh 3 > bench/results/perf-$(date +%F)-noslice.txt
$ python3 bench/summarize.py bench/results/perf-*.txt
```

`perf.sh` starts its own host on port 18180 with `--workers 4` (the edge
comparison's four workers; `GOMAXPROCS=4` for Go), the sample apps with their
per-app limits raised so the load is measured rather than rejected (the edge
server had no per-tenant limits), `proxy` allowed to fetch from the host
itself, admin off and a scratch data directory. It refuses to start if
something already answers on the port.

## Method

`cove-host-load` follows `cove-edge-load --from-intended`:

- **Open loop** (`--rate N`): request *i* is due *i* / N seconds into the
  run and its latency counts **from when it was due**, so a server that
  falls behind is charged for the queue it makes. The send lag (how late
  requests went out) is printed beside it; the generator's own floor is
  about 1 ms at the median (tokio's timer granularity) and 2.5 ms at p99.
- **Closed loop** (`--rate 0`): each connection asks again as soon as it is
  answered; the throughput is the capacity at that many in flight.
- Connections are kept alive, each a task on an eight-thread tokio runtime
  (`cove-edge-load` had eight threads); 200 warm-up requests first.
- The mixes, by app: `hello` is `/hello/?name=load`; `crunch20k` is
  `/crunch/?n=20000`; `crunch` cycles `n` = 20000, 50000, 100000, 150000 (as
  edge's mix did); `slow` is `/slow/?ms=60&times=3`, three parks totalling
  180 ms — edge's `aggregate` is three parks of 20–100 ms (mean 60); `proxy`
  fetches `hello` from the same host. **`cpu-io`** is
  `crunch=35,slow=30,proxy=10,hello=25`: edge's `cpu-io` with `slow` for
  `aggregate`, and `impatient`'s 5% (a deadline test) given to `hello`.

## Conditions

2026-10-05, 03:13–03:18 UTC: Intel i7-10700K (8 cores, 16 threads), macOS
26.6.2, rustc 1.98.1, `--profile checked` (release with debug assertions and
overflow checks), Cove at 9272282, `--backend auto` (native), load average
3.4–4.1 with other agents' work on the machine. The generator runs on the
same machine. Three repetitions; medians, with the range in parentheses.
The raw output is `results/perf-2026-10-05.txt` and
`results/perf-2026-10-05-noslice.txt`.

## Results

### Capacity (closed loop)

| scenario | in flight | cove-host req/s | p50 / p99 ms | edge, same in flight | Go |
| --- | ---: | ---: | ---: | ---: | ---: |
| `hello` | 64 | **110,671** (106,675–113,331) | 0.6 / 1.0 | 84,235 (VM) | 122,725 |
| `hello` | 256 | **105,641** (101,666–106,841) | 2.3 / 4.6 | 119,242 (VM) | 163,543 |
| `crunch n=20000` | 16 | **4,809** (4,750–4,912) | 3.3 / 3.7 | 2,783 native, 907 VM | 3,720 |
| `crunch n=20000` | 64 | **4,805** (4,804–4,910) | 13.2 / 13.8 | 2,778 native | 3,695 |
| `cpu-io` | 256 | **1,831** (1,798–1,841) | 14.7 / 469.9 | 1,044 native | 1,364 |

### Latency against a fixed rate (open loop)

| scenario | offered req/s | answered req/s | p50 ms | p99 ms | edge p99 at that rate | Go p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `hello` | 25,000 | 24,978 | 1.3 | 2.6 | 2.9 (VM) | 2.9 |
| `hello` | 75,000 | 74,673 | 1.8 | 4.9 (4.7–5.7) | 3.2 (VM) | 3.0 |
| `crunch n=20000` | 1,500 | 1,500 | 1.8 | 3.0 | 3.9 native | 3.2 |
| `crunch n=20000` | 3,500 | 3,498 | 1.9 | 3.8 | saturated (native) | 12.3 |
| `slow` (3 × 60 ms) | 2,000 | 1,912 | 184.9 | 187.0 | — | — |

`slow`'s floor is 180 ms of waiting; at 2,000 req/s about 370 requests are
parked at once on four workers, and its p99 is 7 ms over the floor.

### `hello` inside the `cpu-io` mix

| offered req/s | 2 ms slice: `hello` p50 / **p99** | no slice: `hello` p50 / **p99** | `crunch` p50 / p99 (slice) | `slow` p50 (slice) | edge native, 2 ms slice: `hello` p99 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 330 | 1.5 / **2.7** | 1.5 / 2.7 | 6.9 / 13.7 | 184.9 | 2.9 |
| 990 | 1.1 / **2.3** | 1.1 / 2.3 | 6.4 / 13.0 | 184.5 | 13.5 |
| 1,500 (1,434 answered) | 1.3 / **3.0** | 1.2 / 3.5 | 6.8 / 13.6 | 184.7 | — |
| closed loop, 256 in flight (1,831) | 1.2 / **95.1** | 2.5 / 139.0 | 13.5 / 262.7 | 404.9 | — |

Every request in every run was answered 200; none failed.

## What it says

- **No regression against the edge server.** Everywhere the two were
  measured on the native tier, cove-host is at or above edge: `crunch`
  capacity 4,809 against 2,783 req/s, and the mix's 1,831 against 1,044.
  Most of that is not the host. The edge rows predate cove #596 (the native
  tier divides in 32 bits), which this host's Cove revision has, and
  `crunch`'s run is about half as long. So the two are not comparable as a
  host-against-host measure; what they do show is that nothing in
  cove-host's queueing, slicing or HTTP front eats the faster code.
- **`hello` is near Go's capacity at 64 in flight (0.90×) and further
  below it at 256 (0.65×)**, and edge's own server was at 1.46× and 1.37×
  below Go. hyper on tokio is not the edge sample's hand-written HTTP/1.1,
  and the generators are not the same program, so the per-request cost is
  the honest comparison to make next (edge's profile put 29 µs of its 41 µs
  per `hello` in its host). At 256 in flight cove-host's capacity *falls*
  (106k against 111k at 64), which the edge server's did not; a profile of
  the hand-off between tokio and the workers is where to look.
- **Slicing works through cove-host's scheduler.** `hello`'s p99 inside the
  mix is 2.3–3.0 ms from 330 to 1,434 req/s (78% of the mix's capacity),
  within the generator's floor of `hello` alone (2.6 ms at 25,000 req/s).
  Below saturation the faster `crunch` (7 ms a request) rarely holds all four
  workers, so the slice changes little (3.0 against 3.5 ms at 1,434). Past
  saturation it is what bounds the short requests: 95 against 139 ms p99,
  and `hello`'s p50 1.2 against 2.5 ms, while `crunch`'s p99 pays (263
  against 148 ms) — processor sharing's trade, as edge measured it.
- **Parked runs hold no worker.** 370 `slow` requests parked at once on four
  workers, p99 7 ms over the 180 ms floor.

## Not measured

CPU per request (the edge tables have it from `ps`); `aggregate`'s random
upstream latencies (`slow` is fixed); more than one host process; Linux.
CI does not run this: a shared runner's numbers say nothing.

## Beside the algorithm playground

`bench/algo.sh` measures `hello`'s latency (open loop, 500 req/s) while K
clients keep the algorithm playground's heavy matching run in flight
(`cove-host-load --mix algo`, closed loop), four workers, for K = 0, 4, 8, on
one backend; it prints the `algo` app's yield counters after each. Results
and the native-tier finding they led to are in
[`examples/algo/README.md`](../examples/algo/README.md#responsiveness):
`hello`'s p99 goes from 2 ms to under 5 ms beside them, on both tiers.

```console
$ sh bench/algo.sh 3 native > bench/results/algo-$(date +%F)-native.txt
$ sh bench/algo.sh 3 vm > bench/results/algo-$(date +%F)-vm.txt
```

`MIX=algo-sat` runs the same with SAT's heavy run (DPLL refuting 8 pigeons in
7 holes) instead. `bench/repro/` is not a benchmark: it reproduces a
native-tier fault the playground met (a sliced run's `toVector` copy into a
store of the wrong size), for the Cove issue — `sh bench/repro/run.sh native`.
