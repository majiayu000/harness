# GH1609 Tech Spec: Declarative Workflow Definitions

Product spec: `specs/GH1609/product.md`
GitHub issue: `#1609`
Depends on: GH-1608 (registry). Related: GH-1607 (shared signal
vocabulary), GH-1603 (progress ownership).

## Codebase Context (verified anchors)

- WORKFLOW.md loading and deep-merge:
  `crates/harness-core/src/config/workflow.rs:516-612` (per-repo file
  deep-merged field-by-field over the central base registered at startup,
  `crates/harness-cli/src/commands.rs:752-765`).
- Config shape: `config/workflow.rs:235-266` (`WorkflowConfig`,
  `activities: BTreeMap<String, WorkflowActivityPolicy>`).
- Hardcoded success-transition table:
  `crates/harness-workflow/src/runtime/reducer.rs:287-322`; blocked
  fallback for unexpected completions `reducer.rs:313-321`
  (`invalid_agent_output_blocked_decision`); validator selection
  `reducer.rs:459`.
- Generic transition engine:
  `crates/harness-workflow/src/runtime/validator.rs:59-104`
  (`TransitionAllowlist`, `TransitionRule`), evidence-gated terminal
  rules exemplar `validator_github_issue_pr.rs:57-80` (reconciliation
  done requires `github_pr` evidence).
- Structured result contract:
  `crates/harness-workflow/src/runtime/model.rs:679-683`
  (`ActivitySignal`), `model.rs:718-732` (`ActivityResult`), commands
  `model.rs:197-254`, instance pinning field `model.rs:78-79`
  (`definition_version: u32`).
- Dispatch profile selection (already string-keyed):
  `crates/harness-workflow/src/runtime/dispatcher.rs:85`
  (`select(definition_id, activity)`).
- Submission seam: `crates/harness-server/src/workflow_runtime_submission.rs`
  (prompt path at `:198`; declarative path added alongside).
- Registry (from GH-1608): `runtime/state_registry.rs` —
  `WorkflowDefinition { id, states, allowlist }`, `register()`,
  `freeze()`.

## Proposed Design

### Config schema (harness-core, `config/workflow.rs`)

```rust
pub struct WorkflowDefinitionPolicy {
    pub id: String,
    pub initial: String,
    pub states: BTreeMap<String, DeclaredState>,
    pub terminal: BTreeMap<String, String>, // state -> succeeded|failed|cancelled
    pub evidence_required: BTreeMap<String, Vec<String>>, // state -> evidence kinds
    pub recovery_targets: Vec<String>,      // states operators may unblock into
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredProgressMode {
    ExternalWait,
    OperatorGate,
}

pub struct DeclaredState {
    pub activity: Option<String>,
    pub progress: Option<DeclaredProgressMode>,
    pub on_success: Option<String>,
    pub on_failure: Option<String>,
    pub on_blocked: Option<String>,
    pub on_signal: BTreeMap<String, String>,   // signal_type -> next state
}
```

- New `WorkflowConfig` field: `definition: Option<WorkflowDefinitionPolicy>`
  (`#[serde(default)]`, B-001).
- Merge semantics: in the deep-merge at `workflow.rs:591-612`, `definition`
  is treated atomically — a repo-level value replaces the base value
  wholesale; there is no per-field merge of the block (B-012). This is an
  explicit carve-out in the merge function with a unit test.
- Parse-level checks only in harness-core (serde + non-empty fields);
  graph validation lives in harness-workflow (dependency direction:
  harness-workflow already depends on harness-core, not vice versa).

### Structural validation + registration (harness-workflow, new `runtime/declarative.rs`)

`build_declarative_definition(policy, activity_policies) -> anyhow::Result<WorkflowDefinition>`:

1. Every transition target (`on_success`/`on_failure`/`on_blocked`/
   `on_signal` values, `initial`, `recovery_targets`) names a declared
   state (B-003).
2. Reachability from `initial` covers every declared state (B-003).
3. Terminal coverage: at least one state maps to `succeeded` and one to
   `cancelled`; every terminal class referenced by any routing edge must
   be mapped; terminal states appear in `terminal` only and carry no
   outgoing edges (B-003).
4. Non-terminal states declare exactly one progress mode: `activity`
   XOR `progress` (B-003, GH-1603).
5. Declared activities exist in the `activities` policy map (B-010).
6. Id does not collide with built-ins or prior registrations (B-003;
   double-checked by `register()`).
7. Output: `WorkflowDefinition` whose `TransitionAllowlist` is generated
   from the declared edges — one `TransitionRule` per edge, plus
   evidence requirements from `evidence_required` (B-009) and
   operator-recovery rules from `recovery_targets` restricted to the
   operator action context (B-013).

Server startup: after config load, for each managed project's resolved
WORKFLOW.md, build + `register()` declarative definitions, then
`freeze()` (B-004). Any error aborts startup (B-002).

### Content-hash pinning

- `definition_version: u32` = truncated (lower 32 bits) SHA-256 of the
  canonical JSON serialization of `WorkflowDefinitionPolicy`; the full
  hex hash is stored in `instance.data["definition_hash"]` at creation
  for collision-proof auditing (B-005).
- Truncation-collision guard: at startup, if two distinct declarations
  for the same `id` (full SHA-256 differs) truncate to the same u32
  `definition_version`, startup aborts with an actionable error naming
  both hashes — a collision must never make version lookup ambiguous or
  silently reinterpret a pinned instance (B-005). Interpretation
  cross-checks the full hash in `instance.data["definition_hash"]` when
  present, so even an undetected collision fails closed to
  `definition_version_missing`.
- The registry keeps `(id, version) -> Arc<WorkflowDefinition>` for every
  version seen at startup. Interpretation looks up the instance's pinned
  version; a missing version (WORKFLOW.md edited between restarts while
  the instance was in flight) transitions the instance to `blocked` with
  decision `definition_version_missing` — surfaced, never silently
  reinterpreted under the new shape (B-005). Operators resolve by
  reverting the definition or cancelling the instance.

### Generic interpreting reducer (`runtime/reducer.rs`)

At the top of `reduce_success` (before the hardcoded match at
`reducer.rs:287`): if `instance.definition_id` resolves to a declarative
definition, delegate to `declarative::reduce(definition, instance, event,
result)`:

- Status Succeeded: check `on_signal` matches from `result.signals`
  (declared mapping wins, B-008); else `on_success`; missing route for an
  observed completion → `invalid_agent_output_blocked_decision`
  (B-011).
- Status Blocked/Failed: `on_blocked`/`on_failure` when declared; else
  fall through to the existing generic blocked/retry decisions
  (`runtime_failure.rs` paths) unchanged (B-008).
- Emitted decision: next state + driving command derived from the target
  state's progress mode — `enqueue_activity` for activity states (dedupe
  key `"{id}:{state}:{event.id}"`), `Wait` for `external_wait`,
  `RequestOperatorAttention` for `operator_gate`, and for terminal
  states the existing command per terminal class — `succeeded` →
  `MarkDone` (there is no `MarkSucceeded`; see
  `WorkflowCommandType`, `model.rs:197-208`), `failed` → `MarkFailed`,
  `cancelled` → `MarkCancelled` — always in the same decision (B-007).
- Built-in ids never resolve as declarative (registry namespaces are
  disjoint by B-003 collision check), so the hardcoded paths are
  untouched (B-006).

### Validator

No new engine: the generated `TransitionAllowlist` from GH-1608's
registry drives `DecisionValidator` for declarative ids (selection seam
`reducer.rs:459`). Evidence enforcement reuses the rule shape used by
`validator_github_issue_pr.rs` terminal-evidence checks, generalized to
"transition into state S requires evidence kinds [...]" (B-009).

### Submission (harness-server)

`workflow_runtime_submission.rs` gains a declarative path: a submission
naming a registered declarative `definition_id` creates an instance in
the declaration's `initial` state (pinned version/hash) and emits the
initial state's driving command, following the prompt-task submission
shape at `:198`. The initial command has no completion event, so its
dedupe key uses the submission form `"{workflow_id}:{initial_state}:submit"`
— unique per instance, stable across submission retries for the same
instance id (the `{event.id}` form applies only to completion-driven
transitions). Dependencies (`depends_on`) reuse the existing
`awaiting_dependencies` handling only if declared; v1 keeps dependency
gating out of declarative definitions (submission rejects `depends_on`
for declarative ids with a clear error).

## Edge Cases

- Declaration valid at startup, activity policy removed in a later
  restart: startup validation re-runs on every boot; the boot fails
  (B-002/B-010) instead of stranding dispatch.
- Signal type declared both in `on_signal` and produced alongside
  success: declared mapping wins deterministically; multiple mapped
  signals on one result → the lexicographically smallest mapped
  `signal_type` wins (matching `BTreeMap` iteration order, so precedence
  needs no extra dependency and cannot drift from the data structure) and
  the decision reason records the tie (deterministic, auditable).
- Two repos declare the same definition id: second registration fails
  startup (B-003) — ids are process-global in v1; the error message
  tells operators to namespace ids per repo.
- WORKFLOW.md edited and server restarted mid-flight: old-version
  instances hit `definition_version_missing` → blocked with operator
  guidance (B-005); new instances run the new shape.

## Migration / Compatibility

- `WorkflowConfig.definition` is `#[serde(default)]` optional: existing
  WORKFLOW.md files parse unchanged (B-001).
- No schema migration: pinning reuses `definition_version` and
  `instance.data`.
- Registry freeze ordering depends on GH-1608 landing first.

## Verification Plan

- harness-core: parse + atomic-merge tests (`definition` never
  field-merges, B-012); optional-field compat (B-001).
- harness-workflow unit: structural validation matrix — undeclared
  target, unreachable state, missing terminal class, dual/missing
  progress mode, unknown activity, id collision (B-002/B-003/B-010);
  interpreter branches — success route, signal precedence, failure/
  blocked routes, unexpected completion fallback (B-008, B-011); driver
  command per progress mode (B-007); evidence rejection (B-009);
  recovery gating (B-013).
- Pinning: version lookup across simulated definition change;
  `definition_version_missing` → blocked (B-005).
- harness-server: declarative submission e2e through the stub runtime —
  submit → activity completes with mapped signal → terminal state, with
  built-in suites untouched as the B-006 gate.
- Full gates: `cargo check --workspace --all-targets`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`.

## Rollback Plan

Feature is inert without a `definition` block (B-001). Rollback = remove
the block; in-flight declarative instances finish under pinned versions
or are cancelled by operators. Code revert removes the config field,
`declarative.rs`, and the reducer delegation line; built-in paths are
untouched throughout (B-006).

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
|---|---|---|
| B-001 | optional config field | `cargo test -p harness-core config::workflow` (existing files parse) |
| B-002 | startup build+register abort | `cargo test -p harness-workflow declarative::validation` + server boot test |
| B-003 | structural validation matrix | `cargo test -p harness-workflow declarative::validation` |
| B-004 | startup registration before freeze | `cargo test -p harness-server` boot ordering test |
| B-005 | content hash + version lookup | `cargo test -p harness-workflow declarative::pinning` |
| B-006 | delegation guard on declarative ids | full built-in suites green + interpreter unreachable-for-builtin test |
| B-007 | driver command per progress mode | `cargo test -p harness-workflow declarative::interpreter` |
| B-008 | routing precedence | `cargo test -p harness-workflow declarative::interpreter` |
| B-009 | generated evidence rules | `cargo test -p harness-workflow declarative::evidence` |
| B-010 | activity-policy cross-check | `cargo test -p harness-workflow declarative::validation` |
| B-011 | unexpected completion fallback | `cargo test -p harness-workflow declarative::interpreter` |
| B-012 | atomic merge carve-out | `cargo test -p harness-core config::workflow::definition_merge` |
| B-013 | recovery target gating | `cargo test -p harness-workflow declarative::recovery` |
