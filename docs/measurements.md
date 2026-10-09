# minicloud: measurements

## Throughput and latency against the edge sample and Go

`bench/perf.sh` runs `minicloud-load` (a small load generator in this crate)
against a host with four workers — the configuration of the Cove repository's
edge/Go comparison (`examples/edge/compare/README.md`, cove #589/#593) —
using the same method: open loop with latency measured from each request's
**intended** start (no coordinated omission), and closed loop for capacity.
[`bench/README.md`](../bench/README.md) has the conditions, the commands, the
full tables and the comparison; in short, on the same machine
(i7-10700K, macOS, native tier):

| | minicloud | edge (native where measured) | Go |
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
[`examples/algo/README.md`](../examples/algo/README.md#responsiveness)).

```console
$ cargo build --profile checked
$ sh bench/perf.sh 3 > bench/results/perf-$(date +%F).txt
$ SLICE=0 ONLY=mix sh bench/perf.sh 3 > bench/results/perf-$(date +%F)-noslice.txt
$ python3 bench/summarize.py bench/results/perf-*.txt
```

## The cost of a `kv` call against a park

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
