# GH1607 Task Plan

## Linked Issue

GH-1607 (related: GH-1609 declarative definitions, GH-1603 progress ownership)

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1607-T001` Owner: `policy-model` | Done when: `PromptContinuationPolicy` (max_attempts, attempt_delay_secs, active_states, no_progress_limit) exists in `runtime/prompt_task.rs` with `validate()` enforcing bounds and non-empty active_states; submission input carries `Option<&PromptContinuationPolicy>`; policy + counters persist under `instance.data["continuation"]` at submission (B-001, B-002, B-009) | Verify: `cargo test -p harness-workflow prompt_task`
- [ ] `SP1607-T002` Owner: `reducer` | Done when: `prompt_task_success_decision` replaces the fixed done arm in `reducer.rs` with branches for no-policy (unchanged), settled → done, active → continue with attempt-scoped `enqueue_activity` command and counter updates committed atomically with the transition, malformed signal → blocked, exhaustion → blocked, no-progress → blocked; every branch attaches `external_state` evidence (B-004..B-008, B-013) | Verify: `cargo test -p harness-workflow reducer::prompt_continuation`
- [ ] `SP1607-T003` Owner: `validator` | Done when: `prompt_task_defaults()` allows `implementing → implementing` only for `continue_prompt_task` decisions carrying an enqueue command, and `implementing → blocked` for the three continuation-block decisions (GH-1603 alignment) | Verify: `cargo test -p harness-workflow validator`
- [ ] `SP1607-T004` Owner: `persistence` | Done when: store round-trip tests prove counters/policy survive restart and re-reduction of a committed completion is idempotent via attempt-scoped dedupe keys; stale-completion guard covers cancel-between-attempts (B-009, B-012) | Verify: `cargo test -p harness-workflow store`
- [ ] `SP1607-T005` Owner: `server-prompt` | Done when: submission endpoint accepts/rejects continuation policies with actionable errors and the implement prompt packet for attempt > 1 carries attempt number, previous external state, and previous attempt summary; `attempt_delay_secs` maps to the command's not-before dispatch time (B-002, B-011) | Verify: `cargo test -p harness-server workflow_runtime_submission prompt_packet`
- [ ] `SP1607-T006` Owner: `e2e` | Done when: an end-to-end test drives submit → attempt 1 (active) → attempt 2 (settled) → done, plus exhaustion and malformed-signal paths, through the stub runtime (B-004..B-007) | Verify: `cargo test -p harness-server workflow_runtime`

## Parallelization

- `SP1607-T001` first (types feed everything).
- `SP1607-T002` and `SP1607-T003` in parallel after T001 (disjoint files:
  reducer vs validator).
- `SP1607-T004` after T002; `SP1607-T005` after T001 (server files,
  disjoint from harness-workflow); `SP1607-T006` last.

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p harness-workflow -p harness-server`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1607`

## Handoff Notes

The instance-data patch mechanism for counters (atomic with the state
transition) is the one open implementation detail — pin it during T002
against how existing reducers persist decision side effects; the
invariant is B-004/B-009 atomicity, not the mechanism. The signal shape
(`external_state` with a string `state` field) is shared vocabulary with
GH-1609's `on_signal` mapping; keep the parser reusable. Harness must not
gain any tracker/HTTP client for this feature (B-003) — the agent reports
state, the runtime decides.
