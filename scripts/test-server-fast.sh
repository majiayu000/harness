#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

filters=(
  "task_runner::"
  "services::"
  "router::"
  "http::tests::health_route_tests::"
  "http::tests::intake_auth_list_tests::"
)

for filter in "${filters[@]}"; do
  echo "==> cargo test -p harness-server --lib ${filter} $*"
  cargo test -p harness-server --lib "${filter}" "$@"
done
