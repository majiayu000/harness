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
  --project-root PATH     Local project root sent to POST /tasks. Default: current directory.
  --output DIR            Report directory. Default: docs/pr-repair-evals/<timestamp>-<repo>-pr<N>
  --collect-only          Collect GitHub baseline only; do not submit a Harness task.
  --wait-secs N           Eval wait guidance included in the task prompt. Default: 10
  --max-rounds N          Eval round guidance included in the task prompt. Default: 2
  --max-turns N           Eval turn guidance included in the task prompt. Default: 6
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

collect_snapshot() {
  local out="$1"
  local page
  local cursor=""
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
incoming_threads = pr.get("reviewThreads") or {}
existing_threads.setdefault("nodes", []).extend(incoming_threads.get("nodes") or [])
existing_threads["pageInfo"] = incoming_threads.get("pageInfo") or {}

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
active = [
    t for t in threads
    if not t.get("isResolved", False) and not t.get("isOutdated", False)
]
status = (pr.get("statusCheckRollup") or {}).get("state") or "UNKNOWN"
print(f"pr={pr.get('number')} head={pr.get('headRefOid')} merge={pr.get('mergeStateStatus')} checks={status} unresolved_threads={len(active)}")
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

write_collect_report() {
  local baseline="$1"
  local report="$OUTPUT_DIR/summary.md"
  local task_class
  task_class="$(classify_snapshot "$baseline")"
  {
    echo "# PR Repair Evaluation"
    echo
    echo "- Run ID: \`$RUN_ID\`"
    echo "- Repo: \`$REPO\`"
    echo "- PR: \`#$PR\`"
    echo "- Mode: collect-only"
    echo "- Candidate class: \`$task_class\`"
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
  python3 - "$baseline" "$final" "$submission" "$task_detail" "$RUN_ID" "$REPO" "$PR" "$SERVER_URL" "$timed_out" "$WAIT_SECS" "$MAX_ROUNDS" "$MAX_TURNS" "$MAX_BUDGET_USD" "$TIMEOUT_SECS" <<'PY' > "$report"
import json
import sys

(
    baseline_path,
    final_path,
    submission_path,
    task_detail_path,
    run_id,
    repo,
    pr_number,
    server_url,
    timed_out_arg,
    wait_secs,
    max_rounds,
    max_turns,
    max_budget,
    timeout_secs,
) = sys.argv[1:]

def load(path):
    try:
        with open(path, "r", encoding="utf-8") as fh:
            return json.load(fh)
    except Exception:
        return {}

def facts(pr):
    threads = (((pr.get("reviewThreads") or {}).get("nodes")) or [])
    active = [
        t for t in threads
        if not t.get("isResolved", False) and not t.get("isOutdated", False)
    ]
    return {
        "head": pr.get("headRefOid"),
        "merge": pr.get("mergeStateStatus") or "UNKNOWN",
        "checks": (pr.get("statusCheckRollup") or {}).get("state") or "UNKNOWN",
        "unresolved": len(active),
    }

baseline = load(baseline_path)
final = load(final_path)
submission = load(submission_path)
task_detail = load(task_detail_path)
before = facts(baseline)
after = facts(final)
head_changed = before["head"] != after["head"]
task_status = task_detail.get("status") or task_detail.get("workflow", {}).get("state") or submission.get("status") or "unknown"
workflow_id = submission.get("workflow_id") or task_detail.get("workflow", {}).get("id")
task_id = submission.get("task_id") or task_detail.get("id")
timed_out = timed_out_arg == "1"

if before["unresolved"] > 0:
    candidate = "review_feedback_repair"
elif before["checks"] != "SUCCESS":
    candidate = "ci_repair"
elif before["merge"] == "CLEAN" and before["checks"] == "SUCCESS":
    candidate = "ready_noop"
else:
    candidate = "mergeability_repair"

grade = "C"
blockers = []
if after["checks"] != "SUCCESS":
    blockers.append(f"final checks are {after['checks']}")
if after["unresolved"] > 0:
    blockers.append(f"{after['unresolved']} active unresolved review threads remain")
if candidate in ("review_feedback_repair", "ci_repair") and not head_changed:
    blockers.append("PR head did not change for a repair candidate")
if candidate == "mergeability_repair" and after["merge"] != "CLEAN":
    blockers.append(f"final mergeStateStatus is {after['merge']}, not CLEAN")
if task_status in ("failed", "cancelled", "blocked"):
    blockers.append(f"Harness task ended as {task_status}")
if timed_out or task_status in ("unknown", "pending", "running", "implementing", "reviewing"):
    blockers.append("Harness task did not reach a terminal state before the evaluation timeout")

if not blockers:
    grade = "A" if candidate != "ready_noop" or not head_changed else "B"
elif after["checks"] == "SUCCESS" and after["unresolved"] == 0:
    grade = "B"
elif head_changed or after["checks"] == "SUCCESS" or after["unresolved"] < before["unresolved"]:
    grade = "C"
elif task_status in ("failed", "cancelled", "blocked"):
    grade = "D"
else:
    grade = "F"

print("# PR Repair Evaluation")
print()
print(f"- Run ID: `{run_id}`")
print(f"- Repo: `{repo}`")
print(f"- PR: `#{pr_number}`")
print(f"- Server URL: `{server_url}`")
print(f"- Candidate class: `{candidate}`")
print(f"- Task ID: `{task_id}`")
print(f"- Workflow ID: `{workflow_id}`")
print(f"- Task status: `{task_status}`")
print(f"- Timed out: `{str(timed_out).lower()}`")
print(f"- Grade: `{grade}`")
print()
print("## Eval Bounds")
print()
print("| Field | Value |")
print("|---|---|")
print(f"| `wait_secs` | `{wait_secs}` |")
print(f"| `max_rounds` | `{max_rounds}` |")
print(f"| `max_turns` | `{max_turns}` |")
print(f"| `max_budget_usd` | `{max_budget or 'not set'}` |")
print(f"| `timeout_secs` | `{timeout_secs}` |")
print()
print("## Baseline vs Final")
print()
print("| Field | Baseline | Final |")
print("|---|---|---|")
print(f"| `headRefOid` | `{before['head']}` | `{after['head']}` |")
print(f"| `mergeStateStatus` | `{before['merge']}` | `{after['merge']}` |")
print(f"| `statusCheckRollup.state` | `{before['checks']}` | `{after['checks']}` |")
print(f"| active unresolved review threads | `{before['unresolved']}` | `{after['unresolved']}` |")
print(f"| head changed | `false` | `{str(head_changed).lower()}` |")
print()
print("## Blockers")
print()
if blockers:
    for blocker in blockers:
        print(f"- {blocker}")
else:
    print("- None")
PY
}

BASELINE_JSON="$OUTPUT_DIR/baseline_pr.json"
FINAL_JSON="$OUTPUT_DIR/final_pr.json"
SUBMISSION_JSON="$OUTPUT_DIR/submission.json"
TASK_DETAIL_JSON="$OUTPUT_DIR/task_detail_final.json"
TASK_BODY_JSON="$OUTPUT_DIR/task_body.json"
HEALTH_JSON="$OUTPUT_DIR/health.json"

echo "Collecting baseline for $REPO#$PR"
collect_snapshot "$BASELINE_JSON"
snapshot_summary "$BASELINE_JSON"
write_collect_report "$BASELINE_JSON"

if [[ "$COLLECT_ONLY" -eq 1 ]]; then
  echo "Collect-only report: $OUTPUT_DIR/summary.md"
  exit 0
fi

curl_args=(-fsS --max-time 5)
if [[ -n "${HARNESS_API_TOKEN:-}" ]]; then
  curl_args+=(-H "Authorization: Bearer ${HARNESS_API_TOKEN}")
fi

if ! curl "${curl_args[@]}" "$SERVER_URL/health" > "$HEALTH_JSON"; then
  echo "Harness server is not reachable at $SERVER_URL; baseline was saved to $OUTPUT_DIR" >&2
  exit 3
fi

python3 - "$PROJECT_ROOT" "$REPO" "$PR" "$WAIT_SECS" "$MAX_ROUNDS" "$MAX_TURNS" "$MAX_BUDGET_USD" <<'PY' > "$TASK_BODY_JSON"
import json
import sys

project_root, repo, pr, wait_secs, max_rounds, max_turns, max_budget = sys.argv[1:]
body = {
    "project": project_root,
    "repo": repo,
    "source": "pr_repair_eval",
    "external_id": f"pr-repair-eval:{repo}#{pr}",
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
print(json.dumps(body, indent=2))
PY

echo "Submitting Harness PR repair task"
curl "${curl_args[@]}" \
  -X POST "$SERVER_URL/tasks" \
  -H "Content-Type: application/json" \
  --data @"$TASK_BODY_JSON" > "$SUBMISSION_JSON"

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
  echo "POST /tasks did not return task_id; see $SUBMISSION_JSON" >&2
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
collect_snapshot "$FINAL_JSON"
snapshot_summary "$FINAL_JSON"
write_final_report "$BASELINE_JSON" "$FINAL_JSON" "$SUBMISSION_JSON" "$TASK_DETAIL_JSON" "$timed_out"
echo "Evaluation report: $OUTPUT_DIR/summary.md"
