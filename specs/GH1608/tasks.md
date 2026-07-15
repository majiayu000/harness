# GH1608 Task Plan

## Linked Issue

GH-1608 (prerequisite for GH-1609; related: GH-1603)

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1608-T001` Owner: `types` | Done when: `WorkflowStateKey` / `WorkflowStateDefinition` own their strings, const constructors are replaced by owned builders, and the four built-in tables become `*_definition()` functions returning `WorkflowDefinition { id, states, allowlist }` (B-002, B-008) | Verify: `cargo check -p harness-workflow`
- [ ] `SP1608-T002` Owner: `registry` | Done when: `WorkflowDefinitionRegistry` exists behind a `OnceLock<RwLock<..>>` whose initializer seeds the four built-ins; `register()` errors on duplicate id and post-freeze; `freeze()` is idempotent; a test-only fresh-registry constructor exists (B-003, B-004, B-007) | Verify: `cargo test -p harness-workflow state_registry`
- [ ] `SP1608-T003` Owner: `api-migration` | Done when: the free-function API reads through the registry with owned/`Arc` return types and all in-repo consumers (`runtime/mod.rs`, `runtime/model.rs`, `runtime/store/instances.rs`, `runtime/terminal_state.rs`) compile against it with unchanged semantics for unknown ids (B-005, B-006) | Verify: `cargo test -p harness-workflow`
- [ ] `SP1608-T004` Owner: `validator-selection` | Done when: `DecisionValidator` selection at `reducer.rs:459` resolves the allowlist via the registry; the `*_defaults()` constructors remain the single source of built-in rule content, called once at seeding (B-008) | Verify: `cargo test -p harness-workflow validator reducer`
- [ ] `SP1608-T005` Owner: `equivalence-gate` | Done when: an equivalence test hardcodes the pre-refactor ids, state sets, terminal mappings, and per-definition transition rules and asserts registry output matches exactly; server startup calls `freeze()` after registration (B-001, B-003, B-007) | Verify: `cargo test -p harness-workflow state_registry::equivalence && cargo test -p harness-server`

## Parallelization

- `SP1608-T001` first; `SP1608-T002` after T001.
- `SP1608-T003` and `SP1608-T004` in parallel after T002 (disjoint files:
  registry consumers vs validator selection).
- `SP1608-T005` last as the preservation gate.

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p harness-workflow -p harness-server`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1608`

## Handoff Notes

Strictly an enabling refactor: zero observable behavior change is the
acceptance bar, enforced by the equivalence test plus the full existing
suites. GH-1609 consumes `register()`/`freeze()`; GH-1603's progress
contracts should attach per-state metadata to `WorkflowStateDefinition`
once both land — keep the struct open for an additive field. The
equivalence test's hardcoded expectations must be copied from the current
const tables before they are deleted, in the same PR, so the source of
truth never disappears between commits.
