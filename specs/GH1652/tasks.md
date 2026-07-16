# GH1652 Task Plan

## Linked Issue

GH-1652 (related: GH-1609, GH-1608)

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1652-T001` Owner: `helper` | Done when: a shared `operator_definition_ids()` helper reads the frozen registry, sorts ids, and errors on an empty registry, with unit tests for order and the empty case (B-001, B-004, B-006) | Verify: `cargo test -p harness-server definition_ids`
- [ ] `SP1652-T002` Owner: `history-check` | Done when: git history / code comments establish whether dashboard_active_counts and overview intentionally exclude pr_feedback/quality_gate (child workflows); the decision is encoded as an explicit documented predicate in the helper (preserve built-in exclusions exactly, include declarative ids) and recorded in tech.md (B-002, B-003) | Verify: updated tech.md note + code comment in the helper
- [ ] `SP1652-T003` Owner: `monitor` | Done when: `operator_monitor.rs` const is deleted and monitor + `sampling.rs` iterate the helper's ids; built-in-only monitor payload snapshot is unchanged (B-001, B-002) | Verify: `cargo test -p harness-server operator_monitor`
- [ ] `SP1652-T004` Owner: `counts` | Done when: `dashboard_active_counts.rs` and `overview.rs` use the helper (with the T002 predicate); built-in-only counts snapshots unchanged (B-001, B-002) | Verify: `cargo test -p harness-server dashboard overview`
- [ ] `SP1652-T005` Owner: `visibility-tests` | Done when: a declarative fixture definition with a blocked instance appears in monitor payload, sampling output, dashboard active counts, and overview counts; a zero-instance declarative definition contributes nothing (B-003, B-005) | Verify: `cargo test -p harness-server declarative_visibility`
- [ ] `SP1652-T006` Owner: `audit` | Done when: no static definition-id enumeration remains in `harness-server` outside the helper; per-definition behavioral branches are listed as exemptions in tech.md (B-007) | Verify: `git grep -n "DEFINITION_IDS" crates/harness-server/src` returns only the helper module

## Parallelization

- `SP1652-T001` and `SP1652-T002` first, in parallel (helper vs history
  research).
- `SP1652-T003` and `SP1652-T004` in parallel after T001+T002 (disjoint
  files).
- `SP1652-T005` after T003/T004; `SP1652-T006` last.

## Verification

- `cargo check --workspace --all-targets`
- `cargo test -p harness-server`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1652`

## Handoff Notes

The one genuine design decision is T002: whether the two-id lists in
active counts are a bug or an intentional child-workflow exclusion. Do
not guess — check the blame/PR history. Everything else is mechanical.
Declarative visibility tests should reuse the fixture-definition
helpers introduced by the GH-1609 test suites rather than inventing new
fixtures.
