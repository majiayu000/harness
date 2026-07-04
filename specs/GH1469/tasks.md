# Task Plan

## Linked Issue

GH-1469

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1469-T001` Owner: core | Done when: unset `max_turns` resolves to the documented default in `resolve.rs`, explicit values untouched | Verify: `cargo test -p harness-core config`
- [ ] `SP1469-T002` Owner: server | Done when: tasks table has `agent_invocations` counter incremented at the spawn chokepoint | Verify: `cargo test -p harness-server task_db`
- [ ] `SP1469-T003` Owner: server | Done when: spawn refuses past `invocation_budget` and marks the task terminal with `InvocationBudgetExhausted` | Verify: `cargo test -p harness-server invocation_budget`
- [ ] `SP1469-T004` Owner: server | Done when: cross-layer test (transient retry × review rounds) proves the shared ceiling | Verify: `cargo test -p harness-server invocation_budget_layers`
- [ ] `SP1469-T005` Owner: docs | Done when: `config/default.toml.example` documents both knobs | Verify: manual review

## Parallelization

- T001 independent; T002 → T003 → T004 sequential; T005 parallel.

## Verification

- `cargo test --workspace`
