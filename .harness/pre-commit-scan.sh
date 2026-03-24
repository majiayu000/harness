#!/usr/bin/env bash
# pre-commit-scan.sh — Baseline guard scan for staged (added) lines only.
#
# Runs every guard in .harness/guards/ against files that have staged changes,
# then filters violations to lines present in the diff.
#
# Usage (from project root):
#   .harness/pre-commit-scan.sh
#
# To install as a git pre-commit hook:
#   ln -sf ../../.harness/pre-commit-scan.sh .git/hooks/pre-commit
#
# Exit codes:
#   0  — no violations on staged lines
#   1  — violations found (commit blocked for error_block rules)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
GUARDS_DIR="${SCRIPT_DIR}/guards"
SCORECARD="${SCRIPT_DIR}/rule-scorecard.json"

# Collect staged files (Added, Copied, Modified — not deleted).
mapfile -t STAGED_FILES < <(
  git -C "${PROJECT_ROOT}" diff --cached --name-only --diff-filter=ACM 2>/dev/null || true
)

if [[ ${#STAGED_FILES[@]} -eq 0 ]]; then
  exit 0
fi

# Build a map of file → set of added line numbers from the diff.
# Format produced by git diff --unified=0:  "+++ b/path" then "@@...+start[,count]"
declare -A ADDED_LINES   # key: "file:lineno" → "1"

while IFS= read -r diff_line; do
  if [[ "${diff_line}" =~ ^\+\+\+\ b/(.+)$ ]]; then
    current_file="${BASH_REMATCH[1]}"
    continue
  fi
  if [[ "${diff_line}" =~ ^@@.*\+([0-9]+)(,([0-9]+))? ]]; then
    start="${BASH_REMATCH[1]}"
    count="${BASH_REMATCH[3]:-1}"
    for ((i = 0; i < count; i++)); do
      ADDED_LINES["${current_file}:$((start + i))"]="1"
    done
    continue
  fi
done < <(git -C "${PROJECT_ROOT}" diff --cached --unified=0 2>/dev/null || true)

if [[ ! -d "${GUARDS_DIR}" ]]; then
  exit 0
fi

violations=0
blocking=0

# Determine which rules are error_block from the scorecard.
declare -A BLOCKING_RULES
if [[ -f "${SCORECARD}" ]]; then
  while IFS= read -r rule_id; do
    BLOCKING_RULES["${rule_id}"]="1"
  done < <(
    python3 -c "
import json, sys
sc = json.load(open('${SCORECARD}'))
for rid, e in sc.get('rules', {}).items():
    if e.get('lifecycle') == 'error_block':
        print(rid)
" 2>/dev/null || true
  )
fi

# Run each guard and collect output.
for guard in "${GUARDS_DIR}"/*.sh; do
  [[ -f "${guard}" ]] || continue
  # shellcheck disable=SC2207
  mapfile -t guard_output < <(bash "${guard}" "${PROJECT_ROOT}" 2>/dev/null || true)

  for line in "${guard_output[@]}"; do
    # Expected format: FILE:LINE:RULE_ID:MESSAGE
    IFS=':' read -r -a parts <<< "${line}"
    [[ ${#parts[@]} -ge 4 ]] || continue

    file="${parts[0]}"
    lineno="${parts[1]}"
    rule_id="${parts[2]}"

    # Strip project_root prefix if present.
    rel_file="${file#${PROJECT_ROOT}/}"

    # Only report violations on staged added lines.
    key="${rel_file}:${lineno}"
    [[ "${ADDED_LINES[${key}]+_}" ]] || continue

    # Check suppression comment on the preceding line.
    suppression_check_file="${PROJECT_ROOT}/${rel_file}"
    if [[ -f "${suppression_check_file}" && "${lineno}" -gt 1 ]]; then
      prev_lineno=$(( lineno - 1 ))
      prev_line="$(sed -n "${prev_lineno}p" "${suppression_check_file}" 2>/dev/null || true)"
      if echo "${prev_line}" | grep -qF "vibeguard-disable-next-line ${rule_id}"; then
        continue
      fi
    fi

    echo "${line}"
    violations=$(( violations + 1 ))

    if [[ "${BLOCKING_RULES[${rule_id}]+_}" ]]; then
      blocking=$(( blocking + 1 ))
    fi
  done
done

if [[ ${violations} -gt 0 ]]; then
  echo ""
  echo "vibeguard: ${violations} violation(s) on staged lines."
  if [[ ${blocking} -gt 0 ]]; then
    echo "vibeguard: ${blocking} blocking violation(s) — commit rejected."
    echo "  Fix violations or use: git commit --no-verify  (not recommended)"
    exit 1
  fi
  echo "vibeguard: all violations are warn-level — commit allowed."
  echo "  Add feedback to .harness/triage.jsonl to improve rule precision."
fi

exit 0
