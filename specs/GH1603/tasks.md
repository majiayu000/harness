# Task Plan

## Linked Issue

GH-1603

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1603-T001` Owner: `workflow-state-contract` | Dependencies: none | Covers: B-001, B-005, B-010, B-011 | Done when: `WorkflowStateDefinition` carries exactly one typed progress mode for every registered nonterminal state, terminal states remain explicitly terminal, unknown definitions/states fail closed, the four built-in definition tables encode the authoritative ownership matrix exactly, and completeness plus semantic tests prove every allowlisted target has a real durable-driver family, enabled observer, authorized operator action, or parent propagation hook rather than a structurally present label | Verify: `cargo test -p harness-workflow state_registry -- --nocapture && cargo test -p harness-workflow registry_covers_validator_transition_states -- --nocapture && cargo test -p harness-workflow progress_mode_semantics -- --nocapture && cargo test -p harness-server progress_wake_paths -- --nocapture`
- [ ] `SP1603-T002` Owner: `decision-validator` | Dependencies: `SP1603-T001` | Covers: B-002, B-003, B-004, B-005, B-006, B-008 | Done when: the shared decision-validation path requires every decision command payload to be a JSON object before any field lookup, rejects scalar and array payloads with stable validation evidence, accepts a command-driven target only when the same decision contains an allowlisted runtime-job-producing `EnqueueActivity` or `StartChildWorkflow`, treats omitted and empty command lists identically, rejects wait/audit/inline/bookkeeping commands as drivers, preserves valid non-command modes and terminal transitions, applies the same rule to authorized recovery/retry/replay, and cannot borrow stale, unrelated, or dedupe-colliding work | Verify: `cargo test -p harness-workflow rejects_driverless_command_driven_transitions -- --nocapture && cargo test -p harness-workflow scalar_and_array_command_payloads_are_rejected -- --nocapture && cargo test -p harness-workflow empty_and_omitted_commands_share_progress_rejection -- --nocapture && cargo test -p harness-workflow wait_and_inline_commands_do_not_drive_progress -- --nocapture && cargo test -p harness-workflow allows_declared_non_command_progress_modes -- --nocapture && cargo test -p harness-workflow runtime_recovery_requires_progress_driver -- --nocapture && cargo test -p harness-workflow stale_active_work_does_not_satisfy_progress -- --nocapture`
- [ ] `SP1603-T003` Owner: `workflow-decision-ingestion` | Dependencies: `SP1603-T001`, `SP1603-T002` | Covers: B-002, B-006, B-007, B-008 | Done when: structured agent decisions, submission/bootstrap, operator recovery, and reconciliation all use the same progress-contract validator, the initial submission driver uses the exact dedupe key `{workflow_id}:{initial_state}:submit`, later transition drivers use their existing event-derived keys without reusing the initial submission namespace, `ProgressDriverMissing` remains a stable auditable rejection reason, and any separate blocked policy decision retains evidence that distinguishes it from the rejected requested transition | Verify: `cargo test -p harness-workflow decision_validator -- --nocapture && cargo test -p harness-workflow submission_driver_dedupe_key -- --nocapture && cargo test -p harness-workflow event_transition_dedupe_keys -- --nocapture && cargo test -p harness-workflow reducer -- --nocapture && cargo test -p harness-workflow runtime_recovery_requires_progress_driver -- --nocapture`
- [ ] `SP1603-T004` Owner: `workflow-persistence` | Dependencies: `SP1603-T002`, `SP1603-T003` | Covers: B-007, B-008 | Done when: completion persistence validates before mutating the instance or inserting commands, a rejected driverless decision leaves requested state and version unchanged, enqueues no requested command or runtime job, durably records the rejection, and concurrent completion/replay fixtures prove only commands linked to the accepted state-entry decision can satisfy ownership | Verify: `cargo test -p harness-workflow driverless_completion_is_rejected_atomically -- --nocapture && cargo test -p harness-workflow stale_active_work_does_not_satisfy_progress -- --nocapture && cargo test -p harness-workflow driverless_progress -- --nocapture`
- [ ] `SP1603-T005` Owner: `workflow-diagnostics-store` | Dependencies: `SP1603-T001` | Covers: B-008, B-009 | Done when: one bounded snapshot query reports registered command-driven instances lacking an eligible active command or unfinished runtime job linked by `decision_id` to the newest accepted decision that established the persisted current state, reports missing or ambiguous provenance conservatively, excludes stale/rejected/null/unrelated/dedupe-colliding/terminal evidence, orders the newest accepted state-entry decision by persisted workflow-event sequence (or explicitly classifies equal or ambiguous ordering as ambiguous) and never by UUID ordering, returns workflow id, definition, state, age, and provenance status, and never mutates workflow state or history | Verify: `cargo test -p harness-workflow driverless_progress -- --nocapture`
- [ ] `SP1603-T006` Owner: `operator-diagnostics` | Dependencies: `SP1603-T005` | Covers: B-005, B-009, B-011 | Done when: the existing watchdog/operator-monitor evidence path exposes bounded `driverless_progress` rows with workflow id, definition, state, age, and provenance status, enforcement remains independent of watchdog enablement, diagnostic failures are not silently swallowed, and explicit recovery remains the only repair path | Verify: `cargo test -p harness-server driverless_progress -- --nocapture && cargo test -p harness-server progress_wake_paths -- --nocapture`
- [ ] `SP1603-T007` Owner: `cross-surface-verification` | Dependencies: `SP1603-T001`, `SP1603-T002`, `SP1603-T003`, `SP1603-T004`, `SP1603-T005`, `SP1603-T006` | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011 | Done when: positive and negative matrices cover all four definitions and every authoritative progress-ownership row, DB-backed provenance fixtures use a dedicated non-production `HARNESS_DATABASE_URL`, focused workflow/server suites pass, the workspace compiles, formatting and warning-sensitive clippy pass, and SpecRail validates both this packet and the repository | Verify: `cargo test -p harness-workflow decision_validator -- --nocapture && cargo test -p harness-workflow state_registry -- --nocapture && cargo test -p harness-workflow driverless_progress -- --nocapture && cargo test -p harness-server driverless_progress -- --nocapture && cargo check --workspace --all-targets && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && python3 checks/check_workflow.py --repo . --spec-dir specs/GH1603 && python3 checks/check_workflow.py --repo .`

## Parallelization

- `SP1603-T001` owns `crates/harness-workflow/src/runtime/state_registry.rs` and its focused tests. It must land before validation or semantic-matrix work that consumes the new progress contract.
- After `SP1603-T001`, `SP1603-T002`/`SP1603-T003` may share one sequential validator lane, while `SP1603-T005` runs in a disjoint store-diagnostics lane. The validator lane owns `validator.rs`, `reducer.rs`, and submission/recovery call sites; the diagnostics lane owns the read-only store query and its fixtures. These lanes must not edit the same module.
- `SP1603-T004` must run after the validator lane because it owns atomic completion persistence and related transaction tests. It must not overlap a diagnostics lane edit to the same store module; split helpers into disjoint files first or execute the store tasks sequentially.
- `SP1603-T006` may begin after `SP1603-T005` stabilizes its typed result. It owns the watchdog/operator-monitor server projection and server tests, with no workflow-store edits.
- `SP1603-T007` is the final integration lane and starts only after every implementation task has landed. No parallel lane may weaken tests, share a writable file, or substitute an older command/job for decision-scoped ownership.

## Verification

- [ ] Product invariant set is exactly `B-001` through `B-011`, and the union of task `Covers:` fields is exactly `B-001` through `B-011`.
- [ ] `cargo test -p harness-workflow decision_validator -- --nocapture`
- [ ] `cargo test -p harness-workflow scalar_and_array_command_payloads_are_rejected -- --nocapture`
- [ ] `cargo test -p harness-workflow submission_driver_dedupe_key -- --nocapture`
- [ ] `cargo test -p harness-workflow event_transition_dedupe_keys -- --nocapture`
- [ ] `cargo test -p harness-workflow state_registry -- --nocapture`
- [ ] `cargo test -p harness-workflow driverless_progress -- --nocapture`
- [ ] `cargo test -p harness-server driverless_progress -- --nocapture`
- [ ] `cargo check --workspace --all-targets`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1603`
- [ ] `python3 checks/check_workflow.py --repo .`

DB-backed verification must use the repository's dedicated non-production `HARNESS_DATABASE_URL`. If no safe database is configured, record those commands as pending CI evidence rather than pointing tests at an unknown database.

## Handoff Notes

- This task plan implements the authoritative progress ownership matrix in `product.md`; transition allowlists do not infer or override a state's progress mode.
- The newest accepted decision that established the current state must be selected by persisted workflow-event sequence. UUID ordering is forbidden. If the persistence model cannot prove a unique newest state-entry decision because sequence values are equal or otherwise ambiguous, diagnostics must report ambiguous provenance instead of selecting one heuristically.
- An active command/job proves health only when its command is linked by `decision_id` to that accepted state-entry decision and its runtime semantics can advance the state. Workflow identity, command type, activity name, dedupe key, or older work is insufficient.
- Validate every decision command payload as a JSON object before reading command-specific fields. Scalar and array payloads must fail validation explicitly; they must never be interpreted through missing-field fallbacks.
- Pin only the initial submission driver to `{workflow_id}:{initial_state}:submit`. Later transition drivers retain event-derived dedupe keys so an initial submission key cannot collide with or masquerade as later state-entry provenance.
- Rejection and diagnostics are fail-closed and auditable. Historical diagnostics are read-only; rollout must not mutate state, erase history, or auto-recover rows.
- B-005 and B-011 require executable wake/resolution paths, not merely enum values or descriptive reasons. Keep the semantic matrix test tied to enabled observers, callable operator actions, and recorded parent propagation.
- Preserve existing authorization, readiness, review, merge, release, and security gates. GH-1603 implementation must not expand authority.
