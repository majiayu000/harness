#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/collect_webhook_acceptance_evidence.sh [options]

Collect real-environment GitHub webhook + Linear transition evidence for FUT webhook acceptance.

Options:
  --repo <owner/repo>          GitHub repository (default: majiayu000/harness)
  --team-key <TEAM>            Linear team key (default: FUT)
  --webhook-id <id>            Explicit GitHub webhook id (default: auto-detect pull_request_review webhook)
  --delivery-limit <n>         Number of latest deliveries to keep (default: 50)
  --canary <IDENTIFIER>        Linear issue identifier to snapshot (default: FUT-103)
  --output-dir <path>          Output directory (default: docs/webhook-validation/evidence-<utc-ts>)
  --help                       Show this help

Requirements:
  - gh auth with repo scope
  - LINEAR_API_KEY in environment
  - jq + curl
USAGE
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: missing required command: $1" >&2
    exit 1
  fi
}

REPO="majiayu000/harness"
TEAM_KEY="FUT"
WEBHOOK_ID=""
DELIVERY_LIMIT=50
CANARY_IDENTIFIER="FUT-103"
UTC_TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT_DIR="docs/webhook-validation/evidence-${UTC_TS}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      REPO="$2"
      shift 2
      ;;
    --team-key)
      TEAM_KEY="$2"
      shift 2
      ;;
    --webhook-id)
      WEBHOOK_ID="$2"
      shift 2
      ;;
    --delivery-limit)
      DELIVERY_LIMIT="$2"
      shift 2
      ;;
    --canary)
      CANARY_IDENTIFIER="$2"
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

require_cmd gh
require_cmd jq
require_cmd curl

if [[ -z "${LINEAR_API_KEY:-}" ]]; then
  echo "error: LINEAR_API_KEY is required" >&2
  exit 1
fi

mkdir -p "$OUTPUT_DIR"

HOSTNAME="$(hostname)"
WORKDIR="$(pwd)"
HEAD_SHA="$(git rev-parse --short HEAD)"

HOOKS_JSON="$OUTPUT_DIR/github-hooks.json"
DELIVERIES_JSON="$OUTPUT_DIR/github-deliveries.json"
DELIVERIES_STATUS_JSON="$OUTPUT_DIR/github-delivery-status-counts.json"
DELIVERY_DUP_GUIDS_JSON="$OUTPUT_DIR/github-duplicate-guids.json"
LINEAR_TRANSITIONS_JSONL="$OUTPUT_DIR/linear-human-review-transitions.jsonl"
LINEAR_HR_REWORK_JSONL="$OUTPUT_DIR/linear-hr-to-rework.jsonl"
LINEAR_HR_MERGING_JSONL="$OUTPUT_DIR/linear-hr-to-merging.jsonl"
LINEAR_CANARY_JSON="$OUTPUT_DIR/linear-canary-state.json"
SUMMARY_MD="$OUTPUT_DIR/summary.md"

# 1) GitHub webhook config + delivery snapshot

gh api "repos/$REPO/hooks" > "$HOOKS_JSON"

if [[ -z "$WEBHOOK_ID" ]]; then
  WEBHOOK_ID="$(jq -r '.[] | select(.events | index("pull_request_review")) | .id' "$HOOKS_JSON" | head -n1)"
fi

if [[ -z "$WEBHOOK_ID" || "$WEBHOOK_ID" == "null" ]]; then
  echo "error: failed to locate pull_request_review webhook id for $REPO" >&2
  exit 1
fi

gh api "repos/$REPO/hooks/$WEBHOOK_ID/deliveries?per_page=100" \
  | jq --argjson limit "$DELIVERY_LIMIT" '.[0:$limit]' > "$DELIVERIES_JSON"

jq 'group_by(.status_code) | map({status_code: (.[0].status_code // "null"), count: length, status: (.[0].status // "")})' \
  "$DELIVERIES_JSON" > "$DELIVERIES_STATUS_JSON"

jq 'group_by(.guid)
    | map(select(length > 1)
      | {
          guid: .[0].guid,
          attempts: length,
          deliveries: map({id, delivered_at, status_code, status, redelivery})
        }
    )' "$DELIVERIES_JSON" > "$DELIVERY_DUP_GUIDS_JSON"

# 2) Linear transition snapshot for Human Review lineage

: > "$LINEAR_TRANSITIONS_JSONL"

read -r -d '' LINEAR_QUERY <<'GQL' || true
query FindTransitions($teamKey: String!, $after: String) {
  issues(
    filter: { team: { key: { eq: $teamKey } } }
    first: 50
    after: $after
    orderBy: updatedAt
  ) {
    pageInfo {
      hasNextPage
      endCursor
    }
    nodes {
      identifier
      title
      url
      state { name type }
      history(last: 200) {
        nodes {
          createdAt
          fromState { name type }
          toState { name type }
          actor { name }
          botActor { name }
        }
      }
    }
  }
}
GQL

after="null"
while :; do
  if [[ "$after" == "null" ]]; then
    vars="$(jq -cn --arg teamKey "$TEAM_KEY" '{teamKey:$teamKey}')"
  else
    vars="$(jq -cn --arg teamKey "$TEAM_KEY" --arg after "$after" '{teamKey:$teamKey, after:$after}')"
  fi

  payload="$(jq -cn --arg query "$LINEAR_QUERY" --argjson variables "$vars" '{query:$query, variables:$variables}')"

  resp="$(curl -sS https://api.linear.app/graphql \
    -H "Authorization: $LINEAR_API_KEY" \
    -H 'Content-Type: application/json' \
    --data "$payload")"

  echo "$resp" | jq -e '.errors | not' >/dev/null

  echo "$resp" | jq -rc '.data.issues.nodes[] as $i
    | $i.history.nodes[]
    | select(.fromState.name == "Human Review" and .toState.name != null)
    | {
        identifier: $i.identifier,
        title: $i.title,
        issue_url: $i.url,
        createdAt,
        from: .fromState.name,
        to: .toState.name,
        actor: (.actor.name // .botActor.name // "unknown")
      }' >> "$LINEAR_TRANSITIONS_JSONL"

  has_next="$(echo "$resp" | jq -r '.data.issues.pageInfo.hasNextPage')"
  end_cursor="$(echo "$resp" | jq -r '.data.issues.pageInfo.endCursor')"

  if [[ "$has_next" != "true" || -z "$end_cursor" || "$end_cursor" == "null" ]]; then
    break
  fi

  after="$end_cursor"
done

jq -rc 'select(.to == "Rework")' "$LINEAR_TRANSITIONS_JSONL" > "$LINEAR_HR_REWORK_JSONL" || true
jq -rc 'select(.to == "Merging")' "$LINEAR_TRANSITIONS_JSONL" > "$LINEAR_HR_MERGING_JSONL" || true

# 3) Canary issue state snapshot

if [[ "$CANARY_IDENTIFIER" =~ ^[A-Za-z]+-([0-9]+)$ ]]; then
  ISSUE_NUMBER="${BASH_REMATCH[1]}"
  read -r -d '' CANARY_QUERY <<'GQL' || true
query CanaryState($teamKey: String!, $number: Float!) {
  issues(filter: { team: { key: { eq: $teamKey } }, number: { eq: $number } }, first: 1) {
    nodes {
      id
      identifier
      title
      url
      state { id name type }
      history(last: 50) {
        nodes {
          createdAt
          fromState { name type }
          toState { name type }
          actor { name }
          botActor { name }
        }
      }
    }
  }
}
GQL

  canary_vars="$(jq -cn --arg teamKey "$TEAM_KEY" --argjson number "$ISSUE_NUMBER" '{teamKey:$teamKey, number:$number}')"
  canary_payload="$(jq -cn --arg query "$CANARY_QUERY" --argjson variables "$canary_vars" '{query:$query, variables:$variables}')"

  curl -sS https://api.linear.app/graphql \
    -H "Authorization: $LINEAR_API_KEY" \
    -H 'Content-Type: application/json' \
    --data "$canary_payload" > "$LINEAR_CANARY_JSON"
else
  jq -n --arg canary "$CANARY_IDENTIFIER" '{warning: "canary identifier format not supported", canary: $canary}' > "$LINEAR_CANARY_JSON"
fi

# 4) Human-readable summary

LATEST_DELIVERY="$(jq '.[0] // {}' "$DELIVERIES_JSON")"
LATEST_STATUS_CODE="$(echo "$LATEST_DELIVERY" | jq -r '.status_code // "null"')"
LATEST_STATUS_TEXT="$(echo "$LATEST_DELIVERY" | jq -r '.status // "unknown"')"
LATEST_DELIVERY_TIME="$(echo "$LATEST_DELIVERY" | jq -r '.delivered_at // "n/a"')"

HR_REWORK_COUNT="$(wc -l < "$LINEAR_HR_REWORK_JSONL" | tr -d ' ')"
HR_MERGING_COUNT="$(wc -l < "$LINEAR_HR_MERGING_JSONL" | tr -d ' ')"
DUP_GUID_COUNT="$(jq 'length' "$DELIVERY_DUP_GUIDS_JSON")"

{
  echo "# Webhook Acceptance Evidence"
  echo
  echo "Generated (UTC): $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo
  echo '```text'
  echo "${HOSTNAME}:${WORKDIR}@${HEAD_SHA}"
  echo '```'
  echo
  printf -- '- Repo: `%s`\n' "$REPO"
  printf -- '- Team: `%s`\n' "$TEAM_KEY"
  printf -- '- Webhook ID: `%s`\n' "$WEBHOOK_ID"
  printf -- '- Canary issue: `%s`\n' "$CANARY_IDENTIFIER"
  echo
  echo "## GitHub Delivery Snapshot"
  echo
  printf -- '- Latest delivery at: `%s`\n' "$LATEST_DELIVERY_TIME"
  printf -- '- Latest status: `%s %s`\n' "$LATEST_STATUS_CODE" "$LATEST_STATUS_TEXT"
  printf -- '- Duplicate GUID groups in sampled deliveries: `%s`\n' "$DUP_GUID_COUNT"
  echo
  echo "## Linear Transition Snapshot"
  echo
  printf -- '- Human Review -> Rework count: `%s`\n' "$HR_REWORK_COUNT"
  printf -- '- Human Review -> Merging count: `%s`\n' "$HR_MERGING_COUNT"
  echo
  echo "## Acceptance Gate Signals"
  echo
  if [[ "$HR_REWORK_COUNT" -gt 0 ]]; then
    echo "- [x] Historical sample exists for \`Human Review -> Rework\`."
  else
    echo "- [ ] No sample found for \`Human Review -> Rework\`."
  fi

  if [[ "$HR_MERGING_COUNT" -gt 0 ]]; then
    echo "- [x] Historical sample exists for \`Human Review -> Merging\`."
  else
    echo "- [ ] No sample found for \`Human Review -> Merging\`."
  fi

  if [[ "$LATEST_STATUS_CODE" == "202" ]]; then
    echo "- [x] Latest webhook callback accepted by endpoint (202)."
  else
    echo "- [ ] Latest webhook callback not accepted (non-202)."
  fi

  echo
  echo "## Evidence Files"
  echo
  echo "- \`github-hooks.json\`"
  echo "- \`github-deliveries.json\`"
  echo "- \`github-delivery-status-counts.json\`"
  echo "- \`github-duplicate-guids.json\`"
  echo "- \`linear-human-review-transitions.jsonl\`"
  echo "- \`linear-hr-to-rework.jsonl\`"
  echo "- \`linear-hr-to-merging.jsonl\`"
  echo "- \`linear-canary-state.json\`"
} > "$SUMMARY_MD"

echo "evidence written to: $OUTPUT_DIR"
