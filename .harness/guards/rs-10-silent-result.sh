#!/usr/bin/env sh
# RS-10: Detect silent discard of meaningful Result values.
# Patterns: let _ = <expr>, .ok() on non-option, .unwrap_or_default()
# swallowing errors without logging.
# Output format: FILE:LINE:RS-10:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

grep -rn \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E 'let\s+_\s*=' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(//|#\[|let _ = Ok|let _ = Err|= \()' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:RS-10:Silent Result discard — use 'if let Err(e) = ...' and log the error"
done >> "${tmpfile}"

grep -rn \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E '\.(ok|unwrap_or_default)\(\)\s*;' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(//|test)' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:RS-10:Silent Result discard via .ok() or .unwrap_or_default() — log errors explicitly"
done >> "${tmpfile}"

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
