# Product Spec

## Linked Issue

GH-1442

## User Problem

Maintainers reading or contracting the harness codebase (#1434) encounter ~1,800 LOC of statically unreachable code that looks active: an interceptor whose registration is commented out (`rule_enforcer`), a workspace crate nothing depends on (`harness-api`), and a reward store whose read API has zero callers (`q_value_store`). Dead-but-plausible code misleads reviewers, inflates builds, and obscures the true deletion boundary for the kernel contraction.

## Goals

- The three unreachable surfaces are fully removed from the workspace.
- Their empty Postgres tables are archived, never dropped, per the #1434 archive-first rule.
- The removal is independently revertible (one focused PR).

## Non-Goals

- Removing `harness-rules` engine internals or any live consumer (Phase 3 of #1434).
- Removing thread/turn/task layers (#1434 Phase 1 proper, boundary defined by GH-1443).
- Any behavior change on the workflow-runtime hot path.

## Behavior Invariants

1. After removal, every existing server behavior observable via HTTP endpoints, webhook intake, workflow-runtime dispatch, and the dashboard is unchanged.
2. No Postgres table is dropped; `q_value_store` schema tables are renamed to an archive name and their row counts (all 0) recorded in the PR.
3. The workspace builds warning-free and all tests pass after removal.
4. No dangling references remain: no commented-out registrations, no orphan config keys, no CLI or RPC surface that panics or 404s where it previously worked.

## Acceptance Criteria

- [ ] `rule_enforcer.rs`, `crates/harness-api/`, `q_value_store.rs` and all references deleted; workspace members updated.
- [ ] Empty q_value tables archived with counts recorded.
- [ ] `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` and `cargo test --workspace` green.
- [ ] Net LOC delta reported (expected ≈ -1,800).

## Edge Cases

- Config files in the wild may still contain q_value/rule-enforcer-related keys — unknown keys must be ignored (or warned about), not fail config load.
- `builders/intake.rs` legacy completion callback must keep compiling after the q_value write hook is removed (the callback has other duties).
- If any test exercises the removed modules, delete the test with the module (do not weaken shared test helpers).

## Rollout Notes

Pure code removal; no migration. Revert = revert the single PR. Archive rename is reversible with a single `ALTER SCHEMA`/`ALTER TABLE` statement recorded in the PR description.
