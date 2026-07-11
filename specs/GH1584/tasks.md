# GH1584 Task Plan

## Linked Issue

GH-1584

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1584-T001` Owner: `reason-class` | Done when: `StopReasonClass` and the single `classify_stop` classifier land in `harness-workflow` with the full classification table, failing closed to `Terminal` for unknown/missing input (B-001, B-002, B-012, B-014) | Verify: `cargo test -p harness-workflow reason_class`
- [ ] `SP1584-T002` Owner: `stop-metadata` | Done when: blocked/failed payload builders (`support.rs:150`, `:163`) persist `stop_reason_code` and derived `reason_class` into instance data and `last_stop`, with terminal codes set explicitly at human-wait decision sites; additive JSON only | Verify: `cargo test -p harness-workflow runtime_failure`
- [ ] `SP1584-T003` Owner: `policy-config` | Done when: `[intake.github.auto_recovery]` global section (default OFF) plus per-repo `auto_recovery: Option<bool>` on `GitHubRepoConfig` exist with load-time validation of attempts/backoff/jitter bounds (B-003, B-016) | Verify: `cargo test -p harness-core intake`
- [ ] `SP1584-T004` Owner: `attempt-state` | Done when: `auto_recovery` attempt object (episode id, attempts, `next_attempt_at`, last outcome) persists in instance data, resets per stop episode, and is cleared by successful recovery (B-006, B-007, B-011) | Verify: `cargo test -p harness-server auto_recovery_state`
- [ ] `SP1584-T005` Owner: `scheduler` | Done when: `http/background/auto_recovery.rs` tick loop selects only transient-classified instances of opted-in repos, appends the `AutoRecoveryAttempt` audit event before acting, reuses `unblock_submission_by_workflow_id`/`retry_submission_by_workflow_id`, applies bounded backoff+jitter, and is not spawned when globally disabled (B-004, B-005, B-008, B-015) | Verify: `cargo test -p harness-server auto_recovery`
- [ ] `SP1584-T006` Owner: `race-hardening` | Done when: recovery commit carries an `expected_state` guard (store location per tech.md "to locate"), guard failure maps to `superseded` without consuming an attempt, and manual unblock during a pending recheck wins cleanly (B-009, B-013) | Verify: `cargo test -p harness-server recover`
- [ ] `SP1584-T007` Owner: `escalation` | Done when: exhausted episodes emit exactly one idempotent `AutoRecoveryExhausted` event (GH-1582 alerting consumer), and the instance remains visible in the operator monitor as today (B-010) | Verify: `cargo test -p harness-server auto_recovery_exhaust`
- [ ] `SP1584-T008` Owner: `exposure-docs` | Done when: `StuckWorkflow` and runtime tree expose optional `reason_class`, attempt count, `next_recheck_at`, `auto_recovery_exhausted`; `docs/usage-guide.md` documents the opt-in policy and its default-OFF stance | Verify: `cargo test -p harness-server operator_monitor` and docs review in PR

## Parallelization

- `SP1584-T001` first (classifier is the shared dependency).
- `SP1584-T002` (harness-workflow) and `SP1584-T003` (harness-core) are
  disjoint crates and can run in parallel after T001.
- `SP1584-T004` -> `SP1584-T005` are serial (scheduler consumes attempt
  state). `SP1584-T006` can proceed in parallel with T005 in the recover/store
  files, distinct from the new scheduler module.
- `SP1584-T007` and `SP1584-T008` last, after the scheduler exists.

## Verification

- `cargo check --workspace`
- `cargo test -p harness-workflow -p harness-core -p harness-server`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`

## Handoff Notes

Feature ships globally disabled and requires per-repo opt-in. Terminal
classification is the fail-closed default; expanding the transient table is a
config/spec change, never a runtime inference. Escalation events are the
integration point for GH-1582 alerting — keep the event type name stable once
merged.
