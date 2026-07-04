# Task Plan

## Linked Issue

GH-1472

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1472-T001` Owner: server | Done when: index migration lands for task_artifacts(task_id) (+siblings if same pattern) | Verify: `cargo test -p harness-server migrations`
- [ ] `SP1472-T002` Owner: server | Done when: retention config keys parse with disabled default | Verify: `cargo test -p harness-core config`
- [ ] `SP1472-T003` Owner: server | Done when: prune job deletes terminal+aged tasks FK-safely in batches with counts logged | Verify: `cargo test -p harness-server task_retention`
- [ ] `SP1472-T004` Owner: docs | Done when: config example documents retention knobs | Verify: manual review

## Parallelization

- T001 independent; T002 → T003; T004 parallel.

## Verification

- `cargo test -p harness-server`
