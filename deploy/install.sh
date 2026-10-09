#!/usr/bin/env bash
# Installs a cove-tools release (the platform: the binary and deploy/) under
# ~/cove-tools, as the user that runs it. No sudo: the one root step (the
# systemd unit) is printed, not run.
#
#   install.sh [--with-bundled-apps "webhooks ledger algo admin"] [--root DIR] VERSION|TARBALL
#
#   VERSION   a release tag (v0.1.0 or 0.1.0): downloaded from GitHub
#   TARBALL   a local cove-host-<version>-x86_64-linux.tar.gz, with its
#             .sha256 beside it
#
# The apps are not the release's: they are whatever was deployed into
# <root>/apps (`cove-host deploy`, from any repository; README "Deploying an
# app"). Installing a release never removes or replaces one of them.
#
# What it does:
#   1. downloads (or takes) the tarball and verifies its sha256;
#   2. unpacks it into <root>/releases/<version>/;
#   3. creates <root>/env from deploy/env.example with fresh random secrets
#      if there is none (mode 0600); if there is one, appends each key of the
#      release's env.example that it lacks — a `KEY=change-me` as a fresh
#      secret, any other `KEY=value` as written there — and changes nothing
#      else in it;
#   4. checks every app installed in <root>/apps with the release's binary,
#      against that env and the secret store in <root>/data — with the
#      bundled apps named by --with-bundled-apps in place of their installed
#      versions — and stops, switching nothing, if any would be refused
#      (`--deployed`: a key an installed app.toml may no longer say is a
#      warning, as it is when the host starts, not a refusal);
#   5. deploys the bundled apps --with-bundled-apps names, from the release,
#      with `cove-host deploy --into <root>/apps` (each checked again; the
#      version it replaces is kept in <root>/apps/.previous/<app>); none by
#      default. A first install wants them: pass the flag;
#   6. points <root>/current at the release; <root>/data is never touched;
#   7. keeps the three newest releases, and prints what to run with sudo.
set -euo pipefail

REPO=myuon/cove-tools
ROOT="${COVE_TOOLS_ROOT:-$HOME/cove-tools}"
BUNDLED=""
SOURCE=""

die() { echo "install.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --with-bundled-apps) BUNDLED="$2"; shift 2 ;;
    --apps) die "--apps is gone: a release no longer replaces the apps; --with-bundled-apps \"${2:-}\" deploys those of the release" ;;
    --root) ROOT="$2"; shift 2 ;;
    -h|--help) sed -n '2,38p' "$0"; exit 0 ;;
    -*) die "unknown option $1" ;;
    *) [ -z "$SOURCE" ] || die "one VERSION or TARBALL"; SOURCE="$1"; shift ;;
  esac
done
[ -n "$SOURCE" ] || die "usage: install.sh [--with-bundled-apps \"a b\"] [--root DIR] VERSION|TARBALL"
[ "$(id -u)" -ne 0 ] || die "run it as the user the service runs as, not root"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *) die "the release is for x86_64 Linux, not $(uname -s)-$(uname -m)" ;;
esac
for tool in curl sha256sum tar; do
  command -v "$tool" >/dev/null || die "needs $tool"
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# 1. The tarball and its checksum.
if [ -f "$SOURCE" ]; then
  tarball="$(cd "$(dirname "$SOURCE")" && pwd)/$(basename "$SOURCE")"
  [ -f "$tarball.sha256" ] || die "no $tarball.sha256 beside the tarball"
  cp "$tarball" "$tarball.sha256" "$work/"
  name="$(basename "$tarball")"
else
  version="${SOURCE#v}"
  name="cove-host-$version-x86_64-linux.tar.gz"
  url="https://github.com/$REPO/releases/download/v$version"
  echo "downloading $url/$name"
  curl -fsSL -o "$work/$name" "$url/$name"
  curl -fsSL -o "$work/$name.sha256" "$url/$name.sha256"
fi
(cd "$work" && sha256sum -c "$name.sha256") || die "checksum mismatch for $name"
version="${name#cove-host-}"
version="${version%-x86_64-linux.tar.gz}"
if [ -z "$version" ] || [ "$version" = "$name" ]; then
  die "cannot read a version from $name"
fi

# 2. Unpack into releases/<version>, and point current at it.
mkdir -p "$ROOT/releases"
chmod 700 "$ROOT"
tar -xzf "$work/$name" -C "$work"
top="$work/cove-host-$version-x86_64-linux"
[ -x "$top/cove-host" ] || die "$name has no cove-host binary"
release="$ROOT/releases/$version"
rm -rf "$release.tmp"
mv "$top" "$release.tmp"
rm -rf "$release"
mv "$release.tmp" "$release"
"$release/cove-host" --version || die "the binary does not run on this machine"

# 3. The secrets.
fresh_secret() {
  if command -v openssl >/dev/null; then
    openssl rand -hex 32
  else
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
  fi
}
if [ ! -f "$ROOT/env" ]; then
  (
    umask 077
    while IFS= read -r line; do
      case "$line" in
        *=change-me) echo "${line%=change-me}=$(fresh_secret)" ;;
        *) echo "$line" ;;
      esac
    done < "$release/deploy/env.example" > "$ROOT/env"
  )
  chmod 600 "$ROOT/env"
  echo "created $ROOT/env with fresh secrets (mode 0600); read them there"
else
  # A key this release added: append it, leave every existing line alone.
  # A secret is fresh; a setting is the example's value.
  while IFS= read -r line; do
    case "$line" in
      [A-Za-z_]*=*)
        key="${line%%=*}"
        if ! grep -q "^$key=" "$ROOT/env"; then
          # A file whose last line has no newline would swallow the key.
          [ -z "$(tail -c 1 "$ROOT/env")" ] || echo >> "$ROOT/env"
          if [ "$line" = "$key=change-me" ]; then
            (umask 077; echo "$key=$(fresh_secret)" >> "$ROOT/env")
            echo "added $key to $ROOT/env (fresh secret)"
          else
            (umask 077; echo "$line" >> "$ROOT/env")
            echo "added $line to $ROOT/env"
          fi
        fi ;;
    esac
  done < "$release/deploy/env.example"
  chmod 600 "$ROOT/env"
fi

# Runs the release's binary with the env loaded.
with_env() {
  (
    set -a
    # shellcheck source=/dev/null
    . "$ROOT/env"
    set +a
    "$@"
  )
}

# 4. The installed apps, with the bundled ones about to be deployed in their
# place, checked against the new binary before anything is switched.
mkdir -p "$ROOT/apps" "$ROOT/data" "$ROOT/backups"
chmod 700 "$ROOT/data" "$ROOT/backups"
check="$work/check"
mkdir -p "$check"
for dir in "$ROOT"/apps/*/; do
  [ -f "$dir/app.toml" ] || continue
  app="$(basename "$dir")"
  case " $BUNDLED " in *" $app "*) continue ;; esac
  cp -R "${dir%/}" "$check/$app"
done
for app in $BUNDLED; do
  [ -f "$release/apps/$app/app.toml" ] || die "the release has no app \`$app\`"
  cp -R "$release/apps/$app" "$check/$app"
done
with_env "$release/cove-host" check --deployed --apps "$check" --data "$ROOT/data" \
  || die "an app does not check against this release, $ROOT/env and the secret store (see above; a new secret?); nothing switched"

# 5. The bundled apps asked for, deployed as any app is.
for app in $BUNDLED; do
  with_env "$release/cove-host" deploy "$release/apps/$app" --name "$app" --into "$ROOT/apps" --data "$ROOT/data" >/dev/null \
    || die "deploying the bundled app \`$app\` failed (see above); nothing switched"
done

# 6. The switch.
ln -sfn "releases/$version" "$ROOT/current.tmp"
mv -T "$ROOT/current.tmp" "$ROOT/current"

# 7. Keep the three newest releases (and always the current one).
(
  cd "$ROOT/releases"
  # shellcheck disable=SC2012
  ls -1t | tail -n +4 | while IFS= read -r old; do
    [ "$old" = "$version" ] || rm -rf -- "$old"
  done
)

unit=/etc/systemd/system/cove-tools.service
echo
installed="$(for dir in "$ROOT"/apps/*/; do if [ -f "$dir/app.toml" ]; then basename "$dir"; fi; done | tr '\n' ' ')"
installed="${installed% }"
echo "installed cove-tools $version in $ROOT (apps: ${installed:-none})"
if [ -z "$installed" ]; then
  echo "no apps are installed: run again with --with-bundled-apps \"webhooks ledger algo admin\","
  echo "or deploy one with: $ROOT/current/cove-host deploy <app-dir> --admin 127.0.0.1:8791 --token-file $ROOT/data/admin.token"
fi
if [ ! -f "$unit" ]; then
  echo "first time: install the unit and start the service (needs sudo, once):"
  echo
  echo "  sudo install -m644 $ROOT/current/deploy/cove-tools.service $unit && sudo systemctl daemon-reload && sudo systemctl enable --now cove-tools"
elif ! cmp -s "$ROOT/current/deploy/cove-tools.service" "$unit"; then
  echo "the unit changed in this release; install it and restart:"
  echo
  echo "  sudo install -m644 $ROOT/current/deploy/cove-tools.service $unit && sudo systemctl daemon-reload && sudo systemctl restart cove-tools"
else
  echo "restart onto it:"
  echo
  echo "  sudo systemctl restart cove-tools"
fi
echo
echo "then: curl -s http://127.0.0.1:8790/ && curl -s http://127.0.0.1:8791/_host/stats | head"
echo "logs: journalctl -u cove-tools -f   (per app: $ROOT/data/<app>/log.txt)"
echo "admin UI: https://covtools-admin.ramda.io/ (the Cloudflare Access login; ACCESS_* in $ROOT/env)"
echo "roll back the platform: ln -sfn releases/<old> $ROOT/current && sudo systemctl restart cove-tools"
echo "roll back an app: $ROOT/current/cove-host rollback <app> --admin 127.0.0.1:8791 --token-file $ROOT/data/admin.token"
