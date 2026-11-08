# agent-runtime backend (trial)

Branch `agent-runtime-backend`, local only. Trial 2026-11-08 → 2026-11-22.

An opt-in backend that runs Claude, Codex and OpenCode through the
[agent-runtime](https://github.com/majiayu000/agent-runtime) library instead of
the native adapters in `crates/harness-agents`. The native adapters are untouched
and stay the default; the new backend replaces them only for the agent names you
list.

## Turn it on

Build with the feature (the dependency is a path to a local agent-runtime
checkout at `../runtime` during the trial; switch it to a git dependency before
pushing):

```bash
cargo build -p harness-cli --features agent-runtime
```

Add to `harness.toml`:

```toml
[agents.runtime_backend]
enabled_for = ["claude", "codex", "opencode"]
# optional:
# agent_ids = { claude = "claude" }           # agent-runtime agent id per harness name
# channels  = { opencode = "opencode-serve" } # agent-runtime --via
# modes     = { codex = "on-request/workspace-write" }
# config_files = ["/path/to/agent-runtime-overrides.toml"]
# trial_log = "/path/to/agent-runtime-trial.jsonl"
```

Remove the list (or leave it empty) to go back. A harness built without the
feature refuses to start when the list is non-empty instead of silently using
the native adapters.

All entry points (`exec`, `serve`, `gc`, the MCP server) get it, because the
switch lives in `harness_agents::builder::registry_from_config`. For the listed
names the turn and control backends are cleared, so `execute`, `execute_stream`
and runtime turns all go through agent-runtime.

## What changes for a call

| | Native adapters | agent-runtime backend |
|---|---|---|
| Success | per adapter | only `TurnOutcome::Completed`; `unknown` (e.g. `end_turn` with no output), `failed`, `cancelled` are errors |
| Approvals | `--dangerously-skip-permissions` / `--allowedTools` | each request answered "allow once" if the request's tools are in `allowed_tools` (or unrestricted), otherwise rejected; never "always"; unrecognised requests are rejected |
| Codex sandbox | from `sandbox_mode` | `on-request/<sandbox_mode>` |
| User config | per adapter | Claude starts with `--setting-sources project --strict-mcp-config`; Codex with hooks, memories, plugins, apps off |
| Process cleanup | per adapter | agent-runtime's process-group and leftover checks; the leftover count is logged |
| Not carried over | | prompt layers (the flattened prompt is sent), `max_budget_usd`, container isolation / egress proxy, contract capabilities (the backend claims none) |

## Trial log

One JSON line per call at `<data dir>/harness/agent-runtime-trial.jsonl`
(macOS: `~/Library/Application Support/harness/`). Fields: `ts_ms`,
`harness_agent`, `runtime_agent`, `channel`, `mode`, `model`, `outcome`,
`failure_class`, `saw_output`, `reported_success`, `suspect_false_success`,
`leftovers`, `close_graceful`, `cost_usd`, `input_tokens`, `output_tokens`,
`approvals_asked`, `approvals_granted`, `duration_ms`, `error`, `harness_version`.
No prompt or output is written.

```bash
scripts/agent-runtime-trial-stats.py                 # whole log
scripts/agent-runtime-trial-stats.py --since 2026-11-15
```

`suspect_false_success` is `true` when the call returned success without a
`completed` outcome, or `completed` with no output. Any `true` fails the trial.

## Line count

Counted by `scripts/agent-runtime-line-count.py`: physical lines and code lines
(non-blank, not a `//` comment) of non-test sources, with a trailing
`#[cfg(test)] mod tests { … }` block removed.

- Before: the native adapter files for the three agents (`claude*.rs`,
  `codex*.rs` incl. `codex_adapter/`, `opencode*.rs` incl. `opencode_adapter/`).
  Shared infrastructure also used by Cursor (`spawn_supervisor`, `spawn_contract`,
  `process_cleanup`, `streaming`, `output_parsing`, ...) is reported separately
  and not counted as "before".
- After: `agent_runtime_backend.rs` and its `approval.rs`, `trial.rs`, plus the
  config struct, the builder hook and `AgentRegistry::replace`.

The agent-runtime library itself is a dependency, not counted; its size is the
price of the smaller integration and is part of the maintenance criterion.

## Smoke

```bash
HARNESS_BIN=target/debug/harness WORK=/tmp/harness-smoke scripts/agent-runtime-backend-smoke.sh claude codex opencode
```

One real write task per agent through `harness exec` (Claude haiku, Codex
gpt-5.6-luna, OpenCode free model in an isolated HOME).
