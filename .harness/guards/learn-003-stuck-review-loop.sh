#!/usr/bin/env sh
# LEARN-003: Detect stuck review loops — ≥3 consecutive "fixed" rounds for the
# same task without an intervening "complete" or "lgtm".
#
# Scans event store (events.jsonl) for pr_review / task_review patterns.
# Configurable threshold via GUARD_FIXED_THRESHOLD (default: 3).
#
# Output format: FILE:LINE:LEARN-003:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

threshold="${GUARD_FIXED_THRESHOLD:-3}"

# Look for the events file in common locations.
events_file=""
for candidate in \
  "${HOME}/.local/share/harness/events.jsonl" \
  "${HOME}/Library/Application Support/harness/events.jsonl" \
  "${project_root}/.harness/events.jsonl"; do
  if [ -f "${candidate}" ]; then
    events_file="${candidate}"
    break
  fi
done

if [ -z "${events_file}" ] || [ ! -f "${events_file}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Count consecutive "fixed" results per task_id, reset on "lgtm" or "complete".
awk -v threshold="${threshold}" '
  /"hook".*"(pr_review|task_review)"/ || /"result".*"fixed"/ {
    # Extract task_id
    match($0, /"task_id"[[:space:]]*:[[:space:]]*"([^"]+)"/, arr)
    if (arr[1] == "") next
    tid = arr[1]

    if ($0 ~ /"result"[[:space:]]*:[[:space:]]*"fixed"/) {
      streak[tid]++
      if (streak[tid] >= threshold && !reported[tid]) {
        print FILENAME ":0:LEARN-003:stuck review loop — task " tid " has " streak[tid] " consecutive fixed rounds without lgtm/complete"
        reported[tid] = 1
      }
    } else if ($0 ~ /"result"[[:space:]]*:[[:space:]]*"(lgtm|complete)"/) {
      streak[tid] = 0
    }
  }
' "${events_file}" >> "${tmpfile}" 2>/dev/null

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
