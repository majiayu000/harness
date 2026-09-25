# OpenCode ACP Runtime Integration Spec

- Status: Proposed
- Date: 2026-08-06
- Scope: workflow runtime + agents + config
- Related: GH-1876

## 1. Problem

Harness supports `CodexExec`, `CodexJsonrpc`, `ClaudeCode`, `AnthropicApi`, and
`RemoteHost` runtimes. OpenCode (a provider-agnostic coding agent with broad
model support) is not available, so workflows cannot route turns to OpenCode
with full streaming, interrupt, steer, and approval semantics.

OpenCode 1.18 ships `opencode acp`, a stable **Agent Client Protocol (ACP)**
server speaking JSON-RPC over stdio — the same transport shape as `codex
app-server`, which Harness already integrates. This spec adds an OpenCode
runtime through an ACP adapter plus an exec-mode backend for CLI use.

## 2. Goals

- New `RuntimeKind::OpenCode` (`"opencode"`) routable in workflow runtime and
  project configs.
- `OpenCodeAcpAdapter` implementing `AgentAdapter` (start_turn / interrupt /
  respond_approval) over ACP v1, with parity to `CodexAdapter` lifecycle
  (spawn, initialize, stall timeout, reset on failure).
- `OpenCodeExecAgent` implementing `CodeAgent` over `opencode run --format
  json` so `harness exec --agent opencode` works without the server.
- Config section `[agents.opencode]` (cli_path, default_model).
- Protocol behavior pinned by a reference doc.

Non-goals: RemoteHost-style integration with `opencode serve` (private HTTP
protocol mismatch), ACP v2 (draft), steer (ACP v1 has no steer method),
reasoning-effort mapping (no ACP v1 equivalent).

## 3. Verified Protocol Facts

Empirically probed against `opencode 1.18.14` on 2026-08-06:

- `initialize` **requires** `protocolVersion: 1` (number). Response carries
  `agentCapabilities` (loadSession, mcpCapabilities, promptCapabilities,
  sessionCapabilities).
- After initialize, client sends `notifications/initialized`.
- `session/new` **requires** `cwd` and `mcpServers: []` (array). Returns
  `sessionId` and `configOptions` (model select etc.). Model can be pinned via
  `configOptions: [{"id":"model","value":"provider/model"}]`.
- `session/prompt` (id + sessionId + prompt text blocks) streams
  `session/update` notifications:
  - `agent_message_chunk` -> text delta
  - `agent_thought_chunk` -> thinking (ignore)
  - `tool_call` (toolCallId/title/kind/status) and `tool_call_update`
    (in_progress/completed)
  - `usage_update` (used/size/cost)
  - `available_commands_update` (ignore)
- Turn ends with the `session/prompt` **response**: `stopReason` (`end_turn`,
  `cancelled`, `refusal`, ...) plus `usage` counters.
- Permissions arrive as a `session/request_permission` JSON-RPC **request**
  (client must respond with id + result). Observed response
  `{"outcome":"approved"}` is accepted by the agent.
- Cancellation is a `session/cancel` notification (documented; not probed).

## 4. Design

### 4.1 RuntimeKind

`crates/harness-workflow/src/runtime/model.rs`:

```rust
pub enum RuntimeKind {
    CodexExec,
    CodexJsonrpc,
    ClaudeCode,
    AnthropicApi,
    RemoteHost,
    OpenCode, // new; as_str() -> "opencode"
}
```

### 4.2 Config

`crates/harness-core/src/config/agents.rs`:

```rust
pub struct OpenCodeAgentConfig {
    pub cli_path: PathBuf,       // default "opencode"
    pub default_model: String,   // default "" (use opencode's own config)
}
```

`AgentsConfig` gains `pub opencode: OpenCodeAgentConfig` (required field,
matching claude/codex/anthropic_api style).

### 4.3 OpenCodeExecAgent (CodeAgent)

`crates/harness-agents/src/opencode.rs`, modeled on `codex.rs`:

- Spawn `opencode run --format json [--model <m>] <prompt>` in the project dir
  through `spawn_contract::prepare_agent_spawn` + `spawn_supervisor`.
- Parse nd-JSON event lines; reuse `parse_codex_token_usage`-style mapping for
  usage events; collect message deltas into the final output.
- `--auto` only when `uses_dangerously_skip_permissions()` (mirrors codex).
- Registered as `"opencode"` backend so the ACP adapter can attach.

### 4.4 OpenCodeAcpAdapter (AgentAdapter)

`crates/harness-agents/src/opencode_adapter.rs`, modeled on `codex_adapter.rs`
(no protocol.rs split — parse functions live in the same file):

- State: child, stdin, stdout lines, next_id, session_id, active_request_id.
- Spawn: `opencode acp --cwd <workspace>` via
  `spawn_contract::prepare_agent_spawn` (forward_stdin: true), same sandbox and
  supervisor wiring as `CodexAdapter::ensure_child`.
- Handshake: `initialize` (protocolVersion 1, clientInfo harness/<version>) ->
  wait for matching response -> `notifications/initialized` -> `session/new`
  (cwd, `mcpServers: []`, configOptions model when set) -> capture sessionId.
- `start_turn`: effective request (model default), ensure child, send
  `session/prompt`, stream events:
  - `agent_message_chunk` -> `AgentEvent::MessageDelta`
  - `tool_call` -> `AgentEvent::ToolCall { name: title, input: {toolCallId,
    kind} }`; `tool_call_update` status transitions map to
    `ItemStarted`/`ItemCompleted` on in_progress/completed
  - `usage_update` -> `AgentEvent::TokenUsage` (input = used, total = size)
  - `session/request_permission` (a request, not notification) -> send
    `AgentEvent::ApprovalRequest { id: <request id as string>, command: title
    or prompt }`, remember pending response id; do NOT answer until
    `respond_approval`
  - matching `session/prompt` response -> `AgentEvent::TurnCompleted { output:
    accumulated text }`
  - unknown updates ignored; stall timeout applies per read like codex.
- `interrupt`: send `session/cancel` notification for the session.
- `steer`: return `Unsupported` (ACP v1 has no steer).
- `respond_approval`: reply to the remembered request id with
  `{"outcome":"approved"}` / `{"outcome":"rejected","reason":...}`.
- `name()` -> "opencode".

Approval id collision risk: ids are JSON-RPC numeric request ids; harness
`ApprovalRequest.id` is a String. Use `request_id_string()` (numeric to string)
and parse back — same approach as `CodexAdapter::respond_approval`.

### 4.5 Closed-set match sites (must all update)

| Site | Change |
|---|---|
| `runtime/model.rs:458` enum + `as_str` | add variant |
| `harness-server/src/workflow_runtime_worker/runtime_profile.rs:9` `agent_name_for_runtime_kind` | `=> Ok("opencode")` |
| same file `resolve_model` (:140) | `agents.opencode.default_model` |
| same file `resolve_reasoning_effort` (:161) | `None` (no ACP equivalent) |
| same file approval resolution (:190) | explicit policy rejected, like AnthropicApi/RemoteHost |
| `harness-server/src/http/background/runtime_profiles.rs` `runtime_kind_from_config` (:3) | `"opencode" => Some(RuntimeKind::OpenCode)` |
| same file `runtime_profile_from_kind` (:14) | `opencode-default` profile with model |
| same file `runtime_profile_from_agent` (:39) | `"opencode" => ...` |
| `harness-server/src/handlers/usage_monitor_active.rs:90` | `RuntimeKind::OpenCode => "opencode"` |
| `harness-agents/src/builder.rs` `registry_from_config` | register `opencode` backend + adapter factory (ExecuteTurns) |
| `harness-core/src/config/agents.rs` | `AgentsConfig` + `OpenCodeAgentConfig` |

`runtime_kinds_share_agent` needs no special case (`left == right` covers it).

### 4.6 Reference doc

`docs/opencode-acp-reference.md`: protocol handshake, event schema table, known
behavior of `opencode acp` 1.18.14, and divergence notes from the probe.

## 5. Affected Files

- `crates/harness-workflow/src/runtime/model.rs`
- `crates/harness-core/src/config/agents.rs`
- `crates/harness-agents/src/opencode.rs` (new)
- `crates/harness-agents/src/opencode_adapter.rs` (new)
- `crates/harness-agents/src/opencode_adapter_tests.rs` (new)
- `crates/harness-agents/src/lib.rs` (module exports)
- `crates/harness-agents/src/builder.rs`
- `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs`
- `crates/harness-server/src/http/background/runtime_profiles.rs`
- `crates/harness-server/src/handlers/usage_monitor_active.rs`
- `docs/opencode-acp-reference.md` (new)
- `harness.toml.example` (document `[agents.opencode]`)

## 6. Verification

- `cargo check -p harness-agents --all-targets`
- `cargo test -p harness-agents` (adapter parser + builder registry tests)
- `cargo test -p harness-workflow runtime::model` (enum round-trip)
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual: `harness exec --agent opencode "..."` and a workflow runtime
  submission pinned to `runtime_kind: "opencode"` against `opencode acp`.

## 7. Risks / Open Questions

- `session/request_permission` params schema varies by implementation; probe
  showed no permission request in a permissive environment. Command text for
  `ApprovalRequest` may be a best-effort title. (low)
- `opencode acp` cold boot includes provider/model list fetch; first `session/prompt` may be slow. Stall timeout is on lines, so safe. (low)
- `opencode run` exec backend output format must be pinned in the reference doc
  during implementation (probe pending). (medium, implementation-time)
- If `opencode` binary is missing, spawn failure must classify like codex
  (`classify_missing_workspace_spawn_failure`). (low)
