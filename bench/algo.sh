#!/bin/sh
# Responsiveness beside the algorithm playground (issue #4): `hello`'s
# latency, open loop, while K clients keep heavy matching runs in flight.
#
#   cargo build --profile checked
#   sh bench/algo.sh [REPS] [BACKEND] > bench/results/algo-<date>-<backend>.txt
#
# Starts its own host on port 18181 with --workers 4 (admin off, a scratch
# data directory) over `hello` and `algo` as they ship. For each K in
# HEAVY (default "0 4 8"), K connections ask for
# `/algo/matching?example=large&algorithm=augmenting&part=result` (or, with
# MIX=algo-sat, `/algo/sat?example=hard&part=result`) as fast as they are
# answered (`cove-host-load --mix algo`, closed loop) while a second
# generator asks `hello` at RATE req/s (default 500) for REQUESTS requests
# (default 5000), latency from the intended start. After each, the algo
# app's yield counters, as /_host/stats has them, cumulative.
set -eu
REPS=${1:-3}
BACKEND=${2:-auto}
HEAVY=${HEAVY:-0 4 8}
# `algo` (matching), `algo-sat` (DPLL on pigeonhole 8 7) or `algo-anneal`
# (two annealing runs of 200,000 iterations).
MIX=${MIX:-algo}
RATE=${RATE:-500}
REQUESTS=${REQUESTS:-5000}
BIN=${BIN:-./target/checked}
PORT=18181
ADDR=127.0.0.1:$PORT
DATA=$(mktemp -d)
if curl -s -o /dev/null "http://$ADDR/"; then
  echo "something already answers on $ADDR" >&2
  exit 1
fi
APPS=$(mktemp -d)
cp -R examples/hello examples/algo "$APPS"/
"$BIN/cove-host" serve --apps "$APPS" --data "$DATA" --addr "$ADDR" --workers 4 \
  --backend "$BACKEND" --no-admin --quiet 2> "$DATA/host.err" &
HOST=$!
trap 'kill $HOST 2>/dev/null; rm -rf "$DATA" "$APPS"' EXIT
until curl -s -o /dev/null "http://$ADDR/hello/"; do sleep 0.2; done
echo "# $(date -u +%Y-%m-%dT%H:%M:%SZ) $(uname -sm) backend=$BACKEND workers=4 load=$(uptime | sed 's/.*load averages*: //')"
head -4 "$DATA/host.err" | sed 's/^/# /'
yields() {
  curl -s "http://$ADDR/_host/stats" | python3 -c '
import json, sys
a = json.load(sys.stdin)["apps"]["algo"]
print("# algo: tier=%s served=%s yields=%s yield_requests=%s yields_declined=%s overdue_yields=%s worker_ms=%.0f" % (
    a["tier"], a["served"], a["yields"], a["yield_requests"], a["yields_declined"], a["overdue_yields"], a["worker_ms"]))'
}
for rep in $(seq 1 "$REPS"); do
  echo "# rep $rep"
  for k in $HEAVY; do
    echo "## heavy=$k ($MIX) --mix hello --rate $RATE --connections 16 --requests $REQUESTS"
    if [ "$k" -gt 0 ]; then
      "$BIN/cove-host-load" --addr "$ADDR" --mix "$MIX" --connections "$k" --requests 1000000 \
        --warmup 0 > /dev/null 2>&1 &
      HEAVYPID=$!
      # Until the heavy runs hold the workers.
      sleep 1
    fi
    "$BIN/cove-host-load" --addr "$ADDR" --mix hello --rate "$RATE" --connections 16 \
      --requests "$REQUESTS"
    if [ "$k" -gt 0 ]; then
      kill "$HEAVYPID" 2>/dev/null || true
      wait "$HEAVYPID" 2>/dev/null || true
      sleep 0.5
    fi
    yields
  done
done
