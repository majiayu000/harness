#!/usr/bin/env bash
# Validate a PR body file before `gh pr create --body-file <file>`.
#
# Inspired by Symphony's `mix pr_body.check`: the agent writes the body to a
# file, this script lints it, and only on exit 0 does the agent proceed to
# invoke `gh pr create`. Keeps the LLM in charge of the prose while making
# structural requirements (like issue linkage) machine-verifiable.
#
# Usage:
#   scripts/pr_body_check.sh --file /tmp/pr_body.md [--issue <N>]
#
# Checks:
#   1. File exists and is non-empty.
#   2. Body has a `## Summary` section with at least one line of content.
#   3. Body has a `## Test` section (matches "Test plan", "Testing", etc.).
#   4. If --issue N is provided, body contains `Closes #N` or `Fixes #N` or
#      `Resolves #N` on its own line so GitHub auto-closes on merge.
#
# Exits non-zero with a clear, actionable message on any failure.

set -euo pipefail

FILE=""
ISSUE=""

usage() {
  cat <<EOF
Usage: $0 --file <path> [--issue <N>]

Required:
  --file <path>      PR body markdown file to validate

Optional:
  --issue <N>        GitHub issue number this PR closes; when set, the body
                     must contain Closes #N / Fixes #N / Resolves #N on its
                     own line so GitHub auto-closes the issue on merge.

Exit codes:
  0  — body is valid
  1  — file missing or empty
  2  — required section missing
  3  — issue linkage missing or mismatched
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --file) FILE="${2:-}"; shift 2 ;;
    --issue) ISSUE="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 64 ;;
  esac
done

if [ -z "$FILE" ]; then
  echo "ERROR: --file is required" >&2
  usage >&2
  exit 64
fi

if [ ! -s "$FILE" ]; then
  echo "ERROR: body file '$FILE' is missing or empty" >&2
  exit 1
fi

body=$(cat "$FILE")

# Check 2: Summary section
if ! grep -qE '^##[[:space:]]+Summary\b' <<<"$body"; then
  echo "ERROR: PR body is missing a '## Summary' section." >&2
  echo "       Add one with a short bullet list describing what changed and why." >&2
  exit 2
fi

# Check 3: Test section (accept several spellings)
if ! grep -qiE '^##[[:space:]]+(Test(ing|[[:space:]]+plan)?)\b' <<<"$body"; then
  echo "ERROR: PR body is missing a '## Test plan' / '## Testing' / '## Test' section." >&2
  echo "       List the checks you ran (cargo check, cargo test, manual steps, etc.)." >&2
  exit 2
fi

# Check 4: Issue linkage when --issue is passed
if [ -n "$ISSUE" ]; then
  # Match `Closes #123`, `Fixes: #123`, `Resolves #123`, etc., on its own line.
  # The keyword and issue number must both appear; extra trailing whitespace ok.
  pattern="^[[:space:]]*(Closes|Fixes|Resolves)[:[:space:]]+#${ISSUE}[[:space:]]*$"
  if ! grep -qiE "$pattern" <<<"$body"; then
    echo "ERROR: PR body must contain a GitHub closing keyword for issue #${ISSUE}." >&2
    echo "       Add a line on its own that reads exactly:" >&2
    echo "           Closes #${ISSUE}" >&2
    echo "       GitHub uses this to auto-close the issue when the PR merges." >&2
    exit 3
  fi
fi

echo "PR body validation passed: file=${FILE} issue=${ISSUE:-<none>}"
