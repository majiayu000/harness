#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ -z "${HARNESS_DATABASE_URL:-}" ]]; then
  cat >&2 <<'EOF'
HARNESS_DATABASE_URL is required for the full harness-server DB test profile.

Start the local dev database with:
  bash scripts/dev-db.sh

Then run:
  HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness scripts/test-server-db.sh
EOF
  exit 2
fi

RUST_TEST_THREADS=1 cargo test -p harness-server --lib "$@"

for test_file in crates/harness-server/tests/*.rs; do
  test_name="${test_file##*/}"
  test_name="${test_name%.rs}"
  if [[ "$test_name" == "common" ]]; then
    continue
  fi

  RUST_TEST_THREADS=1 cargo test -p harness-server --test "$test_name" "$@"
done
