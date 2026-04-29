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

Authentication: when `api_token` is configured, all routes require proof of the
token; without a configured token the middleware is a no-op (backward
compatibility).  Exempt routes that bypass auth entirely: `/health`,
`/webhook`, `/webhook/feishu`, `/signals`, and `/favicon.ico` (these carry
their own HMAC-based protection or are intentionally public).  For browser
clients that cannot set `Authorization` headers on WebSocket upgrades or
top-level navigation requests, `/ws` and `/` additionally accept a
`?token=<value>` query parameter as a fallback; all other routes only accept
`Authorization: Bearer <token>`.

### JSON-RPC 2.0 — agent-facing (data plane)

JSON-RPC is the protocol used by agents running inside Harness threads. The
transport is selected at server startup with `--transport <mode>`:

* **stdio** (`--transport stdio`) — for agents launched as child processes
* **HTTP + WebSocket** (`--transport http`) — exposes both `GET /ws` (long-running
  connections) and `POST /rpc` (request/response) over the same HTTP listener

Only one transport mode is active per server instance. Clients must connect via
the transport that matches how the server was started.

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
| `turn/respond_approval` | Submit a decision for a pending turn approval request |
| `skill/create` | Register a skill |
| `skill/list` | Query skills |
| `skill/get` | Get skill details |
| `skill/delete` | Remove a skill |
| `skill/governance/view` | Inspect skill governance configuration |
| `skill/governance/history` | Inspect skill governance history |
| `skill/stale` | List stale skills that need review |
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

JSON-RPC requires a server-wide handshake (`initialize` → `initialized`) before
any method other than `initialize`/`initialized` is accepted.  This handshake
is **server-wide, not per-connection**: the first client to complete it sets a
shared flag; all subsequent connections (stdio, WebSocket, or HTTP `/rpc`) can
send methods immediately without repeating the handshake.  Sending `initialize`
again after the server is already initialized returns a `-32600 INVALID_REQUEST`
error.

## Why the split?

The design is intentional:

1. **Security boundary** — Task submission and project registration are privileged
   operations. Keeping them HTTP-only means they go through the same
   token-based authentication middleware (when `api_token` is configured).
   Agent processes communicating over stdio cannot submit new tasks or register
   projects regardless of auth configuration.

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
