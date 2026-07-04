# Tech Spec

## Linked Issue

GH-1442

## Product Spec

See `specs/GH1442/product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Dead interceptor | `crates/harness-server/src/rule_enforcer.rs` | TurnInterceptor impl; registration commented out at `http/builders/services.rs:61`; only other reference is a comment in `periodic_reviewer.rs:778` | Deletion target 1 |
| Dead crate | `crates/harness-api/` | Workspace member, zero dependents in any Cargo.toml | Deletion target 2 |
| Write-only store | `crates/harness-server/src/q_value_store.rs` | Built at `http/builders/storage.rs:96`; written from legacy task completion callback `http/builders/intake.rs:172-213`; read API `q_value_for` (line 218) has zero callers; all tables 0 rows in prod | Deletion target 3 |
| Health surface | `crates/harness-server/src/http/` health/startup store list | `q_value_store` appears in `/health` startup stores (`critical: false`) | Must remove from the store list without breaking health JSON consumers |
| Root manifest | `Cargo.toml` | members list includes `crates/harness-api` | Workspace update |

## Proposed Design

Single PR, three mechanical removals:

1. Delete `rule_enforcer.rs`, the commented registration lines in `services.rs`, its `mod` declaration, and the stale comment in `periodic_reviewer.rs:778`.
2. Delete `crates/harness-api/` and its workspace member entry.
3. Delete `q_value_store.rs`, its construction in `storage.rs:96`, the write calls in `intake.rs:172-213` (keep the surrounding callback intact), its `AppState` field, and its entry in the health startup-store list. Ship a small idempotent SQL snippet (in the PR description and `scripts/`, not auto-run) renaming `q_value_store` schema tables to `q_value_store_archived_*`.

Grep gates before merge: `rg -i "rule_enforcer|harness-api|harness_api|q_value"` returns only changelog/docs history.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 behavior unchanged | hot path untouched | Existing workflow_runtime_worker + HTTP route test suites pass unmodified |
| P2 archive not drop | SQL snippet | Manual: run snippet on dev DB; tables renamed, row counts logged |
| P3 warning-free build | whole workspace | `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` |
| P4 no dangling refs | grep gates | `rg` gates above return no code hits |

## Data Flow

No runtime data-flow change: the q_value write hook fired only on legacy task completion (62 tasks ever); removing it eliminates writes to already-empty tables. Health endpoint loses one non-critical store entry.

## Alternatives Considered

- Fold into #1434 Phase 1 PR — rejected: mixing zero-risk mechanical deletion with the risky task-layer split reduces revertibility (U-09).
- Re-wire rule_enforcer instead of deleting — rejected: harness-rules per-turn enforcement is Phase 3 deletion scope; resurrecting it contradicts the approved contraction direction.
- Keep q_value write path "for future data" — rejected: read end has never existed; writing unreadable data is waste.

## Risks

- Security: none (removal only).
- Compatibility: external dashboards parsing `/health` `stores[]` by index would break — mitigation: consumers observed parse by `name`; note in PR.
- Performance: negligible positive (less code, one fewer store init).
- Maintenance: none; reduces surface.

## Test Plan

- [ ] Unit tests: existing suites pass unmodified (no assertion changes permitted).
- [ ] Integration tests: `/health` returns without the q_value entry; startup succeeds with archived tables present and absent.
- [ ] Manual verification: grep gates; archive SQL dry-run on dev DB.

## Rollback Plan

Revert the PR (single commit after squash). Restore archived tables by reversing the recorded renames. No config or data migration to undo.
