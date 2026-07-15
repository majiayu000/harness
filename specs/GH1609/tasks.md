# GH1609 Task Plan

## Linked Issue

GH-1609 (depends on GH-1608; related: GH-1607, GH-1603)

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1609-T001` Owner: `config-schema` | Done when: `WorkflowDefinitionPolicy` / `DeclaredState` land in `harness-core/src/config/workflow.rs` as an optional `definition` field with parse-level checks, and the deep-merge treats `definition` atomically (repo value replaces base wholesale) with a unit test proving no field-merge (B-001, B-012) | Verify: `cargo test -p harness-core config`
- [ ] `SP1609-T002` Owner: `validation` | Done when: `runtime/declarative.rs` builds a `WorkflowDefinition` from a policy with the full structural matrix — undeclared targets, unreachability, terminal coverage, progress-mode XOR, activity-policy cross-check, id collisions — each with an actionable error and zero partial registration (B-002, B-003, B-010) | Verify: `cargo test -p harness-workflow declarative::validation`
- [ ] `SP1609-T003` Owner: `registration-pinning` | Done when: server startup builds and registers declarative definitions before `freeze()`, `definition_version` is the truncated SHA-256 of the canonical policy JSON with the full hash in `instance.data`, the registry serves `(id, version)` lookups, and a missing pinned version blocks the instance with `definition_version_missing` (B-004, B-005) | Verify: `cargo test -p harness-workflow declarative::pinning && cargo test -p harness-server`
- [ ] `SP1609-T004` Owner: `interpreter` | Done when: `reduce_success` delegates declarative ids to `declarative::reduce` implementing routing precedence (declared signal > on_success; on_failure/on_blocked with generic fallbacks), driver commands per progress mode in the same decision, and the unexpected-completion blocked fallback; built-in ids provably never enter the interpreter (B-006, B-007, B-008, B-011) | Verify: `cargo test -p harness-workflow declarative::interpreter reducer`
- [ ] `SP1609-T005` Owner: `validator-evidence` | Done when: generated `TransitionAllowlist` rules drive `DecisionValidator` for declarative ids including `evidence_required` enforcement and operator-recovery gating restricted to declared targets (B-009, B-013) | Verify: `cargo test -p harness-workflow declarative::evidence declarative::recovery`
- [ ] `SP1609-T006` Owner: `submission` | Done when: the runtime submission path accepts a registered declarative `definition_id`, creates a pinned instance in the declared initial state with its driving command, and rejects `depends_on` for declarative ids with a clear error (B-004, B-005) | Verify: `cargo test -p harness-server workflow_runtime_submission`
- [ ] `SP1609-T007` Owner: `e2e-docs` | Done when: an end-to-end stub-runtime test drives a fixture WORKFLOW.md definition submit → mapped-signal transition → terminal state; WORKFLOW.md documentation gains a worked `definition` example (B-001..B-013 integration) | Verify: `cargo test -p harness-server workflow_runtime && python3 checks/check_workflow.py --repo . --spec-dir specs/GH1609`

## Parallelization

- `SP1609-T001` first; `SP1609-T002` after T001.
- `SP1609-T003` after T002 and after GH-1608 lands (registry API).
- `SP1609-T004` and `SP1609-T005` in parallel after T003 (disjoint:
  reducer/declarative module vs validator wiring).
- `SP1609-T006` after T003; `SP1609-T007` last.

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p harness-core -p harness-workflow -p harness-server`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1609`

## Handoff Notes

Hard ordering: GH-1608 must merge first (registry `register()`/`freeze()`
and owned types). The signal vocabulary (`signal_type` matching) is
shared with GH-1607 — keep the parser in one place. v1 scope fences to
respect during implementation: no child workflows, no candidate fanout,
no dependency gating for declarative ids, no hot-reload (pinning +
`definition_version_missing` is the contract). Process-global definition
ids are a known v1 simplification; if multi-repo id collisions become
real, per-project namespacing is the follow-up, not silent scoping.
