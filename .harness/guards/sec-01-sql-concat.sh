#!/usr/bin/env bash
# SEC-01: Detect SQL string concatenation patterns that may lead to injection.
# Output format: FILE:LINE:SEC-01:MESSAGE
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

# Detect SQL keyword strings followed by concatenation operators in common languages.
# Patterns: "SELECT ..." + , "INSERT ..." + , f"SELECT...", format!("SELECT {
grep -rn \
  --include="*.py" \
  --include="*.js" \
  --include="*.ts" \
  --include="*.go" \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E '"(SELECT|INSERT|UPDATE|DELETE|DROP)\s[^"]*"\s*(\+|\.)|f"(SELECT|INSERT|UPDATE|DELETE|DROP)\s[^"]*\{|`(SELECT|INSERT|UPDATE|DELETE|DROP)\s[^`]*\$\{' \
  "${project_root}" 2>/dev/null \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:SEC-01:SQL string concatenation may lead to injection"
done || true
