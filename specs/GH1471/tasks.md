# Task Plan

## Linked Issue

GH-1471

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1471-T001` Owner: core | Done when: `PromptParts::to_layered()` exists with identity test vs flattened | Verify: `cargo test -p harness-core prompts`
- [ ] `SP1471-T002` Owner: agents | Done when: Claude adapter sends static layer via `--append-system-prompt` in both claude.rs and claude_adapter.rs, arg order verified | Verify: `cargo test --package harness-agents`
- [ ] `SP1471-T003` Owner: agents | Done when: anthropic_api marks static block with cache_control | Verify: `cargo test -p harness-agents anthropic`
- [ ] `SP1471-T004` Owner: core | Done when: skills listing capped + deterministic under `prompts.max_skills_in_context` | Verify: `cargo test -p harness-core context`
- [ ] `SP1471-T005` Owner: server | Done when: pipeline call sites hand layered parts to the invocation contract | Verify: `cargo test -p harness-server pipeline`

## Parallelization

- T001 first; T002/T003/T004 parallel after; T005 last.

## Verification

- `cargo test --workspace`
