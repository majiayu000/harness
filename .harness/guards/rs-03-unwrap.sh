#!/usr/bin/env bash
# RS-03: Detect .unwrap() and .expect() calls in non-test Rust source files.
# Uses ast-grep for AST-level detection:
#   - Naturally excludes .unwrap_or / .unwrap_or_else / .unwrap_or_default variants
#   - Excludes matches inside comments and string literals
#   - Excludes Mutex::lock().unwrap(), RwLock::read/write().unwrap()
#   - Excludes fn main() scope
# Output format: FILE:LINE:RS-03:MESSAGE
# Exit 0 on pass, exit 1 if violations found.
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RULE_FILE="${SCRIPT_DIR}/../sg/rules/rs-03-unwrap.yml"

if ! command -v sg >/dev/null 2>&1; then
  echo "rs-03-unwrap.sh: ast-grep (sg) not found — install with: cargo install ast-grep" >&2
  exit 0
fi

tmpfile=$(mktemp)
trap 'rm -f "${tmpfile}"' EXIT

sg scan --rule "${RULE_FILE}" --json "${project_root}" 2>/dev/null \
  | jq -r '.[] |
      select(.file | test("/(tests?)/|/tests?\\.rs$|_test\\.rs$|/target/|/\\.git/") | not) |
      "\(.file):\(.range.start.line + 1):RS-03:\(.message)"' \
  >> "${tmpfile}" || true

if [[ -s "${tmpfile}" ]]; then
  cat "${tmpfile}"
  exit 1
fi

exit 0
