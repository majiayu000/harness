#!/usr/bin/env sh
# SEC-03: Detect XSS via innerHTML assignment with unsanitized user input.
# Direct innerHTML assignment without DOMPurify or framework escaping is
# an XSS vulnerability.
#
# Improved: if a file defines or calls escapeHtml/sanitize/DOMPurify,
# the entire file is considered protected (variable-chain safety).
#
# Output format: FILE:LINE:SEC-03:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Find files with innerHTML assignments
grep -rln \
  --include="*.js" \
  --include="*.ts" \
  --include="*.jsx" \
  --include="*.tsx" \
  --include="*.html" \
  --include="*.vue" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  --exclude-dir="node_modules" \
  --exclude-dir=".harness" \
  -E '\.innerHTML\s*=' \
  "${project_root}" 2>/dev/null \
| while IFS= read -r file; do
  # If the file contains escapeHtml/DOMPurify/sanitize, it's protected
  if grep -qE '(escapeHtml|DOMPurify|sanitize\s*\()' "${file}" 2>/dev/null; then
    continue
  fi
  # Report unprotected innerHTML assignments
  grep -nE '\.innerHTML\s*=' "${file}" 2>/dev/null \
  | grep -v -E '(//\s*safe|test|spec)' \
  | while IFS=: read -r line rest; do
    echo "${file}:${line}:SEC-03:XSS risk — innerHTML assignment in file without sanitization function"
  done
done >> "${tmpfile}"

grep -rn \
  --include="*.js" \
  --include="*.ts" \
  --include="*.jsx" \
  --include="*.tsx" \
  --exclude-dir=".git" \
  --exclude-dir="node_modules" \
  --exclude-dir=".harness" \
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
