# Agent Loop Design — Claude Code vs Codex Integration Architecture

## Problem

Both Claude Code and Codex already have their own internal agent loops. Harness wrapping them as single subprocess calls loses all observability and control. The question is: what level of integration makes sense for each?

```
Current:
harness → spawn("claude -p 'fix bug'") → [black box] → result
harness → spawn("codex exec 'fix bug'") → [black box] → result
```

## Control Levels

| Level | Capability | Claude Code | Codex |
|-------|-----------|-------------|-------|
| L0 Current | Fire prompt, wait for result | `claude -p` | `codex exec` |
| L1 Streaming | Realtime visibility into each step | `--output-format stream-json` | `codex exec --json` |
| L2 Interruptible | Stop mid-execution | kill process + parse partial output | `turn/interrupt` |
| L3 Approvable | Approve/reject each tool call | Not supported (CLI decides autonomously) | App Server approval flow |
| L4 Full Control | Harness executes tools, manages loop | Anthropic Messages API | OpenAI Responses API |

## Claude Code: Design Options

### Option A: Streaming CLI Parsing (Recommended, L1-L2)

```
harness → spawn("claude --output-format stream-json -p 'fix bug'")
       ← Parse JSONL event stream in realtime:
         {"type": "assistant", "message": "Let me read the file..."}
         {"type": "tool_use", "name": "Read", "input": {"path": "src/main.rs"}}
         {"type": "tool_result", "output": "...file contents..."}
         {"type": "assistant", "message": "Found the bug, fixing..."}
         {"type": "tool_use", "name": "Edit", ...}
```

Pros:
- No agent loop reimplementation — leverages Claude Code's mature tool ecosystem
- Realtime event parsing → push delta notifications to clients
- Process kill enables interruption
- ~300 lines of work

Cons:
- Cannot approve/reject individual tool calls (Claude Code decides internally)
- Limited steering — can only kill and restart with new prompt

### Option B: Anthropic API Direct (L4, Full Control)

```
harness → POST /v1/messages (prompt + tool definitions)
       ← {"type": "tool_use", "name": "read_file", "input": {"path": "src/main.rs"}}
       → harness executes read_file, checks approval policy
       → POST /v1/messages (tool_result + continue)
       ← {"type": "tool_use", "name": "edit_file", ...}
       → harness checks sandbox policy, executes edit
       → POST /v1/messages (tool_result)
       ← {"type": "text", "text": "Done, bug fixed."}
```

Pros:
- Full control over every step
- Approval gates, sandbox enforcement, custom tool implementations
- Can inject context/constraints between iterations

Cons:
- Must reimplement all Claude Code tools (Read/Edit/Bash/Glob/Grep/LSP/Agent/...)
- Massive duplication of effort (~5000+ lines)
- Loses Claude Code's battle-tested tool implementations

### Claude Code Conclusion

**Do A first (streaming parse), B only if approval gates become mandatory.** Claude Code's internal tools are mature — rewriting them adds no value. Streaming parsing covers 80% of needs (observe + interrupt).

## Codex: Design Options

### Option C: App Server Protocol (Recommended, L3-L4)

Codex natively exposes App Server protocol (JSON-RPC 2.0 over stdio), designed specifically for harness-layer integration:

```
harness → spawn("codex" as stdio subprocess)
       → send: {"method": "initialize", "params": {...}}
       ← recv: {"result": {"capabilities": {...}}}
       → send: {"method": "initialized"}

       → send: {"method": "thread/start", "params": {...}}
       ← recv: notification: {"method": "thread/started"}

       → send: {"method": "turn/start", "params": {"text": "fix bug"}}
       ← recv: notification: {"method": "item/started", "params": {"type": "tool_call"}}
       ← recv: request: {"method": "approval/request", "params": {"command": "rm -rf test/"}}
       → send: response: {"result": {"decision": "reject"}}  ← harness can reject!
       ← recv: notification: {"method": "item/completed"}
       ← recv: notification: {"method": "turn/completed"}
```

Pros:
- Full bidirectional control
- Every tool call goes through harness approval
- Native streaming events
- `turn/steer` (redirect mid-turn) and `turn/interrupt` (cancel)
- No tool reimplementation — Codex executes internally, harness only approves
- ~500 lines of work

Cons:
- Requires managing long-lived stdio subprocess
- Protocol versioning concerns as Codex evolves

### Option D: MCP Server Mode (L2-L3)

```
harness → connect to "codex mcp-server"
       → call tool: codex(prompt="fix bug")
       ← result: session_id + output
       → call tool: codex-reply(session_id, "now add tests")
       ← result: continued output
```

Pros:
- Simpler integration model
- Good for harness acting as higher-level orchestrator

Cons:
- Less granular than App Server (no per-tool-call approval)
- Session management overhead

### Codex Conclusion

**Use App Server protocol directly.** It's the interface Codex was designed to expose to harness layers. Our protocol is already JSON-RPC 2.0, so natural alignment.

## Unified Architecture

```
                    ┌─────────────────────────┐
                    │     harness-server       │
                    │   (unified task pipeline) │
                    └────────┬────────────────┘
                             │
                    ┌────────▼────────────┐
                    │   AgentAdapter trait  │
                    └──┬──────────────┬───┘
                       │              │
          ┌────────────▼──┐   ┌──────▼──────────────┐
          │ ClaudeAdapter  │   │   CodexAdapter       │
          │                │   │                      │
          │ L1: stream-json│   │ L3: App Server proto │
          │ L4: API direct │   │ L4: API direct       │
          │                │   │                      │
          │ Parse events   │   │ JSON-RPC bidirectional│
          │ kill = interrupt│  │ approval gate        │
          │ No approval    │   │ turn/steer redirect  │
          └────────────────┘   └──────────────────────┘
```

### Unified AgentAdapter Trait

```rust
pub trait AgentAdapter: Send + Sync {
    /// Start a turn, return event stream
    fn start_turn(&self, req: TurnRequest) -> Pin<Box<dyn Stream<Item = AgentEvent>>>;

    /// Interrupt mid-execution
    async fn interrupt(&self) -> Result<()>;

    /// Steer mid-turn (append instructions)
    async fn steer(&self, text: String) -> Result<()>;

    /// Respond to approval request (Codex uses this, Claude returns Unsupported)
    async fn respond_approval(&self, id: String, decision: ApprovalDecision) -> Result<()>;
}

pub enum AgentEvent {
    TurnStarted,
    ItemStarted { item_type: String },
    MessageDelta { text: String },
    ToolCall { name: String, input: Value },
    ApprovalRequest { id: String, command: String },
    ItemCompleted,
    TurnCompleted { output: String },
    Error { message: String },
}

pub enum ApprovalDecision {
    Accept,
    Reject { reason: String },
    Amend { modified_command: String },
}
```

### Event Flow: Adapter → Server → Client

```
AgentAdapter (stream-json / App Server)
    │
    ▼ AgentEvent
harness-server (task_executor)
    │ - Log to EventStore
    │ - Check interceptors
    │ - Apply approval policy
    │
    ▼ RpcNotification
WebSocket / stdio
    │
    ▼
Client (CLI / Web / IDE)
```

## Implementation Priority

| Step | Effort | Value | Description |
|------|--------|-------|-------------|
| 1. ClaudeAdapter stream-json | ~300 lines | High | Immediate realtime observability |
| 2. CodexAdapter App Server | ~500 lines | High | Full bidirectional control |
| 3. AgentEvent → WebSocket push | ~200 lines | Medium | Clients see events in realtime |
| 4. Anthropic API direct (optional) | ~800 lines | Low | Full Claude control, but duplicates CLI tools |

Step 1 has the best effort/value ratio — parsing `--output-format stream-json` transforms the current black-box into a transparent pipeline.

## Capability Matrix (Post-Implementation)

| Capability | ClaudeAdapter (A) | CodexAdapter (C) | API Direct (B/D) |
|-----------|------------------|------------------|-------------------|
| Realtime visibility | Yes (stream-json) | Yes (notifications) | Yes (streaming API) |
| Interrupt | Yes (kill process) | Yes (turn/interrupt) | Yes (cancel request) |
| Steer | No | Yes (turn/steer) | Yes (append messages) |
| Approve tool calls | No | Yes (approval flow) | Yes (harness executes) |
| Sandbox enforcement | Agent's own | App Server policy | Harness-managed |
| Tool ecosystem | Claude Code built-in | Codex built-in | Must reimplement |
| Session resume | New process each time | thread/resume | Manual state management |

## Decision Record

- **Claude Code: Option A first** — streaming CLI parsing gives 80% value at 20% cost. Option B (API direct) only if per-tool-call approval becomes a hard requirement.
- **Codex: Option C** — App Server protocol is the designed integration point. Natural JSON-RPC alignment with our existing protocol layer.
- **Unified trait** — AgentAdapter abstracts over both, so task_executor doesn't care which agent is running. Unsupported capabilities (e.g., Claude approval) return `Err(Unsupported)` gracefully.
