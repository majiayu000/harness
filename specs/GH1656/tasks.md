# GH1656 Task Plan

## Linked Issue

GH-1656 (depends on GH-1609, merged; related: GH-1652, GH-1607)

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1656-T001` Owner: `config-schema` | Done when: `WorkflowDefinitionIntakePolicy` / `IntakeFilterPolicy` land as an optional `intake` field on `WorkflowDefinitionPolicy` with parse-level checks (non-empty labels, no duplicates/empties, cap >= 1) and an atomic-merge regression test covering a definition with an `intake` block (B-001, B-002) | Verify: `cargo test -p harness-core config`
- [ ] `SP1656-T002` Owner: `binding-registry` | Done when: startup registration validates bindings fail-closed against configured intake sources and builds an immutable project-scoped `IntakeBindingRegistry` on AppState; invalid bindings abort startup with actionable errors (B-002) | Verify: `cargo test -p harness-server` boot-validation tests
- [ ] `SP1656-T003` Owner: `matcher` | Done when: the source-agnostic matcher (source name + all-labels + no-exclude-labels over normalized `IncomingIssue`) exists with unit tests including a non-GitHub fixture source and deterministic multi-match tie-break with audited tie list (B-003, B-004) | Verify: `cargo test -p harness-server intake::binding`
- [ ] `SP1656-T004` Owner: `routing` | Done when: `poll_tick`'s shared dispatch path routes matched issues through `resolve_declarative_definition_for_project` + `record_declarative_submission` with subject metadata and trust class, exclusive of the github path; unmatched issues are byte-identical to today; mark/unmark_dispatched mirrors the existing failure handling; webhook-mode delivery goes through the same step (B-005, B-006) | Verify: `cargo test -p harness-server intake`
- [ ] `SP1656-T005` Owner: `dedupe-cap` | Done when: store-backed `(definition_id, external_id)` dedupe (live suppresses, terminal reopens) and per-binding `max_active_instances` enforcement land, both with audited outcomes and natural next-tick retry (B-007, B-008) | Verify: `cargo test -p harness-server intake::dedupe`
- [ ] `SP1656-T006` Owner: `audit` | Done when: `intake_routing` observe events record routed / suppressed_live / skipped_at_cap / unmatched outcomes with issue, source, definition, and tie list; cross-project mismatches are audited without failing the tick (B-009, B-012) | Verify: `cargo test -p harness-server intake::audit`
- [ ] `SP1656-T007` Owner: `e2e-docs` | Done when: an end-to-end stub-runtime test drives fixture-source emission → routing → declarative instance → terminal → dedupe reopen; `docs/workflow-declarative-definitions.md` gains an `intake` section with a doc-lint-validated example (extending the GH-1653 test) (B-001..B-012 integration) | Verify: `cargo test -p harness-server intake_e2e && cargo test -p harness-workflow --test docs_examples`

## Parallelization

- `SP1656-T001` first; `SP1656-T002` and `SP1656-T003` in parallel after
  T001 (registry vs matcher, disjoint files).
- `SP1656-T004` after T002+T003; `SP1656-T005` alongside T004's tail
  (store queries are separable); `SP1656-T006` after T004.
- `SP1656-T007` last.

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p harness-core -p harness-server`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1656`

## Handoff Notes

Source pluggability is the load-bearing constraint (maintainer decision):
the binding schema and matcher must consume only normalized
`IncomingIssue` fields so a future Linear source (separate issue, purely
additive behind `IntakeSource`) needs zero schema work. Routing is
exclusive — a matched issue must never also dispatch to github_issue_pr.
The dedupe key is `(definition_id, external_id)`; the query plan is the
implementer's choice but the invariant is store-backed, not in-memory.
Cap skips must not call `mark_dispatched`, or the issue becomes
permanently invisible — this is the easiest correctness trap in the
whole feature.
