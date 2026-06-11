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

mkdir -p "$LOG_DIR"
PID_FILE="$LOG_DIR/harness-${PORT}.pid"
LOG_FILE="$LOG_DIR/harness-${PORT}.log"
TMUX_SESSION="harness-${PORT}"

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
  exit 0
fi

if [[ "$STATUS" -eq 1 ]]; then
  pid="$(recorded_pid)"
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
    curl -sS --max-time 2 "$health_url" || true
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

echo "starting harness server on $health_url"
echo "log: $LOG_FILE"
echo "sanitized vars: ${unset_args[*]:-none}"

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
    printf -v tmux_command_args "%q " "${tmux_command[@]}"
    tmux_command_string="cd ${quoted_pwd} && ${tmux_command_args}"
    tmux new-session -d -s "$TMUX_SESSION" "$tmux_command_string"
    echo "started tmux session $TMUX_SESSION"
  fi

  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if command -v curl >/dev/null 2>&1 && curl -sS --max-time 2 "$health_url" >/tmp/harness-health-"$PORT".json 2>/dev/null; then
      listener_pid="$(lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1 || true)"
      if [[ -n "$listener_pid" ]]; then
        echo "$listener_pid" > "$PID_FILE"
        echo "harness server started pid=$listener_pid tmux_session=$TMUX_SESSION"
      else
        echo "harness server health is ready tmux_session=$TMUX_SESSION"
      fi
      cat /tmp/harness-health-"$PORT".json
      echo
      exit 0
    fi
    sleep 1
  done

  echo "tmux session $TMUX_SESSION did not become healthy within 10s" >&2
  echo "check: scripts/start-harness-codex-safe.sh --status --port $PORT" >&2
  echo "inspect: tmux capture-pane -pt $TMUX_SESSION -S -120" >&2
  exit 4
fi

nohup env "${unset_args[@]}" "${cmd[@]}" > "$LOG_FILE" 2>&1 &
server_pid="$!"
echo "$server_pid" > "$PID_FILE"

for _ in 1 2 3 4 5; do
  if ! pid_alive "$server_pid"; then
    echo "harness server exited during startup; last log lines:" >&2
    tail -n 80 "$LOG_FILE" >&2 || true
    rm -f "$PID_FILE"
    exit 4
  fi
  if command -v curl >/dev/null 2>&1 && curl -sS --max-time 2 "$health_url" >/tmp/harness-health-"$PORT".json 2>/dev/null; then
    echo "harness server started pid=$server_pid"
    cat /tmp/harness-health-"$PORT".json
    echo
    exit 0
  fi
  sleep 1
done

echo "harness server pid=$server_pid did not become healthy within 5s" >&2
echo "check: scripts/start-harness-codex-safe.sh --status --port $PORT" >&2
echo "inspect: tail -n 120 $LOG_FILE" >&2
exit 4
