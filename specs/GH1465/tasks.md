# Task Plan

## Linked Issue

GH-1465

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1465-T001` Owner: runtime | Done when: terminal outcome `stalled` (or structured failed-reason) exists with `{reason, rounds_used, last_status, waiting_on}` payload and a single CAS-style terminal emission function enforces exactly-one terminal event per task | Verify: `cargo test -p harness-server terminal_exactly_once`
- [ ] `SP1465-T002` Owner: runtime | Done when: budget exhaustion in the spawn/executor loop transitions the task to the terminal state via the CAS function before loop exit | Verify: `cargo test -p harness-server round_budget_exhausted`
- [ ] `SP1465-T003` Owner: runtime | Done when: every early `break`/return in `task_runner/spawn.rs` (incl. workspace-lifecycle paths) records a terminal event or re-queues, with an on-exit guard asserting terminal-or-requeued | Verify: `cargo test -p harness-server spawn_exit_paths`
- [ ] `SP1465-T004` Owner: server-http | Done when: status/overview endpoints and dashboard classify `stalled` as terminal with visible reason | Verify: `cargo test -p harness-server status_stalled_terminal`
- [ ] `SP1465-T005` Owner: runtime | Done when: restart-mid-review and shutdown-drain tests prove tasks resume or terminalize, never stay in limbo | Verify: `cargo test -p harness-server recovery_no_limbo`
- [ ] `SP1465-T006` Owner: eval | Done when: an event-stream liveness audit (created == terminal after drain) runs as a deterministic check and is wired into the regression eval loop | Verify: audit check red on a synthetic limbo fixture, green on suite

## Parallelization

- T001 first (shared foundation).
- T002 ∥ T003 ∥ T004 after T001 (disjoint: executor loop vs spawn exits vs http).
- T005 after T002/T003; T006 last.

## Verification

- `cargo test --workspace`
- Synthetic fixture: turn cap 2 + always-failing review → exactly one `stalled` terminal event, status API terminal, audit green.

## Handoff Notes

- Do not reuse `completed` for exhausted budgets — downstream automation treats `completed` as success.
- GH-1438 (reconciler illegal transition) stays separate; only the on-exit guard here may overlap — coordinate reason strings.
