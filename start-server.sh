#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")"

CONFIG_PATH="${HARNESS_CONFIG:-config/claude.toml}"

if [ ! -f "$CONFIG_PATH" ]; then
    echo "ERROR: Harness config not found: $CONFIG_PATH" >&2
    exit 1
fi

config_http_addr() {
    awk -F= '
        $0 ~ /^\[server\]/ { in_server = 1; next }
        $0 ~ /^\[/ { in_server = 0 }
        in_server && $1 ~ /^[[:space:]]*http_addr[[:space:]]*$/ {
            value = $2
            sub(/^[[:space:]]*/, "", value)
            sub(/[[:space:]]*#.*/, "", value)
            gsub(/"/, "", value)
            print value
            exit
        }
    ' "$CONFIG_PATH"
}

BIND_ADDR="${HARNESS_HTTP_ADDR:-$(config_http_addr)}"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:9800}"
BIND_PORT="${BIND_ADDR##*:}"

listener_pids() {
    lsof -nP -tiTCP:"$BIND_PORT" -sTCP:LISTEN 2>/dev/null || true
}

listener_pid_args() {
    printf "%s" "$1" | paste -sd, -
}

print_listener_table() {
    local pids="$1"
    local pid_args

    pid_args="$(listener_pid_args "$pids")"
    if [ -n "$pid_args" ]; then
        ps -p "$pid_args" -o pid=,comm=,command= >&2 || true
    fi
}

is_harness_listener() {
    local pid="$1"
    local command

    command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
    [[ "$command" == *"harness serve"* ]]
}

ensure_bind_port_available() {
    local pids
    local pid
    local health_url

    pids="$(listener_pids)"
    if [ -z "$pids" ]; then
        return 0
    fi

    if [ "${HARNESS_RESTART:-}" = "1" ]; then
        for pid in $pids; do
            if ! is_harness_listener "$pid"; then
                echo "ERROR: port $BIND_PORT is in use by a non-Harness process:" >&2
                print_listener_table "$pids"
                exit 1
            fi
        done

        echo "Stopping existing Harness server on $BIND_ADDR..."
        for pid in $pids; do
            kill -TERM "$pid"
        done

        for _ in {1..50}; do
            if [ -z "$(listener_pids)" ]; then
                return 0
            fi
            sleep 0.1
        done

        echo "ERROR: existing Harness server did not release $BIND_ADDR after SIGTERM." >&2
        print_listener_table "$(listener_pids)"
        exit 1
    fi

    echo "Harness server is already running on $BIND_ADDR:" >&2
    print_listener_table "$pids"
    health_url="http://$BIND_ADDR/health"
    if curl -fsS --max-time 2 "$health_url" >/dev/null 2>&1; then
        echo "Health check OK: $health_url" >&2
    fi
    echo "No new server was started. To restart explicitly, run:" >&2
    echo "  HARNESS_RESTART=1 ./start-server.sh" >&2
    exit 0
}

ensure_bind_port_available

if docker compose version >/dev/null 2>&1; then
    echo "Starting Postgres and waiting until healthy..."
    docker compose -f docker-compose.yml up -d --wait postgres
else
    echo "WARNING: Docker Compose v2 not available; assuming Postgres is already running." >&2
fi

# Auto-detect GITHUB_TOKEN from gh CLI if not already set
if [ -z "${GITHUB_TOKEN:-}" ]; then
    GITHUB_TOKEN=$(gh auth token 2>/dev/null || true)
    if [ -z "$GITHUB_TOKEN" ]; then
        echo "WARNING: GITHUB_TOKEN not available. Run 'gh auth login' first."
        echo "Review bot auto-trigger will be disabled."
    else
        export GITHUB_TOKEN
        echo "GITHUB_TOKEN loaded from gh CLI"
    fi
fi

exec ./target/release/harness serve --config "$CONFIG_PATH"
