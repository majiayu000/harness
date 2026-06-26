#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/start-harness-codex-safe.sh [options]

Options:
  --port N              HTTP port. Default: 9800
  --project-root PATH   Project root passed to harness serve. If omitted, use config/default logic
  --config PATH         Optional Harness config file
  --bin PATH            Harness binary. Default: ./target/release/harness, then ./target/debug/harness
  --log-dir PATH        Log/PID directory. Default: .harness/local
  --wait-secs N         Seconds to wait for /health in background mode. Default: 60
  --foreground          Run in the foreground instead of backgrounding
  --stop                Stop the PID recorded for this port
  --status              Print recorded PID and health status
  -h, --help            Show this help

This wrapper is safe to run from Codex sessions. It preserves normal shell
environment such as PATH, HOME, GITHUB_TOKEN, and API keys, but removes local
Codex/Claude wrapper variables that can confuse spawned agent subprocesses.
When available, background mode runs the server in a detached tmux session so it
does not depend on the calling tool process lifetime.
EOF
}

require_value() {
  local option="$1"
  local value="${2:-}"
  if [[ -z "$value" ]]; then
    echo "$option requires a value" >&2
    exit 2
  fi
}

PORT=9800
PROJECT_ROOT=""
PROJECT_ROOT_EXPLICIT=0
CONFIG=""
BIN="${HARNESS_BIN:-}"
LOG_DIR=".harness/local"
WAIT_SECS="${HARNESS_STARTER_WAIT_SECS:-60}"
HEALTH_CURL_TIMEOUT_SECS="${HARNESS_STARTER_HEALTH_TIMEOUT_SECS:-2}"
POLL_INTERVAL_SECS="${HARNESS_STARTER_POLL_INTERVAL_SECS:-1}"
FOREGROUND=0
STOP=0
STATUS=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --port)
      require_value "$1" "${2:-}"
      PORT="$2"
      shift 2
      ;;
    --project-root)
      require_value "$1" "${2:-}"
      PROJECT_ROOT="$2"
      PROJECT_ROOT_EXPLICIT=1
      shift 2
      ;;
    --config)
      require_value "$1" "${2:-}"
      CONFIG="$2"
      shift 2
      ;;
    --bin)
      require_value "$1" "${2:-}"
      BIN="$2"
      shift 2
      ;;
    --log-dir)
      require_value "$1" "${2:-}"
      LOG_DIR="$2"
      shift 2
      ;;
    --wait-secs)
      require_value "$1" "${2:-}"
      WAIT_SECS="$2"
      shift 2
      ;;
    --foreground)
      FOREGROUND=1
      shift
      ;;
    --stop)
      STOP=1
      shift
      ;;
    --status)
      STATUS=1
      shift
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

if ! [[ "$PORT" =~ ^[0-9]+$ ]]; then
  echo "--port must be a number" >&2
  exit 2
fi
if ! [[ "$WAIT_SECS" =~ ^[0-9]+$ ]] || [[ "$WAIT_SECS" -lt 1 ]]; then
  echo "--wait-secs must be a positive number" >&2
  exit 2
fi
if ! [[ "$HEALTH_CURL_TIMEOUT_SECS" =~ ^[0-9]+$ ]] || [[ "$HEALTH_CURL_TIMEOUT_SECS" -lt 1 ]]; then
  echo "HARNESS_STARTER_HEALTH_TIMEOUT_SECS must be a positive number" >&2
  exit 2
fi
if ! [[ "$POLL_INTERVAL_SECS" =~ ^[0-9]+$ ]] || [[ "$POLL_INTERVAL_SECS" -lt 1 ]]; then
  echo "HARNESS_STARTER_POLL_INTERVAL_SECS must be a positive number" >&2
  exit 2
fi

mkdir -p "$LOG_DIR"
PID_FILE="$LOG_DIR/harness-${PORT}.pid"
LOG_FILE="$LOG_DIR/harness-${PORT}.log"
STATUS_FILE="$LOG_DIR/harness-${PORT}.status"
HEALTH_FILE="$LOG_DIR/harness-${PORT}.health.json"
HEALTH_ERROR_FILE="$LOG_DIR/harness-${PORT}.health.err"
TMUX_SESSION="harness-${PORT}"

now_utc() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

record_start_status() {
  local state_name="$1"
  shift || true
  local message="${*:-}"
  {
    printf 'updated_at=%s\n' "$(now_utc)"
    printf 'state=%s\n' "$state_name"
    printf 'port=%s\n' "$PORT"
    printf 'health_url=%s\n' "$health_url"
    printf 'pid_file=%s\n' "$PID_FILE"
    printf 'log_file=%s\n' "$LOG_FILE"
    printf 'health_file=%s\n' "$HEALTH_FILE"
    printf 'tmux_session=%s\n' "$TMUX_SESSION"
    printf 'wait_secs=%s\n' "$WAIT_SECS"
    if [[ -n "$message" ]]; then
      printf 'message=%s\n' "$message"
    fi
  } > "$STATUS_FILE"
}

check_health() {
  curl -fsS --max-time "$HEALTH_CURL_TIMEOUT_SECS" "$health_url" \
    > "$HEALTH_FILE" 2> "$HEALTH_ERROR_FILE"
}

print_health_error() {
  if [[ -s "$HEALTH_ERROR_FILE" ]]; then
    echo "last health check error:" >&2
    sed -n '1,20p' "$HEALTH_ERROR_FILE" >&2 || true
  fi
}

print_startup_diagnostics() {
  echo "status: $STATUS_FILE" >&2
  echo "health: $HEALTH_FILE" >&2
  print_health_error
  if [[ -f "$LOG_FILE" ]]; then
    echo "last log lines:" >&2
    tail -n 120 "$LOG_FILE" >&2 || true
  fi
  if command -v tmux >/dev/null 2>&1 && tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
    echo "tmux pane tail:" >&2
    tmux capture-pane -pt "$TMUX_SESSION" -S -80 2>/dev/null >&2 || true
  fi
}

pid_alive() {
  local pid="$1"
  [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1
}

recorded_pid() {
  if [[ -f "$PID_FILE" ]]; then
    tr -d '[:space:]' < "$PID_FILE"
  fi
}

listener_pid_for_port() {
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1 || true
  fi
}

require_listener_check_for_start() {
  if ! command -v lsof >/dev/null 2>&1; then
    echo "refusing to start harness server: lsof is required to verify port $PORT ownership" >&2
    echo "install lsof or verify the port manually before starting from this wrapper" >&2
    exit 4
  fi
}

require_curl_for_background_start() {
  if ! command -v curl >/dev/null 2>&1; then
    echo "refusing to start harness server in background: curl is required for /health readiness checks" >&2
    echo "install curl or run with --foreground to manage readiness yourself" >&2
    exit 4
  fi
}

health_url="http://127.0.0.1:${PORT}/health"

if [[ "$STOP" -eq 1 ]]; then
  pid="$(recorded_pid)"
  if pid_alive "$pid"; then
    kill "$pid"
    echo "stopped harness server pid=$pid port=$PORT"
  else
    echo "no live recorded harness server for port=$PORT"
  fi
  if command -v tmux >/dev/null 2>&1 && tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
    tmux kill-session -t "$TMUX_SESSION"
    echo "stopped tmux session $TMUX_SESSION"
  fi
  rm -f "$PID_FILE"
  record_start_status "stopped" "stop requested"
  exit 0
fi

if [[ "$STATUS" -eq 1 ]]; then
  pid="$(recorded_pid)"
  if [[ -f "$STATUS_FILE" ]]; then
    echo "recorded_status=$STATUS_FILE"
    sed -n '1,40p' "$STATUS_FILE"
  fi
  if pid_alive "$pid"; then
    echo "$pid" > "$PID_FILE"
    echo "pid=$pid status=running log=$LOG_FILE"
  else
    if command -v lsof >/dev/null 2>&1; then
      listener_pid="$(listener_pid_for_port)"
      if [[ -n "$listener_pid" ]]; then
        echo "pid=$listener_pid status=unmanaged_listener log=$LOG_FILE"
      else
        echo "pid=${pid:-none} status=not_running log=$LOG_FILE"
      fi
    else
      echo "pid=${pid:-none} status=unknown listener_check=unavailable log=$LOG_FILE"
    fi
  fi
  if command -v curl >/dev/null 2>&1; then
    if check_health; then
      cat "$HEALTH_FILE"
    else
      echo "health=unavailable url=$health_url"
      print_health_error
    fi
    echo
  fi
  if command -v tmux >/dev/null 2>&1 && tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
    echo "tmux_session=$TMUX_SESSION status=running"
  fi
  exit 0
fi

require_listener_check_for_start

pid="$(recorded_pid)"
if pid_alive "$pid"; then
  echo "harness server already recorded as running pid=$pid port=$PORT"
  exit 0
fi

listener_pid="$(listener_pid_for_port)"
if [[ -n "$listener_pid" ]]; then
  echo "refusing to start harness server: port $PORT already has listener pid=$listener_pid" >&2
  echo "choose another --port or stop the existing process explicitly" >&2
  exit 4
fi

if [[ -z "$BIN" ]]; then
  if [[ -x ./target/release/harness ]]; then
    BIN="./target/release/harness"
  elif [[ -x ./target/debug/harness ]]; then
    BIN="./target/debug/harness"
  else
    echo "no harness binary found; build one first, e.g. cargo build --release -p harness-cli" >&2
    exit 3
  fi
fi
if [[ ! -x "$BIN" ]]; then
  echo "harness binary is not executable: $BIN" >&2
  exit 3
fi

unset_args=()
while IFS='=' read -r name _; do
  case "$name" in
    CODEX*|Codex*)
      unset_args+=("-u" "$name")
      ;;
    CLAUDECODE|CLAUDE_CODE|CLAUDE_CODE_ENTRYPOINT|CLAUDE_CODE_SESSION_ID|CLAUDE_SESSION_ID)
      unset_args+=("-u" "$name")
      ;;
  esac
done < <(env)

cmd=("$BIN")
if [[ -n "$CONFIG" ]]; then
  cmd+=(--config "$CONFIG")
fi
cmd+=(serve --transport http --port "$PORT")
if [[ "$PROJECT_ROOT_EXPLICIT" -eq 1 ]]; then
  cmd+=(--project-root "$PROJECT_ROOT")
fi

if [[ "$FOREGROUND" -eq 0 ]]; then
  require_curl_for_background_start
fi

echo "starting harness server on $health_url"
echo "log: $LOG_FILE"
echo "status: $STATUS_FILE"
echo "health wait: ${WAIT_SECS}s (curl timeout ${HEALTH_CURL_TIMEOUT_SECS}s)"
echo "sanitized vars: ${unset_args[*]:-none}"
record_start_status "starting" "launching server"

if [[ "$FOREGROUND" -eq 1 ]]; then
  exec env "${unset_args[@]}" "${cmd[@]}"
fi

if command -v tmux >/dev/null 2>&1 && [[ -z "${HARNESS_STARTER_NO_TMUX:-}" ]]; then
  if tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
    echo "tmux session already exists: $TMUX_SESSION"
  else
    tmux_command=(
      "HARNESS_STARTER_NO_TMUX=1"
      "$0"
      "--foreground"
      "--port"
      "$PORT"
      "--log-dir"
      "$LOG_DIR"
    )
    if [[ "$PROJECT_ROOT_EXPLICIT" -eq 1 ]]; then
      tmux_command+=("--project-root" "$PROJECT_ROOT")
    fi
    if [[ -n "$CONFIG" ]]; then
      tmux_command+=("--config" "$CONFIG")
    fi
    if [[ -n "$BIN" ]]; then
      tmux_command+=("--bin" "$BIN")
    fi
    printf -v quoted_pwd "%q" "$PWD"
    printf -v quoted_log_file "%q" "$LOG_FILE"
    printf -v tmux_command_args "%q " "${tmux_command[@]}"
    tmux_command_string="cd ${quoted_pwd} && ${tmux_command_args} >> ${quoted_log_file} 2>&1"
    tmux new-session -d -s "$TMUX_SESSION" "$tmux_command_string"
    echo "started tmux session $TMUX_SESSION"
  fi

  start_seconds="$SECONDS"
  next_progress=5
  while [[ $((SECONDS - start_seconds)) -lt "$WAIT_SECS" ]]; do
    if ! tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
      record_start_status "failed" "tmux session exited before health became ready"
      echo "tmux session $TMUX_SESSION exited before /health became ready" >&2
      print_startup_diagnostics
      exit 4
    fi
    if check_health; then
      listener_pid="$(lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1 || true)"
      if [[ -n "$listener_pid" ]]; then
        echo "$listener_pid" > "$PID_FILE"
        echo "harness server started pid=$listener_pid tmux_session=$TMUX_SESSION"
      else
        echo "harness server health is ready tmux_session=$TMUX_SESSION"
      fi
      record_start_status "healthy" "health check succeeded"
      cat "$HEALTH_FILE"
      echo
      exit 0
    fi
    elapsed=$((SECONDS - start_seconds))
    if [[ "$elapsed" -ge "$next_progress" ]]; then
      echo "waiting for harness health (${elapsed}/${WAIT_SECS}s) url=$health_url"
      record_start_status "starting" "waiting for health elapsed=${elapsed}s"
      next_progress=$((next_progress + 5))
    fi
    sleep "$POLL_INTERVAL_SECS"
  done

  record_start_status "timeout" "tmux session did not become healthy within ${WAIT_SECS}s"
  echo "tmux session $TMUX_SESSION did not become healthy within ${WAIT_SECS}s" >&2
  echo "check: scripts/start-harness-codex-safe.sh --status --port $PORT" >&2
  echo "inspect: tail -n 120 $LOG_FILE" >&2
  print_startup_diagnostics
  exit 4
fi

nohup env "${unset_args[@]}" "${cmd[@]}" > "$LOG_FILE" 2>&1 &
server_pid="$!"
echo "$server_pid" > "$PID_FILE"

start_seconds="$SECONDS"
next_progress=5
while [[ $((SECONDS - start_seconds)) -lt "$WAIT_SECS" ]]; do
  if ! pid_alive "$server_pid"; then
    record_start_status "failed" "process exited during startup"
    echo "harness server exited during startup; last log lines:" >&2
    tail -n 80 "$LOG_FILE" >&2 || true
    rm -f "$PID_FILE"
    exit 4
  fi
  if check_health; then
    echo "harness server started pid=$server_pid"
    record_start_status "healthy" "health check succeeded"
    cat "$HEALTH_FILE"
    echo
    exit 0
  fi
  elapsed=$((SECONDS - start_seconds))
  if [[ "$elapsed" -ge "$next_progress" ]]; then
    echo "waiting for harness health (${elapsed}/${WAIT_SECS}s) pid=$server_pid url=$health_url"
    record_start_status "starting" "waiting for health elapsed=${elapsed}s pid=${server_pid}"
    next_progress=$((next_progress + 5))
  fi
  sleep "$POLL_INTERVAL_SECS"
done

record_start_status "timeout" "pid=${server_pid} did not become healthy within ${WAIT_SECS}s"
echo "harness server pid=$server_pid did not become healthy within ${WAIT_SECS}s" >&2
echo "check: scripts/start-harness-codex-safe.sh --status --port $PORT" >&2
echo "inspect: tail -n 120 $LOG_FILE" >&2
print_startup_diagnostics
exit 4
