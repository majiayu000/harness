# Tech Spec

## Linked Issue

GH-1437

## Product Spec

See `specs/GH1437/product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Stall detection (exists) | `crates/harness-server/src/task_executor/turn_lifecycle.rs:280-310` | `select!` arm sleeps until `last_activity + stall_timeout`, fails turn with "Agent stream stalled: no output for {}s" | Mechanism already implemented for the task-executor path |
| Stall default (root cause) | `crates/harness-core/src/config/misc.rs:118-120,169` | `concurrency.stall_timeout_secs` default is **3600** — identical to the wall-clock default, so the stall arm effectively never fires first | The observed 3600 s failures are the stall/wall race collapsing into one hour-long wait |
| Wall-clock timeout | `turn_lifecycle.rs:297-310` | `execution_timeout` from `options.timeout_secs` → "Agent turn timed out after {timeout_secs}s" | Message that dominates production failures; must stay distinct from stall reason |
| Per-task override | `crates/harness-server/src/task_runner/request.rs:56-58,152,191,229` | `stall_timeout_secs` override plumbed per task | Invariant 6 clamping goes here |
| Workflow runtime jobs | `crates/harness-server/src/workflow_runtime_worker/` + `crates/harness-agents/src/codex_adapter.rs` | Runtime jobs run agent turns with activity-profile `timeout_secs` (3600 default in `harness-core/src/config/workflow.rs` activity profiles); stall coverage for the codex_exec path must be audited — `stall_timeout` plumbing is only visible in the task-executor path | Invariant 5: the dominant production traffic is workflow runtime, not task executor |
| Failure classes | #1430 design (`workflow_runtime_worker.rs` WorkerTickStats) | No failure classification yet | Stall reason string should be a stable class seed |

## Proposed Design

1. Change `default_stall_timeout_secs()` to 600 (10 min) and add a validation step in config load: `stall_timeout_secs >= wall-clock timeout` logs a warning and clamps to `wall-clock - 1s`; floor at 60 s.
2. Audit the workflow-runtime job execution path for stall coverage: if runtime jobs bypass `turn_lifecycle.rs` (e.g. codex_exec adapter drives the stream itself), lift the stall `select!` arm into the shared stream-consumption layer (`harness-agents` adapters or the worker's turn driver) so both paths share one implementation.
3. Keep the two failure strings distinct and stable: `Agent stream stalled: no output for {n}s` vs `Agent turn timed out after {n}s`; document both as failure-class seeds for #1430 (`stall-silent` vs `wall-clock-timeout`).
4. Surface the stall window in effect at turn start (debug log) so incident forensics can confirm configuration.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 stall aborts early | `turn_lifecycle.rs` + shared driver | Mock stream: no items → failure within window (test clock) |
| P2 default ordering enforced | `config/misc.rs` validation | Unit test: equal/larger stall default clamps + warns |
| P3 activity resets clock | stall arm reset on item | Test: item every window/2 → runs to wall-clock |
| P4 distinct reasons | failure strings | Assert both strings; `GET /tasks` fixture |
| P5 uniform coverage | workflow runtime path | Runtime-job test with silent codex_exec stream |
| P6 override clamped | `task_runner/request.rs` | Unit test: override > wall-clock → clamped with warning |

## Data Flow

Input: agent stream items (stdout/events) per turn; config `concurrency.stall_timeout_secs`, per-task override, activity-profile `timeout_secs`. Output: turn failure with classified reason → task status, runtime job result, logs. No new persistence; failure strings flow into existing `runtime_jobs` error columns.

## Alternatives Considered

- Application-level heartbeat protocol (agent must emit keepalives) — requires CLI cooperation across three runtimes; stream-activity proxy achieves the goal without protocol changes.
- Lowering wall-clock `timeout_secs` globally — kills legitimately long productive turns; rejected in the issue.
- TCP keepalive tuning on upstream connections — helps some hangs but not silent-but-connected sessions; complementary, out of scope.

## Risks

- Security: none.
- Compatibility: deployments that intentionally raised nothing but have slow-starting agents (e.g. cold container pulls > 10 min before first output) would now fail; mitigate with the pre-first-output grace equal to `max(stall, 600)` or document raising the key.
- Performance: negligible (one timer per turn).
- Maintenance: single shared stall implementation avoids the current risk of path-specific drift.

## Test Plan

- [ ] Unit tests: config validation/clamping, floor, override clamp.
- [ ] Integration tests: silent stream stall abort (task executor and workflow runtime paths), activity-reset behavior, post-abort late-result rejection.
- [ ] Manual verification: replay a proxy-drop window locally (kill upstream mid-turn) and confirm failure within the stall window with the stall reason in `GET /tasks`.

## Rollback Plan

Set `concurrency.stall_timeout_secs = 3600` (old effective behavior) in deployment config; no schema or data changes to revert.
