#!/usr/bin/env sh
# RS-12: Detect dual systems for the same responsibility.
# Heuristic: look for parallel type hierarchies (e.g., Task*/Todo*, Manager*/Handler*)
# where two naming conventions coexist for the same domain concept.
# Output format: FILE:LINE:RS-12:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Detect structs/enums named with Task* and Todo* in the same codebase.
has_task=$(grep -rl \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  --exclude-dir="tests" \
  --exclude-dir=".harness" \
  -E 'struct\s+Task[A-Z]|enum\s+Task[A-Z]' \
  "${project_root}" 2>/dev/null | head -1)

has_todo=$(grep -rl \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  --exclude-dir="tests" \
  --exclude-dir=".harness" \
  -E 'struct\s+Todo[A-Z]|enum\s+Todo[A-Z]' \
  "${project_root}" 2>/dev/null | head -1)

if [ -n "${has_task}" ] && [ -n "${has_todo}" ]; then
  grep -rn \
    --include="*.rs" \
    --exclude-dir=".git" \
    --exclude-dir="target" \
    -E 'struct\s+Task[A-Z]|enum\s+Task[A-Z]' \
    "${project_root}" 2>/dev/null \
  | while IFS=: read -r file line rest; do
    echo "${file}:${line}:RS-12:Dual system detected — Task* and Todo* types coexist; consolidate to a single domain model"
  done >> "${tmpfile}"
fi

# Detect Manager*/Handler* coexisting for the same concept.
has_manager=$(grep -rl \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E 'struct\s+[A-Z][a-zA-Z]+Manager\b' \
  "${project_root}" 2>/dev/null | head -1)

has_handler=$(grep -rl \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E 'struct\s+[A-Z][a-zA-Z]+Handler\b' \
  "${project_root}" 2>/dev/null | head -1)

if [ -n "${has_manager}" ] && [ -n "${has_handler}" ]; then
  grep -rn \
    --include="*.rs" \
    --exclude-dir=".git" \
    --exclude-dir="target" \
    -E 'struct\s+[A-Z][a-zA-Z]+Manager\b' \
    "${project_root}" 2>/dev/null \
  | while IFS=: read -r file line rest; do
    echo "${file}:${line}:RS-12:Dual system detected — *Manager and *Handler types coexist; unify under one abstraction"
  done >> "${tmpfile}"
fi

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
