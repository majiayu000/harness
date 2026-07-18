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
| `POST /api/workflows/runtime/submissions` | submit | Create a durable workflow-runtime submission |
| `GET  /api/workflows/runtime/submissions/{id}` | get | Read runtime submission status |
| `GET  /api/workflows/runtime/submissions/{id}/artifacts` | artifacts | Read runtime output artifacts |
| `POST /api/workflows/runtime/transcripts/reconstruct` | transcript reconstruction | Restore a missing or corrupt runtime transcript from an upstream provider re-export |
| `GET  /api/workflows/runtime/submissions/{id}/stream` | stream | Stream runtime submission events |
| `POST /api/workflows/runtime/turns/{turn_id}/approvals/{request_id}` | approval | Respond to a live runtime approval request |
| `POST /webhook` | webhook | GitHub webhook (HMAC-verified) |
| `POST /webhook/feishu` | webhook | Feishu bot webhook |
| `POST /signals` | ingest | Signal ingestion (rate-limited) |
| `GET  /health` | health | Server health check |

#### Degraded persistence responses

Runtime persistence failures are part of the HTTP contract, not log-only
events. `GET /health` returns a JSON `status` of `degraded` when startup store
initialization failed or runtime state is dirty; the `persistence.startup.stores`
array reports each store by name with `critical`, `ready`, and a redacted
`error` code.

`GET /tasks` keeps returning the task rows it can safely load, but includes a
`degraded` object when runtime-only submissions could not be loaded:

```json
{
  "degraded": {
    "partial": true,
    "missing": ["workflow_runtime_submissions"],
    "reason": "runtime_submission_summaries_unavailable"
  }
}
```

`GET /tasks/{id}` and `GET /tasks/{id}/proof` return `503 Service Unavailable`
with `error: "workflow runtime store unavailable"` when the requested handle
could be runtime-backed but the required workflow runtime store is unavailable.

Runtime-host mutations, including watched-project sync, fail with
`503 Service Unavailable` when runtime state persistence was required at startup
but the runtime state store is unavailable. This prevents a successful response
from hiding non-durable host state.

Authentication: `harness serve` fails closed unless `api_token` or
`HARNESS_API_TOKEN` is configured, or `allow_unauthenticated = true` is set
explicitly for tokenless local development. When a token is configured, all
non-exempt routes require `Authorization: Bearer <token>`; if both a token and
`allow_unauthenticated = true` are set, the token wins and the opt-in is
ignored. Exempt routes that bypass auth entirely: `/health`, `/webhook`,
`/webhook/feishu`, `/signals`, and `/favicon.ico` (these carry their own
HMAC-based protection or are intentionally public). For browser clients that
cannot set `Authorization` headers on SSE requests, `/tasks/{id}/stream` and
`/api/workflows/runtime/submissions/{id}/stream` additionally accept a
`?token=<value>` query parameter as a fallback; all other routes only accept
`Authorization: Bearer <token>`.

### JSON-RPC 2.0 — agent-facing (data plane)

JSON-RPC is the protocol used by agent-facing support integrations. Workflow
submission and live turn lifecycle control are HTTP control-plane concerns. The
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
   natural fit for long-running job management and live runtime control.
   JSON-RPC request/response semantics remain a fit for bounded agent support
   integrations such as rules, skills, plans, and observability.

3. **Client simplicity** — CI scripts, GitHub Actions, and dashboard frontends use
   HTTP. Agent frameworks use JSON-RPC. Neither audience is burdened with the
   other protocol.

## Choosing a transport

| You want to… | Use |
|---|---|
| Submit a task from a CI script | `POST /tasks` (HTTP) |
| Stream live output from a running task | `GET /tasks/{id}/stream` (HTTP SSE) |
| Register a new project | `POST /projects` (HTTP) |
| Run a prompt through the workflow runtime | `POST /api/workflows/runtime/submissions` (HTTP) |
| Inspect a running runtime submission | `GET /api/workflows/runtime/submissions/{id}` (HTTP) |
| Respond to a live approval request | `POST /api/workflows/runtime/turns/{turn_id}/approvals/{request_id}` (HTTP) |
| Load rules for an agent to respect | `rule/load` (JSON-RPC) |
| Record an event from within an agent | `event/log` (JSON-RPC) |

## HTTP task list

`GET /tasks` returns a paginated envelope, not a raw array:

```json
{
  "data": [],
  "page": { "limit": 50, "has_more": false, "next_cursor": null },
  "counts": {
    "total": 0,
    "running": 0,
    "failed": 0,
    "by_status": {},
    "by_scheduler_state": {}
  }
}
```

Supported query parameters are `status`, `scheduler_state`, `active`, `kind`,
`source`, `repo`, `project_id`, `limit`, and `cursor`. `status` is the task
lifecycle status; `scheduler_state` is the ownership/execution state. For
example, currently executing work is queried with
`/tasks?scheduler_state=running`, not `status=running`.

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
