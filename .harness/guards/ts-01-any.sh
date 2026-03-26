#!/usr/bin/env bash
# TS-01: Detect 'any' type annotations in TypeScript and TSX source files.
# Uses ast-grep AST-level detection:
#   - Only matches type_annotation nodes (not comments or strings containing "any")
#   - Catches: let x: any, function foo(x: any): any, interface { field: any }
# Output format: FILE:LINE:TS-01:MESSAGE
# Exit 0 on pass, exit 1 if violations found.
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RULE_FILE="${SCRIPT_DIR}/../sg/rules/ts-01-any.yml"

if ! command -v sg >/dev/null 2>&1; then
  echo "ts-01-any.sh: ast-grep (sg) not found — install with: cargo install ast-grep" >&2
  exit 0
fi

tmpfile=$(mktemp)
trap 'rm -f "${tmpfile}"' EXIT

sg scan --rule "${RULE_FILE}" --json "${project_root}" 2>/dev/null \
  | jq -r '.[] |
      select(.file | test("/\\.git/|/node_modules/|/dist/|/build/|\\.d\\.ts$") | not) |
      "\(.file):\(.range.start.line + 1):TS-01:\(.message)"' \
  >> "${tmpfile}" || true

if [[ -s "${tmpfile}" ]]; then
  cat "${tmpfile}"
  exit 1
fi

exit 0
