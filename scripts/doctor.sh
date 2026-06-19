#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
    cat <<'EOF'
Usage:
  scripts/doctor.sh [options]

Options:
  --config PATH   Harness config file to inspect. Defaults to HARNESS_CONFIG,
                  config/default.toml, config/claude.toml, then user config.
  --http-addr ADDR
                  HTTP bind address to inspect. Defaults to HARNESS_HTTP_ADDR,
                  server.http_addr, then 127.0.0.1:9800.
  --dry-run       Run non-mutating checks and always exit 0 after reporting.
  -h, --help      Show this help.

The doctor does not start services, build binaries, create config files, or
write credentials. It reports whether the documented startup path is ready.
EOF
}

CONFIG_PATH="${HARNESS_CONFIG:-}"
HTTP_ADDR="${HARNESS_HTTP_ADDR:-}"
DRY_RUN=0
FAILURES=0
WARNINGS=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)
            if [ -z "${2:-}" ]; then
                echo "--config requires a value" >&2
                exit 2
            fi
            CONFIG_PATH="$2"
            shift 2
            ;;
        --http-addr)
            if [ -z "${2:-}" ]; then
                echo "--http-addr requires a value" >&2
                exit 2
            fi
            HTTP_ADDR="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h | --help)
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

resolve_config_path() {
    if [ -n "$CONFIG_PATH" ]; then
        if [ ! -f "$CONFIG_PATH" ]; then
            fail "config file not found: $CONFIG_PATH"
            CONFIG_PATH=""
            return 0
        fi
        return 0
    fi

    if [ -f "config/default.toml" ]; then
        CONFIG_PATH="config/default.toml"
    elif [ -f "config/claude.toml" ]; then
        CONFIG_PATH="config/claude.toml"
    elif CONFIG_PATH="$(discover_config_path)"; then
        :
    else
        CONFIG_PATH=""
    fi
}

config_value() {
    local section="$1"
    local key="$2"

    [ -n "$CONFIG_PATH" ] || return 1

    awk -v section="$section" -v key="$key" '
        /^[[:space:]]*\[/ {
            name = $0
            sub(/^[[:space:]]*\[/, "", name)
            sub(/\][[:space:]]*$/, "", name)
            sub(/^[[:space:]]+/, "", name)
            sub(/[[:space:]]+$/, "", name)
            in_section = (name == section)
            next
        }
        in_section {
            idx = index($0, "=")
            if (idx == 0) {
                next
            }
            name = substr($0, 1, idx - 1)
            sub(/^[[:space:]]+/, "", name)
            sub(/[[:space:]]+$/, "", name)
            if (name == key) {
                value = substr($0, idx + 1)
                sub(/^[[:space:]]*/, "", value)
                sub(/[[:space:]]*#.*/, "", value)
                sub(/[[:space:]]*$/, "", value)
                if (value ~ /^".*"$/ || value ~ /^\047.*\047$/) {
                    value = substr(value, 2, length(value) - 2)
                }
                print value
                exit
            }
        }
    ' "$CONFIG_PATH"
}

config_server_value() {
    config_value "server" "$1"
}

config_intake_github_value() {
    config_value "intake.github" "$1"
}

github_intake_mode() {
    local mode
    mode="$(config_intake_github_value "mode" || true)"
    printf '%s\n' "${mode:-poll}"
}

github_intake_poller_enabled() {
    case "$1" in
        poll | hybrid | both) return 0 ;;
        *) return 1 ;;
    esac
}

status_ok() {
    printf 'OK: %s\n' "$1"
}

warn() {
    WARNINGS=$((WARNINGS + 1))
    printf 'WARN: %s\n' "$1"
}

fail() {
    FAILURES=$((FAILURES + 1))
    printf 'FAIL: %s\n' "$1"
}

redact_database_url() {
    local url="$1"
    case "$url" in
        postgres://*@*) printf 'postgres://***@%s\n' "${url#*@}" ;;
        postgresql://*@*) printf 'postgresql://***@%s\n' "${url#*@}" ;;
        *) printf '%s\n' "$url" ;;
    esac
}

database_host_port() {
    local url="$1"
    local authority
    local host
    local port
    local rest

    case "$url" in
        postgres://*) rest="${url#postgres://}" ;;
        postgresql://*) rest="${url#postgresql://}" ;;
        *) return 1 ;;
    esac

    authority="${rest%%/*}"
    authority="${authority%%\?*}"
    authority="${authority##*@}"
    if [[ "$authority" == \[*\]* ]]; then
        host="${authority%%]*}"
        host="${host#[}"
        rest="${authority#*]}"
        if [[ "$rest" == :* ]]; then
            port="${rest#:}"
        else
            port="5432"
        fi
    else
        host="${authority%%:*}"
        if [[ "$authority" == *:* ]]; then
            port="${authority#*:}"
        else
            port="5432"
        fi
    fi
    port="${port%%\?*}"
    [ -n "$host" ] || return 1
    [ -n "$port" ] || port="5432"
    printf '%s %s\n' "$host" "$port"
}

check_release_binary() {
    if [ -x "./target/release/harness" ]; then
        status_ok "release binary is executable: ./target/release/harness"
        return 0
    fi

    if [ -e "./target/release/harness" ]; then
        fail "release binary exists but is not executable: ./target/release/harness"
        return 0
    fi

    if command -v cargo >/dev/null 2>&1; then
        warn "release binary is missing; ./start-server.sh will run cargo build --release -p harness-cli before starting"
    else
        fail "release binary is missing and cargo is not available to build it"
    fi
}

resolve_database_url() {
    local configured

    if [ -n "${HARNESS_DATABASE_URL:-}" ]; then
        DATABASE_URL="$HARNESS_DATABASE_URL"
        DATABASE_SOURCE="HARNESS_DATABASE_URL"
        return 0
    fi

    configured="$(config_server_value "database_url" || true)"
    if [ -n "$configured" ]; then
        DATABASE_URL="$configured"
        DATABASE_SOURCE="server.database_url in $CONFIG_PATH"
        return 0
    fi

    DATABASE_URL="postgres://harness:harness@localhost:5432/harness"
    DATABASE_SOURCE="./start-server.sh local Postgres fallback"
}

check_database() {
    local host_port
    local host
    local port

    resolve_database_url
    status_ok "database URL resolved from $DATABASE_SOURCE: $(redact_database_url "$DATABASE_URL")"

    if command -v pg_isready >/dev/null 2>&1; then
        if pg_isready -d "$DATABASE_URL" -t 2 >/dev/null 2>&1; then
            status_ok "Postgres is reachable with pg_isready"
        else
            fail "Postgres is not reachable with pg_isready; start it with scripts/dev-db.sh or fix the configured database URL"
        fi
        return 0
    fi

    if command -v nc >/dev/null 2>&1; then
        if host_port="$(database_host_port "$DATABASE_URL")"; then
            host="${host_port% *}"
            port="${host_port#* }"
            if nc -z -w 2 "$host" "$port" >/dev/null 2>&1 ||
                nc -z -G 2 "$host" "$port" >/dev/null 2>&1; then
                status_ok "Postgres TCP endpoint is reachable at $host:$port"
            else
                fail "Postgres TCP endpoint is not reachable at $host:$port; start it with scripts/dev-db.sh or fix the configured database URL"
            fi
        else
            warn "database URL could not be parsed for TCP reachability; install pg_isready for an exact Postgres check"
        fi
        return 0
    fi

    warn "pg_isready and nc are unavailable; database reachability could not be verified"
}

resolve_http_addr() {
    local configured

    if [ -n "$HTTP_ADDR" ]; then
        return 0
    fi

    configured="$(config_server_value "http_addr" || true)"
    HTTP_ADDR="${configured:-127.0.0.1:9800}"
}

is_local_bind_host() {
    case "$1" in
        "127."* | "::1") return 0 ;;
        *) return 1 ;;
    esac
}

is_valid_port() {
    local port="$1"
    [[ "$port" =~ ^[0-9]+$ ]] && [ "$port" -le 65535 ]
}

is_valid_ipv4_literal() {
    local host="$1"
    local a
    local b
    local c
    local d
    local extra
    local part

    IFS=. read -r a b c d extra <<EOF
$host
EOF
    [ -z "${extra:-}" ] || return 1
    for part in "$a" "$b" "$c" "$d"; do
        [[ "$part" =~ ^[0-9]+$ ]] || return 1
        [ "$part" -le 255 ] || return 1
    done
}

is_valid_ip_literal_host() {
    local host="$1"

    if command -v python3 >/dev/null 2>&1; then
        python3 - "$host" <<'PY' >/dev/null 2>&1
import ipaddress
import sys

try:
    ipaddress.ip_address(sys.argv[1])
except ValueError:
    sys.exit(1)
PY
        return $?
    fi

    if [[ "$host" == *.* ]]; then
        is_valid_ipv4_literal "$host"
        return $?
    fi

    if [[ "$host" == *:* && "$host" =~ ^[0-9A-Fa-f:.]+$ ]]; then
        return 0
    fi
    return 1
}

http_addr_host_port() {
    local addr="$1"
    local host
    local rest
    local port

    if [[ "$addr" == \[* ]]; then
        [[ "$addr" == *\]* ]] || return 1
        host="${addr%%]*}"
        host="${host#[}"
        rest="${addr#*]}"
        [[ "$rest" == :* ]] || return 1
        port="${rest#:}"
        [[ "$host" == *:* ]] || return 1
    else
        [[ "$addr" == *:* ]] || return 1
        [[ "$addr" != *:*:* ]] || return 1
        host="${addr%:*}"
        port="${addr##*:}"
    fi

    is_valid_port "$port" || return 1
    is_valid_ip_literal_host "$host" || return 1
    printf '%s %s\n' "$host" "$port"
}

config_has_non_empty_server_array() {
    local key="$1"

    [ -n "$CONFIG_PATH" ] || return 1

    awk -v key="$key" '
        /^[[:space:]]*\[/ {
            section = $0
            sub(/^[[:space:]]*\[/, "", section)
            sub(/\][[:space:]]*$/, "", section)
            sub(/^[[:space:]]+/, "", section)
            sub(/[[:space:]]+$/, "", section)
            in_server = (section == "server")
            in_array = 0
            next
        }
        !in_server {
            next
        }
        in_array {
            value = $0
            sub(/[[:space:]]*#.*/, "", value)
            sub(/^[[:space:]]+/, "", value)
            sub(/[[:space:]]+$/, "", value)
            if (value ~ /^\]/) {
                exit 1
            }
            if (value != "") {
                found_array_value = 1
                in_array = 0
                exit 0
            }
            next
        }
        {
            idx = index($0, "=")
            if (idx == 0) {
                next
            }
            name = substr($0, 1, idx - 1)
            sub(/^[[:space:]]+/, "", name)
            sub(/[[:space:]]+$/, "", name)
            if (name != key) {
                next
            }
            value = substr($0, idx + 1)
            sub(/[[:space:]]*#.*/, "", value)
            sub(/^[[:space:]]+/, "", value)
            sub(/[[:space:]]+$/, "", value)
            if (value == "" || value == "[]") {
                exit 1
            }
            if (value == "[") {
                in_array = 1
                next
            }
            if (value ~ /^\[/) {
                gsub(/[\[\]]/, "", value)
                gsub(/,/, "", value)
                sub(/^[[:space:]]+/, "", value)
                sub(/[[:space:]]+$/, "", value)
                exit(value == "" ? 1 : 0)
            }
            exit 0
        }
        END {
            if (in_array && !found_array_value) {
                exit 1
            }
        }
    ' "$CONFIG_PATH"
}

check_http_exposure() {
    local host
    local host_port
    local api_token

    resolve_http_addr
    if ! host_port="$(http_addr_host_port "$HTTP_ADDR")"; then
        fail "HTTP bind address must be a valid IP literal SocketAddr, not a hostname or malformed address: $HTTP_ADDR"
        return 0
    fi
    host="${host_port% *}"

    if is_local_bind_host "$host"; then
        status_ok "HTTP bind address is local: $HTTP_ADDR"
        return 0
    fi

    api_token="${HARNESS_API_TOKEN:-}"
    if [ -z "$api_token" ]; then
        api_token="$(config_server_value "api_token" || true)"
    fi

    if [ -z "$api_token" ]; then
        fail "non-local HTTP bind $HTTP_ADDR requires HARNESS_API_TOKEN or server.api_token before serving"
    else
        status_ok "API token is configured for non-local HTTP bind $HTTP_ADDR"
    fi

    if config_has_non_empty_server_array "allowed_project_roots"; then
        status_ok "allowed_project_roots is configured for non-local project registration"
    else
        fail "non-local HTTP bind $HTTP_ADDR should configure server.allowed_project_roots to constrain project registration"
    fi
}

check_port() {
    local host_port
    local port
    local pids

    resolve_http_addr
    if ! host_port="$(http_addr_host_port "$HTTP_ADDR")"; then
        fail "HTTP address must be a valid IP literal SocketAddr: $HTTP_ADDR"
        return 0
    fi
    port="${host_port#* }"

    if ! command -v lsof >/dev/null 2>&1; then
        warn "lsof is unavailable; port occupancy for $HTTP_ADDR could not be verified"
        return 0
    fi

    pids="$(lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null | awk '!seen[$0]++' || true)"
    if [ -z "$pids" ]; then
        status_ok "port $port is available for $HTTP_ADDR"
        return 0
    fi

    fail "port $port already has a listener; stop it or change HARNESS_HTTP_ADDR/server.http_addr"
    ps -p "$(printf '%s' "$pids" | paste -sd, -)" -o pid=,comm=,command= || true
}

command_exists_or_executable() {
    local path="$1"
    if [[ "$path" == */* ]]; then
        [ -x "$path" ]
    else
        command -v "$path" >/dev/null 2>&1
    fi
}

check_agent_cli() {
    local codex_cli
    local claude_cli
    local found=0

    codex_cli="$(config_value "agents.codex" "cli_path" || true)"
    claude_cli="$(config_value "agents.claude" "cli_path" || true)"
    codex_cli="${codex_cli:-codex}"
    claude_cli="${claude_cli:-claude}"

    if command_exists_or_executable "$codex_cli"; then
        status_ok "Codex CLI is available: $codex_cli"
        found=1
    else
        warn "Codex CLI is not available on PATH/configured path: $codex_cli"
    fi

    if command_exists_or_executable "$claude_cli"; then
        status_ok "Claude CLI is available: $claude_cli"
        found=1
    else
        warn "Claude CLI is not available on PATH/configured path: $claude_cli"
    fi

    if [ "$found" -eq 0 ] && [ -z "${ANTHROPIC_API_KEY:-}" ]; then
        fail "no common agent CLI was found and ANTHROPIC_API_KEY is not set; autonomous execution will not be able to run"
    elif [ "$found" -eq 0 ]; then
        status_ok "ANTHROPIC_API_KEY is set, so the Anthropic API adapter can be used without local agent CLIs"
    fi
}

check_github_token() {
    local configured

    if [ -n "${GITHUB_TOKEN:-}" ] || [ -n "${GH_TOKEN:-}" ]; then
        status_ok "GitHub token is available from GITHUB_TOKEN/GH_TOKEN"
        return 0
    fi

    configured="$(config_server_value "github_token" || true)"
    if [ -n "$configured" ]; then
        status_ok "GitHub token is configured in server.github_token"
        return 0
    fi

    if command -v gh >/dev/null 2>&1 && gh auth token >/dev/null 2>&1; then
        status_ok "GitHub token is discoverable from gh auth"
        return 0
    fi

    warn "GitHub token was not found; issue/PR automation needs gh auth login, GITHUB_TOKEN, GH_TOKEN, or server.github_token"
}

workflow_enabled_value() {
    local section="$1"

    [ -f "WORKFLOW.md" ] || return 1

    awk -v section="$section" '
        $0 ~ "^" section ":" { in_section = 1; next }
        in_section && $0 ~ /^[[:alnum:]_]+:/ { in_section = 0 }
        in_section && $0 ~ /^[[:space:]]+enabled:[[:space:]]*/ {
            value = $0
            sub(/^[[:space:]]+enabled:[[:space:]]*/, "", value)
            sub(/[[:space:]]*#.*/, "", value)
            sub(/[[:space:]]*$/, "", value)
            print value
            exit
        }
    ' WORKFLOW.md
}

check_workflow_runtime_flags() {
    local section
    local value
    local mode
    local missing=0

    if [ ! -f "WORKFLOW.md" ]; then
        warn "WORKFLOW.md was not found; GitHub issue intake cannot prove runtime backlog ownership from this checkout"
        return 0
    fi

    mode="$(github_intake_mode)"

    for section in repo_backlog runtime_dispatch runtime_worker; do
        if [ "$section" = "repo_backlog" ] && ! github_intake_poller_enabled "$mode"; then
            status_ok "GitHub intake mode '$mode' does not require repo_backlog polling"
            continue
        fi

        value="$(workflow_enabled_value "$section" || true)"
        if [ "$value" = "true" ]; then
            status_ok "WORKFLOW.md enables $section"
        else
            fail "WORKFLOW.md must set $section.enabled = true for autonomous GitHub issue intake and execution"
            missing=1
        fi
    done

    if [ "$missing" -eq 0 ]; then
        status_ok "workflow runtime flags are ready for GitHub issue intake handoff"
    fi
}

check_webhook_secret() {
    local mode
    local configured

    mode="$(config_intake_github_value "mode" || true)"
    mode="${mode:-poll}"

    case "$mode" in
        webhook | hybrid | both)
            ;;
        *)
            status_ok "GitHub intake mode does not require a webhook secret: $mode"
            return 0
            ;;
    esac

    if [ -n "${GITHUB_WEBHOOK_SECRET:-}" ]; then
        status_ok "GitHub webhook secret is available from GITHUB_WEBHOOK_SECRET"
        return 0
    fi

    configured="$(config_server_value "github_webhook_secret" || true)"
    if [ -n "$configured" ]; then
        status_ok "GitHub webhook secret is configured in server.github_webhook_secret"
        return 0
    fi

    fail "GitHub intake mode '$mode' requires GITHUB_WEBHOOK_SECRET or server.github_webhook_secret before receiving webhooks"
}

main() {
    echo "Harness startup doctor"
    resolve_config_path
    if [ -n "$CONFIG_PATH" ]; then
        status_ok "config file selected: $CONFIG_PATH"
    else
        warn "no config file selected; startup will use built-in defaults plus ./start-server.sh local Postgres fallback"
    fi

    check_database
    check_release_binary
    check_github_token
    check_webhook_secret
    check_agent_cli
    check_workflow_runtime_flags
    check_port
    check_http_exposure

    echo "summary: failures=$FAILURES warnings=$WARNINGS"
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "dry run: no services were started, no binaries were built, and no files were written"
        exit 0
    fi

    if [ "$FAILURES" -ne 0 ]; then
        exit 1
    fi
}

main "$@"
