# Task Plan

## Linked Issue

GH-1448

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1448-T001` Owner: workflow-store | Done when: `workflow_repo_memory` migration + store methods exist in the workflow namespace with CRUD tests | Verify: `cargo test -p harness-workflow repo_memory_store`
- [ ] `SP1448-T002` Owner: workflow-reducer | Done when: deterministic extractors run on the terminal path for done and failed runs, produce outcome-flagged records, and extractor errors never block run completion (error-level logged) | Verify: `cargo test -p harness-workflow memory_extract`
- [ ] `SP1448-T003` Owner: workflow-retrieval | Done when: retrieval ranks by activity match + recency with an outcome-mix guarantee (failure lessons surface), enforcing K=5 and the token budget by whole-record truncation | Verify: `cargo test -p harness-workflow memory_retrieval`
- [ ] `SP1448-T004` Owner: workflow-dispatch | Done when: dispatcher appends the fenced `repo-memory` section with the untrusted-background-evidence preamble; fresh repos get no section | Verify: `cargo test -p harness-workflow memory_inject`
- [ ] `SP1448-T005` Owner: server-api | Done when: list per repo and delete per record endpoints work and pruned records vanish from the next packet | Verify: `cargo test -p harness-server memory_api`
- [ ] `SP1448-T006` Owner: config | Done when: `memory.enabled` project flag gates extraction+injection, and store unavailability at dispatch degrades visibly in run evidence instead of silently skipping | Verify: `cargo test -p harness-workflow memory_flag_degraded`

## Parallelization

- T001 first (schema owner). Then T002 (reducer files) ∥ T003 (retrieval module) ∥ T005 (handler files) with disjoint file ownership.
- T004 after T003; T006 last.

## Verification

- `cargo test --workspace`
- Integration: two sequential fixture runs — the second dispatch packet contains the first run's lesson, including a failed-run lesson.
- Manual: list/prune via API, confirm pruned record absent from the next packet.

## Handoff Notes

- Do not depend on harness-skills or rules stores (GH-1434 Phase 3 removes them).
- Memory content is untrusted background evidence by contract; keep the preamble text stable and covered by a test.
