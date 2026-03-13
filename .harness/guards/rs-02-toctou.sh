#!/usr/bin/env sh
# RS-02: Detect TOCTOU — get() followed by insert() without Entry API.
# Using .get() then .insert() on a map releases the lock between calls,
# creating a race condition. Prefer .entry(key).or_insert(val) instead.
# Output format: FILE:LINE:RS-02:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Detect .get( followed by .insert( within 10 lines in the same file.
find "${project_root}" -name "*.rs" \
  ! -path "*/target/*" \
  ! -path "*/.git/*" \
  ! -name "*_test.rs" \
  ! -path "*/tests/*" \
  -print0 2>/dev/null \
| xargs -0 awk '
  /\.get\(/ {
    last_get_line = NR
  }
  /\.insert\(/ {
    if (last_get_line > 0 && (NR - last_get_line) <= 10) {
      print FILENAME ":" last_get_line ":RS-02:TOCTOU — use Entry API (.entry().or_insert()) instead of get()+insert()"
      last_get_line = 0
    }
  }
' 2>/dev/null >> "${tmpfile}"

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
