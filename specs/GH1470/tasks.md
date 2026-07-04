# Task Plan

## Linked Issue

GH-1470

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1470-T001` Owner: server | Done when: completed review records carry the reviewed head SHA | Verify: `cargo test -p harness-server review_store`
- [ ] `SP1470-T002` Owner: server | Done when: pre-enqueue gate compares current head vs last reviewed and skips with log+counter | Verify: `cargo test -p harness-server periodic_reviewer_gate`
- [ ] `SP1470-T003` Owner: server | Done when: unknown-head fallback enqueues (conservative), covered by test | Verify: `cargo test -p harness-server periodic_reviewer_gate`

## Parallelization

- T001 → T002; T003 with T002.

## Verification

- `cargo test -p harness-server periodic_reviewer`
