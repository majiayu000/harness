#!/usr/bin/env bash
# Start the local dev Postgres container and print the HARNESS_DATABASE_URL.
# Requires Docker with Compose v2 plugin (docker compose, not docker-compose v1).
# Safe to run multiple times — docker compose up -d is idempotent.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

# Guard: --wait requires Compose v2.1+; fail fast with a clear message.
if ! docker compose version &>/dev/null; then
  echo "ERROR: Docker Compose v2 plugin not found." >&2
  echo "       Install via Docker Desktop, the 'docker-compose-plugin' package" >&2
  echo "       from your package manager, or the manual CLI plugin instructions at" >&2
  echo "       https://docs.docker.com/compose/install/." >&2
  echo "       The legacy 'docker-compose' v1 command is not supported." >&2
  exit 1
fi

# Guard: --wait was added in Compose v2.1; older v2 installs will fail at runtime.
_dc_ver=$(docker compose version --short 2>/dev/null | sed 's/^v//')
_dc_major=$(printf '%s' "$_dc_ver" | cut -d. -f1)
_dc_minor=$(printf '%s' "$_dc_ver" | cut -d. -f2)
if [ "${_dc_major:-0}" -lt 2 ] || { [ "${_dc_major:-0}" -eq 2 ] && [ "${_dc_minor:-0}" -lt 1 ]; }; then
  echo "ERROR: Docker Compose v2.1+ required for --wait (found: v${_dc_ver})." >&2
  echo "       Upgrade via Docker Desktop or your package manager." >&2
  echo "       See https://docs.docker.com/compose/install/ for options." >&2
  exit 1
fi

echo "Starting Postgres container and waiting until healthy..."
docker compose -f "$REPO_ROOT/docker-compose.yml" up -d --wait postgres

echo ""
echo "Postgres is ready. Set the following in your shell:"
echo ""
echo "  export HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness"
echo ""
echo "Migrations run automatically when the server starts — no manual step needed."
