#!/usr/bin/env sh
# SEC-03: Detect XSS via innerHTML assignment with unsanitized user input.
# Direct innerHTML assignment without DOMPurify or framework escaping is
# an XSS vulnerability.
# Output format: FILE:LINE:SEC-03:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

grep -rn \
  --include="*.js" \
  --include="*.ts" \
  --include="*.jsx" \
  --include="*.tsx" \
  --include="*.html" \
  --include="*.vue" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  --exclude-dir="node_modules" \
  -E '\.innerHTML\s*=' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(DOMPurify|sanitize|escapeHtml|//\s*safe|test|spec)' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:SEC-03:XSS risk — innerHTML assignment without sanitization; use DOMPurify or textContent"
done >> "${tmpfile}"

grep -rn \
  --include="*.js" \
  --include="*.ts" \
  --include="*.jsx" \
  --include="*.tsx" \
  --exclude-dir=".git" \
  --exclude-dir="node_modules" \
  -E 'dangerouslySetInnerHTML\s*=\s*\{\s*\{[^}]*\}' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(sanitize|DOMPurify|test|spec)' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:SEC-03:XSS risk — dangerouslySetInnerHTML without sanitization"
done >> "${tmpfile}"

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
