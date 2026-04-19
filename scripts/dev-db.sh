#!/usr/bin/env bash
# Start the local dev Postgres container and print the DATABASE_URL.
# Requires Docker with Compose v2 plugin (docker compose, not docker-compose v1).
# Safe to run multiple times — docker compose up -d is idempotent.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

echo "Starting Postgres container..."
docker compose -f "$REPO_ROOT/docker-compose.yml" up -d postgres

echo "Waiting for Postgres to be ready (max 30s)..."
deadline=$(( $(date +%s) + 30 ))
until docker compose -f "$REPO_ROOT/docker-compose.yml" exec -T postgres \
    pg_isready -U harness -d harness -q 2>/dev/null; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "ERROR: Postgres did not become ready within 30 seconds." >&2
    exit 1
  fi
  sleep 1
done

echo ""
echo "Postgres is ready. Set the following in your shell:"
echo ""
echo "  export DATABASE_URL=postgres://harness:harness@localhost:5432/harness"
echo ""
echo "Migrations run automatically when the server starts — no manual step needed."
