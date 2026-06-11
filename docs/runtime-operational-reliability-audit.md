# Runtime Operational Reliability Audit

## Purpose

Capture the design-level reliability risks found during the 2026-05-30
read-only architecture audit and map them to small remediation issues. This is
not a code change plan for a single large refactor. Each item should land as a
small Issue -> PR -> verification chain.

The audit intentionally did not start `harness serve`. Runtime verification for
production readiness still needs live workload evidence after code fixes land.

## Priority Order

1. Bound the Postgres schema lifecycle and stop creating unowned schemas.
2. Make workflow runtime side effects and completion use one atomic boundary.
3. Connect `RemoteHost` workflow runtime jobs to runtime-host claim and
   completion APIs.
4. Replace silent runtime persistence degradation with explicit API and health
   contracts.
5. Fail turns when adapters emit terminal error events.
6. Preserve `CodexExec` versus `CodexJsonrpc` execution strategy across the
   agent/runtime boundary.
7. Continue the existing repo backlog polling work without opening a duplicate
   remediation track.

## Issue Map

| Priority | Issue | Risk | First PR shape |
|---|---|---|---|
| P1 | #1199 | Path-derived Postgres schemas have no ownership or cleanup lifecycle. | Add schema ownership registry and dry-run cleanup inventory. |
| P2 | #1200 | Runtime activity side effects can be durable before lease-owned completion commits. | Move transition, event, decision, command, and completion writes behind one store boundary. |
| P3 | #1201 | `remote_host` is configurable but workflow runtime jobs are not claimable through the runtime-host API. | Add runtime-job claim/complete/fail APIs backed by `WorkflowRuntimeStore`. |
| P4 | #1202 | Runtime persistence/query failures can return successful or partial responses without a response-level contract. | Classify required stores and add explicit degraded/partial response behavior. |
| P5 | #1203 | Adapter terminal errors can be appended as items while the turn is still marked completed. | Promote terminal adapter errors to failed turn execution with regression tests. |
| P6 | #1204 | `CodexExec` and `CodexJsonrpc` can collapse to the same `codex` agent execution path. | Carry execution strategy into turn executor selection and test both paths. |
| P7 | #1170 | Repo backlog polling can still starve useful work under active or stuck repositories. | Continue the existing lightweight/observable backlog polling work instead of opening a duplicate issue. |

## Findings

### 1. Postgres Schema Lifecycle Is Unbounded

`crates/harness-core/src/db_pg.rs` derives schemas from store identity paths via
`pg_schema_for_path`, then creates them on demand with `CREATE SCHEMA IF NOT
EXISTS`. That preserved SQLite-era store isolation, but it does not record
ownership, retention class, last seen time, or cleanup eligibility.

This matters because production catalog growth is a real operational failure
mode. A database can contain many abandoned `h*` schemas without any reliable
way to prove which ones are active, stale, canonical, or safe to drop.

Required invariant: every schema created by Harness must have a durable owner
record before it is considered valid runtime state.

### 2. Workflow Runtime Side Effects Are Not Always Atomic

The worker executes an activity, records `ActivityResultReady`, and then commits
completion only if the worker still owns the lease. Some server-owned activity
handlers, such as child workflow start and issue auto-submit, can write durable
state before that completion boundary.

This creates a crash/retry window. If the worker loses its lease after creating a
child workflow but before parent completion commits, replay can see parent and
child state that no longer describe one committed transition.

Required invariant: a workflow transition should commit instance state, event,
decision, command outbox, and runtime job completion in one transaction, or the
side effect must be idempotent against the exact parent runtime job lease.

### 3. `RemoteHost` Is Configurable But Not Executable Through The Host API

Runtime config accepts `remote_host` and maps it to a runtime profile. The local
worker rejects `RemoteHost` jobs because they are meant for external runtime
hosts. The current runtime-host claim endpoint, however, scans legacy pending
tasks instead of `WorkflowRuntimeStore` jobs.

This makes `RemoteHost` look supported while behaving like inert metadata for
workflow runtime jobs.

Required invariant: if `RemoteHost` is a valid `RuntimeKind`, external hosts must
be able to claim, complete, and fail those runtime jobs through a lease-owned
workflow runtime API.

### 4. Runtime Persistence Degradation Is Too Quiet

Some startup stores are treated as optional, and later request handlers can keep
returning success after runtime state or runtime submission summaries are
missing. Logs are useful for operators, but they are not an API contract.

This matters because callers can receive a normal response that omits data they
reasonably expect to be authoritative.

Required invariant: missing runtime durability must either fail readiness, fail
the request, or be visible in a typed partial/degraded response field.

### 5. Adapter Terminal Errors Can Become Completed Turns

`AgentEvent::Error` is mapped to a stream error item. The helper appends that
item to the turn, but the outer turn lifecycle still completes the turn when the
adapter task returns `Ok(())`.

This hides adapter protocol failures from retry policy, dashboards, and workflow
completion reducers.

Required invariant: a terminal adapter error must fail the turn while preserving
the error item for operator inspection.

### 6. Codex Runtime Strategy Can Collapse By Agent Name

`RuntimeKind::CodexExec` and `RuntimeKind::CodexJsonrpc` both map to agent name
`codex`. Turn execution is selected by agent name, so runtime policy can lose the
strategy requested by config before execution starts.

This matters because `codex_exec` and `codex_jsonrpc` are distinct contracts.
Treating them as the same executor makes runtime policy misleading and makes
adapter regressions harder to isolate.

Required invariant: runtime execution must preserve the chosen strategy all the
way from config, to runtime profile, to turn execution.

## Verification Expectations

Each remediation PR should include fresh verification from the PR branch:

- `cargo fmt --all`
- `cargo check`
- targeted package tests for the changed path
- `cargo test` before submission
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` before PR

For production-facing changes, code-complete is not the same as
production-verified. Live evidence should include the relevant runtime logs,
catalog counts, state-machine transitions, or runtime-host lease behavior after
deployment.

## Non-Goals

- Do not run the one-time production schema cleanup in this PR.
- Do not change long-running server process ownership policy in this PR.
- Do not combine all runtime architecture fixes into one large PR.
