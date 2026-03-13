#!/usr/bin/env bash
# U-11: Detect divergent hardcoded default database/config paths across binaries.
# Flags multiple distinct .db path strings in production Rust code, which
# indicates that different binaries may be using different data files.
# Output format: FILE:LINE:U-11:MESSAGE
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

# Collect all hardcoded .db path joins outside of test and target directories.
find "${project_root}" -name "*.rs" \
  ! -path "*/target/*" \
  ! -path "*/.git/*" \
  ! -name "*_test.rs" \
  ! -path "*/tests/*" \
  -print0 2>/dev/null \
| xargs -0 grep -n '\.join("[^"]*\.db")' 2>/dev/null \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:U-11:Hardcoded .db path — ensure all binaries use a shared default_db_path() function"
done || true
