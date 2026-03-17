# Harness API Contract

Harness exposes two protocols over two distinct transports. Each transport has a
well-defined role. Clients must choose the transport that matches their use case.

## Transport roles

| Transport | Role | Typical caller |
|-----------|------|----------------|
| **HTTP REST** | Operator / control plane | Dashboard, CI scripts, batch jobs, webhooks |
| **JSON-RPC 2.0** (stdio / WebSocket / HTTP `/rpc`) | Agent / data plane | Spawned agents (Claude Code, Codex), MCP integrations |

### HTTP REST — operator-facing (control plane)

HTTP is the entry point for human operators and automation that submits work and
observes results. The following capabilities are **only available over HTTP**:

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `POST /tasks` | create | Enqueue a single task |
| `GET  /tasks` | list | List all tasks |
| `POST /tasks/batch` | batch create | Enqueue multiple tasks at once |
| `GET  /tasks/{id}` | get | Fetch task details |
| `GET  /tasks/{id}/stream` | stream | Server-Sent Events live output |
| `POST /projects` | register | Register a project root |
| `GET  /projects` | list | List registered projects |
| `GET  /projects/{id}` | get | Get project details |
| `DELETE /projects/{id}` | delete | Remove project registration |
| `GET  /projects/queue-stats` | stats | Per-project queue statistics |
| `GET  /api/dashboard` | dashboard | Dashboard data |
| `GET  /api/intake` | intake | Intake source status |
| `POST /webhook` | webhook | GitHub webhook (HMAC-verified) |
| `POST /webhook/feishu` | webhook | Feishu bot webhook |
| `POST /signals` | ingest | Signal ingestion (rate-limited) |
| `GET  /health` | health | Server health check |

Authentication: all routes (except `/health`) require an `Authorization: Bearer
<token>` header validated with constant-time comparison.

### JSON-RPC 2.0 — agent-facing (data plane)

JSON-RPC is the protocol used by agents running inside Harness threads. It is
available over three transports simultaneously:

* **stdio** — for agents launched as child processes
* **WebSocket** (`GET /ws`) — for long-running agent connections
* **HTTP POST** (`POST /rpc`) — for request/response without a persistent connection

All three transports share the same method set. The following capabilities are
**only available via JSON-RPC**:

| Method | Purpose |
|--------|---------|
| `initialize` / `initialized` | Protocol handshake |
| `thread/start` | Create a new thread |
| `thread/list` | List threads |
| `thread/delete` | Delete a thread |
| `thread/resume` | Reopen a thread |
| `thread/fork` | Branch a thread from a turn |
| `thread/compact` | Compact thread history |
| `turn/start` | Begin a generation turn |
| `turn/cancel` | Cancel an in-flight turn |
| `turn/status` | Query turn status |
| `turn/steer` | Append an instruction to a running turn |
| `skill/create` | Register a skill |
| `skill/list` | Query skills |
| `skill/get` | Get skill details |
| `skill/delete` | Remove a skill |
| `rule/load` | Load project rules |
| `rule/check` | Check files against rules |
| `exec_plan/init` | Initialise an execution plan |
| `exec_plan/update` | Update a plan |
| `exec_plan/status` | Query plan status |
| `event/log` | Record an event |
| `event/query` | Query events |
| `metrics/collect` | Gather project metrics |
| `metrics/query` | Query metrics |
| `task/classify` | Classify prompt/issue/PR complexity |
| `learn/rules` | Learn from rule violations |
| `learn/skills` | Learn from skill usage |
| `health/check` | Per-project health status |
| `stats/query` | Query aggregate statistics |
| `agent/list` | List registered agents |
| `gc/run` | Trigger garbage collection |
| `gc/status` | GC status |
| `gc/drafts` | List GC draft PRs |
| `gc/adopt` | Accept a draft PR |
| `gc/reject` | Reject a draft PR |
| `preflight` | Pre-flight validation |
| `cross_review` | Cross-agent code review |

JSON-RPC requires a handshake (`initialize` → `initialized`) before any other
method is accepted.

## Why the split?

The design is intentional:

1. **Security boundary** — Task submission and project registration are privileged
   operations. Keeping them HTTP-only means they are always protected by the same
   token-based authentication middleware. Agent processes communicating over stdio
   cannot submit new tasks or register projects.

2. **Semantic clarity** — HTTP semantics (status codes, REST verbs, SSE) are a
   natural fit for long-running job management. JSON-RPC semantics (request/
   response over a single channel, server-push notifications) are a natural fit for
   interactive agent sessions.

3. **Client simplicity** — CI scripts, GitHub Actions, and dashboard frontends use
   HTTP. Agent frameworks use JSON-RPC. Neither audience is burdened with the
   other protocol.

## Choosing a transport

| You want to… | Use |
|---|---|
| Submit a task from a CI script | `POST /tasks` (HTTP) |
| Stream live output from a running task | `GET /tasks/{id}/stream` (HTTP SSE) |
| Register a new project | `POST /projects` (HTTP) |
| Run an agent thread interactively | `turn/start` (JSON-RPC) |
| Inspect or cancel a running turn | `turn/status` / `turn/cancel` (JSON-RPC) |
| Load rules for an agent to respect | `rule/load` (JSON-RPC) |
| Record an event from within an agent | `event/log` (JSON-RPC) |

## Error codes

All JSON-RPC errors follow [JSON-RPC 2.0](https://www.jsonrpc.org/specification).

| Code | Name | Meaning |
|------|------|---------|
| `-32700` | `PARSE_ERROR` | Invalid JSON |
| `-32600` | `INVALID_REQUEST` | Malformed request object |
| `-32601` | `METHOD_NOT_FOUND` | Method does not exist |
| `-32602` | `INVALID_PARAMS` | Invalid method parameters |
| `-32603` | `INTERNAL_ERROR` | Internal server error |
| `-32001` | `NOT_FOUND` | Resource not found |
| `-32002` | `CONFLICT` | Resource already exists |
| `-32003` | `NOT_INITIALIZED` | Handshake not yet completed |
| `-32004` | `STORAGE_ERROR` | Persistence layer error |
| `-32005` | `AGENT_ERROR` | Agent execution error |
| `-32006` | `VALIDATION_ERROR` | Input validation failure |

HTTP errors follow standard HTTP status codes (200, 201, 400, 401, 404, 409, 500).
