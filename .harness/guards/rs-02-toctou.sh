#!/usr/bin/env sh
# RS-02: Detect TOCTOU — get() followed by insert() on the SAME map without Entry API.
# Using .get() then .insert() on a map releases the lock between calls,
# creating a race condition. Prefer .entry(key).or_insert(val) instead.
# Output format: FILE:LINE:RS-02:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Detect RECEIVER.get( followed by RECEIVER.insert( within 10 lines in the same file.
# Uses same-variable matching to avoid false positives from unrelated get/insert calls
# (e.g. db.get() followed by cache.insert() on different receivers).
# Resets state on each new file (FNR == 1) to prevent cross-file contamination.
find "${project_root}" -name "*.rs" \
  ! -path "*/target/*" \
  ! -path "*/.git/*" \
  ! -name "*_test.rs" \
  ! -path "*/tests/*" \
  -print0 2>/dev/null \
| xargs -0 awk '
  FNR == 1 {
    last_get_var = ""
    last_get_line = 0
  }
  /[A-Za-z_][A-Za-z0-9_]*\.get\(/ {
    match($0, /[A-Za-z_][A-Za-z0-9_]*\.get\(/)
    token = substr($0, RSTART, RLENGTH - 5)
    last_get_var = token
    last_get_line = FNR
  }
  /[A-Za-z_][A-Za-z0-9_]*\.insert\(/ {
    match($0, /[A-Za-z_][A-Za-z0-9_]*\.insert\(/)
    token = substr($0, RSTART, RLENGTH - 8)
    if (token == last_get_var && last_get_line > 0 && (FNR - last_get_line) <= 10) {
      print FILENAME ":" last_get_line ":RS-02:TOCTOU — use Entry API (.entry().or_insert()) instead of " token ".get()+" token ".insert()"
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
