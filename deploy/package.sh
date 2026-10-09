#!/usr/bin/env bash
# Packages a built minicloud as the release tarball the install script takes:
#
#   deploy/package.sh VERSION BINARY OUTDIR
#
# writes OUTDIR/minicloud-VERSION-x86_64-linux.tar.gz (the binary, a
# `cove-host` symbolic link to it, apps/ — the one bundled app, admin; not
# examples/ — deploy/, docs/ and README.md under one directory of that name) and its
# .sha256.
# Run from the repository's root. The release workflow and CI both use it.
set -euo pipefail

[ $# -eq 3 ] || { echo "usage: deploy/package.sh VERSION BINARY OUTDIR" >&2; exit 2; }
version="$1"
binary="$2"
out="$3"
name="minicloud-$version-x86_64-linux"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name" "$out"
install -m755 "$binary" "$stage/$name/minicloud"
# The name before 0.5.0, for an installed unit that runs
# `.../current/cove-host serve`.
ln -s minicloud "$stage/$name/cove-host"
mkdir -p "$stage/$name/apps"
cp -R apps/admin "$stage/$name/apps/"
cp -R deploy docs README.md "$stage/$name/"
tar -C "$stage" -czf "$out/$name.tar.gz" --owner=0 --group=0 "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$out/$name.tar.gz"
