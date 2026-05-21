#!/bin/bash
set -euo pipefail

cd "$(dirname "$0")"

CONFIG_PATH="${HARNESS_CONFIG:-}"
CONFIG_ARGS=()

is_absolute_path() {
    case "$1" in
        /*) return 0 ;;
        *) return 1 ;;
    esac
}

discover_config_path() {
    local candidate

    if [ -n "${XDG_CONFIG_HOME:-}" ] && is_absolute_path "$XDG_CONFIG_HOME"; then
        candidate="$XDG_CONFIG_HOME/harness/config.toml"
        if [ -f "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    fi

    if [ -n "${HOME:-}" ] && is_absolute_path "$HOME"; then
        candidate="$HOME/.config/harness/config.toml"
        if [ -f "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi

        if [ "$(uname -s 2>/dev/null || true)" = "Darwin" ]; then
            candidate="$HOME/Library/Application Support/harness/config.toml"
            if [ -f "$candidate" ]; then
                printf '%s\n' "$candidate"
                return 0
            fi
        fi
    fi

    return 1
}

if [ -n "$CONFIG_PATH" ]; then
    if [ ! -f "$CONFIG_PATH" ]; then
        echo "ERROR: Harness config not found: $CONFIG_PATH" >&2
        exit 1
    fi
    CONFIG_ARGS=(--config "$CONFIG_PATH")
elif [ -f "config/default.toml" ]; then
    CONFIG_PATH="config/default.toml"
    CONFIG_ARGS=(--config "$CONFIG_PATH")
elif [ -f "config/claude.toml" ]; then
    CONFIG_PATH="config/claude.toml"
    CONFIG_ARGS=(--config "$CONFIG_PATH")
elif CONFIG_PATH="$(discover_config_path)"; then
    CONFIG_ARGS=(--config "$CONFIG_PATH")
    echo "Using discovered Harness config: $CONFIG_PATH" >&2
else
    CONFIG_PATH=""
    echo "No local config file found; using built-in defaults plus local Postgres defaults." >&2
fi

config_server_value() {
    local key="$1"

    awk -F= -v key="$key" '
        $0 ~ /^\[server\]/ { in_server = 1; next }
        $0 ~ /^\[/ { in_server = 0 }
        in_server && $1 ~ "^[[:space:]]*" key "[[:space:]]*$" {
            value = $2
            sub(/^[[:space:]]*/, "", value)
            sub(/[[:space:]]*#.*/, "", value)
            sub(/[[:space:]]*$/, "", value)
            if (value ~ /^".*"$/ || value ~ /^\047.*\047$/) {
                value = substr(value, 2, length(value) - 2)
            }
            print value
            exit
        }
    ' "$CONFIG_PATH"
}

config_http_addr() {
    config_server_value "http_addr"
}

config_database_url() {
    config_server_value "database_url"
}

BIND_ADDR="${HARNESS_HTTP_ADDR:-}"
if [ -z "$BIND_ADDR" ] && [ -n "$CONFIG_PATH" ]; then
    BIND_ADDR="$(config_http_addr)"
fi
BIND_ADDR="${BIND_ADDR:-127.0.0.1:9800}"
BIND_PORT="${BIND_ADDR##*:}"
BIND_HOST="${BIND_ADDR%:*}"
BIND_HOST="${BIND_HOST#[}"
BIND_HOST="${BIND_HOST%]}"

listener_name_matches_bind() {
    local name="$1"
    local listener_host

    listener_host="${name%:*}"
    listener_host="${listener_host#[}"
    listener_host="${listener_host%]}"

    case "$BIND_HOST" in
        "" | "*" | "0.0.0.0" | "::")
            return 0
            ;;
    esac

    case "$listener_host" in
        "*" | "0.0.0.0" | "::")
            return 0
            ;;
    esac

    [ "$listener_host" = "$BIND_HOST" ]
}

listener_pids() {
    local line
    local lsof_output
    local lsof_status=0
    local name
    local pid

    if ! command -v lsof >/dev/null 2>&1; then
        if [ "${HARNESS_RESTART:-}" = "1" ]; then
            echo "ERROR: lsof is required to restart Harness on $BIND_ADDR." >&2
            exit 1
        fi
        echo "WARNING: lsof is unavailable; skipping listener preflight for $BIND_ADDR." >&2
        return 0
    fi

    lsof_output="$(lsof -nP -iTCP:"$BIND_PORT" -sTCP:LISTEN 2>&1)" || lsof_status=$?
    if [ "$lsof_status" -ne 0 ] &&
        ! printf '%s\n' "$lsof_output" |
            awk '$0 !~ /^COMMAND/ && $2 ~ /^[0-9]+$/ && $0 ~ / TCP / && $0 ~ /\(LISTEN\)/ { found = 1 } END { exit found ? 0 : 1 }'; then
        if [ -n "$lsof_output" ]; then
            echo "WARNING: lsof could not complete listener preflight for $BIND_ADDR; continuing without listener results:" >&2
            printf '%s\n' "$lsof_output" >&2
        fi
        return 0
    fi

    while IFS= read -r line; do
        [ -n "$line" ] || continue
        [[ "$line" == COMMAND* ]] && continue
        [[ "$line" == *" TCP "* ]] || continue
        [[ "$line" == *"(LISTEN)"* ]] || continue
        set -- $line
        pid="${2:-}"
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        name="${line##* TCP }"
        name="${name% (LISTEN)}"
        if listener_name_matches_bind "$name"; then
            printf '%s\n' "$pid"
        fi
    done <<< "$lsof_output" | awk '!seen[$0]++'
}

health_check_addr() {
    case "$BIND_HOST" in
        "" | "*" | "0.0.0.0")
            printf '127.0.0.1:%s\n' "$BIND_PORT"
            ;;
        "::")
            printf '[::1]:%s\n' "$BIND_PORT"
            ;;
        *)
            printf '%s\n' "$BIND_ADDR"
            ;;
    esac
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
    [[ "$command" =~ (^|[[:space:]/])harness([[:space:]]|$) ]] &&
        [[ "$command" =~ (^|[[:space:]])serve([[:space:]]|$) ]]
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

    for pid in $pids; do
        if ! is_harness_listener "$pid"; then
            echo "ERROR: port $BIND_PORT is in use by a non-Harness process:" >&2
            print_listener_table "$pids"
            exit 1
        fi
    done

    echo "Harness server is already running on $BIND_ADDR:" >&2
    print_listener_table "$pids"
    health_url="http://$(health_check_addr)/health"
    if command -v curl >/dev/null 2>&1; then
        if curl -fsS --max-time 2 "$health_url" >/dev/null 2>&1; then
            echo "Health check OK: $health_url" >&2
        else
            echo "ERROR: existing Harness server did not pass health check: $health_url" >&2
            echo "To restart explicitly, run:" >&2
            echo "  HARNESS_RESTART=1 ./start-server.sh" >&2
            exit 1
        fi
    else
        echo "WARNING: curl is unavailable; skipping health check for existing Harness server at $health_url." >&2
    fi
    echo "No new server was started. To restart explicitly, run:" >&2
    echo "  HARNESS_RESTART=1 ./start-server.sh" >&2
    exit 0
}

ensure_bind_port_available

uses_local_postgres() {
    if [ -n "${HARNESS_DATABASE_URL:-}" ]; then
        return 1
    fi

    if [ -n "$CONFIG_PATH" ] && [ -n "$(config_database_url)" ]; then
        return 1
    fi

    return 0
}

if uses_local_postgres; then
    if docker compose version >/dev/null 2>&1; then
        echo "Starting Postgres and waiting until healthy..."
        docker compose -f docker-compose.yml up -d --wait postgres
    else
        echo "WARNING: Docker Compose v2 not available; assuming Postgres is already running." >&2
    fi

    export HARNESS_DATABASE_URL="postgres://harness:harness@localhost:5432/harness"
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

exec ./target/release/harness serve "${CONFIG_ARGS[@]}" --transport http --project-root "$(pwd)"
