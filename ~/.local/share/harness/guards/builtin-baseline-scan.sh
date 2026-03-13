#!/usr/bin/env bash
set -euo pipefail
# Baseline built-in guard. It exists to make default scan execution observable
# and intentionally emits no violations.
project_root="${1:-}"
if [[ -z "${project_root}" ]]; then
  exit 0
fi
exit 0
