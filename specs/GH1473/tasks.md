# Task Plan

## Linked Issue

GH-1473

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1473-T001` Owner: core | Done when: `intake.discovery` config key parses (`agent` default) | Verify: `cargo test -p harness-core config`
- [ ] `SP1473-T002` Owner: server | Done when: REST poller registered under `rest` mode and agent backlog dispatch disabled for those repos | Verify: `cargo test -p harness-server intake`
- [ ] `SP1473-T003` Owner: server | Done when: throttle + round-robin cursor implemented with unit tests | Verify: `cargo test -p harness-server github_issues`
- [ ] `SP1473-T004` Owner: server | Done when: dedupe test proves webhook+rest same issue yields one task | Verify: `cargo test -p harness-server intake_dedupe`

## Parallelization

- T001 → T002; T003 parallel with T002; T004 last.

## Verification

- `cargo test -p harness-server`
