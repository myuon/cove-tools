#!/usr/bin/env python3
"""Converts the existing Cove benchmark results into ledger runs, and posts runs.

    python3 examples/ledger/importer/ledger_import.py convert \
        --cove ../cove --tools . --out examples/ledger/samples
    python3 examples/ledger/importer/ledger_import.py post \
        --url http://127.0.0.1:8080/ledger examples/ledger/samples/*.json

`convert` reads exactly the formats below and writes one JSON file per run in
the ledger's schema (examples/ledger/README.md). It does not try to read every
format there is: a new kind of result gets a converter here, or posts the
schema itself.

- Cove `examples/edge/compare/results/{capacity,sweep,connections,waiting}.jsonl`
  (cove #589, the Go comparison): one line per repetition of one scenario on
  one server.
- `.../results/native/{capacity,sweep}.jsonl` (cove #593, the native tier,
  re-measured).
- `.../results/ab-*.jsonl` and `idle-cost.jsonl` (cove #595, verifying the
  diagnosis): A/B rounds of scratch builds; one run per build.
- cove-tools `bench/results/perf-*.txt` (`bench/perf.sh`, cove-host-load's
  console output).

`post` sends each file to `POST <url>/api/runs` with the token from
`$LEDGER_TOKEN` (or `--token`), and prints what the ledger answered. Posting
the same file twice is harmless: the second is answered as a duplicate.
"""
import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from collections import OrderedDict

# The machine every one of these was measured on (the edge comparison's
# README, "Conditions"; bench/README.md, "Conditions").
MACHINE = OrderedDict(
    [
        ("cpu", "Intel i7-10700K, 8 cores / 16 threads"),
        ("memory", "32 GB"),
        ("os", "macOS 26.6.2"),
        ("arch", "x86_64"),
    ]
)

EDGE_TOOLCHAIN = {"rustc": "1.98.1", "go": "1.23.2", "profile": "release"}
EDGE_CONDITIONS = {"server": "cove-edge", "workers": "4", "generator": "cove-edge-load"}

BACKENDS = {"cove": "vm", "cove-native": "native", "go": "go"}


def edge_metrics(rows):
    """The metrics of a group of edge rows (one per repetition)."""
    metrics = OrderedDict()

    def add(name, unit, key, better=None):
        values = [row[key] for row in rows if row.get(key) is not None]
        if values:
            metric = {"unit": unit, "values": values}
            if better:
                metric["better"] = better
            metrics[name] = metric

    add("throughput", "req/s", "throughput")
    add("p50", "ms", "p50_ms")
    add("p90", "ms", "p90_ms")
    add("p99", "ms", "p99_ms")
    add("max", "ms", "max_ms")
    add("cpu", "s", "server_cpu_s")
    add("cpuPerRequest", "us", "cpu_us_per_req")
    add("memory", "KiB", "rss_peak_kib")
    add("idleWakes", "/s", "idle_wakes_per_s", better="lower")
    return metrics


def latency_from(rows):
    return "intended start" if rows[0].get("from_intended") else "send"


def edge_input(row, kind):
    """The input label and size of an edge row."""
    c = row.get("concurrency")
    if kind == "capacity":
        return f"c={c}", c
    if kind == "sweep":
        return f"rate={row['offered']} c={c}", row["offered"]
    if kind == "connections":
        return f"pool={row['pool']} rate={row['rate']}", row["pool"]
    if kind == "waiting":
        return f"inflight={row['inflight']}", row["inflight"]
    raise ValueError(kind)


def edge_results(rows, kind):
    """Edge rows grouped into the ledger's results: one per scenario, server
    and input, the repetitions in order."""
    groups = OrderedDict()
    for row in rows:
        label, size = edge_input(row, kind)
        key = (row["scenario"], label, row["server"])
        groups.setdefault(key, {"size": size, "rows": []})["rows"].append(row)
    results = []
    for (scenario, label, server), group in groups.items():
        reps = sorted(group["rows"], key=lambda r: r.get("rep", 0))
        result = OrderedDict(
            [
                ("case", scenario),
                ("input", label),
                ("inputSize", group["size"]),
                ("backend", BACKENDS[server]),
                ("conditions", {"latency": f"from the {latency_from(reps)}",
                                "keepAlive": "yes" if reps[0].get("keep_alive") else "no"}),
                ("load", [r["load_before"] for r in reps]),
                ("metrics", edge_metrics(reps)),
            ]
        )
        results.append(result)
    return results


def read_jsonl(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def edge_run(cove, rel, kind, run_id, commit, measured_at, note):
    rows = read_jsonl(os.path.join(cove, rel))
    return OrderedDict(
        [
            ("schema", 1),
            ("id", run_id),
            ("repository", "myuon/cove"),
            ("commit", commit),
            ("measuredAt", measured_at),
            ("source", rel),
            ("note", note),
            ("environment", MACHINE),
            ("toolchain", EDGE_TOOLCHAIN),
            ("conditions", EDGE_CONDITIONS),
            ("results", edge_results(rows, kind)),
        ]
    )


# The scratch builds the #595 A/B rows name (`scratch/cove-edge-<variant>`),
# and what each was: the edge comparison's README, "Reproducing".
VARIANTS = {
    "base": ("fcf64a9", "the build the comparison was made with (#589)"),
    "sock": ("64c666b", "the branch that set a connection's socket options once, later merged as #597 (64c666b)"),
    "wake": ("bb13e6f", "the branch with one waker byte per idle wake-up, on top of `sock`, later merged as #598 (bb13e6f)"),
    "kqueue": ("kqueue-draft", "an earlier version of the parking lot waiting in kevent, not merged"),
    "kqueue2": ("9272282", "the parking lot waiting in kevent, later merged as #599 (9272282)"),
}


def ab_case(args):
    """The case and input of an `ab.py` row's load arguments."""
    flags = {}
    i = 0
    while i < len(args):
        if args[i].startswith("--"):
            if i + 1 < len(args) and not args[i + 1].startswith("--"):
                flags[args[i][2:]] = args[i + 1]
                i += 2
                continue
            flags[args[i][2:]] = True
        i += 1
    case = flags["mix"].split("=")[0] if "mix" in flags else flags["path"].strip("/")
    parts = []
    if "rate" in flags:
        parts.append(f"rate={flags['rate']}")
    parts.append(f"c={flags['concurrency']}")
    if "keep-alive" not in flags:
        parts.append("no keep-alive")
    latency = "intended start" if "from-intended" in flags else "send"
    return case, " ".join(parts), int(flags["concurrency"]), latency, "keep-alive" in flags


def ab_runs(cove):
    runs = []
    folder = "examples/edge/compare/results"
    for name in sorted(os.listdir(os.path.join(cove, folder))):
        if not (name.startswith("ab-") and name.endswith(".jsonl")):
            continue
        rel = f"{folder}/{name}"
        rows = read_jsonl(os.path.join(cove, rel))
        experiment = name[len("ab-"):-len(".jsonl")]
        for variant in sorted({row["variant"] for row in rows}):
            mine = sorted([r for r in rows if r["variant"] == variant], key=lambda r: r["round"])
            case, label, size, latency, keep_alive = ab_case(mine[0]["load_args"])
            commit, what = VARIANTS[variant]
            runs.append(
                OrderedDict(
                    [
                        ("schema", 1),
                        ("id", f"cove-595-ab-{experiment}-{variant}"),
                        ("repository", "myuon/cove"),
                        ("commit", commit),
                        ("measuredAt", min(r["time"] for r in mine) + "+09:00"),
                        ("source", f"{rel} (variant `{variant}`)"),
                        ("note", f"#595's A/B `{experiment}`, build `{variant}`: {what}. "
                                 "Rounds interleaved with the other builds of the same file."),
                        ("environment", MACHINE),
                        ("toolchain", EDGE_TOOLCHAIN),
                        ("conditions", EDGE_CONDITIONS),
                        ("results", [OrderedDict([
                            ("case", case),
                            ("input", label),
                            ("inputSize", size),
                            ("backend", "vm"),
                            ("conditions", {"latency": f"from the {latency}",
                                            "keepAlive": "yes" if keep_alive else "no"}),
                            ("load", [r["load_before"] for r in mine]),
                            ("metrics", edge_metrics(mine)),
                        ])]),
                    ]
                )
            )
    return runs


def idle_cost_run(cove):
    rel = "examples/edge/compare/results/idle-cost.jsonl"
    rows = read_jsonl(os.path.join(cove, rel))
    groups = OrderedDict()
    for row in rows:
        groups.setdefault(row["connections"], []).append(row)
    results = []
    for connections, reps in groups.items():
        reps.sort(key=lambda r: r["rep"])
        results.append(OrderedDict([
            ("case", "hello-idle"),
            ("input", f"rate={reps[0]['rate']} conns={connections}"),
            ("inputSize", connections),
            ("backend", "vm"),
            ("conditions", {"latency": "from the intended start", "keepAlive": "yes"}),
            ("load", [r["load_before"] for r in reps]),
            ("metrics", edge_metrics(reps)),
        ]))
    return OrderedDict([
        ("schema", 1),
        ("id", "cove-595-idle-cost"),
        ("repository", "myuon/cove"),
        ("commit", "92c0ef0"),
        ("measuredAt", "2026-10-05T10:02:33+09:00"),
        ("source", rel),
        ("note", "#595's count of the idle poller's cost, on a build with wake-up counters "
                 "(`scratch/cove-edge-lotkq-stats`). Measured before the commit; the time is the commit's."),
        ("environment", MACHINE),
        ("toolchain", EDGE_TOOLCHAIN),
        ("conditions", EDGE_CONDITIONS),
        ("results", results),
    ])


def perf_run(path, run_id, commit):
    """bench/perf.sh's console output as a run."""
    header = None
    results = OrderedDict()
    scenario = None
    for line in open(path):
        if header is None and line.startswith("# 20"):
            header = line
            continue
        if line.startswith("## "):
            args = line[3:].split()
            flags = dict(zip(args[0::2], args[1::2]))
            mix = flags["--mix"]
            parts = []
            if "--rate" in flags:
                parts.append(f"rate={flags['--rate']}")
            parts.append(f"c={flags['--connections']}")
            scenario = (mix, " ".join(parts), int(flags.get("--rate", flags["--connections"])),
                        "--rate" in flags)
            continue
        if scenario is None:
            continue
        mix, label, size, open_loop = scenario

        def bucket(case):
            key = (case, label)
            if key not in results:
                results[key] = {"size": size, "open": open_loop, "metrics": OrderedDict()}
            return results[key]["metrics"]

        def push(metrics, name, unit, value):
            metrics.setdefault(name, {"unit": unit, "values": []})["values"].append(float(value))

        if m := re.search(r"in [\d.]+ s: (\d+) req/s", line):
            push(bucket(mix), "throughput", "req/s", m.group(1))
        elif m := re.search(r"latency ms: p50 ([\d.]+)\s+p90 ([\d.]+)\s+p99 ([\d.]+)\s+max ([\d.]+)", line):
            metrics = bucket(mix)
            for name, value in zip(["p50", "p90", "p99", "max"], m.groups()):
                push(metrics, name, "ms", value)
        elif m := re.match(r"  (\w+)\s+\d+ x [\d, x]+?\s+([\d.]+) req/s\s+p50\s+([\d.]+) ms\s+p99\s+([\d.]+) ms", line):
            if mix == "cpu-io":
                metrics = bucket(f"cpu-io/{m.group(1)}")
                push(metrics, "throughput", "req/s", m.group(2))
                push(metrics, "p50", "ms", m.group(3))
                push(metrics, "p99", "ms", m.group(4))
    h = re.match(r"# (\S+) (\S+) (\S+) backend=(\S+) slice=(\S+) load=([\d.]+)", header)
    when, _, _, backend, slice_ms, load = h.groups()
    out = []
    for (case, label), r in results.items():
        out.append(OrderedDict([
            ("case", case),
            ("input", label),
            ("inputSize", r["size"]),
            ("backend", "native" if backend == "auto" else backend),
            ("conditions", {"latency": "from the intended start" if r["open"] else "from the send",
                            "keepAlive": "yes"}),
            ("metrics", r["metrics"]),
        ]))
    return OrderedDict([
        ("schema", 1),
        ("id", run_id),
        ("repository", "myuon/cove-tools"),
        ("commit", commit),
        ("measuredAt", when),
        ("source", os.path.relpath(path)),
        ("note", f"bench/perf.sh against cove-host, `--backend {backend}` (the native tier on this machine). "
                 f"The machine's load average was {load} at the start; perf.sh takes it once, so no "
                 "repetition carries its own."),
        ("environment", MACHINE),
        ("toolchain", {"rustc": "1.98.1", "profile": "checked", "cove": "9272282"}),
        ("conditions", {"server": "cove-host", "workers": "4", "generator": "cove-host-load",
                        "slice": f"{slice_ms} ms"}),
        ("results", out),
    ])


def convert(args):
    cove, tools, out = args.cove, args.tools, args.out
    os.makedirs(out, exist_ok=True)
    note589 = ("#589's comparison of the edge server against the same service in Go. Measured on "
               "2026-10-04 before this commit (the rows carry no time); the time is the commit's.")
    runs = [
        edge_run(cove, "examples/edge/compare/results/capacity.jsonl", "capacity",
                 "cove-589-capacity", "fcf64a9", "2026-10-04T22:29:20+09:00", note589),
        edge_run(cove, "examples/edge/compare/results/sweep.jsonl", "sweep",
                 "cove-589-sweep", "fcf64a9", "2026-10-04T22:29:20+09:00", note589),
        edge_run(cove, "examples/edge/compare/results/connections.jsonl", "connections",
                 "cove-589-connections", "fcf64a9", "2026-10-04T22:29:20+09:00", note589),
        edge_run(cove, "examples/edge/compare/results/waiting.jsonl", "waiting",
                 "cove-589-waiting", "fcf64a9", "2026-10-04T22:29:20+09:00", note589),
    ]
    note593 = ("#593's native tier against the VM and Go, re-measured on a quieter machine "
               "(results/native/run.log: 02:09-02:22 JST).")
    runs += [
        edge_run(cove, "examples/edge/compare/results/native/capacity.jsonl", "capacity",
                 "cove-593-capacity", "11ce12f", "2026-10-05T02:09:26+09:00", note593),
        edge_run(cove, "examples/edge/compare/results/native/sweep.jsonl", "sweep",
                 "cove-593-sweep", "11ce12f", "2026-10-05T02:12:12+09:00", note593),
    ]
    runs += ab_runs(cove)
    runs.append(idle_cost_run(cove))
    for name, run_id in [("perf-2026-10-05.txt", "cove-host-perf-2026-10-05"),
                         ("perf-2026-10-05-noslice.txt", "cove-host-perf-2026-10-05-noslice")]:
        path = os.path.join(tools, "bench/results", name)
        runs.append(perf_run(path, run_id, "00b397e"))
    for run in runs:
        path = os.path.join(out, f"{run['id']}.json")
        with open(path, "w") as f:
            json.dump(run, f, indent=1, ensure_ascii=False)
            f.write("\n")
        print(f"{path}: {len(run['results'])} results")


def post(args):
    token = args.token or os.environ.get("LEDGER_TOKEN")
    if not token:
        sys.exit("set LEDGER_TOKEN or pass --token")
    failed = 0
    for path in args.files:
        with open(path, "rb") as f:
            body = f.read()
        request = urllib.request.Request(
            args.url.rstrip("/") + "/api/runs",
            data=body,
            method="POST",
            headers={"content-type": "application/json", "authorization": f"Bearer {token}"},
        )
        try:
            with urllib.request.urlopen(request) as answer:
                status, text = answer.status, answer.read().decode()
        except urllib.error.HTTPError as error:
            status, text = error.code, error.read().decode()
        if status >= 300:
            failed += 1
        print(f"{status} {path}: {text.strip()}")
    sys.exit(1 if failed else 0)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    c = sub.add_parser("convert", help="convert the existing results into run files")
    c.add_argument("--cove", required=True, help="a checkout of myuon/cove")
    c.add_argument("--tools", default=".", help="this repository (for bench/results)")
    c.add_argument("--out", required=True, help="where to write the run files")
    c.set_defaults(func=convert)
    p = sub.add_parser("post", help="post run files to a ledger")
    p.add_argument("--url", required=True, help="the ledger's root, e.g. http://127.0.0.1:8080/ledger")
    p.add_argument("--token", help="the post secret (default: $LEDGER_TOKEN)")
    p.add_argument("files", nargs="+")
    p.set_defaults(func=post)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
