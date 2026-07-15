# GH1608 Tech Spec: Owned Workflow Definition Registry

Product spec: `specs/GH1608/product.md`
GitHub issue: `#1608`
Related: GH-1609 (consumer of the registration API), GH-1603 (progress
contracts will attach per-state metadata to registry entries)

## Codebase Context (verified anchors)

- Static tables: `crates/harness-workflow/src/runtime/state_registry.rs:53-58`
  (`WORKFLOW_DEFINITION_IDS`), `:60-157` (four `const` state tables built
  from `WorkflowStateDefinition::active/terminal`).
- `&'static str` types: `state_registry.rs:15-25` (`WorkflowStateKey`,
  `WorkflowStateDefinition` with `definition_id: &'static str`,
  `state: &'static str`), const constructors `:27-51`.
- Free-function API: `state_registry.rs:159-196`
  (`known_workflow_definition_ids`, `workflow_states_for_definition`,
  `workflow_terminal_state_names_for_definition`,
  `workflow_state_definition`, `workflow_state_exists`,
  `workflow_state_terminal_state`).
- Consumers of that API: `runtime/mod.rs`, `runtime/model.rs`
  (`WorkflowInstance::terminal_state` at `model.rs:120-122`),
  `runtime/store/instances.rs`, `runtime/terminal_state.rs`.
- Validator allowlists (generic engine, hardcoded defaults):
  `crates/harness-workflow/src/runtime/validator.rs:59-104`
  (`TransitionAllowlist`), per-definition constructors `:105`
  (`github_issue_pr_defaults`), `:260` (`quality_gate_defaults`), `:278`
  (`pr_feedback_defaults`), `:297` (`prompt_task_defaults`), selection
  constructors `:439-456`, hardcoded selection in the reducer at
  `runtime/reducer.rs:459`.
- Definition-id constants: `runtime/reducer.rs:50`
  (`GITHUB_ISSUE_PR_DEFINITION_ID`), `runtime/prompt_task.rs:3`,
  `runtime/quality_gate.rs` and `runtime/pr_feedback.rs` equivalents
  (imported at `state_registry.rs:1-4`).

## Proposed Design

### Owned types

```rust
// state_registry.rs
pub struct WorkflowStateKey {
    pub definition_id: String,
    pub state: String,
}

pub struct WorkflowStateDefinition {
    pub key: WorkflowStateKey,
    pub terminal_state: Option<WorkflowTerminalState>,
}

pub struct WorkflowDefinition {
    pub id: String,
    pub states: Vec<WorkflowStateDefinition>,
    pub allowlist: TransitionAllowlist,
}
```

Const constructors go away; built-in tables become functions returning
owned `WorkflowDefinition`s (`github_issue_pr_definition()`, ...), each
pairing the existing state list with the existing
`DecisionValidator::*_defaults()` allowlist (B-002, B-008).

### Registry

```rust
pub struct WorkflowDefinitionRegistry {
    definitions: HashMap<String, Arc<WorkflowDefinition>>,
    frozen: bool,
}
```

- Process-wide handle: `static REGISTRY: OnceLock<RwLock<WorkflowDefinitionRegistry>>`,
  initialized on first access with the four built-ins already seeded
  (B-003) — seeding inside the `OnceLock` initializer makes "empty
  registry observed" unrepresentable.
- `register(def: WorkflowDefinition) -> anyhow::Result<()>`: errors on
  duplicate id (B-004) or when frozen (B-007).
- `freeze()`: called by server startup after config-driven registration
  (GH-1609) and by test harnesses; post-freeze `register` errors.
- Reads clone `Arc<WorkflowDefinition>` out of a short read-lock; no lock
  is held across await points (B-006). The existing free functions keep
  their signatures where possible; the two returning `&'static` slices
  change to owned/`Arc` returns:
  - `workflow_states_for_definition(&str) -> Arc<[WorkflowStateDefinition]>`
    (or `Vec` clone; states are small),
  - `known_workflow_definition_ids() -> Vec<String>`.
  Callers are updated mechanically (~5 files).

### Validator selection through the registry

`DecisionValidator` selection at `reducer.rs:459` switches from a
hardcoded match to `registry.allowlist_for(definition_id)`, falling back
to today's behavior for unknown ids. The four `*_defaults()`
constructors remain the single source of built-in rule content — they are
called once at seeding, not deleted (B-001, B-008).

### What does NOT change

- `WorkflowInstance` (already owns `definition_id: String`,
  `model.rs:76-91`).
- Reducer completion logic, dispatcher, store schemas, worker.
- Public behavior for unknown definition ids (B-005): empty state list,
  `None` terminal lookups.

## Edge Cases

- Two subsystems racing first access of the `OnceLock`: initializer runs
  once; both observe the fully-seeded registry (B-003).
- A test registering a fixture definition twice: second call errors;
  tests needing re-registration use a fresh registry via a test-only
  constructor (`WorkflowDefinitionRegistry::new_for_tests()`), not the
  global (B-004).
- Freeze ordering in binaries that never load config definitions (CLI
  tools, tests using the store directly): freeze is idempotent and the
  built-ins are always present pre-freeze, so consumers that never call
  `freeze()` still see a correct (just unfrozen) registry; only
  registration attempts after processing starts are a bug this catches
  opportunistically (B-007 is enforced at the server startup seam).

## Migration / Compatibility

- Compile-time only: no persisted data, wire format, or config change.
- `&'static` → owned return types are source-breaking inside the
  workspace only; all consumers are in-repo (~5 files).
- Equivalence test snapshots the old tables (hardcoded expectation in the
  test, copied from the current consts) against registry output so drift
  during the refactor is caught mechanically (B-001).

## Verification Plan

- `cargo test -p harness-workflow state_registry` — equivalence test:
  ids, state sets, terminal mappings, and allowlist rules for all four
  built-ins match the pre-refactor tables exactly (B-001, B-008).
- `cargo test -p harness-workflow` + `cargo test -p harness-server` —
  full behavior-preservation gate (B-001).
- New unit tests: duplicate registration errors (B-004), post-freeze
  registration errors (B-007), unknown-id lookups (B-005).
- `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, `cargo check --workspace --all-targets`.

## Rollback Plan

No persisted or wire surface: revert the commits and the static tables
return. GH-1609 must not merge before this lands (dependency ordering is
one-way).

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
|---|---|---|
| B-001 | whole refactor | full `harness-workflow` + `harness-server` suites + equivalence test |
| B-002 | owned types in `state_registry.rs` | compile-time; grep gate: no `&'static str` in registry types |
| B-003 | `OnceLock` initializer seeds built-ins | `cargo test -p harness-workflow state_registry` (first-access test) |
| B-004 | `register()` duplicate check | `cargo test -p harness-workflow state_registry::duplicate` |
| B-005 | lookup fallbacks | `cargo test -p harness-workflow state_registry::unknown_id` |
| B-006 | short read-lock reads | review gate + no `.await` inside lock scope (clippy `await_holding_lock`) |
| B-007 | `freeze()` semantics | `cargo test -p harness-workflow state_registry::freeze` |
| B-008 | allowlist registered with states | equivalence test compares `rules()` output per definition |
