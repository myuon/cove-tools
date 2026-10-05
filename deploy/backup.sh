#!/usr/bin/env bash
# Backs up every app's store with SQLite's online backup, as the service's
# user (no sudo), while the host runs. Keeps KEEP_DAYS days of them.
#
#   backup.sh                      # ~/cove-tools/data/*/kv.sqlite3 -> ~/cove-tools/backups/<stamp>/
#
# A crontab line for it (crontab -e, as ioijoi):
#
#   17 3 * * * /home/ioijoi/cove-tools/current/deploy/backup.sh >> /home/ioijoi/cove-tools/backups/backup.log 2>&1
#
# The backups are on the same disk as the data: copy ~/cove-tools/backups
# somewhere else for a backup that survives the machine.
set -euo pipefail

ROOT="${COVE_TOOLS_ROOT:-$HOME/cove-tools}"
DEST="${COVE_TOOLS_BACKUPS:-$ROOT/backups}"
KEEP_DAYS="${KEEP_DAYS:-14}"

command -v sqlite3 >/dev/null || { echo "backup.sh: needs sqlite3" >&2; exit 1; }
umask 077
stamp="$(date +%Y-%m-%dT%H%M%S)"
dir="$DEST/$stamp"
mkdir -p "$dir"

status=0
shopt -s nullglob
for db in "$ROOT"/data/*/kv.sqlite3; do
  app="$(basename "$(dirname "$db")")"
  out="$dir/$app.kv.sqlite3"
  # `.backup` copies a consistent snapshot; the WAL writer is not blocked.
  if sqlite3 "$db" ".timeout 10000" ".backup '$out'" \
    && [ "$(sqlite3 "$out" 'PRAGMA quick_check;')" = "ok" ]; then
    gzip -f "$out"
    echo "$stamp $app: $(du -h "$out.gz" | cut -f1)"
  else
    echo "$stamp $app: FAILED" >&2
    status=1
  fi
done

# Older than KEEP_DAYS days: gone.
find "$DEST" -mindepth 1 -maxdepth 1 -type d -mtime +"$KEEP_DAYS" -exec rm -rf {} +
exit "$status"
