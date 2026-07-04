# Tech Spec

## Linked Issue

GH-1469

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Config default | `crates/harness-core/src/config/misc.rs:142,188` | `max_turns: Option<u32> = None` | Default must become Some(N) |
| Turn check | `crates/harness-server/src/task_executor/run_task.rs:410-411` | Enforced only when Some | Enforcement point exists |
| Transient retry | `crates/harness-server/src/task_runner/spawn.rs:49,813-925` | ≤2 re-runs, 30/60s backoff | One multiplying layer |
| Agent review loop | `crates/harness-server/src/task_executor/agent_review.rs:146-154,507-532` | ≤3 rounds × 2 calls | One multiplying layer |
| Review-bot loop | `crates/harness-server/src/task_executor/review_loop.rs:204` | re-invokes implementor per round | One multiplying layer |
| Periodic retry | `crates/harness-server/src/periodic_retry.rs:78-133` | re-enqueues failed tasks, count persisted | One multiplying layer |
| Task DB | `crates/harness-server/src/task_db/` | persists task state | Home for the invocation counter |

## Proposed Design

1. **Default `max_turns`** — change the unset case to a documented default
   (proposal: 40) resolved in `resolve.rs`, keeping explicit configs intact.
2. **Invocation counter** — add `agent_invocations` column to the tasks table
   (migration). Increment in one chokepoint: the spawn path in
   `task_runner/spawn.rs` that all layers already flow through.
3. **Budget enforcement** — new config `concurrency.invocation_budget`
   (default proposal: 60). Checked at the same chokepoint before spawn; on
   exhaustion, mark task failed with `TaskFailureReason::InvocationBudgetExhausted`
   (new variant) at error level.
4. **Layer visibility** — the failure reason includes the per-layer breakdown
   available from existing retry counters where cheap.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 total cap across layers | spawn chokepoint counter | test driving transient retry + review rounds past budget |
| P2 explicit terminal reason | failure-reason enum + task_db | test asserts terminal state + reason string |
| P3 explicit configs untouched | config resolve | unit test: Some(n) survives resolve |

## Alternatives Considered

- Alert-only (no hard stop) — rejected: bill keeps growing while nobody watches.
- Per-layer caps only — rejected: layers multiply; only a shared ceiling bounds the product.
