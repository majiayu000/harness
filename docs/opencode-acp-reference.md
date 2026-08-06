# OpenCode ACP Reference

> Verified against `opencode 1.18.14` (2026-08-06) unless noted.

OpenCode integration reference for the `opencode` runtime kind. Two surfaces
are wired:

- **Exec backend** (`OpenCodeAgent`): `opencode run --format json` — used by
  `harness exec --agent opencode` and the legacy `CodeAgent` path.
- **ACP adapter** (`OpenCodeAcpAdapter`): `opencode acp` — JSON-RPC over stdio
  implementing the stable Agent Client Protocol v1; used by the workflow
  runtime for streaming turns with approval and cancellation.

## Configuration

```toml
[agents.opencode]
cli_path = "opencode"          # or an absolute path
default_model = ""             # empty = opencode's own configured default
```

`default_model` accepts any `provider/model` identifier opencode knows
(e.g. `anthropic/claude-sonnet-4`, `openai/gpt-5.4`).

Dispatch via `runtime_kind: "opencode"` in workflow runtime dispatch policy,
or `agent: "opencode"` in prompt execution policies.

## `opencode run --format json` event schema

| type | part | mapping |
|---|---|---|
| `step_start` | — | ignored |
| `text` | `text` | `StreamItem::MessageDelta` / response output |
| `tool_use` | `state.input.command`, `state.output` | `Item::ShellCommand` |
| `step_finish` | `reason`, `tokens{input,output,total}`, `cost` | `TokenUsage`; `reason != "stop"` is an error |

Exit code from the process; stdout is a JSONL stream of these events.

## `opencode acp` handshake (ACP v1)

1. Client -> `initialize` with **`protocolVersion: 1`** (number) and
   `clientInfo`. Omitting `protocolVersion` returns `-32602 Invalid params`.
2. Client -> `notifications/initialized`.
3. Client -> `session/new` with **`cwd`** and **`mcpServers: []`** (array).
   Omitting `mcpServers` returns `-32602`. Response `sessionId` +
   `configOptions`. Pin the model with
   `configOptions: [{"id":"model","value":"provider/model"}]`.
4. Client -> `session/prompt` (`sessionId` + `prompt` text blocks).
   Agent streams `session/update` notifications; the turn ends when the
   original `session/prompt` request receives its response
   (`stopReason`: `end_turn`, `cancelled`, `refusal`, `max_tokens`,
   `max_turn_requests`).

## Event mapping (session/update -> AgentEvent)

| sessionUpdate | AgentEvent |
|---|---|
| `agent_message_chunk` | `MessageDelta` |
| `agent_thought_chunk` | ignored |
| `tool_call` | `ToolCall` (name=title, input=toolCallId) |
| `tool_call_update` (`in_progress`) | `ItemStarted` |
| `tool_call_update` (`completed`/`error`) | `ItemCompleted` |
| `usage_update` | `TokenUsage` (input=used, total=size, cost) |
| `available_commands_update` | ignored |

Permissions arrive as a `session/request_permission` **JSON-RPC request**
(the client must respond with its id). Harness surfaces it as
`AgentEvent::ApprovalRequest` (id = request id as string, command = prompt)
and responds `{"outcome":"approved"}` / `{"outcome":"rejected","reason":...}`.

Cancellation: `session/cancel` notification; the agent then answers the
pending `session/prompt` with `stopReason: "cancelled"`.

## Capabilities and limitations

- `steer` is unsupported (ACP v1 has no steer method) — the adapter returns
  `Unsupported`.
- `reasoning_effort` is not mapped (no ACP v1 option); opencode model variants
  can be selected via the model id.
- Approval policy (`approval_policy` in runtime profiles) is rejected for
  `opencode`, matching the Claude/Anthropic policy contract.
- exec backend maps `allowed_tools` to the `OPENCODE_PERMISSION` env var
  (inlined JSON, e.g. `{"bash":"allow"}`); an empty list maps to
  `{"*":"deny"}`. No allowlist (`None`) maps to `--auto`.

## Platform notes

- **macOS seatbelt**: the OpenCode binary (Bun runtime) crashes with
  `SIGTRAP` during startup under the Seatbelt `workspace-write` sandbox,
  matching Claude Code's behavior. Use
  `--sandbox-mode danger-full-access` on macOS, or rely on Linux
  landlock/bubblewrap which work with the default sandbox.
- OpenCode reads `OPENCODE` / `OPENCODE_PID` environment variables as nested
  session markers. When Harness itself runs inside an OpenCode session, the
  child OpenCode inherits them; this is harmless for `opencode run` /
  `opencode acp` and does not need stripping.
