#!/usr/bin/env sh
# SEC-07: Detect path traversal vulnerabilities from unvalidated user-supplied paths.
# User-controlled input used in file operations without canonicalization or
# prefix validation allows reading/writing arbitrary files.
# Output format: FILE:LINE:SEC-07:MESSAGE
# Exit 0 on pass, exit 1 if violations found.

project_root="${1:-}"
if [ -z "${project_root}" ]; then
  exit 0
fi

tmpfile=$(mktemp)

# Python: open() or Path() with user-controlled variable without sanitization
grep -rn \
  --include="*.py" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E 'open\((request\.|params\[|args\[|data\[|form\[)' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(safe_join|realpath|resolve|abspath|test|spec)' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:SEC-07:Path traversal risk — validate and canonicalize paths from user input before file operations"
done >> "${tmpfile}"

# Rust: path constructed from user input without canonicalize()
grep -rn \
  --include="*.rs" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  -E 'PathBuf::from\(|Path::new\(' \
  "${project_root}" 2>/dev/null \
| while IFS=: read -r file line rest; do
  # Check surrounding 5 lines for canonicalize or starts_with validation
  safe=$(awk "NR>=$((line > 5 ? line - 5 : 1)) && NR<=$((line + 5))" "${file}" 2>/dev/null \
    | grep -c -E '(canonicalize|starts_with|strip_prefix|contains\("\.\."\))' || true)
  if [ "${safe}" = "0" ]; then
    # Only flag if there's evidence of user input (query, param, header, body, arg)
    user_input=$(awk "NR>=$((line > 3 ? line - 3 : 1)) && NR<=$((line + 3))" "${file}" 2>/dev/null \
      | grep -c -E '(query|param|header|body|arg|request|user|input)' || true)
    if [ "${user_input}" -gt 0 ]; then
      echo "${file}:${line}:SEC-07:Path traversal risk — canonicalize path and verify it starts within allowed base dir"
    fi
  fi
done >> "${tmpfile}"

# JavaScript/TypeScript: path.join with user-controlled input
grep -rn \
  --include="*.js" \
  --include="*.ts" \
  --exclude-dir=".git" \
  --exclude-dir="node_modules" \
  -E 'path\.(join|resolve)\(.*req\.' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(normalize|resolve.*__dirname|test|spec)' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:SEC-07:Path traversal risk — validate that resolved path stays within allowed directory"
done >> "${tmpfile}"

if [ -s "${tmpfile}" ]; then
  cat "${tmpfile}"
  rm -f "${tmpfile}"
  exit 1
fi

rm -f "${tmpfile}"
exit 0
