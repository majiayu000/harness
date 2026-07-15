# Tech Spec

## Linked Issue

GH-1603

## Product Spec

See [`product.md`](product.md).

## Current System

| Area | Verified anchor | Current behavior |
| --- | --- | --- |
| Decision model | `crates/harness-workflow/src/runtime/model.rs:368` | `WorkflowDecision.commands` is serde-defaulted and `WorkflowDecision::new` initializes it as empty at line 397. |
| State registry | `crates/harness-workflow/src/runtime/state_registry.rs:21` | `WorkflowStateDefinition` records only state identity and optional terminal classification; the four definition tables begin at lines 60, 92, 114, and 135. |
| Transition policy | `crates/harness-workflow/src/runtime/validator.rs:13` | `TransitionRule` records allowed commands, but not whether a target requires a progress driver. Several command-driven transitions are declared at lines 127-191. |
| Command validation | `crates/harness-workflow/src/runtime/validator.rs:541` | Existing commands are validated individually; `required_command_for_transition` at lines 723-737 covers selected PR/batch/terminal transitions rather than all command-driven states. |
| Command provenance | `crates/harness-workflow/src/runtime/model.rs:329` | `WorkflowCommandRecord.decision_id` identifies the creating decision; the runtime tables persist that link in `store_migrations.rs:41-72`, and accepted completion commands receive the new record id in `store/runtime_completion.rs:90-103`. |
| Agent decision ingestion | `crates/harness-workflow/src/runtime/reducer.rs:414` | A `workflow_decision` artifact is deserialized and accepted when the definition validator returns success at lines 455-468. |
| Atomic completion | `crates/harness-workflow/src/runtime/store/runtime_completion.rs:59` | Accepted follow-up commands are inserted at lines 90-101 and the next state is persisted at lines 102-104; an accepted empty list therefore creates no outbox work. |
| Historical diagnostics | `crates/harness-workflow/src/runtime/store/instances.rs:458` | The current store can list aged instances for a caller-supplied state list, but it does not classify driverless command-driven instances. |
| Watchdog surface | `crates/harness-server/src/http/workflow_watchdog.rs:8` | The watchdog considers only `blocked` and `awaiting_feedback`, so driverless automated progress states are outside its query. |
| Operator monitor | `crates/harness-server/src/handlers/operator_monitor.rs:271` | The stuck-workflow projection is built from the same aged wait-state query at lines 285-298. |

## Proposed Design

### 1. Register progress ownership

Add a typed `WorkflowProgressMode` to `WorkflowStateDefinition`:

- `CommandDriven`: entering the state requires at least one durable driver.
- `ExternalWait`: a named background or external observer owns the wake-up.
- `OperatorGate`: an explicit operator action owns resolution.
- `ParentHandoff`: child outcome propagation owns resolution.
- Terminal states continue to use `terminal_state` and do not carry a nonterminal
  progress mode.

Enumerate the mode for every state in all four definition tables. Expose a lookup
helper beside `workflow_state_definition`; unknown definitions/states return no
contract and fail validation. Keep the enum internal to the workflow crate unless
the operator projection needs a serialized display value.

The exhaustive assignment is the Authoritative Progress Ownership Matrix in
`product.md`; implementation must encode that matrix directly. In particular,
`quality_gate.pending` and `pr_feedback.pending` are command-driven bootstrap states,
the three PR-feedback result states are parent handoffs, and the two definitions'
`blocked` states are operator gates. An allowlist accepting `Wait` or an empty command
list does not change a state's mode.

### 2. Validate the target's driver contract

After locating the allowlist rule and validating command types/payloads, resolve the
target state definition. For `CommandDriven`, require at least one command whose
runtime semantics create claimable durable work. Initially this is the closed set of
runtime-job-producing commands (`EnqueueActivity` and `StartChildWorkflow`), derived
through the existing `requires_runtime_job()` behavior rather than duplicated string
checks.

Do not count `Wait`, `RecordPlanConcern`, `BindPr`, terminal markers, or operator
attention as drivers. Progress validation does not expand `allowed_commands`; both
checks must pass. Add a dedicated rejection kind such as
`ProgressDriverMissing` so persistence and operator evidence are stable.

Apply the same validator path to normal completion, submission, operator recovery,
and reconciliation. Existing terminal command requirements and workflow-specific
evidence checks remain additive.

### 3. Preserve atomic failure semantics

Validation remains before command insertion and instance update in the store-owned
transaction. A rejected decision record carries `ProgressDriverMissing`; its
requested state and commands are not applied. Where the reducer converts invalid
agent output into a separate blocked decision, retain evidence linking the rejected
artifact/reason to the blocked outcome so the two decisions are not conflated.

### 4. Detect historical violations

Add a read-only store query for registered `CommandDriven` instances. For each row,
derive state-entry provenance from the newest accepted workflow decision, ordered by
persisted decision creation order: it must name the instance's current state as
`decision.next_state`. A missing newest accepted decision, or one whose target does
not match the persisted current state, is `missing_state_entry_provenance` and is
reported conservatively.

Only a runtime-job-producing command whose `workflow_commands.decision_id` equals
that state-entry decision id may prove ownership. The command must be active
(`pending`, `dispatching`, or `dispatched`) or have an unfinished joined runtime job
(`pending` or `running`). Commands linked to rejected/older decisions, commands with
`decision_id IS NULL`, jobs reached through those commands, terminal work, and rows
that merely share workflow id, command type, activity, or dedupe key do not count.
This also makes a dedupe conflict that retained an older `decision_id` visible rather
than healthy. Use decision-scoped `NOT EXISTS` predicates inside one snapshot query
to avoid a commands/jobs observation race. Bound and order results by age and id.

Surface these rows through the existing operator-monitor/watchdog evidence path with
workflow id, definition, state, age, and a stable classification such as
`driverless_progress`. Detection logs/reports only; recovery remains an explicit
operator action. The diagnostic must not depend on the watchdog being enabled to
enforce new decisions, because validation is the correctness boundary.

### 5. Exhaustive contract tests

Extend the registry completeness test to enumerate every registry entry and
allowlist source/target and assert that it has either terminal metadata or exactly
one progress mode. Add a table-driven semantic fixture that reproduces every row in
the product matrix and proves that the named owner/wake path is real: eligible
command family for `CommandDriven`, registered observer and selector for
`ExternalWait`, callable authorized action for `OperatorGate`, and recorded parent
identity plus propagation hook for `ParentHandoff`. A nonempty enum value alone is
not sufficient. Add validator tests across the four definitions, reducer tests for
structured agent output, store transaction tests, recovery tests, and provenance-
aware operator diagnostic tests.

## Data Flow

```text
activity/submission/recovery decision
  -> transition allowlist
  -> command/payload/budget/dedupe validation
  -> target progress-mode validation
  -> accepted: event + decision + commands + instance update in one transaction
  -> rejected: auditable reason; requested state/outbox unchanged

existing workflow rows
  -> newest accepted current-state-entry decision
  -> decision_id-scoped NOT EXISTS command/job diagnostic
  -> bounded operator evidence + error log
  -> explicit existing recovery API when an operator chooses
```

No new external call or database column is required. The diagnostic reads existing
workflow, command, and runtime-job tables.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Executable verification |
| --- | --- | --- |
| B-001 every nonterminal state has one mode | `state_registry.rs` enum/tables | `cargo test -p harness-workflow state_registry -- --nocapture` |
| B-002 command-driven target requires a driver | `validator.rs` progress validation | `cargo test -p harness-workflow rejects_driverless_command_driven_transitions -- --nocapture` |
| B-003 omitted and empty commands are equivalent | model deserialization + validator table tests | `cargo test -p harness-workflow empty_and_omitted_commands_share_progress_rejection -- --nocapture` |
| B-004 non-driving commands do not count | validator command classification tests | `cargo test -p harness-workflow wait_and_inline_commands_do_not_drive_progress -- --nocapture` |
| B-005 declared non-command modes remain valid | registry/validator positive matrix | `cargo test -p harness-workflow allows_declared_non_command_progress_modes -- --nocapture` |
| B-006 recovery cannot bypass the contract | recovery validator/store tests | `cargo test -p harness-workflow runtime_recovery_requires_progress_driver -- --nocapture` |
| B-007 rejection is atomic and auditable | `store/runtime_completion.rs` DB transaction tests | `cargo test -p harness-workflow driverless_completion_is_rejected_atomically -- --nocapture` |
| B-008 concurrency/replay cannot borrow stale work | validator dedupe + DB snapshot tests | `cargo test -p harness-workflow stale_active_work_does_not_satisfy_progress -- --nocapture` |
| B-009 historical rows use current-state-entry provenance and are reported, not mutated | store diagnostic + operator monitor tests, including older/null/unrelated `decision_id` fixtures | `cargo test -p harness-server driverless_progress -- --nocapture` |
| B-010 allowlist/registry completeness | exhaustive registry test | `cargo test -p harness-workflow registry_covers_validator_transition_states -- --nocapture` |
| B-011 every mode has its declared executable wake path | registry semantic matrix plus observer/action/propagation tests | `cargo test -p harness-workflow progress_mode_semantics -- --nocapture` and `cargo test -p harness-server progress_wake_paths -- --nocapture` |

## Alternatives Considered

- Require at least one command for every nonterminal transition. Rejected because
  external waits, operator gates, and parent handoffs intentionally have no
  claimable runtime work.
- Put required command sets directly on every transition rule. This is precise but
  duplicates the state ownership policy across many source transitions and makes a
  new target state easy to configure inconsistently. State-level progress metadata
  plus the existing per-transition command allowlist keeps the two concerns
  separate.
- Rely on the aged workflow watchdog. Rejected because it detects failures after a
  delay, is currently scoped to two wait states, and can be disabled.
- Treat any active command/job already associated with the workflow as sufficient.
  Rejected because stale or unrelated work can mask a driverless transition and
  introduces a race between validation and persistence.

## Risks

- **Security:** No new authority is introduced. Recovery remains behind its existing
  authorization path; B-006 prevents that authority from bypassing validation.
- **Compatibility:** Existing custom decision producers may rely on permissive empty
  command lists. The negative diagnostic and table-driven positive tests identify
  intentional wait/handoff states before enforcement.
- **Performance:** The historical diagnostic joins three runtime tables. Use bounded
  `NOT EXISTS` queries and existing status/index columns; inspect `EXPLAIN` in a DB
  integration test if production-shaped fixtures show sequential scans.
- **Maintenance:** Incorrect state classification could block legitimate progress.
  Exhaustive registry/allowlist tests and one closed enum centralize this decision.
- **Operations:** Historical rows may be numerous. Report in bounded batches and do
  not auto-recover them.

## Test Plan

- [ ] Unit: progress-mode lookup, all-state completeness, exact authoritative matrix,
      missing/empty command equivalence, driver/non-driver matrix, unknown
      definition/state fail-closed.
- [ ] Unit: positive external-wait, operator-gate, parent-handoff, and terminal
      transition matrix across all four definitions; each row proves the named
      observer, authorized action, or propagation hook rather than only enum shape.
- [ ] Unit: structured `workflow_decision` with `implementing -> replanning` and no
      driver is rejected or converted into an auditable blocked policy decision.
- [ ] Integration: runtime completion rejection preserves instance state/version,
      inserts no requested command/job, and persists the rejection reason.
- [ ] Integration: recovery/replay with no driver is rejected even for an authorized
      actor; a valid driver succeeds exactly once.
- [ ] Integration: historical diagnostic recognizes only active/unfinished work
      linked to the newest accepted decision that establishes the current state;
      stale, unrelated, rejected, null-provenance, dedupe-colliding, terminal, and
      state-mismatched fixtures remain reportable and no row is mutated.
- [ ] Server: operator evidence includes stable `driverless_progress` classification
      and remains bounded.
- [ ] Compile and policy checks:

```sh
cargo test -p harness-workflow decision_validator -- --nocapture
cargo test -p harness-workflow state_registry -- --nocapture
cargo test -p harness-workflow driverless_progress -- --nocapture
cargo test -p harness-server driverless_progress -- --nocapture
cargo check --workspace --all-targets
python3 checks/check_workflow.py --repo . --spec-dir specs/GH1603
```

DB-backed tests require the repository's dedicated `HARNESS_DATABASE_URL` test
database and must never target production.

## Rollback Plan

Revert the progress-mode metadata, validation branch, and diagnostic projection as
one change. No data migration rollback is needed because the design adds no columns
and never rewrites historical rows. Decisions rejected while enforcement was active
remain in the audit log; operators may explicitly retry them after rollback. Do not
remove or rewrite those records.
