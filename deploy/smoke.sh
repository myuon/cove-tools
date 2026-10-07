#!/usr/bin/env bash
# Installs a release tarball into a scratch home and runs the service's own
# command line against it, as CI and the release workflow do:
#
#   deploy/smoke.sh TARBALL
#
# It runs install.sh as on the server (with the bundled apps; then checks a
# reinstall leaves an app that is not the release's alone, and refuses to
# switch while an installed app does not check), takes ExecStart from
# cove-tools.service with /home/ioijoi moved to the scratch home, starts
# it, checks the public and admin listeners answer as deployed (apps on
# 8790, /_host/ only on 8791, Cloudflare Access on), deploys an app onto it
# from an archive on stdin and rolls it back, stops it with SIGTERM as
# systemd would, starts it again with Access off to check the token way in,
# and runs backup.sh. Linux, x86-64; ports 8790 and 8791 free.
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

bundled="webhooks ledger algo admin"
bash "$here/install.sh" --with-bundled-apps "$bundled" "$tarball"
[ -L "$root/current" ] || fail "no current link"
[ "$(stat -c %a "$root/env")" = 600 ] || fail "env is not mode 0600"
for app in $bundled; do
  [ -f "$root/apps/$app/app.toml" ] || fail "app $app not installed"
done
[ ! -e "$root/apps/hello" ] || fail "a sample app was installed unasked"

# An app that is not the release's, deployed into the apps directory: a
# release must not remove it, and must refuse to switch while it would not
# load.
"$root/current/cove-host" deploy "$root/current/apps/hello" --into "$root/apps" > /dev/null \
  || fail "cove-host deploy --into refused the hello app"
bash "$here/install.sh" "$tarball" > /dev/null
[ -f "$root/apps/hello/hello.cove" ] || fail "a reinstall removed an app that is not the release's"
for app in $bundled; do
  [ -f "$root/apps/$app/app.toml" ] || fail "a reinstall without --with-bundled-apps removed $app"
  [ ! -e "$root/apps/.previous/$app" ] || fail "a reinstall without --with-bundled-apps redeployed $app"
done
echo 'fn broken( {' >> "$root/apps/hello/hello.cove"
ln -sfn releases/before "$root/current"
if bash "$here/install.sh" "$tarball" > "$scratch/refused.log" 2>&1; then
  fail "install.sh switched with an installed app that does not check"
fi
grep -q 'nothing switched' "$scratch/refused.log" || fail "install.sh did not say why it refused: $(cat "$scratch/refused.log")"
[ "$(readlink "$root/current")" = releases/before ] || fail "install.sh switched current although an app does not check"
grep -q 'fn broken' "$root/apps/hello/hello.cove" || fail "install.sh touched the app it refused"
sed -i '/^fn broken( {$/d' "$root/apps/hello/hello.cove"
# Redeploying the bundled apps keeps the versions they replace.
bash "$here/install.sh" --with-bundled-apps "$bundled" "$tarball" > /dev/null
[ -f "$root/apps/.previous/webhooks/app.toml" ] || fail "a redeploy did not keep the webhook lab's previous version"
[ -f "$root/apps/hello/hello.cove" ] || fail "a redeploy of the bundled apps removed hello"
# Installing the same release again is allowed (a reinstall). A secret the
# env lacks is appended fresh, and the others are left as they were.
webhooks_before="$(sed -n 's/^WEBHOOKS_ADMIN_TOKEN=//p' "$root/env")"
sed -i '/^ADMIN_UI_TOKEN=/d' "$root/env"
bash "$here/install.sh" "$tarball" > /dev/null
grep -q '^ADMIN_UI_TOKEN=.' "$root/env" || fail "the reinstall did not add ADMIN_UI_TOKEN back"
[ "$(sed -n 's/^WEBHOOKS_ADMIN_TOKEN=//p' "$root/env")" = "$webhooks_before" ] \
  || fail "the reinstall changed WEBHOOKS_ADMIN_TOKEN"
[ "$(stat -c %a "$root/env")" = 600 ] || fail "env is not mode 0600 after the reinstall"
# A setting the env lacks is appended with the example's value; one it has
# is kept, even when it differs.
sed -i -e '/^ACCESS_TEAM_DOMAIN=/d' -e 's/^COVTOOLS_ACCESS_AUD=.*/COVTOOLS_ACCESS_AUD=kept/' "$root/env"
bash "$here/install.sh" "$tarball" > /dev/null
grep -q '^ACCESS_TEAM_DOMAIN=ioijoi.cloudflareaccess.com$' "$root/env" \
  || fail "the reinstall did not add ACCESS_TEAM_DOMAIN back"
grep -q '^COVTOOLS_ACCESS_AUD=kept$' "$root/env" || fail "the reinstall changed COVTOOLS_ACCESS_AUD"
sed -i "s/^COVTOOLS_ACCESS_AUD=kept$/$(grep '^COVTOOLS_ACCESS_AUD=' "$here/env.example")/" "$root/env"

# The unit's command line, at the scratch home.
command="$(sed -n '/^ExecStart=/,/[^\\]$/p' "$root/current/deploy/cove-tools.service" \
  | sed -e 's/^ExecStart=//' -e 's/\\$//' | tr '\n' ' ' | sed "s|/home/ioijoi|$HOME|g")"
echo "running: $command"

# Starts the host as systemd would, with the env and then `$*` (assignments
# that override it).
start_host() {
  (
    cd "$root"
    set -a
    # shellcheck source=/dev/null
    . "$root/env"
    for assignment in "$@"; do export "${assignment?}"; done
    set +a
    # shellcheck disable=SC2086
    exec $command
  ) &
  pid=$!
  for _ in $(seq 100); do
    curl -fsS -o /dev/null http://127.0.0.1:8790/ 2>/dev/null && break
    sleep 0.2
  done
}

stop_host() {
  kill -TERM "$pid"
  wait "$pid" || fail "the host did not exit cleanly on SIGTERM"
  pid=""
}

status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
header() { curl -s -o /dev/null -D - "$@" | tr -d '\r'; }
admin_token="$(sed -n 's/^ADMIN_UI_TOKEN=//p' "$root/env")"
token="$(sed -n 's/^WEBHOOKS_ADMIN_TOKEN=//p' "$root/env")"
admin_host='Host: covtools-admin.ramda.io'

# 1. As deployed: Cloudflare Access on. Nothing here comes through Access,
# so the admin UI and the webhook lab's pages refuse it — the apps' own
# tokens too — and prompt for nothing. No request carries a well-formed
# token, so the host fetches no keys.
start_host
[ "$(status http://127.0.0.1:8790/algo/)" = 200 ] || fail "the algo app is not served"
[ "$(status http://127.0.0.1:8790/_host/stats)" = 404 ] || fail "/_host/ is on the public listener"
[ "$(status http://127.0.0.1:8791/_host/stats)" = 200 ] || fail "/_host/ is not on the admin listener"
[ "$(status -X POST http://127.0.0.1:8791/apps/algo/update)" = 401 ] || fail "the admin listener took an update without its token"
[ "$(status -H "$admin_host" http://127.0.0.1:8790/)" = 403 ] || fail "the admin UI answered without an Access token"
[ "$(status -H "$admin_host" -u "admin:$admin_token" http://127.0.0.1:8790/)" = 403 ] \
  || fail "the admin UI took its token with Access on"
[ "$(status -H "$admin_host" -H 'Cf-Access-Jwt-Assertion: not.a.token' http://127.0.0.1:8790/)" = 403 ] \
  || fail "the admin UI took a malformed Access token"
header -H "$admin_host" http://127.0.0.1:8790/ | grep -qi '^www-authenticate' \
  && fail "the admin UI prompts for a login behind Access"
[ "$(status -H "Authorization: Bearer $token" http://127.0.0.1:8790/webhooks/admin)" = 403 ] \
  || fail "the webhook lab's pages took the token with Access on"
[ "$(status -X POST --data hi http://127.0.0.1:8790/webhooks/in/0000000000000000)" = 404 ] \
  || fail "a receive URL asked for credentials"
[ "$(status http://127.0.0.1:8790/admin/)" = 404 ] || fail "the admin app is on the main hostname"

# Deploying onto the running host, as from another repository over ssh: an
# archive on stdin, checked, written and updated to.
[ "$(status http://127.0.0.1:8790/hello/)" = 200 ] || fail "the deployed hello app is not served"
deployed="$(tar -C "$root/current/apps/hello" -c . \
  | "$root/current/cove-host" deploy - --name hello --admin 127.0.0.1:8791 --token-file "$root/data/admin.token")" \
  || fail "cove-host deploy - failed"
case "$deployed" in
  *'"version": "v2-'*) ;;
  *) fail "cove-host deploy - did not update the running host: $deployed" ;;
esac
[ -d "$root/apps/.previous/hello" ] || fail "the deploy kept no previous version"
"$root/current/cove-host" rollback hello --admin 127.0.0.1:8791 --token-file "$root/data/admin.token" > /dev/null \
  || fail "cove-host rollback failed"
[ "$(status http://127.0.0.1:8790/hello/)" = 200 ] || fail "hello is not served after the rollback"
stop_host

# 2. Access off (no team): the apps' tokens are the way in, as before.
start_host ACCESS_TEAM_DOMAIN=
# The webhook lab writes its receive URLs at the public origin.
made="$(curl -fsS -H "Authorization: Bearer $token" -H 'Accept: application/json' \
  --data 'name=smoke' http://127.0.0.1:8790/webhooks/admin/endpoints)"
case "$made" in
  *'"https://covtools.ramda.io/webhooks/in/'*) ;;
  *) fail "the receive URL is not at the public origin: $made" ;;
esac
# The admin UI: reached by its hostname only, behind its own secret.
[ "$(status -H "$admin_host" http://127.0.0.1:8790/)" = 401 ] || fail "the admin UI answered without its token"
header -H "$admin_host" http://127.0.0.1:8790/ | grep -qi '^www-authenticate: basic' \
  || fail "the admin UI does not prompt for its token with Access off"
[ "$(status -H "$admin_host" -u "admin:$admin_token" http://127.0.0.1:8790/)" = 200 ] || fail "the admin UI refused its token"
[ "$(status http://127.0.0.1:8790/admin/)" = 404 ] || fail "the admin app is on the main hostname"
[ "$(status -X POST -H "$admin_host" -u "admin:$admin_token" -H 'Origin: https://evil.example' \
  http://127.0.0.1:8790/apps/hello/disable)" = 403 ] || fail "the admin UI took a cross-site POST"

stop_host

bash "$root/current/deploy/backup.sh"
ls "$root"/backups/*/webhooks.kv.sqlite3.gz > /dev/null || fail "no backup of the webhook lab's store"
echo "smoke.sh: ok"
