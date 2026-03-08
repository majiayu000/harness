#!/bin/bash
# Self-repair: submit audit issues to harness for automated fix
# Usage: ./scripts/self-repair.sh [start_issue] [end_issue]

set -euo pipefail

HARNESS_URL="${HARNESS_URL:-http://127.0.0.1:9800}"
WAIT_SECS=30
MAX_ROUNDS=3
TURN_TIMEOUT=1800
POLL_INTERVAL=30

# Issues to fix (in dependency order: critical first, then independent)
ISSUES=(82 83 84 85 86 87 88 89 90 91 92 93 94 95)

START=${1:-0}
END=${2:-${#ISSUES[@]}}

echo "=== Harness Self-Repair ==="
echo "Server: $HARNESS_URL"
echo "Issues: ${ISSUES[*]:$START:$((END-START))}"
echo ""

for i in $(seq "$START" "$((END-1))"); do
    ISSUE=${ISSUES[$i]}
    echo ">>> Submitting issue #$ISSUE ($(($i+1))/$END)"

    RESPONSE=$(curl -s -X POST "$HARNESS_URL/tasks" \
        -H 'Content-Type: application/json' \
        -d "{\"issue\": $ISSUE, \"wait_secs\": $WAIT_SECS, \"max_rounds\": $MAX_ROUNDS, \"turn_timeout_secs\": $TURN_TIMEOUT}")

    TASK_ID=$(echo "$RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin)['task_id'])")
    echo "    task_id=$TASK_ID"

    # Poll until terminal state
    while true; do
        TASK=$(curl -s "$HARNESS_URL/tasks/$TASK_ID")
        STATUS=$(echo "$TASK" | python3 -c "import sys,json; print(json.load(sys.stdin)['status'])")
        TURN=$(echo "$TASK" | python3 -c "import sys,json; print(json.load(sys.stdin)['turn'])")
        ROUNDS=$(echo "$TASK" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('rounds',[])))")
        PR=$(echo "$TASK" | python3 -c "import sys,json; print(json.load(sys.stdin).get('pr_url','none'))")

        echo "    [$(date +%H:%M:%S)] status=$STATUS turn=$TURN rounds=$ROUNDS pr=$PR"

        if [ "$STATUS" = "done" ] || [ "$STATUS" = "failed" ]; then
            if [ "$STATUS" = "failed" ]; then
                ERROR=$(echo "$TASK" | python3 -c "import sys,json; e=json.load(sys.stdin).get('error',''); print(e[:120] if e else 'unknown')")
                echo "    ERROR: $ERROR"
            fi
            break
        fi
        sleep $POLL_INTERVAL
    done

    echo "    Result: status=$STATUS pr=$PR"
    echo ""
done

echo "=== Self-Repair Complete ==="
