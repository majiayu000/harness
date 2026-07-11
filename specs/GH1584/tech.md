# Tech Spec

## Linked Issue

GH-1584

## Product Spec

`specs/GH1584/product.md`

## Codebase Context (verified anchors)

| Area | Anchor | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Operator recovery handlers | `crates/harness-server/src/http/task_mutation_routes.rs:222` (`unblock_workflow_runtime`), `:231` (`retry_workflow_runtime`), `:260` (`recover_workflow_runtime`) | Body-based operator actions dispatch to the recover module and map outcomes to 200/404/409/500/503. | Auto-recovery must reuse the same outcome semantics, not the HTTP layer. |
| Routes | `crates/harness-server/src/http/http_router.rs:90-97` | `/api/workflows/runtime/unblock` and `/retry` registered behind `auth::api_auth_middleware` (`:198-201`). | API contract stays unchanged (B-003, B-013). |
| Recovery core | `crates/harness-server/src/workflow_runtime_submission/recover.rs:77` (`unblock_submission_by_workflow_id`), `:86` (`retry_...`), `:130` (`recover_submission`), `:33` (`RECOVERY_STATE = "planning"`), `:22` (`ACTIVE_STOP_FIELDS`), `:227` (`clear_active_stop_fields`) | Validates definition + state, appends an audit event first (`:157`), commits a decision with a plan-activity re-enqueue (`:170-190`), preserves `last_stop`, writes `last_recovery`. | This is the single eligibility path a successful recheck must re-run (B-005, B-008). |
| Non-retryable gate | `crates/harness-server/src/workflow_runtime_submission/recover.rs:205` (`non_retryable_error_kind`) | `fatal` and `configuration` reject retry. | Seed of the terminal class for failed workflows. |
| Decision commit | `crates/harness-server/src/workflow_runtime_submission.rs:239` (`commit_runtime_decision`) | Validates the decision against the in-memory instance state, records rejected decisions, then upserts. No store-level compare-and-swap on `state`. | Race hardening for B-009 lands here or in the store (see Edge Cases). |
| Stop metadata producers | `crates/harness-workflow/src/runtime/reducer/support.rs:150` (`runtime_blocked_payload`), `:163` (`runtime_failed_payload`), `:180` (`runtime_stop_metadata`), `:208` (`failed_retry_hint`) | Blocked/failed payloads persist `blocked_reason`/`failure_reason`, hints, `error_kind`, `last_stop`. Reasons are free text; `error_kind` is the only machine-stable field. | Classification needs a stable `stop_reason_code` written here. |
| Blocked/failed decision sites | `crates/harness-workflow/src/runtime/reducer/runtime_failure.rs:29` and `:52`; `crates/harness-workflow/src/runtime/reducer/github_issue_completion.rs:40`, `:98` | All transitions into `blocked`/`failed` flow through `runtime_blocked_command`/`runtime_failed_command` (`support.rs:122`, `:136`). | Single choke point to attach reason codes. |
| Error-kind taxonomy | `crates/harness-workflow/src/runtime/model.rs:652` (`ActivityErrorKind`: `Retryable`, `SpawnFailure`, `Timeout`, `Fatal`, `Configuration`, `ExternalDependency`, `Unknown`) | Serialized snake_case into stop payloads. | Basis for classifying failed workflows. |
| Coverage gate | `crates/harness-server/src/intake/github_coverage_gate.rs:34` (`runtime_issue_state_is_covered`), `:101` (`check_github_issue_coverage`) | `blocked`/`failed` are covered, so intake skips the issue until state changes. | Recovery to `planning` clears the hold; the gate itself is untouched. |
| Per-repo config precedent | `crates/harness-core/src/config/intake.rs:7` (`GitHubRepoConfig`), `:18` (`auto_merge: Option<bool>` per-repo opt-in), `:168` (global `auto_merge: GitHubAutoMergeConfig`), `:189` (`effective_repos`) | Per-repo `Option<bool>` overrides a global policy section, default OFF. | Exact precedent for `auto_recovery`. |
| Background worker placement | `crates/harness-server/src/http/background/system_recovery.rs:5` (`spawn_system_task_recovery`), re-exported at `crates/harness-server/src/http/background.rs:75`, spawned at `crates/harness-server/src/http/mod.rs:237` | Recovery-style background loops live under `http/background/` and are spawned during server startup. | Recheck scheduler goes here. |
| Periodic retry escalation | `crates/harness-server/src/periodic_retry.rs:31` (`start`), `:42` (`retry_loop`), `:57` (`run_retry_tick`), `~:302` (already-escalated stuck handling) | Stalled-task scanner with tick loop, escalation dedupe, and summary events. | Pattern for tick cadence, dedupe of repeated escalation, and summary evidence. |
| Operator monitor | `crates/harness-server/src/handlers/operator_monitor.rs:95` (`StuckWorkflow`), `:138` (handler), `:257` (`list_stuck_workflows`) | Stopped workflows surface to operators with state/age; no reason-class or attempt info. | Exposure surface for class, attempts, next recheck. |
| Existing tests | `crates/harness-server/src/workflow_runtime_submission/recover_tests.rs` | Covers manual unblock/retry outcomes. | Extend for automated-actor path and races. |

To locate during implementation: (a) whether `WorkflowRuntimeStore::upsert_instance`
(`crates/harness-workflow/src/runtime/store/instances.rs`) can enforce an
`expected_state` guard for the CAS in B-009; (b) the GH-1582 escalation event
naming convention once that issue lands (cross-reference only; emit a stable
event type here).

## Proposed Design

### 1. Reason classification (harness-workflow)

New module `crates/harness-workflow/src/runtime/reason_class.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReasonClass {
    Transient,
    Terminal,
}

/// Single classifier used by scheduler, API serialization, and audit events
/// (B-001). Fail closed: anything unrecognized is Terminal (B-002).
pub fn classify_stop(
    stop_reason_code: Option<&str>,
    error_kind: Option<ActivityErrorKind>,
) -> StopReasonClass
```

Classification table (initial):

| Input | Class |
| --- | --- |
| `stop_reason_code`: `rate_limited`, `ci_backend_unavailable`, `merge_base_drift`, `circuit_breaker_cooldown` | Transient |
| `error_kind`: `retryable`, `timeout`, `spawn_failure`, `external_dependency` | Transient |
| `stop_reason_code`: `maintainer_input_required`, `invalid_agent_output`, anything else | Terminal |
| `error_kind`: `fatal`, `configuration`, `unknown` | Terminal |
| both absent / legacy rows | Terminal (B-002, B-014) |

`runtime_blocked_payload` / `runtime_failed_payload` (`support.rs:150`, `:163`)
gain an optional `stop_reason_code` parameter threaded from decision sites and
persist `stop_reason_code` + derived `reason_class` into instance data and
`last_stop`. Additive JSON only; no migration. Blocked decision sites that
represent "waiting for a human" (e.g. `github_issue_completion.rs:40`) set
terminal codes explicitly.

### 2. Policy config (harness-core)

Follow the `auto_merge` precedent in `crates/harness-core/src/config/intake.rs`:

```toml
[intake.github.auto_recovery]
enabled = false            # global default OFF
max_attempts = 3
initial_backoff_secs = 300
max_backoff_secs = 14400
jitter_ratio = 0.2
tick_interval_secs = 60
```

Plus per-repo `auto_recovery: Option<bool>` on `GitHubRepoConfig` (mirrors
`intake.rs:18`); effective policy = repo override else global flag. Validation
at load (B-016): when enabled, `max_attempts >= 1`,
`initial_backoff_secs <= max_backoff_secs`, `jitter_ratio` in `[0, 1]`.

### 3. Attempt state (persisted in instance data)

Written under an `auto_recovery` object in workflow instance `data` (additive,
survives restart — B-006/B-007):

```json
{
  "auto_recovery": {
    "episode_event_id": "evt-of-current-stop",
    "attempts": 2,
    "next_attempt_at": "2026-07-11T10:00:00Z",
    "last_outcome": "recheck_failed"
  }
}
```

`episode_event_id` ties attempts to the current stop episode; a new stop
episode (different `last_stop.event_id`) resets the counter (B-011).
`clear_active_stop_fields` (`recover.rs:227`) additionally clears
`auto_recovery` on successful recovery.

### 4. Recheck scheduler (harness-server)

New `crates/harness-server/src/http/background/auto_recovery.rs`, spawned from
`http/mod.rs` next to `spawn_system_task_recovery` (`mod.rs:237`), gated on the
config flag so the task is not even spawned when globally disabled (B-003).
Each tick:

1. List runtime instances in `blocked`/`failed` for opted-in repos.
2. Skip unless `classify_stop(...) == Transient` (B-004, B-012).
3. Skip if `next_attempt_at` is in the future or `attempts >= max_attempts`.
4. Append an `AutoRecoveryAttempt` audit event (reason class, attempt number)
   BEFORE any action (B-008), then run the recheck: re-read remote facts /
   the transient condition where checkable, and call the existing
   `unblock_submission_by_workflow_id` / `retry_submission_by_workflow_id`
   (`recover.rs:77`, `:86`) with an automated actor reason such as
   `"auto-recovery attempt 2/3: rate_limited cooldown elapsed"` (B-005).
5. Map outcomes: `Recovered` → append `AutoRecoveryOutcome{succeeded}`;
   `WrongState` → outcome `superseded`, stop scheduling, do not increment
   (B-009); recheck condition still failing → increment attempts, persist
   `next_attempt_at = now + min(max, initial * 2^attempts) * (1 ± jitter)`,
   outcome `recheck_failed` (B-015); store error → error-level log, no
   counter consumption.
6. On `attempts == max_attempts` with no success: append one
   `AutoRecoveryExhausted` event (idempotent per episode) for GH-1582
   alerting; instance stays stopped for the operator monitor (B-010).

Race hardening (B-009): before committing, `recover_submission` already
re-checks `instance.state` (`recover.rs:144`), but `commit_runtime_decision`
(`workflow_runtime_submission.rs:239`) has no store-level CAS. Add an
`expected_state` guard to the instance upsert path (location to confirm in
`store/instances.rs`); a guard failure maps to `WrongState`/`superseded`.

### 5. Exposure

`StuckWorkflow` (`operator_monitor.rs:95`) and the runtime tree gain optional
`reason_class`, `auto_recovery_attempts`, `next_recheck_at`,
`auto_recovery_exhausted` fields (serde-optional; absent for legacy rows).

## Edge Cases

- Store unavailable at tick: log error, reschedule, attempt not consumed.
- Restart mid-backoff: schedule derives entirely from persisted
  `next_attempt_at`; no in-memory timer state.
- Repo opts out mid-episode: tick filter excludes the repo; stale
  `auto_recovery` data is inert and cleared on the next stop/recovery.
- Concurrent manual unblock: automated attempt observes `WrongState`,
  records `superseded`, exits (B-009).
- Issue closed during backoff: recovery re-enters `planning`; the plan
  activity produces a fresh structured blocker (same as manual path).
- Config enabled but no repos opted in: scheduler runs, matches nothing.

## Compatibility with GH-1567 API

Request/response shapes of `/api/workflows/runtime/unblock` and `/retry` are
unchanged. `RuntimeRecoverOutcome` and `RuntimeRecoverError` are reused, not
forked. New JSON fields are additive with `#[serde(default)]` /
`skip_serializing_if` so existing dashboard consumers keep working.

## Product-to-Test Mapping

| Invariant | Area | Verification |
| --- | --- | --- |
| B-001 | reason_class | `cargo test -p harness-workflow reason_class` — table test: every `stop_reason_code`/`error_kind` combination maps to one class; scheduler/API/audit call the same fn (grep-guard test asserting single classifier export) |
| B-002 | reason_class | unit test: `None`/empty/garbage codes → `Terminal`; serialized payload includes the class |
| B-003 | scheduler | `cargo test -p harness-server auto_recovery` — with global flag off (default config), spawn is skipped and a seeded blocked instance is untouched after N ticks |
| B-004 | scheduler | tick test: terminal-classified blocked instance is never selected; transient one is |
| B-005 | recover core | test: automated recovery result equals manual `unblock_submission_by_workflow_id` result (state `planning`, plan command enqueued, `last_stop` preserved, active fields cleared), diffing only actor/reason |
| B-006 | attempt state | test: persist attempts=`max`, restart-simulated fresh scheduler → no further attempts; counter read from instance data, not memory |
| B-007 | attempt state | test: `next_attempt_at` computed with exponential growth capped at `max_backoff_secs`, jitter within `[1-r, 1+r]`; fresh scheduler honors persisted future timestamp |
| B-008 | audit | test: mock store failing the state commit — `AutoRecoveryAttempt` event exists, no transition; ordering asserted via event log sequence |
| B-009 | concurrency | test: instance state changed between listing and recovery → `WrongState` mapped to `superseded`, attempts not incremented, single decision recorded; store-guard test once CAS lands |
| B-010 | escalation | test: exhausting attempts emits exactly one `AutoRecoveryExhausted` per episode (idempotent across ticks); instance still listed by `list_stuck_workflows` |
| B-011 | attempt state | test: new stop episode (different `last_stop.event_id`) resets attempts to 0 |
| B-012 | reason_class + scheduler | test: `error_kind = fatal` with policy enabled and a forged `stop_reason_code = rate_limited` — classifier returns Terminal for failed+fatal and scheduler skips (classifier precedence test) |
| B-013 | HTTP | existing `recover_tests.rs` suite green unchanged; new test: manual unblock succeeds while a recheck is pending |
| B-014 | compat | test: legacy instance data without `stop_reason_code`/`reason_class` → classified Terminal, monitor serialization omits optional fields |
| B-015 | scheduler | test: failed recheck increments counter, reschedules with larger backoff, no state transition, outcome event `recheck_failed` |
| B-016 | config | `cargo test -p harness-core intake` — invalid policy values rejected at config load with descriptive errors |

## Verification Plan

- `cargo check --workspace` during implementation.
- `cargo test -p harness-workflow reason_class`,
  `cargo test -p harness-server auto_recovery`,
  `cargo test -p harness-server recover`,
  `cargo test -p harness-core intake`.
- Pre-PR: `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --all -- --check` (repo CI gates).

## Rollback Plan

Remove the scheduler spawn and config section; classification fields are
additive JSON in instance data and serde-optional in APIs, so no migration is
needed. Manual GH-1567 recovery is untouched at every rollback stage.
