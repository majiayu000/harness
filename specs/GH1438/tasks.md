# Task Plan

## Linked Issue

GH-1438

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1438-T001` Owner: `workflow-validator` | Done when: `local_review_gate -> done` is accepted only for reconciliation decisions with merged PR evidence, and non-reconciliation or unevidenced attempts are rejected | Verify: `cargo test -p harness-workflow validator`
- [ ] `SP1438-T002` Owner: `server-reconciliation` | Done when: runtime reconciliation moves a `local_review_gate` workflow with an externally merged linked PR to `done` and records `last_decision = "reconcile_pr_merged"` plus PR evidence fields | Verify: `cargo test -p harness-server reconciliation`
- [ ] `SP1438-T003` Owner: `regression` | Done when: existing blocked, ready-to-merge, open PR, and closed PR reconciliation behavior remains unchanged | Verify: `cargo test -p harness-server reconciliation`

## Parallelization

T001 and T002 touch different crates but are behaviorally coupled. Implement
T001 first so server reconciliation can rely on the validator contract. T003 is
verification-only and runs after T001 and T002.

## Verification

- `cargo test -p harness-workflow validator`
- `cargo test -p harness-server reconciliation`
- `cargo check -p harness-workflow --all-targets`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1438`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

Keep `local_review_gate -> done` out of the advertised default transition
allowlist. The intended fix is a reconciliation-only validator exception that
reuses the existing PR-merge evidence requirements.
