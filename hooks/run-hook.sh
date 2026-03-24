#!/usr/bin/env bash
# hooks/run-hook.sh — cross-platform hook runner
#
# Usage: hooks/run-hook.sh <script> [args...]
#
# Sets Python UTF-8 encoding env vars before delegating to the hook script
# so that guard scripts invoking Python work correctly on Windows (CP-1252)
# and other non-UTF-8 default locales.
set -euo pipefail

export PYTHONUTF8=1
export PYTHONIOENCODING=utf-8

if [[ $# -lt 1 ]]; then
    echo "Usage: $0 <script> [args...]" >&2
    exit 1
fi

exec bash "$@"
