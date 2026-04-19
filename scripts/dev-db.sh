#!/usr/bin/env bash
# Start the local dev Postgres container and print the DATABASE_URL.
# Requires Docker with Compose v2 plugin (docker compose, not docker-compose v1).
# Safe to run multiple times — docker compose up -d is idempotent.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

echo "Starting Postgres container and waiting until healthy..."
docker compose -f "$REPO_ROOT/docker-compose.yml" up -d --wait postgres

echo ""
echo "Postgres is ready. Set the following in your shell:"
echo ""
echo "  export DATABASE_URL=postgres://harness:harness@localhost:5432/harness"
echo ""
echo "Migrations run automatically when the server starts — no manual step needed."
