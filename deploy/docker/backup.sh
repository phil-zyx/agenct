#!/usr/bin/env bash
# Daily backup. Example cron (host user acme):
# 30 3 * * * /srv/agenct/backup.sh >> /srv/agenct/backups/backup.log 2>&1
set -euo pipefail
cd "$(dirname "$0")"
stamp=$(date +%Y%m%d-%H%M%S)
if [[ "${1:-}" == "--full" ]]; then
  running=$(docker compose ps --status running -q agenct)
  if [[ -n "$running" ]]; then
    trap 'docker compose start agenct' EXIT
    docker compose stop agenct
  fi
  docker compose run --rm --no-deps agenct backup-state "/data/backups/state-$stamp.zip"
  find data/backups -name 'state-*.zip' -mtime +14 -delete
  exit 0
fi
if [[ $# -ne 0 ]]; then
  echo 'Usage: backup.sh [--full] (--full briefly stops the service)' >&2
  exit 2
fi
# Default remains an online DATABASE-ONLY snapshot for existing cron schedules.
docker compose exec -T agenct python3 /opt/agenct/deploy/docker/entrypoint.py backup "/data/backups/hub-$stamp.sqlite3"
# Keep 14 days. Backups contain private chats: same protection as the database.
find data/backups -name 'hub-*.sqlite3' -mtime +14 -delete
