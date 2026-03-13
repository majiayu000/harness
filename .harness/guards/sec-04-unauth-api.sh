#!/usr/bin/env sh
# SEC-04: Detect API endpoints that lack authentication/authorization checks.
# Heuristic: route handler functions without middleware or guard annotations.
# Output format: FILE:LINE:SEC-04:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Python/Flask/FastAPI: route decorators without login_required or dependency
grep -rn \
  --include="*.py" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E '@(app|router|blueprint)\.(get|post|put|delete|patch)\(' \
  "${project_root}" 2>/dev/null \
| while IFS=: read -r file line rest; do
  # Check if the next few lines contain auth references
  auth_check=$(awk "NR>=${line} && NR<=$((line + 5))" "${file}" 2>/dev/null \
    | grep -c -E '(login_required|current_user|Depends|authenticate|authorize|jwt|token|permission)' || true)
  if [ "${auth_check}" = "0" ]; then
    echo "${file}:${line}:SEC-04:API endpoint may lack authentication — add auth middleware or guard"
  fi
done >> "${tmpfile}"

# JavaScript/TypeScript Express: router.get/post without auth middleware
grep -rn \
  --include="*.js" \
  --include="*.ts" \
  --exclude-dir=".git" \
  --exclude-dir="node_modules" \
  -E 'router\.(get|post|put|delete|patch)\(' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(authenticate|authorize|requireAuth|isAuthenticated|verifyToken|checkJwt|passport|guard|middleware)' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:SEC-04:API route may lack authentication middleware"
done >> "${tmpfile}"

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
