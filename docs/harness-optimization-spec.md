# Harness Optimization Spec

> Date: 2026-03-19
> Sources: Fowler SDD critique, TDAD paper, Faithfulness Loss paper, Spec-Kit Constitution
> Status: Draft pending user confirmation

## Goal

Close Harness's declaration-execution gap by turning declared architecture into verified runtime behavior. The goal is to move from strong framework declarations to an end-to-end loop that can be tested and observed.

Non-goals: do not add new subsystems, protocol methods, or crates for this pass.

## Constraints

- Run `cargo check --workspace` after each change batch.
- Run `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` before PR.
- Preserve the VibeGuard exemptions listed in the repository rules.
- Do not change the `TurnInterceptor` trait signature unless the migration is explicitly approved.

## Phase 1: Close The Interceptor Loop

Problem: `hook_enforcer.rs` implements `TurnInterceptor`, but the app state previously initialized interceptors as an empty list. Register `HookEnforcer` in app state construction and add integration tests that prove `post_tool_use` and `pre_execute` are invoked.

Acceptance:

- `cargo test -p harness-server` passes.
- New integration coverage proves violations are written to `EventStore` and blocking results reject a turn.

## Phase 2: Inject Golden Principles

Problem: Golden Principles exist in prompt documentation but are not consistently injected into agent context.

Plan:

- Add a `config/constitution.md` file that captures invariant architecture principles.
- Prefix turn-start prompts with the constitution before agent execution.
- Verify turn logs include the GP-01 through GP-05 content.

## Phase 3: Incremental GC Scan

Problem: GC still performs broad scans even though checkpoint paths are declared.

Plan:

- Wire GC checkpoint configuration at app-state construction.
- Make GC tool allowlists configurable through `GcConfig`.
- Add a lightweight changed-file scope using checkpoint state plus file-reference discovery.

Non-goal: do not build a full AST dependency graph in this phase.

## Phase 4: Configuration Consistency

Problem: some runtime IDs and constants are derived from defaults instead of a single configured source.

Plan:

- Resolve project identity from registry/config rather than anonymous IDs.
- Move hard-coded concurrency and review constants into configuration.
- Update stale gap-analysis documentation to match current implementation.

## Suggested Order

1. Close the interceptor loop.
2. Fix configuration consistency.
3. Inject Golden Principles.
4. Add incremental GC scanning.

## Acceptance Matrix

| Phase | Acceptance |
|---|---|
| 1 | Server tests pass and interceptor enforcement coverage is green |
| 2 | Turn execution logs include the constitution content |
| 3 | GC logs show incremental file counts and checkpoint updates |
| 4 | No anonymous project IDs or unconfigured constants remain in app-state setup |

## Expected File Impact

| File | Change Type | Phase |
|---|---|---|
| `crates/harness-server/src/http.rs` | App-state construction | 1, 3, 4 |
| `crates/harness-server/src/hook_enforcer.rs` | Session/event association | 1 |
| `crates/harness-core/src/interceptor.rs` | Optional session metadata | 1 |
| `crates/harness-server/tests/interceptor_enforcement.rs` | New integration tests | 1 |
| `config/constitution.md` | New constitution file | 2 |
| `crates/harness-server/src/handlers/turn.rs` | Prompt context injection | 2 |
| `crates/harness-gc/src/gc_agent.rs` | Checkpoint and impact analysis | 3 |
| `crates/harness-core/src/config/gc.rs` | Configurable tool allowlist | 3 |
| `docs/gap-analysis.md` | Stale documentation update | 4 |
