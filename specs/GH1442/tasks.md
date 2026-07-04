# Task Plan

## Linked Issue

GH-1442

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1442-T1` Owner: agent — Delete `rule_enforcer.rs` plus its commented registration in `services.rs`, mod declaration, and the stale reference comment in `periodic_reviewer.rs`. Done when: grep for rule_enforcer returns no code hits and workspace builds. Verify: `rg -i rule_enforcer crates && cargo check -p harness-server`
- [ ] `SP1442-T2` Owner: agent — Remove `crates/harness-api/` and its workspace member entry. Done when: grep for harness-api/harness_api returns no code hits and workspace builds. Verify: `cargo check --workspace`
- [ ] `SP1442-T3` Owner: agent — Delete `q_value_store.rs`, its construction, AppState field, health store-list entry, and the write calls inside `intake.rs` legacy callback; add archive SQL snippet under `scripts/`. Done when: `/health` no longer lists q_value_store and the legacy callback still compiles and passes its tests. Verify: `cargo test -p harness-server`

## Parallelization

T1, T2, T3 touch disjoint files (`rule_enforcer.rs`+`services.rs`+`periodic_reviewer.rs` / `crates/harness-api`+root `Cargo.toml` / `q_value_store.rs`+`storage.rs`+`intake.rs`+health) and can run in parallel.

## Verification

- [ ] `SP1442-T4` Owner: agent — Full-tree gate. Done when: warning-free check and full test suite green with net LOC delta recorded in the PR. Verify: `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets && cargo test --workspace`

## Handoff Notes

Evidence basis: static census 2026-07-04 (registration commented out at `services.rs:61`; zero dependents for harness-api; `q_value_for` zero callers; all q_value tables 0 rows). Decision: pulled forward from #1434 Phase 3 because static unreachability removes the need for the 7-day usage-probe gate. Do NOT touch `task_executor/` in this PR — that boundary belongs to GH-1443.
