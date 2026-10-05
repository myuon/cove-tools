#!/usr/bin/env bash
# Installs a release tarball into a scratch home and runs the service's own
# command line against it, as CI and the release workflow do:
#
#   deploy/smoke.sh TARBALL
#
# It runs install.sh as on the server, takes ExecStart from
# cove-tools.service with /home/ioijoi moved to the scratch home, starts
# it, checks the public and admin listeners answer as deployed (apps on
# 8790, /_host/ only on 8791), stops it with SIGTERM as systemd would, and
# runs backup.sh. Linux, x86-64; ports 8790 and 8791 free.
set -euo pipefail

tarball="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
here="$(cd "$(dirname "$0")" && pwd)"
scratch="$(mktemp -d)"
trap 'kill "$pid" 2>/dev/null || true; rm -rf "$scratch"' EXIT
pid=""
export HOME="$scratch/home"
mkdir -p "$HOME"
root="$HOME/cove-tools"

fail() { echo "smoke.sh: $*" >&2; exit 1; }

bash "$here/install.sh" "$tarball"
[ -L "$root/current" ] || fail "no current link"
[ "$(stat -c %a "$root/env")" = 600 ] || fail "env is not mode 0600"
for app in webhooks ledger algo; do
  [ -f "$root/apps/$app/app.toml" ] || fail "app $app not installed"
done
# Installing the same release again is allowed (a reinstall).
bash "$here/install.sh" "$tarball" > /dev/null

# The unit's command line, at the scratch home.
command="$(sed -n '/^ExecStart=/,/[^\\]$/p' "$root/current/deploy/cove-tools.service" \
  | sed -e 's/^ExecStart=//' -e 's/\\$//' | tr '\n' ' ' | sed "s|/home/ioijoi|$HOME|g")"
echo "running: $command"
(
  cd "$root"
  set -a
  # shellcheck source=/dev/null
  . "$root/env"
  set +a
  # shellcheck disable=SC2086
  exec $command
) &
pid=$!
for _ in $(seq 100); do
  curl -fsS -o /dev/null http://127.0.0.1:8790/ 2>/dev/null && break
  sleep 0.2
done

status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
[ "$(status http://127.0.0.1:8790/algo/)" = 200 ] || fail "the algo app is not served"
[ "$(status http://127.0.0.1:8790/_host/stats)" = 404 ] || fail "/_host/ is on the public listener"
[ "$(status http://127.0.0.1:8791/_host/stats)" = 200 ] || fail "/_host/ is not on the admin listener"
[ "$(status -X POST http://127.0.0.1:8791/apps/algo/update)" = 401 ] || fail "the admin listener took an update without its token"
# The webhook lab writes its receive URLs at the public origin.
token="$(sed -n 's/^WEBHOOKS_ADMIN_TOKEN=//p' "$root/env")"
made="$(curl -fsS -H "Authorization: Bearer $token" -H 'Accept: application/json' \
  --data 'name=smoke' http://127.0.0.1:8790/webhooks/admin/endpoints)"
case "$made" in
  *'"https://covtools.ramda.io/webhooks/in/'*) ;;
  *) fail "the receive URL is not at the public origin: $made" ;;
esac

kill -TERM "$pid"
wait "$pid" || fail "the host did not exit cleanly on SIGTERM"
pid=""

bash "$root/current/deploy/backup.sh"
ls "$root"/backups/*/webhooks.kv.sqlite3.gz > /dev/null || fail "no backup of the webhook lab's store"
echo "smoke.sh: ok"
