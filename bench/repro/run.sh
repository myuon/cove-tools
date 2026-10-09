#!/bin/sh
# A regression check for a native-tier fault found at Cove 9272282 and fixed
# upstream in Cove 2ca1c94 (cove#604, ADR 0086): a run that was sliced while
# it copied an array into a vector (`Array.toVector`, and `Vector.snapshot`)
# could resume with a store of the wrong size:
#
#   error[cove::runtime]: `runCopy` writes 12000 element(s) to 0 of a destination of 0
#   error[cove::runtime]: this run has no memory left
#
# 160 requests, 16 at a time, on two workers. Measured 2026-10-05 (macOS
# x86-64): at 9272282, native 10 to 28 of 160 failed (with --slice 0, no
# yields, 0; --backend vm 0); at 2ca1c94, native 0 of 160 in three runs.
# Exits non-zero unless every answer is right; CI runs it on the native tier.
#
#   cargo build --profile checked
#   sh bench/repro/run.sh [native|vm] [SLICE_MS]
set -eu
BACKEND=${1:-native}
SLICE=${2:-2}
BIN=${BIN:-./target/checked}
ADDR=127.0.0.1:18197
DATA=$(mktemp -d)
"$BIN/minicloud" serve --apps bench/repro --data "$DATA" --addr "$ADDR" --workers 2 \
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
RIGHT=$(cat "$OUT"/* | grep -c '^60000$' || true)
curl -s "http://$ADDR/_host/stats" | python3 -c '
import json, sys
a = json.load(sys.stdin)["apps"]["yieldcopy"]
print("tier=%s served=%s yields=%s runtime_errors=%s" % (a["tier"], a["served"], a["yields"], a["errors"]["runtime"]))'
rm -rf "$OUT"
if [ "$RIGHT" -ne 160 ]; then
  echo "FAIL: $RIGHT of 160 answers are right" >&2
  exit 1
fi
