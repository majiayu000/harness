# OpenAI Codex App Server — Technical Reference

> Source: OpenAI developer docs, GitHub codex-rs, InfoQ, community research (2026-03)

## Architecture Overview

Codex has three runtime modes:

| Mode | Command | Use Case |
|------|---------|----------|
| Interactive TUI | `codex` | Developer terminal |
| Headless exec | `codex exec "prompt"` | CI/scripts, non-interactive |
| App Server | `codex app-server` | Long-lived process, IDE/Web integration |

### App Server Four Components

```
Client (VS Code / CLI / Web / SDK)
    ↕  JSON-RPC 2.0 over stdio (JSONL) or WebSocket
┌──────────────────────────────────────────┐
│  1. Stdio Reader        — parse JSONL    │
│  2. Message Processor   — translate to   │
│                           Core operations│
│  3. Thread Manager      — one Core       │
│                           Session/Thread │
│  4. Core Threads        — agent loop     │
│                           execution      │
└──────────────────────────────────────────┘
```

The stdio reader and message processor serve as the **translation layer**: they convert client JSON-RPC requests into Codex core operations, listen to the internal event stream, and transform low-level events into stable, UI-ready JSON-RPC notifications.

## Three Primitives

| Primitive | Description |
|-----------|-------------|
| **Thread** | Conversation between user and agent. Persistent, resumable, forkable. |
| **Turn** | One user request + all agent work. Contains multiple Items. |
| **Item** | Atomic I/O: message, command execution, file change, tool call, reasoning. |

## JSON-RPC Protocol

### Initialization Handshake

```json
→ { "method": "initialize", "id": 0, "params": { "clientInfo": { "name": "my_app", "title": "My App", "version": "0.1.0" } } }
← { "id": 0, "result": { ... } }
→ { "method": "initialized", "params": {} }
```

### Thread Lifecycle

| Method | Purpose |
|--------|---------|
| `thread/start` | Create new thread (params: model, cwd, sandbox, personality) |
| `thread/resume` | Reopen existing thread by ID |
| `thread/fork` | Branch history into new thread |
| `thread/list` | Page through threads (filter, sort, pagination) |
| `thread/read` | Fetch without resuming |
| `thread/archive` / `thread/unarchive` | Archive management |
| `thread/compact/start` | History compaction |
| `thread/rollback` | Drop last N turns |

### Turn Lifecycle

| Method | Purpose |
|--------|---------|
| `turn/start` | Begin generation (params: threadId, input[], model, effort, sandbox overrides) |
| `turn/steer` | Append input to in-flight turn |
| `turn/interrupt` | Cancel in-flight turn |

Input types: `text`, `image`, `localImage`, `skill`

### Notifications (Server → Client)

| Notification | When |
|-------------|------|
| `thread/started` | Thread created |
| `turn/started` | Turn begins |
| `item/started` | Item processing begins |
| `item/agentMessage/delta` | Streaming content delta |
| `item/completed` | Item finished |
| `turn/completed` | Turn finished (includes token usage) |
| `turn/diff/updated` | Diff changed |
| `turn/plan/updated` | Plan updated |
| `thread/status/changed` | Thread state change |

### Item Types

- `userMessage` — user input
- `agentMessage` — model response (streamed via deltas)
- `commandExecution` — shell command with approval flow
- `fileChange` — proposed edits with approval flow
- `mcpToolCall` — MCP server tool invocations
- `webSearch` — search requests
- `reasoning` — extended thinking
- `plan` — step-by-step reasoning
- `enteredReviewMode` / `exitedReviewMode` — code review

### Approval Flow

Commands and file changes require client approval:
- Request: `{ itemId, threadId, turnId, reason?, command? }`
- Response: `accept`, `acceptForSession`, `decline`, `cancel`

### Direct Command Execution (no Thread)

```json
{ "method": "command/exec", "id": 50, "params": {
    "command": ["ls", "-la"], "cwd": "/path",
    "sandboxPolicy": { "type": "workspaceWrite" },
    "timeoutMs": 10000
} }
```

### Skills

```json
{ "method": "skills/list", "id": 25, "params": { "cwds": ["/project"], "forceReload": true } }
```

Invoke via `$<skill-name>` in turn input.

### Code Review

```json
{ "method": "review/start", "id": 40, "params": {
    "threadId": "thr_123", "delivery": "inline",
    "target": { "type": "commit", "sha": "abc123", "title": "..." }
} }
```

## CLI Reference

### codex exec (Headless)

```bash
codex exec "prompt"                              # basic
codex exec --full-auto "Write tests"             # no approvals
codex exec --json "analyze code" > events.jsonl  # JSONL stream
codex exec -m gpt-5.1-codex "refactor"           # model override
codex exec resume --last "continue"              # resume session
cat task.txt | codex exec -                      # stdin prompt
```

Key flags:
- `--full-auto` — no approval prompts
- `--json` / `--experimental-json` — JSONL event stream
- `--ephemeral` — no session persistence
- `--skip-git-repo-check` — run outside git repo
- `-o PATH` — write final message to file
- `--output-schema PATH` — enforce structured JSON output
- `-a <policy>` — approval policy (untrusted/on-request/never)
- `-s <sandbox>` — sandbox policy (read-only/workspace-write/danger-full-access)
- `-C <dir>` — working directory

### JSONL Event Format

```json
{"type":"thread.started","thread_id":"..."}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","status":"in_progress"}}
{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"..."}}
{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"output_tokens":122}}
```

## SDK (TypeScript)

```bash
npm install @openai/codex-sdk
```

```typescript
import { Codex } from "@openai/codex-sdk";

const codex = new Codex();
const thread = codex.startThread();
const result = await thread.run("Diagnose CI failures");

// Continue on same thread
const result2 = await thread.run("Implement the fix");

// Resume past thread
const thread2 = codex.resumeThread("<thread-id>");
```

## SDK (Node.js Raw JSON-RPC)

```javascript
import { spawn } from "node:child_process";
import readline from "node:readline";

const proc = spawn("codex", ["app-server"], { stdio: ["pipe", "pipe", "inherit"] });
const rl = readline.createInterface({ input: proc.stdout });
const send = (msg) => proc.stdin.write(`${JSON.stringify(msg)}\n`);

rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.id === 1 && msg.result?.thread?.id) {
    send({ method: "turn/start", id: 2, params: {
      threadId: msg.result.thread.id,
      input: [{ type: "text", text: "Summarize this repo." }]
    } });
  }
});

send({ method: "initialize", id: 0, params: { clientInfo: { name: "my_app", title: "My App", version: "0.1.0" } } });
send({ method: "initialized", params: {} });
send({ method: "thread/start", id: 1, params: { model: "gpt-5.1-codex" } });
```

## Sandbox Policies

| Policy | Description |
|--------|-------------|
| `dangerFullAccess` | Unrestricted |
| `readOnly` | Read-only with optional access restrictions |
| `workspaceWrite` | Write to specific roots |
| `externalSandbox` | Host app manages sandbox |

Implementation: macOS Seatbelt / Linux Landlock.

## Authentication

Three modes: `apikey` (OpenAI API key), `chatgpt` (OAuth), `chatgptAuthTokens` (host-supplied tokens).

## Key Design Decisions

1. **App Server is the agent runtime** — models are called directly via API, not through CLI subprocess
2. **Abandoned MCP** — streaming diffs, approval flows, and thread persistence don't map to MCP tool model
3. **Bidirectional JSON-RPC** — one client request can trigger many server notifications (rich event stream)
4. **Bounded queues with backpressure** — overload returns `-32001`, clients use exponential backoff

## Harness vs Codex Positioning

| Aspect | Codex App Server | Harness |
|--------|-----------------|---------|
| Core | Native agent runtime (direct LLM API) | Agent orchestrator (wraps CLI tools) |
| Agent | Built-in gpt-5.3-codex | Pluggable: Claude Code, Codex CLI, Anthropic API |
| Thread | Full persistence + resume + fork | In-memory (planned: SQLite) |
| Protocol | Complete JSON-RPC 2.0 | Partial implementation |
| Sandbox | Seatbelt/Landlock native | Seatbelt (macOS), Landlock/bwrap strategy (Linux) for CLI agent subprocesses |
| Value | Single-agent deep integration | Multi-agent orchestration + rules + GC |

## Sources

- https://developers.openai.com/codex/app-server/
- https://developers.openai.com/codex/cli/reference/
- https://developers.openai.com/codex/sdk/
- https://developers.openai.com/codex/noninteractive/
- https://openai.com/index/unlocking-the-codex-harness/
- https://openai.com/index/unrolling-the-codex-agent-loop/
- https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md
- https://www.infoq.com/news/2026/02/opanai-codex-app-server/
