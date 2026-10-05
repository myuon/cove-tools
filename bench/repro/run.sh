#!/bin/sh
# Reproduces a native-tier fault at Cove 9272282 (for upstream): a run that
# is sliced while it copies an array into a vector (`Array.toVector`, and
# `Vector.snapshot`) can resume with a store of the wrong size:
#
#   error[cove::runtime]: `runCopy` writes 12000 element(s) to 0 of a destination of 0
#   error[cove::runtime]: this run has no memory left
#
# 160 requests, 16 at a time, on two workers. Measured 2026-10-05 (macOS
# x86-64): native 10 to 28 of 160 fail; native with --slice 0 (no yields) 0;
# --backend vm 0.
#
#   cargo build --profile checked
#   sh bench/repro/run.sh [native|vm] [SLICE_MS]
set -eu
BACKEND=${1:-native}
SLICE=${2:-2}
BIN=${BIN:-./target/checked}
ADDR=127.0.0.1:18197
DATA=$(mktemp -d)
"$BIN/cove-host" serve --apps bench/repro --data "$DATA" --addr "$ADDR" --workers 2 \
  --backend "$BACKEND" --slice "$SLICE" --no-admin --quiet 2> "$DATA/host.err" &
HOST=$!
trap 'kill $HOST 2>/dev/null; rm -rf "$DATA"' EXIT
until curl -s -o /dev/null "http://$ADDR/yieldcopy/"; do sleep 0.2; done
OUT=$(mktemp -d)
for round in $(seq 1 10); do
  PIDS=""
  for i in $(seq 1 16); do
    curl -s -o "$OUT/$round-$i" "http://$ADDR/yieldcopy/" &
    PIDS="$PIDS $!"
  done
  # Not a bare `wait`, which would wait for the host too.
  wait $PIDS
done
echo "answers (60000 is right):"
cat "$OUT"/* | grep -E '^[0-9]+$|^error' | sort | uniq -c
curl -s "http://$ADDR/_host/stats" | python3 -c '
import json, sys
a = json.load(sys.stdin)["apps"]["yieldcopy"]
print("tier=%s served=%s yields=%s runtime_errors=%s" % (a["tier"], a["served"], a["yields"], a["errors"]["runtime"]))'
rm -rf "$OUT"
