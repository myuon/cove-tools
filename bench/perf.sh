#!/bin/sh
# The performance check: minicloud under the load the Cove repository's
# examples/edge/compare measured edge and Go with, at four workers.
#
#   cargo build --profile checked
#   sh bench/perf.sh [REPS] [BACKEND] > bench/results/perf-<date>.txt
#   SLICE=0 ONLY=mix sh bench/perf.sh 3     # the mix, with no time slice
#
# Starts its own host on port 18180 (admin off, a scratch data directory)
# with --workers 4, runs each scenario REPS times (default 3), and stops it.
# Each line of the output is one minicloud-load run; bench/README.md reads
# them.
set -eu
REPS=${1:-3}
BACKEND=${2:-auto}
SLICE=${SLICE:-2}
ONLY=${ONLY:-all}
BIN=${BIN:-./target/checked}
PORT=18180
ADDR=127.0.0.1:$PORT
DATA=$(mktemp -d)
if curl -s -o /dev/null "http://$ADDR/"; then
  echo "something already answers on $ADDR" >&2
  exit 1
fi
# The sample apps, with proxy allowed to reach this port.
APPS=$(mktemp -d)
cp -R apps/* examples/* "$APPS"/
sed -i.bak "s|allow = \[.*\]|allow = [\"http://127.0.0.1:$PORT\"]|" "$APPS/proxy/app.toml"
# Room for the load: the samples' per-app limits are sized for a demo, and
# past them the host answers 429 (which is the limit working, not the
# measurement). The edge server had no per-tenant limits.
for app in crunch slow proxy hello; do
  toml="$APPS/$app/app.toml"
  sed -i.bak -e '/^max_in_flight/d' -e '/^max_queued/d' "$toml"
  grep -q '^\[limits\]' "$toml" || printf '\n[limits]\n' >> "$toml"
  sed -i.bak 's|^\[limits\]$|[limits]\
max_in_flight = 10000\
max_queued = 10000|' "$toml"
done
"$BIN/minicloud" serve --apps "$APPS" --data "$DATA" --addr "$ADDR" --workers 4 \
  --slice "$SLICE" --backend "$BACKEND" --no-admin --quiet 2> "$DATA/host.err" &
HOST=$!
trap 'kill $HOST 2>/dev/null; rm -rf "$DATA" "$APPS"' EXIT
until curl -s -o /dev/null "http://$ADDR/"; do sleep 0.2; done
echo "# $(date -u +%Y-%m-%dT%H:%M:%SZ) $(uname -sm) backend=$BACKEND slice=$SLICE load=$(uptime | sed 's/.*load averages*: //')"
head -6 "$DATA/host.err" | sed 's/^/# /'
load() {
  echo "## $*"
  "$BIN/minicloud-load" --addr "$ADDR" "$@"
}
for rep in $(seq 1 "$REPS"); do
  echo "# rep $rep"
  if [ "$ONLY" = all ]; then
    load --mix hello --connections 64 --requests 200000
    load --mix hello --connections 256 --requests 200000
    load --mix crunch20k --connections 16 --requests 4000
    load --mix crunch20k --connections 64 --requests 4000
    load --mix hello --rate 25000 --connections 64 --requests 100000
    load --mix hello --rate 75000 --connections 256 --requests 300000
    load --mix crunch20k --rate 1500 --connections 64 --requests 6000
    load --mix crunch20k --rate 3500 --connections 128 --requests 14000
    load --mix slow --rate 2000 --connections 1024 --requests 8000
  fi
  load --mix cpu-io --rate 330 --connections 200 --requests 1500
  load --mix cpu-io --rate 990 --connections 400 --requests 4000
  load --mix cpu-io --rate 1500 --connections 600 --requests 6000
  load --mix cpu-io --connections 256 --requests 3000
done
