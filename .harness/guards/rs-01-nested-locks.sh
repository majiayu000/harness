#!/usr/bin/env sh
# RS-01: Detect nested RwLock/Mutex acquisition patterns that risk deadlock.
# Heuristic: two lock acquisition calls within 5 source lines of each other.
# Output format: FILE:LINE:RS-01:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

find "${project_root}" -name "*.rs" \
  ! -path "*/target/*" \
  ! -path "*/.git/*" \
  ! -name "*_test.rs" \
  ! -path "*/tests/*" \
  -print0 2>/dev/null \
| xargs -0 awk '
  /\.(lock|read|write)\(\)/ {
    if (last_lock_line > 0 && (NR - last_lock_line) <= 5) {
      print FILENAME ":" last_lock_line ":RS-01:Nested lock acquisition — risk of deadlock"
    }
    last_lock_line = NR
  }
' 2>/dev/null >> "${tmpfile}"

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
