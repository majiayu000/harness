#!/usr/bin/env bash
# Compare completed runs using the existing eval completeness and regression gates.
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "Usage: $0 HARNESS_BIN BASELINE_REPORT CANDIDATE_REPORT DIFF_OUTPUT" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for report in "$2" "$3"; do
  python3 "$SCRIPT_DIR/check-eval-baseline-eligibility.py" \
    --report "$report" --baseline-present false --comparison-outcome not_run
done

exec "$1" eval diff "$2" "$3" \
  --max-pass-drop 0 --fail-on-new-f-gate --json --output "$4"
