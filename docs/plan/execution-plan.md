# P0/P1 Execution Plan

## Baseline

- 207 tests passing across workspace
- 10 crates, ~10,800 lines
- Branch: each step creates its own branch from main, PR, merge, then next

## Dependency Graph

```
#103 Protocol ──────────────────────────┐
                                        │
#102 Agent Loop ──→ #104 Streaming ─────┤──→ #108 Approval
                                        │
#105 Sandbox (independent) ─────────────┤
                                        │
#107 AGENTS.md (independent) ───────────┤
                                        │
#110 Multi-agent dispatch (independent) ┤
                                        │
#106 GitHub webhook ────────────────────┤
                                        │
#109 CI/CD (depends on most) ───────────┘
```

## Execution Order

---

### Step 1: Protocol Compatibility (#103)

**Goal**: Support slash-style method names while keeping existing snake_case working.

#### 1a. Method name mapping layer
- File: `harness-protocol/src/methods.rs`
- Add `method_name()` -> slash-style string for each variant
- Custom deserializer accepts both `"thread_start"` and `"thread/start"`
- Tests: round-trip serde for both naming styles
- `cargo test -p harness-protocol`

#### 1b. Initialize handshake state machine
- File: `harness-server/src/router.rs`
- Add `initialized: AtomicBool` to router/AppState
- Reject non-init methods before handshake completes
- `Initialize` -> set capabilities, return server info
- `Initialized` -> flip flag, allow subsequent methods
- Tests: pre-init rejection, handshake sequence, double-init error
- `cargo test -p harness-server`

#### 1c. Notification naming alignment
- File: `harness-protocol/src/notifications.rs`
- Rename notification method strings to slash-style: `turn/started`, `item/completed`, etc.
- Tests: notification serialization produces slash-style
- `cargo test -p harness-protocol`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 2: Agent Loop — AgentAdapter Trait (#102 part 1)

**Goal**: Define the new streaming adapter trait alongside existing CodeAgent (no breaking changes yet).

#### 2a. Define AgentAdapter trait and AgentEvent enum
- File: `harness-core/src/agent.rs` (extend, don't replace CodeAgent yet)
- Add `AgentAdapter` trait with `start_turn()`, `interrupt()`, `steer()`, `respond_approval()`
- Add `AgentEvent` enum: TurnStarted, ItemStarted, MessageDelta, ToolCall, ApprovalRequest, ItemCompleted, TurnCompleted, Error
- Add `ApprovalDecision` enum
- Tests: AgentEvent serde round-trip
- `cargo test -p harness-core`

#### 2b. ClaudeAdapter — stream-json parser
- File: `harness-agents/src/claude_adapter.rs` (new file)
- Spawn `claude --output-format stream-json -p <prompt>`
- Parse JSONL line by line -> map to AgentEvent stream
- Handle process kill for interrupt()
- steer() and respond_approval() return Unsupported
- Tests: parse sample stream-json output (mock subprocess with fixture data)
- `cargo test -p harness-agents`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 3: Agent Loop — CodexAdapter (#102 part 2)

**Goal**: Codex App Server protocol adapter.

#### 3a. JSON-RPC stdio transport
- File: `harness-agents/src/codex_adapter.rs` (new file)
- Spawn `codex` as long-lived stdio subprocess
- Bidirectional JSON-RPC: send requests via stdin, read responses/notifications from stdout
- Implement initialize handshake
- Tests: mock subprocess with fixture JSON-RPC exchanges
- `cargo test -p harness-agents`

#### 3b. Full turn lifecycle
- Implement `start_turn()` -> `turn/start` + event stream from notifications
- Implement `interrupt()` -> `turn/interrupt`
- Implement `steer()` -> `turn/steer`
- Implement `respond_approval()` -> approval response
- Tests: turn start -> events -> complete sequence; interrupt mid-turn
- `cargo test -p harness-agents`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 4: Wire AgentAdapter into Task Executor (#102 part 3)

**Goal**: task_executor uses AgentAdapter instead of CodeAgent for task execution.

#### 4a. Adapter registry
- File: `harness-agents/src/registry.rs`
- Add `AdapterRegistry` alongside existing `AgentRegistry`
- Register ClaudeAdapter and CodexAdapter
- Fallback: if no adapter available, use legacy CodeAgent path
- Tests: registry lookup, fallback behavior
- `cargo test -p harness-agents`

#### 4b. Task executor integration
- File: `harness-server/src/task_executor.rs`
- When adapter available: use `start_turn()`, consume AgentEvent stream
- Map AgentEvent -> log to EventStore + emit notifications
- When no adapter: fall back to existing `CodeAgent::execute()` path
- Tests: mock adapter -> verify events logged and notifications emitted
- `cargo test -p harness-server`

#### 4c. Remove legacy CodeAgent (deferred)
- Only after adapters are proven stable in production
- Delete `CodeAgent` trait, `execute()`, `execute_stream()`
- Migrate all call sites

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 5: Streaming Delta Output (#104)

**Goal**: AgentEvents flow through to WebSocket clients in realtime.

#### 5a. Mount WebSocket on HTTP server
- File: `harness-server/src/http.rs`
- Add `/ws` route using existing `websocket.rs` handler
- Unify notification channels: handler notify_tx -> broadcast -> WebSocket
- Tests: WebSocket connection + receive notification
- `cargo test -p harness-server`

#### 5b. AgentEvent -> RpcNotification mapping
- File: `harness-server/src/task_executor.rs`
- Each AgentEvent emitted during task execution maps to an RpcNotification
- MessageDelta -> new `message/delta` notification type
- ToolCall -> new `tool_call/started` notification type
- Tests: verify notification sequence from mock adapter
- `cargo test -p harness-server`

#### 5c. Notification filtering
- File: `harness-protocol/src/notifications.rs`
- Client specifies which notifications to receive during initialize
- Server filters before sending
- Tests: filtered vs unfiltered notification delivery
- `cargo test -p harness-protocol`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 6: AGENTS.md Support (#107)

**Goal**: Load project instructions from cascading AGENTS.md files.

#### 6a. Discovery and loading
- File: `harness-core/src/agents_md.rs` (new)
- Scan: `~/.harness/AGENTS.md` -> repo root -> subdirectories toward cwd
- Override: `AGENTS.override.md` replaces at that level
- 32KB combined limit (configurable via `project_doc_max_bytes`)
- Tests: cascading merge, override, size limit
- `cargo test -p harness-core`

#### 6b. Inject into agent prompts
- File: `harness-core/src/prompts.rs`
- Prepend merged AGENTS.md content to implementation/review prompts
- File: `harness-server/src/task_executor.rs`
- Call `load_agents_md(project_root)` before constructing AgentRequest
- Tests: prompt contains AGENTS.md content; empty when no files
- `cargo test -p harness-server`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 7: Multi-Agent Dispatch (#110)

**Goal**: Task complexity routes to different agents.

#### 7a. Activate complexity router
- File: `harness-server/src/complexity_router.rs`
- Implement `classify()`: analyze issue text -> TaskComplexity
- Simple heuristics: file count keywords, label parsing, code vs docs
- Tests: classification for various issue descriptions
- `cargo test -p harness-server`

#### 7b. Wire dispatch into task creation
- File: `harness-server/src/http.rs`
- `create_task()` calls `classify()` -> `registry.dispatch()` instead of `default_agent()`
- Register codex + anthropic-api at HTTP server startup
- Fallback to default_agent if dispatch returns None
- Tests: complex issue -> claude, simple issue -> codex
- `cargo test -p harness-server`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 8: OS-Level Sandbox (#105)

**Goal**: Enforce filesystem/network isolation at OS level.

#### 8a. Sandbox policy types
- New crate: `harness-sandbox/`
- Define `SandboxMode`: ReadOnly, WorkspaceWrite, DangerFullAccess
- Define `SandboxPolicy`: writable_roots, network_access, protected_paths
- Config integration: `sandbox_mode` in config.rs
- Tests: policy construction, defaults
- `cargo test -p harness-sandbox`

#### 8b. macOS Seatbelt implementation
- File: `harness-sandbox/src/macos.rs`
- Generate Seatbelt profile from SandboxPolicy
- Wrap subprocess with `sandbox-exec -f <profile>`
- Protected: `.git/`, `.harness/` always read-only
- Tests: profile generation for each mode (validate syntax, not execution)
- `cargo test -p harness-sandbox`

#### 8c. Linux Landlock implementation
- File: `harness-sandbox/src/linux.rs`
- Apply Landlock rules before exec
- Fallback to bwrap if Landlock unavailable (kernel < 5.13)
- Tests: rule generation for each mode
- `cargo test -p harness-sandbox`

#### 8d. Wire into agent adapters
- File: `harness-agents/src/claude_adapter.rs`, `codex_adapter.rs`
- Wrap subprocess spawn with sandbox enforcement
- Tests: verify sandbox-exec/landlock arguments in command construction
- `cargo test -p harness-agents`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 9: Approval Policy (#108)

**Goal**: Human-in-the-loop gates for agent tool calls.

#### 9a. Policy types and config
- File: `harness-core/src/config.rs`
- Add `ApprovalPolicy`: Suggest (default), AutoEdit, FullAuto
- Add `RejectMap`: sandbox_approvals, network_access, mcp_elicitations
- Tests: config parse, defaults
- `cargo test -p harness-core`

#### 9b. Approval gate in task executor
- File: `harness-server/src/task_executor.rs`
- When AgentEvent::ApprovalRequest received (from CodexAdapter):
  - Check policy: FullAuto -> auto-accept; Suggest -> emit notification, wait for client response
  - AutoEdit -> auto-accept file changes, prompt for shell commands
- Tests: each policy path with mock events
- `cargo test -p harness-server`

#### 9c. Client-side approval flow
- File: `harness-protocol/src/methods.rs`
- Add `ApprovalRespond` method for client -> server approval responses
- File: `harness-server/src/router.rs`
- Route approval responses to pending task executor
- Tests: approval request -> client response -> execution continues
- `cargo test -p harness-server`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 10: GitHub Webhook (#106)

**Goal**: Automated task creation from GitHub events.

#### 10a. Webhook endpoint
- File: `harness-server/src/http.rs`
- Add `POST /webhook` route
- Verify GitHub webhook signature (HMAC-SHA256)
- Config: `webhook_secret` in config.rs
- Tests: signature verification, invalid signature rejection
- `cargo test -p harness-server`

#### 10b. Event parsing and task dispatch
- File: `harness-server/src/webhook.rs` (new)
- Parse `issue_comment` events: detect `@harness` mention
- Parse `pull_request` events: auto-review if enabled
- Parse `check_suite` events: detect CI failures
- Create task via existing `spawn_task()` path
- Tests: parse sample GitHub webhook payloads -> correct task creation
- `cargo test -p harness-server`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

### Step 11: CI/CD GitHub Action (#109)

**Goal**: Reusable GitHub Action for PR pipelines.

#### 11a. Action definition
- File: `.github/actions/harness-action/action.yml`
- Inputs: prompt, sandbox, model, output-file, harness-version
- Steps: install harness CLI, run `harness exec`
- Security: drop-sudo default, allow-users filter
- Tests: action syntax validation (actionlint)

#### 11b. Exec command enhancements
- File: `harness-cli/src/commands.rs`
- `harness exec` supports `--json` output, `--output-file`, non-interactive mode
- Exit code reflects success/failure
- Tests: exec with mock agent
- `cargo test -p harness-cli`

**Gate**: `cargo test --workspace` all pass. Commit + PR + merge.

---

## Progress Tracking

| Step | Issue | Status | PR | Tests Added |
|------|-------|--------|----|----|
| 1a | #103 | pending | - | - |
| 1b | #103 | pending | - | - |
| 1c | #103 | pending | - | - |
| 2a | #102 | pending | - | - |
| 2b | #102 | pending | - | - |
| 3a | #102 | pending | - | - |
| 3b | #102 | pending | - | - |
| 4a | #102 | pending | - | - |
| 4b | #102 | pending | - | - |
| 5a | #104 | pending | - | - |
| 5b | #104 | pending | - | - |
| 5c | #104 | pending | - | - |
| 6a | #107 | pending | - | - |
| 6b | #107 | pending | - | - |
| 7a | #110 | pending | - | - |
| 7b | #110 | pending | - | - |
| 8a | #105 | pending | - | - |
| 8b | #105 | pending | - | - |
| 8c | #105 | pending | - | - |
| 8d | #105 | pending | - | - |
| 9a | #108 | pending | - | - |
| 9b | #108 | pending | - | - |
| 9c | #108 | pending | - | - |
| 10a | #106 | pending | - | - |
| 10b | #106 | pending | - | - |
| 11a | #109 | pending | - | - |
| 11b | #109 | pending | - | - |

## Rules

1. Each step: implement -> `cargo check` -> `cargo test --workspace` -> commit -> PR -> merge
2. One branch per step (e.g., `feat/protocol-slash-methods`)
3. Never accumulate changes across steps — merge before starting next
4. If a step breaks existing tests, fix before proceeding
5. Each step adds tests for new code (minimum 80% coverage for new lines)
