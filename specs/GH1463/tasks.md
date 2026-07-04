# Task Plan

## Linked Issue

GH-1463

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1463-T001` Owner: server | Done when: `http/background/` scaffold + registry `spawn_all` exists with the first loop moved | Verify: `cargo test -p harness-server`
- [ ] `SP1463-T002` Owner: server | Done when: tranche 2 (next 3-4 loops) moved, pure motion | Verify: `cargo test -p harness-server`
- [ ] `SP1463-T003` Owner: server | Done when: tranche 3 moved; old background.rs is a thin re-export or deleted | Verify: `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`
- [ ] `SP1463-T004` Owner: docs | Done when: CLAUDE.md U-16 exemption for background.rs removed | Verify: manual review

## Parallelization

- Tranches are sequential (same file); T004 last.

## Verification

- `cargo test --workspace` + warnings-as-errors check
