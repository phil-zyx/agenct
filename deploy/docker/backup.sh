#!/usr/bin/env bash
# Daily backup. Example cron (host user acme):
# 30 3 * * * /srv/agenct/backup.sh >> /srv/agenct/backups/backup.log 2>&1
set -euo pipefail
cd "$(dirname "$0")"
stamp=$(date +%Y%m%d-%H%M%S)
docker compose exec -T agenct python3 /opt/agenct/deploy/docker/entrypoint.py backup "/data/backups/hub-$stamp.sqlite3"
# Keep 14 days. Backups contain private chats: same protection as the database.
find data/backups -name 'hub-*.sqlite3' -mtime +14 -delete
