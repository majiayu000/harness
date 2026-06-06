#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/evaluate_pr_repair.sh --repo OWNER/REPO --pr N [options]

Options:
  --repo OWNER/REPO       GitHub repository slug.
  --pr N                  Pull request number to evaluate.
  --server-url URL        Harness HTTP server URL. Default: http://127.0.0.1:9800
  --project-root PROJECT  Project root, registry id, or name sent to POST /tasks. Must be registered for live mode.
  --output DIR            Report directory. Default: docs/pr-repair-evals/<timestamp>-<repo>-pr<N>
  --collect-only          Collect GitHub baseline only; do not submit a Harness task.
  --wait-secs N           Structured Harness wait bound and task prompt guidance. Default: 10
  --max-rounds N          Structured Harness round bound and task prompt guidance. Default: 2
  --max-turns N           Structured Harness turn bound and task prompt guidance. Default: 6
  --max-budget-usd N      Optional Harness max budget in USD.
  --poll-secs N           Poll interval for GET /tasks/{id}. Default: 30
  --timeout-secs N        Overall task poll timeout. Default: 7200
  -h, --help              Show this help.

Environment:
  HARNESS_API_TOKEN       Optional Bearer token for Harness HTTP routes.

The script never starts `harness serve`; start the server from a standalone
terminal before a live evaluation.
EOF
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

REPO=""
PR=""
SERVER_URL="http://127.0.0.1:9800"
PROJECT_ROOT="$(pwd -P)"
OUTPUT_DIR=""
COLLECT_ONLY=0
WAIT_SECS=10
MAX_ROUNDS=2
MAX_TURNS=6
MAX_BUDGET_USD=""
POLL_SECS=30
TIMEOUT_SECS=7200

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      REPO="${2:-}"
      shift 2
      ;;
    --pr)
      PR="${2:-}"
      shift 2
      ;;
    --server-url)
      SERVER_URL="${2:-}"
      shift 2
      ;;
    --project-root)
      PROJECT_ROOT="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT_DIR="${2:-}"
      shift 2
      ;;
    --collect-only)
      COLLECT_ONLY=1
      shift
      ;;
    --wait-secs)
      WAIT_SECS="${2:-}"
      shift 2
      ;;
    --max-rounds)
      MAX_ROUNDS="${2:-}"
      shift 2
      ;;
    --max-turns)
      MAX_TURNS="${2:-}"
      shift 2
      ;;
    --max-budget-usd)
      MAX_BUDGET_USD="${2:-}"
      shift 2
      ;;
    --poll-secs)
      POLL_SECS="${2:-}"
      shift 2
      ;;
    --timeout-secs)
      TIMEOUT_SECS="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$REPO" || -z "$PR" ]]; then
  usage >&2
  exit 2
fi
if [[ "$REPO" != */* ]]; then
  echo "--repo must be OWNER/REPO" >&2
  exit 2
fi
if ! [[ "$PR" =~ ^[0-9]+$ ]]; then
  echo "--pr must be a number" >&2
  exit 2
fi

require_cmd gh
require_cmd curl
require_cmd python3
require_cmd date
require_cmd cargo

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
OWNER="${REPO%%/*}"
NAME="${REPO#*/}"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
SAFE_REPO="${REPO//\//-}"
if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="docs/pr-repair-evals/${RUN_ID}-${SAFE_REPO}-pr${PR}"
fi
mkdir -p "$OUTPUT_DIR"

GRAPHQL_QUERY='
query($owner:String!, $name:String!, $pr:Int!, $threadsCursor:String) {
  repository(owner:$owner, name:$name) {
    pullRequest(number:$pr) {
      number
      url
      title
      headRefName
      headRefOid
      baseRefName
      isDraft
      mergeStateStatus
      reviewDecision
      statusCheckRollup {
        state
      }
      reviewThreads(first: 100, after: $threadsCursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          comments(first: 5) {
            nodes {
              author {
                login
              }
              body
              createdAt
              url
            }
          }
        }
      }
    }
  }
}'

GRAPHQL_FILES_QUERY='
query($owner:String!, $name:String!, $pr:Int!, $filesCursor:String) {
  repository(owner:$owner, name:$name) {
    pullRequest(number:$pr) {
      files(first: 100, after: $filesCursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          path
          additions
          deletions
          changeType
        }
      }
    }
  }
}'

collect_snapshot() {
  local out="$1"
  local page
  local cursor=""
  local files_cursor=""
  local tmp
  tmp="$(mktemp)"

  while :; do
    page="$(mktemp)"
    if [[ -n "$cursor" ]]; then
      gh api graphql \
        -f owner="$OWNER" \
        -f name="$NAME" \
        -F pr="$PR" \
        -f threadsCursor="$cursor" \
        -f query="$GRAPHQL_QUERY" > "$page"
    else
      gh api graphql \
        -f owner="$OWNER" \
        -f name="$NAME" \
        -F pr="$PR" \
        -f query="$GRAPHQL_QUERY" > "$page"
    fi

    python3 - "$tmp" "$page" <<'PY'
import json
import sys

tmp_path, page_path = sys.argv[1:]
with open(page_path, "r", encoding="utf-8") as fh:
    page = json.load(fh)

pr = ((page.get("data") or {}).get("repository") or {}).get("pullRequest")
if not pr:
    raise SystemExit("GitHub GraphQL response did not include pullRequest")

incoming_threads = pr.get("reviewThreads") or {}
incoming_nodes = list(incoming_threads.get("nodes") or [])
incoming_page_info = incoming_threads.get("pageInfo") or {}
existing = None
try:
    with open(tmp_path, "r", encoding="utf-8") as fh:
        existing = json.load(fh)
except (FileNotFoundError, json.JSONDecodeError):
    existing = None

if existing is None:
    existing = pr
    existing.setdefault("reviewThreads", {})["nodes"] = []

existing_threads = existing.setdefault("reviewThreads", {})
existing_threads.setdefault("nodes", []).extend(incoming_nodes)
existing_threads["pageInfo"] = incoming_page_info

with open(tmp_path, "w", encoding="utf-8") as fh:
    json.dump(existing, fh, indent=2, sort_keys=True)
    fh.write("\n")
PY

    cursor="$(python3 - "$page" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as fh:
        data = json.load(fh)
except json.JSONDecodeError:
    print("")
    raise SystemExit(0)

page_info = (
    (((data.get("data") or {}).get("repository") or {}).get("pullRequest") or {})
    .get("reviewThreads") or {}
).get("pageInfo") or {}
if page_info.get("hasNextPage"):
    print(page_info.get("endCursor") or "")
else:
    print("")
PY
)"
	    rm -f "$page"
	    [[ -z "$cursor" ]] && break
	  done

  while :; do
    page="$(mktemp)"
    if [[ -n "$files_cursor" ]]; then
      gh api graphql \
        -f owner="$OWNER" \
        -f name="$NAME" \
        -F pr="$PR" \
        -f filesCursor="$files_cursor" \
        -f query="$GRAPHQL_FILES_QUERY" > "$page"
    else
      gh api graphql \
        -f owner="$OWNER" \
        -f name="$NAME" \
        -F pr="$PR" \
        -f query="$GRAPHQL_FILES_QUERY" > "$page"
    fi

    python3 - "$tmp" "$page" <<'PY'
import json
import sys

tmp_path, page_path = sys.argv[1:]
with open(page_path, "r", encoding="utf-8") as fh:
    page = json.load(fh)

pr = ((page.get("data") or {}).get("repository") or {}).get("pullRequest")
if not pr:
    raise SystemExit("GitHub GraphQL response did not include pullRequest")

incoming_files = pr.get("files") or {}
incoming_nodes = list(incoming_files.get("nodes") or [])
incoming_page_info = incoming_files.get("pageInfo") or {}
try:
    with open(tmp_path, "r", encoding="utf-8") as fh:
        existing = json.load(fh)
except (FileNotFoundError, json.JSONDecodeError):
    existing = {}

existing_files = existing.setdefault("files", {})
existing_files.setdefault("nodes", []).extend(incoming_nodes)
existing_files["pageInfo"] = incoming_page_info

with open(tmp_path, "w", encoding="utf-8") as fh:
    json.dump(existing, fh, indent=2, sort_keys=True)
    fh.write("\n")
PY

    files_cursor="$(python3 - "$page" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as fh:
        data = json.load(fh)
except json.JSONDecodeError:
    print("")
    raise SystemExit(0)

page_info = (
    (((data.get("data") or {}).get("repository") or {}).get("pullRequest") or {})
    .get("files") or {}
).get("pageInfo") or {}
if page_info.get("hasNextPage"):
    print(page_info.get("endCursor") or "")
else:
    print("")
PY
)"
    rm -f "$page"
    [[ -z "$files_cursor" ]] && break
  done

  mv "$tmp" "$out"
}

task_status_from_json() {
  python3 - "$1" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as fh:
        data = json.load(fh)
except (OSError, json.JSONDecodeError):
    print("unknown")
    raise SystemExit(0)

print(data.get("status") or data.get("workflow", {}).get("state") or "unknown")
PY
}

snapshot_summary() {
  python3 - "$1" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as fh:
    pr = json.load(fh)

threads = (((pr.get("reviewThreads") or {}).get("nodes")) or [])
files = (((pr.get("files") or {}).get("nodes")) or [])
active = [
    t for t in threads
    if not t.get("isResolved", False) and not t.get("isOutdated", False)
]
status = (pr.get("statusCheckRollup") or {}).get("state") or "UNKNOWN"
print(f"pr={pr.get('number')} head={pr.get('headRefOid')} merge={pr.get('mergeStateStatus')} checks={status} unresolved_threads={len(active)} changed_files={len(files)}")
PY
}

url_encode_path_segment() {
  python3 - "$1" <<'PY'
import sys
from urllib.parse import quote

print(quote(sys.argv[1], safe=""))
PY
}

classify_snapshot() {
  python3 - "$1" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as fh:
    pr = json.load(fh)

threads = (((pr.get("reviewThreads") or {}).get("nodes")) or [])
unresolved = len([
    t for t in threads
    if not t.get("isResolved", False) and not t.get("isOutdated", False)
])
checks = (pr.get("statusCheckRollup") or {}).get("state") or "UNKNOWN"
merge = pr.get("mergeStateStatus") or "UNKNOWN"
if unresolved > 0:
    print("review_feedback_repair")
elif checks != "SUCCESS":
    print("ci_repair")
elif merge == "CLEAN" and checks == "SUCCESS":
    print("ready_noop")
else:
    print("mergeability_repair")
PY
}

snapshot_scenario() {
  local fallback="$1"
  python3 - "$QUALITY_SNAPSHOT_JSON" "$fallback" <<'PY'
import json
import sys

snapshot_path, fallback = sys.argv[1:]
try:
    with open(snapshot_path, "r", encoding="utf-8") as fh:
        snapshot = json.load(fh)
except (OSError, json.JSONDecodeError):
    snapshot = {}

print(snapshot.get("scenario") or fallback)
PY
}

write_collect_report() {
  local baseline="$1"
  local report="$OUTPUT_DIR/summary.md"
  local task_class
  task_class="$(snapshot_scenario "$(classify_snapshot "$baseline")")"
  {
    echo "# PR Repair Evaluation"
    echo
    echo "- Run ID: \`$RUN_ID\`"
    echo "- Repo: \`$REPO\`"
    echo "- PR: \`#$PR\`"
    echo "- Mode: collect-only"
    echo "- Scenario: \`$task_class\`"
    echo
    echo "## Baseline"
    echo
    echo "\`\`\`text"
    snapshot_summary "$baseline"
    echo "\`\`\`"
  } > "$report"
}

write_final_report() {
  local baseline="$1"
  local final="$2"
  local submission="$3"
  local task_detail="$4"
  local timed_out="$5"
  local report="$OUTPUT_DIR/summary.md"
  local timed_out_flag=()
  if [[ "$timed_out" == "1" ]]; then
    timed_out_flag=(--timed-out)
  fi
  python3 "$SCRIPT_DIR/evaluate_pr_repair_render_report.py" \
    --baseline "$baseline" \
    --final "$final" \
    --submission "$submission" \
    --task-detail "$task_detail" \
    --quality-snapshot "$QUALITY_SNAPSHOT_JSON" \
    --output "$report" \
    --run-id "$RUN_ID" \
    --repo "$REPO" \
    --pr "$PR" \
    --server-url "$SERVER_URL" \
    "${timed_out_flag[@]}" \
    --wait-secs "$WAIT_SECS" \
    --max-rounds "$MAX_ROUNDS" \
    --max-turns "$MAX_TURNS" \
    --max-budget-usd "$MAX_BUDGET_USD" \
    --timeout-secs "$TIMEOUT_SECS"
}

write_live_preflight_failure_report() {
  local reason="$1"
  local exit_code="$2"
  python3 "$SCRIPT_DIR/evaluate_pr_repair_preflight.py" write-failure \
    --submission "$SUBMISSION_JSON" \
    --task-detail "$TASK_DETAIL_JSON" \
    --reason "$reason"
  echo "$reason" >&2
  echo "Collecting final PR snapshot"
  FINAL_COLLECTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  collect_snapshot "$FINAL_JSON"
  snapshot_summary "$FINAL_JSON"
  write_quality_snapshot "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "$BASELINE_COLLECTED_AT" "$FINAL_COLLECTED_AT"
  write_final_report "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "0"
  echo "Evaluation report: $OUTPUT_DIR/summary.md"
  echo "Quality snapshot: $QUALITY_SNAPSHOT_JSON"
  exit "$exit_code"
}

BASELINE_JSON="$OUTPUT_DIR/baseline_pr.json"
FINAL_JSON="$OUTPUT_DIR/final_pr.json"
SUBMISSION_JSON="$OUTPUT_DIR/submission.json"
TASK_DETAIL_JSON="$OUTPUT_DIR/task_detail_final.json"
TASK_BODY_JSON="$OUTPUT_DIR/task_body.json"
PR_REPAIR_EVAL_INPUT_JSON="$OUTPUT_DIR/pr_repair_eval_input.json"
QUALITY_SNAPSHOT_JSON="$OUTPUT_DIR/quality_snapshot.json"
HEALTH_JSON="$OUTPUT_DIR/health.json"
PROJECTS_JSON="$OUTPUT_DIR/projects.json"

write_quality_snapshot() {
  local baseline="$1"
  local final="$2"
  local submission="$3"
  local task_detail="$4"
  local baseline_collected_at="$5"
  local final_collected_at="$6"
  local args=(
    run
    --quiet
    --manifest-path "$REPO_ROOT/Cargo.toml"
    -p harness-eval
    --bin score_pr_repair
    --
    --repo "$REPO"
    --pr "$PR"
    --baseline "$baseline"
    --final "$final"
    --baseline-collected-at "$baseline_collected_at"
    --final-collected-at "$final_collected_at"
    --input-output "$PR_REPAIR_EVAL_INPUT_JSON"
    --snapshot-output "$QUALITY_SNAPSHOT_JSON"
  )
  if [[ -s "$submission" ]]; then
    args+=(--submission "$submission")
  fi
  if [[ -s "$task_detail" ]]; then
    args+=(--task-detail "$task_detail")
  fi
  cargo "${args[@]}"
}

echo "Collecting baseline for $REPO#$PR"
BASELINE_COLLECTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
collect_snapshot "$BASELINE_JSON"
snapshot_summary "$BASELINE_JSON"
cp "$BASELINE_JSON" "$FINAL_JSON"
FINAL_COLLECTED_AT="$BASELINE_COLLECTED_AT"
write_quality_snapshot "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "$BASELINE_COLLECTED_AT" "$FINAL_COLLECTED_AT"
write_collect_report "$BASELINE_JSON"

if [[ "$COLLECT_ONLY" -eq 1 ]]; then
  echo "Collect-only report: $OUTPUT_DIR/summary.md"
  echo "Quality snapshot: $QUALITY_SNAPSHOT_JSON"
  exit 0
fi

curl_args=(-fsS --max-time 5)
if [[ -n "${HARNESS_API_TOKEN:-}" ]]; then
  curl_args+=(-H "Authorization: Bearer ${HARNESS_API_TOKEN}")
fi

if ! curl "${curl_args[@]}" "$SERVER_URL/health" > "$HEALTH_JSON"; then
  write_live_preflight_failure_report \
    "Harness server is not reachable at $SERVER_URL/health" \
    3
fi

echo "Checking Harness project registry for $PROJECT_ROOT"
if ! curl "${curl_args[@]}" "$SERVER_URL/projects" > "$PROJECTS_JSON"; then
  write_live_preflight_failure_report \
    "Harness project registry is not reachable at $SERVER_URL/projects" \
    5
fi

PREFLIGHT_ERROR="$OUTPUT_DIR/project_preflight_error.txt"
if ! registered_project="$(python3 "$SCRIPT_DIR/evaluate_pr_repair_preflight.py" check-project --project-root "$PROJECT_ROOT" --projects-json "$PROJECTS_JSON" 2>"$PREFLIGHT_ERROR")"; then
  reason="$(tr '\n' ' ' < "$PREFLIGHT_ERROR")"
  if [[ -z "$reason" ]]; then
    reason="Harness project root is not registered for live eval: $PROJECT_ROOT"
  fi
  echo "Register the project with POST /projects or pass a registered --project-root." >&2
  write_live_preflight_failure_report "$reason" 5
fi
echo "Registered project: $registered_project"

python3 - "$registered_project" "$REPO" "$PR" "$WAIT_SECS" "$MAX_ROUNDS" "$MAX_TURNS" "$MAX_BUDGET_USD" <<'PY' > "$TASK_BODY_JSON"
import json
import sys

project_root, repo, pr, wait_secs, max_rounds, max_turns, max_budget = sys.argv[1:]
wait_secs_value = int(wait_secs)
max_rounds_value = int(max_rounds)
max_turns_value = int(max_turns)
body = {
    "project": project_root,
    "repo": repo,
    "source": "pr_repair_eval",
    "external_id": f"pr-repair-eval:{repo}#{pr}",
    "wait_secs": wait_secs_value,
    "max_rounds": max_rounds_value,
    "max_turns": max_turns_value,
    "prompt": (
        f"PR repair capability evaluation for {repo}#{pr}. Inspect the current "
        "PR feedback, status checks, mergeability, and head SHA. Address "
        "actionable review feedback or failing checks with the smallest safe "
        "change. Commit and push only to the existing PR branch. Do not create "
        "a new PR. Evaluation envelope: wait_secs="
        f"{wait_secs}, max_rounds={max_rounds}, max_turns={max_turns}"
        f"{', max_budget_usd=' + max_budget if max_budget else ''}. If those "
        "bounds are insufficient, stop and report the limit instead of "
        "continuing. Report the validation commands and final PR evidence."
    ),
}
if max_budget:
    body["max_budget_usd"] = float(max_budget)
print(json.dumps(body, indent=2))
PY

echo "Submitting Harness PR repair task"
if ! python3 "$SCRIPT_DIR/evaluate_pr_repair_submit.py" --server-url "$SERVER_URL" --body "$TASK_BODY_JSON" --output "$SUBMISSION_JSON" --mode prompt_task; then
  printf '{"status":"failed"}\n' > "$TASK_DETAIL_JSON"
  echo "POST /tasks failed; see $SUBMISSION_JSON" >&2
  echo "Collecting final PR snapshot"
  FINAL_COLLECTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  collect_snapshot "$FINAL_JSON"
  snapshot_summary "$FINAL_JSON"
  write_quality_snapshot "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "$BASELINE_COLLECTED_AT" "$FINAL_COLLECTED_AT"
  write_final_report "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "0"
  echo "Evaluation report: $OUTPUT_DIR/summary.md"
  echo "Quality snapshot: $QUALITY_SNAPSHOT_JSON"
  exit 4
fi

TASK_ID="$(python3 - "$SUBMISSION_JSON" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], "r", encoding="utf-8") as fh:
        data = json.load(fh)
except json.JSONDecodeError:
    data = {}

print(data.get("task_id") or data.get("submission_id") or "")
PY
)"
if [[ -z "$TASK_ID" ]]; then
  printf '{"status":"failed"}\n' > "$TASK_DETAIL_JSON"
  echo "POST /tasks did not return task_id; see $SUBMISSION_JSON" >&2
  echo "Collecting final PR snapshot"
  FINAL_COLLECTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  collect_snapshot "$FINAL_JSON"
  snapshot_summary "$FINAL_JSON"
  write_quality_snapshot "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "$BASELINE_COLLECTED_AT" "$FINAL_COLLECTED_AT"
  write_final_report "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "0"
  echo "Evaluation report: $OUTPUT_DIR/summary.md"
  echo "Quality snapshot: $QUALITY_SNAPSHOT_JSON"
  exit 4
fi

echo "Submitted task_id=$TASK_ID"
ENCODED_TASK_ID="$(url_encode_path_segment "$TASK_ID")"
deadline=$(( $(date +%s) + TIMEOUT_SECS ))
status="unknown"
timed_out=0
while [[ "$(date +%s)" -lt "$deadline" ]]; do
  if curl "${curl_args[@]}" "$SERVER_URL/tasks/$ENCODED_TASK_ID" > "$TASK_DETAIL_JSON"; then
    status="$(task_status_from_json "$TASK_DETAIL_JSON")"
    echo "task status: $status"
    case "$status" in
      done|passed|failed|cancelled|blocked|ready_to_merge)
        break
        ;;
    esac
  fi
  sleep "$POLL_SECS"
done
case "$status" in
  done|passed|failed|cancelled|blocked|ready_to_merge)
    ;;
  *)
    timed_out=1
    echo "task status did not reach a terminal state before timeout"
    ;;
esac

echo "Collecting final PR snapshot"
FINAL_COLLECTED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
collect_snapshot "$FINAL_JSON"
snapshot_summary "$FINAL_JSON"
write_quality_snapshot "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "$BASELINE_COLLECTED_AT" "$FINAL_COLLECTED_AT"
write_final_report "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "$timed_out"
echo "Evaluation report: $OUTPUT_DIR/summary.md"
echo "Quality snapshot: $QUALITY_SNAPSHOT_JSON"
