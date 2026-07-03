# Task Plan

## Linked Issue

GH-1437

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1437-T1` Owner: agent — Lower `default_stall_timeout_secs` to 600, add config validation (clamp stall below wall-clock, floor 60 s, warn on clamp) and per-task override clamping. Done when: unit tests cover default, clamp, floor, and override cases and `config/default.toml.example` documents both windows. Verify: `cargo test -p harness-core config`
- [ ] `SP1437-T2` Owner: agent — Audit workflow-runtime job execution for stall coverage and unify the stall `select!` arm into the shared stream driver so task-executor and codex_exec paths use one implementation with distinct stall vs wall-clock failure strings. Done when: a silent-stream runtime-job test fails with the stall reason within the window. Verify: `cargo test -p harness-server turn_lifecycle workflow_runtime_worker`
- [ ] `SP1437-T3` Owner: agent — Surface stall window at turn start (debug log) and document the two failure strings as failure-class seeds for GH-1430. Done when: log assertions pass and docs updated. Verify: `cargo test -p harness-server`

## Parallelization

T1 (harness-core config) and T3 (logging/docs) touch disjoint files; T2 depends on T1's validated config type.

## Verification

- [ ] `SP1437-T4` Owner: agent — Full-tree verification. Done when: workspace check and tests are green. Verify: `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets && cargo test --workspace`

## Handoff Notes

Root cause: stall detection already exists (`turn_lifecycle.rs:280`) but its default window equals the wall-clock default (3600 s, `misc.rs:169`), making it vacuous — production evidence 2026-07-03: 14/28 task failures were 3600 s timeouts correlated with upstream tls-handshake drops. Open question for T2: confirm whether codex_exec runtime jobs flow through `turn_lifecycle.rs` at all; if not, stall coverage there is absent, not just misconfigured.
