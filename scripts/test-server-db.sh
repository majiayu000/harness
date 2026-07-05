#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

runner="${HARNESS_SERVER_TEST_RUNNER:-cargo-test}"

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  cat <<'EOF'
Run the full harness-server DB test profile.

Usage:
  HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-db.sh [test filters...]

Runner:
  HARNESS_SERVER_TEST_RUNNER=cargo-test  Use cargo test (default)
  HARNESS_SERVER_TEST_RUNNER=nextest     Use cargo nextest run

The profile runs with default test parallelism. Tests that mutate true
process-global state keep their own explicit locks.
EOF
  exit 0
fi

case "$runner" in
  cargo-test|nextest) ;;
  *)
    echo "unsupported HARNESS_SERVER_TEST_RUNNER: $runner" >&2
    exit 2
    ;;
esac

if [[ -z "${HARNESS_DATABASE_URL:-}" ]]; then
  cat >&2 <<'EOF'
HARNESS_DATABASE_URL is required for the full harness-server DB test profile.

Start the local dev database with:
  bash scripts/dev-db.sh

Then run:
  HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-db.sh

Postgres-backed tests refuse non-test database names by default. Use a database
named harness_test, ending in _test, or starting with test_.
EOF
  exit 2
fi

run_harness_server_test() {
  case "$runner" in
    cargo-test) cargo test -p harness-server "$@" ;;
    nextest) cargo nextest run --no-tests=pass -p harness-server "$@" ;;
  esac
}

echo "==> ${runner}: harness-server --lib"
run_harness_server_test --lib "$@"

for test_file in crates/harness-server/tests/*.rs; do
  test_name="${test_file##*/}"
  test_name="${test_name%.rs}"
  if [[ "$test_name" == "common" ]]; then
    continue
  fi

  echo "==> ${runner}: harness-server --test ${test_name}"
  run_harness_server_test --test "$test_name" "$@"
done
