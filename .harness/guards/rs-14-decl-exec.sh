#!/usr/bin/env bash
# RS-14: Detect declaration-execution gaps in Rust source files.
# Two sub-checks via ast-grep AST-level detection:
#
#   rs-14-config-default: Config struct initialized with Default::default()
#     instead of calling load()/from_env()/from_file().
#
#   rs-14-trait-unregistered: impl blocks for *Store/*Registry/*Manager/*Handler
#     traits — verify startup registration in build_app_state() or main().
#
# Output format: FILE:LINE:RS-14:MESSAGE
# Exit 0 on pass, exit 1 if violations found.
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RULE_FILE="${SCRIPT_DIR}/../sg/rules/rs-14-decl-exec.yml"

if ! command -v sg >/dev/null 2>&1; then
  echo "rs-14-decl-exec.sh: ast-grep (sg) not found — install with: cargo install ast-grep" >&2
  exit 0
fi

tmpfile=$(mktemp)
trap 'rm -f "${tmpfile}"' EXIT

sg scan --rule "${RULE_FILE}" --json "${project_root}" 2>/dev/null \
  | jq -r '.[] |
      select(.file | test("/(tests?)/|_test\\.rs$|/target/|/\\.git/") | not) |
      "\(.file):\(.range.start.line + 1):RS-14:\(.message)"' \
  >> "${tmpfile}" || true

if [[ -s "${tmpfile}" ]]; then
  cat "${tmpfile}"
  exit 1
fi

exit 0
