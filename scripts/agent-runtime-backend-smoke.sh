#!/bin/bash
# harness exec through the agent-runtime backend: one real write task per agent.
# Usage: HARNESS_BIN=<harness built with --features agent-runtime> WORK=<dir> \
#        scripts/agent-runtime-backend-smoke.sh claude codex opencode
# OpenCode uses its free model in an isolated HOME (OC_HOME). Claude and Codex use
# the logged-in CLIs with small models; each run is one short task.
set -u
: "${HARNESS_BIN:?}" "${WORK:?}"
OC_BIN="${OC_BIN:-$HOME/.opencode/bin/opencode}"
OC_HOME="${OC_HOME:-$WORK/opencode-home}"
mkdir -p "$WORK" "$OC_HOME"
cat > "$WORK/agent-runtime-overlay.toml" <<TOML
[[agents]]
id = "opencode"
[[agents.channels]]
id = "opencode-serve"
[agents.channels.launch]
program = "$OC_BIN"
[agents.channels.launch.env]
set = { HOME = "$OC_HOME", XDG_CONFIG_HOME = "$OC_HOME/.config", XDG_DATA_HOME = "$OC_HOME/.local/share", XDG_STATE_HOME = "$OC_HOME/.local/state", XDG_CACHE_HOME = "$OC_HOME/.cache", OPENCODE_CONFIG_CONTENT = '{"model":"opencode/big-pickle","permission":{"edit":"ask","bash":"ask"}}' }
TOML
# Start from the repository's example config, then route three agents through agent-runtime.
REPO="$(cd "$(dirname "$0")/.." && pwd)"
cp "$REPO/config/default.toml.example" "$WORK/harness.toml"
cat >> "$WORK/harness.toml" <<TOML

[agents.runtime_backend]
enabled_for = ["claude", "codex", "opencode"]
config_files = ["$WORK/agent-runtime-overlay.toml"]
TOML
model_for() { case "$1" in claude) echo haiku;; codex) echo "${CODEX_MODEL:-gpt-5.6-luna}";; *) echo "";; esac; }
for agent in "$@"; do
  project="$WORK/project-$agent-$$"; mkdir -p "$project"; git -C "$project" init -q
  echo "[$agent]"
  model="$(model_for "$agent")"
  start=$(date +%s)
  perl -e 'alarm 300; exec @ARGV' "$HARNESS_BIN" --config "$WORK/harness.toml" exec --project "$project" --agent "$agent" ${model:+--model "$model"} \
    "Create a file named hello.txt in the current directory containing exactly the text OK. Use your file writing tool. Then reply DONE." \
    > "$WORK/$agent.out" 2> "$WORK/$agent.err"
  code=$?
  echo "  exit=$code secs=$(( $(date +%s) - start )) output=$(tr '\n' ' ' < "$WORK/$agent.out" | cut -c1-120)"
  echo "  hello.txt: $(cat "$project/hello.txt" 2>/dev/null || echo MISSING)"
  grep -v 'preserving unmanaged' "$WORK/$agent.err" | grep -iE 'error|agent-runtime' | tail -2 | cut -c1-300 | sed 's/^/  stderr: /'
done
