#!/usr/bin/env bash
# GO-01: Detect discarded error return values in Go source files.
# Uses ast-grep AST-level detection:
#   - Matches short_var_declaration with blank identifier discarding second return
#   - Excludes for-range loops (for _, v := range ...) via not:inside:for_statement
#   - Pattern $X, _ := $CALL($$$ARGS) only matches function call expressions,
#     not range expressions, so for-range FPs are naturally excluded
# Output format: FILE:LINE:GO-01:MESSAGE
# Exit 0 on pass, exit 1 if violations found.
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RULE_FILE="${SCRIPT_DIR}/../sg/rules/go-01-error.yml"

if ! command -v sg >/dev/null 2>&1; then
  echo "go-01-error.sh: ast-grep (sg) not found — install with: cargo install ast-grep" >&2
  exit 0
fi

tmpfile=$(mktemp)
trap 'rm -f "${tmpfile}"' EXIT

sg scan --rule "${RULE_FILE}" --json "${project_root}" 2>/dev/null \
  | jq -r '.[] |
      select(.file | test("/\\.git/|/vendor/|_test\\.go$") | not) |
      "\(.file):\(.range.start.line + 1):GO-01:\(.message)"' \
  >> "${tmpfile}" || true

if [[ -s "${tmpfile}" ]]; then
  cat "${tmpfile}"
  exit 1
fi

exit 0
