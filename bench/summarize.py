#!/usr/bin/env python3
"""Medians (and ranges) of bench/perf.sh's repetitions, one row per scenario.

    python3 bench/summarize.py bench/results/perf-2026-10-05.txt
"""
import re
import statistics
import sys
from collections import defaultdict

def parse(path):
    runs = defaultdict(list)
    scenario = None
    current = None
    for line in open(path):
        if line.startswith("## "):
            scenario = line[3:].strip()
            current = {"apps": {}}
            runs[scenario].append(current)
        elif current is None:
            continue
        elif m := re.search(r"in [\d.]+ s: (\d+) req/s", line):
            current["throughput"] = float(m.group(1))
        elif m := re.search(r"latency ms: p50 ([\d.]+) .* p99 ([\d.]+) ", line):
            current["p50"], current["p99"] = float(m.group(1)), float(m.group(2))
        elif m := re.match(r"  (\w+)\s+(.+?)\s+([\d.]+) req/s\s+p50\s+([\d.]+) ms\s+p99\s+([\d.]+) ms", line):
            current["apps"][m.group(1)] = (m.group(2).strip(), float(m.group(4)), float(m.group(5)))
    return runs

def med(values):
    values = sorted(values)
    return f"{statistics.median(values):.1f} ({values[0]:.1f}–{values[-1]:.1f})" if len(values) > 1 else f"{values[0]:.1f}"

for path in sys.argv[1:]:
    print(f"### {path}")
    for scenario, runs in parse(path).items():
        print(f"{scenario}: {med([r['throughput'] for r in runs])} req/s, "
              f"p50 {med([r['p50'] for r in runs])} ms, p99 {med([r['p99'] for r in runs])} ms")
        apps = sorted({a for r in runs for a in r["apps"]})
        if len(apps) > 1:
            for app in apps:
                rows = [r["apps"][app] for r in runs if app in r["apps"]]
                statuses = {row[0] for row in rows}
                print(f"    {app}: p50 {med([x[1] for x in rows])} ms, p99 {med([x[2] for x in rows])} ms  [{'; '.join(sorted(statuses))}]")
