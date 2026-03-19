#!/usr/bin/env bash
# RS-03: Detect .unwrap() and .expect() calls in non-test Rust source files.
# Output format: FILE:LINE:RS-03:MESSAGE
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

# Flags a line only if it is NOT inside a #[cfg(test)] module.
# Strategy: scan each file, track whether we're inside a cfg(test) block,
# and emit findings only for lines outside those blocks.
check_file() {
  local file="$1"
  local in_test_block=0
  local brace_depth=0
  local test_start_depth=-1
  local lineno=0

  while IFS= read -r line; do
    lineno=$((lineno + 1))

    # Detect #[cfg(test)] annotation — next '{' opens the test block
    if echo "$line" | grep -qE '#\[cfg\(test\)\]'; then
      in_test_block=1
      test_start_depth=$brace_depth
      continue
    fi

    # Track brace depth
    opens=$(echo "$line" | grep -o '{' | wc -l | tr -d ' ')
    closes=$(echo "$line" | grep -o '}' | wc -l | tr -d ' ')
    brace_depth=$(( brace_depth + opens - closes ))

    # Exit test block when we return to the depth where it opened
    if [[ $in_test_block -eq 1 && $brace_depth -le $test_start_depth ]]; then
      in_test_block=0
      test_start_depth=-1
    fi

    # Skip lines inside cfg(test) blocks
    [[ $in_test_block -eq 1 ]] && continue

    # Detect .unwrap() — flag unless it's unwrap_or / unwrap_or_else / unwrap_or_default
    if echo "$line" | grep -qP '\.unwrap\(\)' && ! echo "$line" | grep -qP '//.*\.unwrap\(\)'; then
      echo "${file}:${lineno}:RS-03:unwrap() in non-test code risks panic in production"
    fi

    # Detect .expect("...") calls
    if echo "$line" | grep -qP '\.expect\(' && ! echo "$line" | grep -qP '//.*\.expect\('; then
      echo "${file}:${lineno}:RS-03:expect() in non-test code risks panic in production"
    fi

  done < "$file"
}

export -f check_file

find "${project_root}" -name "*.rs" \
  ! -path "*/target/*" \
  ! -path "*/.git/*" \
  ! -name "*_test.rs" \
  ! -path "*/tests/*" \
  -print0 2>/dev/null \
| xargs -0 -I{} bash -c 'check_file "$@"' _ {} 2>/dev/null || true
