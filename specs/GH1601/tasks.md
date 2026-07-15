# Task Plan

## Linked Issue

GH-1601

## Spec Packet

- Product: `specs/GH1601/product.md`
- Tech: `specs/GH1601/tech.md`

## Implementation Tasks

- [ ] `SP1601-T001` Owner: `workflow-runtime-schema` | Dependencies: none | Covers: B-001, B-004, B-006, B-010, B-011, B-012 | Done when: `WorkflowCommandStatus::Deferred`, the closed typed barrier vocabulary, validated barrier construction, scheduling/attempt/generation fields, and the additive PostgreSQL migration are wired through persisted command records; legacy rows retain every prior status and receive zero/NULL defaults without fabricated barrier evidence; the named status constraint and due-command index accept only the specified new state and non-negative generation | Evidence: migration tests seed every legacy status before upgrade, typed-reason tests reject empty or mismatched evidence, and a repository-wide status search is attached to the implementation PR | Verify: `cargo test -p harness-workflow dispatch_barrier_reason`; `cargo test -p harness-workflow runtime_store_migrates_deferred_commands`; `rg -n "WorkflowCommandStatus|command_status|dispatch_lease" crates/harness-workflow/src crates/harness-server/src`
- [ ] `SP1601-T002` Owner: `workflow-runtime-store` | Dependencies: `SP1601-T001` | Covers: B-003, B-006, B-008, B-010, B-013 | Done when: command claiming includes pending, due deferred, and expired dispatching candidates under `FOR UPDATE ... SKIP LOCKED`; every successful claim atomically increments and returns a persisted generation, rejects overflow without mutation, honors `dispatch_not_before`, preserves barrier history until enqueue, and computes positive saturating exponential backoff capped at the configured finite ceiling | Evidence: deterministic store tests prove early claims are excluded, exact delay steps and ceiling, restart-safe eligibility, generation increment/overflow behavior, and competing-claimer exclusion | Verify: `cargo test -p harness-workflow deferred_command_backoff`; `cargo test -p harness-workflow deferred_command_survives_store_restart`; `cargo test -p harness-workflow command_store`
- [ ] `SP1601-T003` Owner: `workflow-runtime-store` | Dependencies: `SP1601-T002` | Covers: B-001, B-002, B-003, B-004, B-006, B-008, B-009, B-013 | Done when: `defer_claimed_command_if_owned` locks workflow and command in the established order and commits status, due time, attempt, typed barrier, lease release, and `WorkflowRuntimeDispatchDeferred` in one transaction; only the exact owner plus claim generation may mutate; exact same-generation replay is write-free `AlreadyDeferred`; stale, reclaimed, repeated, and terminal-workflow cases cannot append duplicate evidence or reopen work | Evidence: transaction-failure, stale-owner, same-owner/new-generation, exact-replay, terminal-race, and two-dispatcher tests assert both row state and event cardinality | Verify: `cargo test -p harness-workflow defer_claimed_command_is_atomic`; `cargo test -p harness-workflow defer_claimed_command_rejects_stale_owner`; `cargo test -p harness-workflow deferred_command_dispatcher_race_has_one_winner`; `cargo test -p harness-workflow terminal_workflow_wins_dispatch_race`; `cargo test -p harness-workflow failed_defer_reclaims_after_lease_expiry`
- [ ] `SP1601-T004` Owner: `workflow-runtime-store` | Dependencies: `SP1601-T003` | Covers: B-003, B-007, B-008, B-009 | Done when: claimed enqueue accepts the claim generation and, in one transaction, validates owner plus exact generation and terminal workflow state before job lookup/insert; a successful retry uses the original command ID and `(workflow_id, dedupe_key)`, clears deferred schedule/barrier metadata, and exact committed replay returns `AlreadyExists`, while any different generation returns `StaleClaim` without writes | Evidence: tests prove one job across retries/replays, preserved identity, stale generation rejection even with a reused owner string, and terminal state winning before enqueue | Verify: `cargo test -p harness-workflow deferred_command_dispatches_original_command_once`; `cargo test -p harness-workflow deferred_command_dispatcher_race_has_one_winner`; `cargo test -p harness-workflow terminal_workflow_wins_dispatch_race`
- [ ] `SP1601-T005` Owner: `workflow-runtime-dispatcher` | Dependencies: `SP1601-T003`, `SP1601-T004` | Covers: B-001, B-004, B-005, B-007, B-008, B-009 | Done when: required-tier unavailability returns `CommandDispatchOutcome::Deferred`, carries the resolved required tier and trust class into valid typed evidence, calls the fenced deferral API, creates no runtime job or agent invocation, re-resolves availability on every due retry, and never selects an available weaker tier | Evidence: dispatcher tests cover initial dispatch, due retry, successful dispatch after the required tier returns, stale claim, and terminal race with zero fallback calls | Verify: `cargo test -p harness-workflow command_dispatcher`; `cargo test -p harness-server runtime_command_dispatch_tick_defers_unavailable_isolation_without_fallback`
- [ ] `SP1601-T006` Owner: `server-runtime-dispatch` | Dependencies: `SP1601-T003`, `SP1601-T004` | Covers: B-001, B-004, B-006, B-007, B-014 | Done when: disabled dispatch/workers and invalid `WORKFLOW.md` are classified only as `runtime_policy_disabled` and `workflow_config_invalid`, respectively; each path performs fenced atomic deferral and returns/counts `Deferred`; enabling policy or repairing configuration allows the original due command to enqueue once; unrelated selection errors retain lease-expiry recovery instead of being mislabeled | Evidence: database-backed server tests assert one live deferred continuation, zero jobs/agent metrics while blocked, persisted bounded scheduling, correct latest reason across attempts, and one original-command job after each barrier clears | Verify: `cargo test -p harness-server runtime_command_dispatch_tick_defers_`; `cargo test -p harness-server runtime_command_dispatch_tick_defers_disabled_policy_without_agent_metrics`; `cargo test -p harness-server runtime_command_dispatch`
- [ ] `SP1601-T007` Owner: `runtime-observability` | Dependencies: `SP1601-T001`, `SP1601-T003`, `SP1601-T005`, `SP1601-T006` | Covers: B-001, B-004, B-010, B-011, B-012, B-014 | Done when: every active-command predicate, cancellation/supersession path, replay/PR-feedback suppression path, runtime tree/summary serialization, SSE/usage projection, and aggregate status count treats deferred as a live non-success continuation; projections expose reason code, non-empty reason, attempt, generation, due time, and isolation fields when applicable; missing, empty, mismatched, or invalid persisted barrier data returns explicit invalid evidence or an error, never invented success | Evidence: the implementation PR lists every audited exhaustive match and SQL vocabulary site, and projection tests cover valid deferred, invalid barrier JSON, legacy rows, cancellation, replay suppression, and aggregate counts | Verify: `cargo test -p harness-server runtime_tree_reports_deferred_command`; `cargo test -p harness-server github_coverage_gate`; `cargo test -p harness-server workflow_runtime_pr_feedback`; `cargo check --workspace --all-targets`

## Verification Tasks

- [ ] `SP1601-T008` Owner: `workflow-runtime-verification` | Dependencies: `SP1601-T001`, `SP1601-T002`, `SP1601-T003`, `SP1601-T004`, `SP1601-T005` | Covers: B-002, B-003, B-006, B-007, B-008, B-009, B-010, B-011, B-013 | Done when: the PostgreSQL-backed workflow-runtime suite executes the specified migration, restart, rollback, same-owner/different-generation race, exact-replay, terminal-race, backoff-ceiling, and original-identity scenarios with fresh output; any required database omission is reported as a blocker rather than treated as a pass | Evidence: implementation PR check output identifies the database target and records each named test result | Verify: `cargo test -p harness-workflow command_dispatcher`; `cargo test -p harness-workflow deferred_command`; `cargo test -p harness-workflow terminal_workflow_wins_dispatch_race`; `cargo test -p harness-workflow runtime_store_migrates_deferred_commands`
- [ ] `SP1601-T009` Owner: `server-runtime-verification` | Dependencies: `SP1601-T005`, `SP1601-T006`, `SP1601-T007` | Covers: B-001, B-004, B-005, B-007, B-012, B-014 | Done when: fresh server integration output proves all three barriers defer without a job, isolation never downgrades on initial attempt or retry, repaired barriers dispatch the original command at most once, disabled time is excluded from agent success/dispatch metrics, and valid plus invalid operator evidence is distinguishable | Evidence: implementation PR records the named server test results and shows no test was skipped or weakened | Verify: `cargo test -p harness-server runtime_command_dispatch`; `cargo test -p harness-server runtime_tree_reports_deferred_command`; `cargo test -p harness-server runtime_command_dispatch_tick_defers_disabled_policy_without_agent_metrics`
- [ ] `SP1601-T010` Owner: `release-readiness` | Dependencies: `SP1601-T008`, `SP1601-T009` | Covers: B-001, B-002, B-005, B-007, B-010, B-011, B-012, B-013, B-014 | Done when: formatting, workspace compilation, warning-denying lint, and full relevant tests pass; a reviewer verifies the stop-claimers/drain-or-expire rollout order and rollback precondition; a manual database check links one deferred row to exactly one audit event, survives a process restart, then enqueues exactly one original-command job after the barrier clears | Evidence: PR gate includes fresh command logs, the inspected command/event/job IDs, restart timestamps, and an explicit rollback verdict; absence of configured PostgreSQL or manual evidence blocks merge readiness | Verify: `cargo fmt --all -- --check`; `cargo check --workspace --all-targets`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p harness-workflow`; `cargo test -p harness-server`

## Parallelization

- `SP1601-T001` owns `crates/harness-workflow/src/runtime/status.rs`,
  `crates/harness-workflow/src/runtime/model.rs`, and
  `crates/harness-workflow/src/runtime/store_migrations.rs`. It lands first;
  no other writable lane may edit those files until its API and migration are
  stable.
- `SP1601-T002`, `SP1601-T003`, and `SP1601-T004` all own command persistence
  and tests under `crates/harness-workflow/src/runtime/store.rs` and
  `crates/harness-workflow/src/runtime/tests/`; run them serially in that order.
  They must also share one documented workflow/command lock order.
- After `SP1601-T004`, `SP1601-T005` may own
  `crates/harness-workflow/src/runtime/dispatcher.rs` and its dispatcher tests
  while `SP1601-T006` owns
  `crates/harness-server/src/http/background/runtime_command_dispatch.rs` and
  `crates/harness-server/src/http/tests/runtime_dispatch_tests.rs`. These two
  lanes may run in parallel only if they freeze the store API and do not edit
  each other's files.
- `SP1601-T007` is an integration lane across exhaustive status consumers. It
  starts only after `SP1601-T005` and `SP1601-T006` land, and must publish its
  exact file ownership before editing because it can overlap workflow-runtime
  and server projections.
- `SP1601-T008` and `SP1601-T009` are read-only execution lanes once their test
  fixtures have landed and may run in parallel against separate databases.
  `SP1601-T010` is the final serialized release-readiness lane.

## Invariant Coverage

- Product invariant set: `B-001` through `B-014`.
- Task coverage union: `B-001` through `B-014`.
- Missing invariants: none.
- Extra invariants: none.

## Handoff Notes

- This task-plan PR contains no implementation. Do not close GH-1601 when it
  merges; implementation and verification tasks remain open.
- Treat `dispatch_claim_generation` as an unconditional fence. Owner equality,
  lease timestamps, or legacy generation zero are not compatibility substitutes.
- Preserve the existing workflow business state while infrastructure is
  unavailable. Do not broaden this issue into a generic blocked transition,
  recovery endpoint, watchdog, isolation installer, or retry policy for
  unrelated terminal commands.
- Use the existing workflow/command transaction lock order for defer, enqueue,
  cancellation, and terminal-state races. A new order requires explicit
  deadlock analysis before implementation.
- Rollback is safe for an old binary only after claims stop and no deferred rows
  remain, or after an explicit compatibility migration converts deferred rows
  to pending while preserving command ID and dedupe key. Never convert them to
  skipped or failed.
- Required-isolation behavior is security-sensitive and requires human review.
  No implementation PR may claim merge readiness without fresh negative tests
  for both the initial attempt and a due retry.
