#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")"

# Auto-detect GITHUB_TOKEN from gh CLI if not already set
if [ -z "${GITHUB_TOKEN:-}" ]; then
    GITHUB_TOKEN=$(gh auth token 2>/dev/null || true)
    if [ -z "$GITHUB_TOKEN" ]; then
        echo "WARNING: GITHUB_TOKEN not available. Run 'gh auth login' first."
        echo "Review bot auto-trigger will be disabled."
    else
        export GITHUB_TOKEN
        echo "GITHUB_TOKEN loaded from gh CLI"
    fi
fi

exec ./target/release/harness serve --transport http --port 9800 --project-root "$(pwd)"
