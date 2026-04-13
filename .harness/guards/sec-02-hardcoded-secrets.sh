#!/usr/bin/env bash
# SEC-02: Detect hardcoded API keys, tokens, and secrets.
# Output format: FILE:LINE:SEC-02:MESSAGE
#
# Only runs on projects that opt in via .harness/guards/ (rule_enforcer
# skips projects without that directory).  Guard patterns are tuned for
# the harness codebase — external projects are excluded at the enforcer
# level, not here.
set -euo pipefail

project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi

# Detect common secret/key assignment patterns with non-trivial string values.
grep -rn \
  --include="*.py" \
  --include="*.rs" \
  --include="*.go" \
  --include="*.js" \
  --include="*.ts" \
  --include="*.yaml" \
  --include="*.yml" \
  --include="*.env" \
  --exclude-dir=".git" \
  --exclude-dir="target" \
  --exclude-dir="node_modules" \
  --exclude-dir="tests" \
  --exclude-dir="test" \
  --exclude-dir="benches" \
  --exclude-dir="fixtures" \
  --exclude-dir="testdata" \
  -E '(api_key|apikey|api_secret|secret_key|access_token|auth_token|password|passwd)\s*[=:]\s*"[^"$\{]{8,}"' \
  "${project_root}" 2>/dev/null \
| grep -v -E '(_test\.(rs|go|py|ts|js):|_spec\.(rs|ts|js):)' \
| grep -v -E '(test|example|placeholder|changeme|your_|<|>|\*{3}|xxx|TODO|FIXME|mock|fake|dummy|sample)' \
| while IFS=: read -r file line rest; do
  echo "${file}:${line}:SEC-02:Hardcoded secret or API key detected"
done || true
