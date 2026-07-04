#!/usr/bin/env bash
# Dry-run or apply the same path-derived Postgres schema reaper used by the
# running server.
#
# The server and this wrapper both handle two classes:
#   1. registered path-derived schemas with dead owner paths
#   2. legacy unregistered h<16-hex> schemas whose hash does not match any live
#      workspace directory or known logical store path under the configured
#      workspace root
#
# Safety model:
#   - Defaults to dry-run.
#   - Pass --confirm to drop selected schemas via DROP SCHEMA IF EXISTS ... CASCADE.
#   - Legacy reaping fails closed if the configured workspace root cannot be
#     listed, because live ownership cannot be proven.
#   - The legacy class is bounded by storage.orphan_reaper_legacy_batch in server
#     config and by the CLI default when run here.
set -euo pipefail

HARNESS_PG_SCHEMA_CLEANUP="${HARNESS_PG_SCHEMA_CLEANUP:-harness-pg-schema-cleanup}"

CONFIRM_ARGS=()
CONFIG_ARGS=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --confirm)
      CONFIRM_ARGS=(--confirm-drop)
      shift
      ;;
    --config)
      if [ "$#" -lt 2 ]; then
        echo "--config requires a path" >&2
        exit 2
      fi
      CONFIG_ARGS=(--config "$2")
      shift 2
      ;;
    --help|-h)
      cat <<'EOF'
Usage: scripts/pg-orphan-cleanup.sh [--config PATH] [--confirm]

Runs harness-pg-schema-cleanup --reap-orphans with the same registered and
legacy path-derived schema semantics as the background server reaper.
EOF
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

exec "$HARNESS_PG_SCHEMA_CLEANUP" "${CONFIG_ARGS[@]}" --reap-orphans "${CONFIRM_ARGS[@]}"
