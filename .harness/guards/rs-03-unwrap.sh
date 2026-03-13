#!/usr/bin/env bash
# RS-03: Detect .unwrap() calls in non-test Rust source files.
# Output format: FILE:LINE:RS-03:MESSAGE
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

# Find .unwrap() in Rust files, excluding test files and the target directory.
find "${project_root}" -name "*.rs" \
  ! -path "*/target/*" \
  ! -path "*/.git/*" \
  ! -name "*_test.rs" \
  ! -path "*/tests/*" \
  -print0 2>/dev/null \
| xargs -0 grep -n '\.unwrap()' 2>/dev/null \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:RS-03:unwrap() in non-test code risks panic in production"
done || true
