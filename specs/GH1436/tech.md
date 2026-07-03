# Tech Spec

## Linked Issue

GH-1436

## Product Spec

See `specs/GH1436/product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Server reaper loop | `crates/harness-server/src/http/orphan_reaper.rs` | Periodic task calls `reap_orphaned_path_schemas(pool, true)` per `storage.orphan_reaper_*` config | Entry point that must gain legacy coverage; also verify tick recurrence (only one run logged in 8.7 h) |
| Reap implementation | `crates/harness-core/src/db_pg_schema_registry.rs:606-655` | `reap_orphaned_path_schemas_with_workspace_roots` selects **only registry rows** with `owner_kind IN (path_derived_store, path_derived_directory_store)`; `scanned` = registry row count (1,133 in prod) | Root cause: unregistered legacy schemas are invisible to this query |
| Inventory/plan query | `crates/harness-core/src/db_pg_schema_registry.rs:262-349` | `pg_schema_cleanup_plan(pool, include_unregistered)` already scans `pg_namespace` for `^h[0-9a-f]{16}$` and LEFT JOINs the registry, classifying unregistered schemas | Reusable building block for legacy discovery |
| Manual CLI | `crates/harness-cli/src/bin/harness-pg-schema-cleanup.rs` | Plan/apply flow; unregistered schemas require explicit allowlist | Must stay consistent with new automatic semantics |
| Ownership registration | `crates/harness-core/src/db_pg.rs:336` | New stores register `PgSchemaOwnership::path_derived` since #1213 | Explains why only post-#1213 schemas are registered |
| Cleanup script | `scripts/pg-orphan-cleanup.sh` | Wraps the CLI | Docs/behavior must be updated together |

## Proposed Design

Extend the reap path with a legacy scan phase:

1. In `reap_orphaned_path_schemas_with_workspace_roots`, after the registry-driven pass, run a second query based on the existing `pg_schema_cleanup_plan` SQL: all `pg_namespace` entries matching the canonical path-derived pattern with **no** registry row.
2. For each unregistered candidate, apply the same orphan test used for registered directory-owned schemas: derive the candidate owner path set from the configured workspace roots (pass real roots from the server, not `&[]`); a schema is orphaned only when every plausible owner location is definitively absent (`try_exists() == Ok(false)` on the parent), mirroring the conservative rule in `orphaned_path_schema_names`. Since legacy schemas cannot be mapped back to a path from the hash alone, the practical test is: schema has no registry row AND no live workspace directory currently maps to it (recompute `h<hex>` hashes for all directories under the workspace roots and subtract). Anything unmatched and unregistered is a dead orphan.
3. Drop in bounded batches (e.g. 200 per tick, configurable `storage.orphan_reaper_legacy_batch`) via existing `drop_schema_cascade_if_exists`.
4. Extend `PgSchemaReapReport` with `legacy_scanned` / `legacy_reaped`; update the reaper log line.
5. Fix/verify tick recurrence in `orphan_reaper.rs` (expected one log line per interval when work exists; add a debug line on idle ticks so liveness is observable).

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 legacy orphans reaped | `db_pg_schema_registry.rs` legacy phase | Integration test: seed unregistered `hXX` schema + no matching dir → reaped |
| P2 live schemas kept | hash-recompute subtraction | Test: dir exists under workspace root → schema survives both passes |
| P3 ambiguous kept | conservative orphan test | Test: unreadable root (permission) → schema kept, reported |
| P4 observable counts | `PgSchemaReapReport`, reaper log | Unit test on report fields; log assertion in reaper test |
| P5 config honored / recurring ticks | `orphan_reaper.rs` | Test with short interval: ≥2 ticks observed |
| P6 bounded batch | batch limit | Test: seed batch+1 orphans → first tick reaps ≤ batch |

## Data Flow

Input: `pg_namespace` + `harness_pg_schema_registry` rows, workspace root listing (filesystem). Output: `DROP SCHEMA ... CASCADE` statements, registry row deletions, `PgSchemaReapReport`. No external network calls. Persistence: Postgres only.

## Alternatives Considered

- Backfill registration for all legacy schemas, then let the existing registry-driven reaper handle them — two-phase, more writes, and registration of dead schemas is churn for no benefit.
- One-off manual cleanup via `scripts/pg-orphan-cleanup.sh` allowlist — does not fix the class for other deployments and requires exporting 17k names by hand.
- Aggressive: drop every unregistered `h<hex>` schema unconditionally — violates invariant 2 for pre-#1213 deployments with live legacy stores.

## Risks

- Security: none new; DROP is already gated to the canonical name pattern with `validate_schema_name`.
- Compatibility: pre-#1213 deployments with *live* unregistered stores rely on the hash-recompute subtraction being complete; missing a workspace root config would misclassify a live store as orphaned. Mitigation: default legacy phase to dry-run-log-only for one release, or gate behind `storage.orphan_reaper_legacy_enabled` (default true only after verification).
- Performance: first drain drops ~17k schemas; bounded batches + `CASCADE` keep each tick short. Catalog locks are per-schema.
- Maintenance: two scan paths; mitigated by sharing the plan query.

## Test Plan

- [ ] Unit tests: report fields, batch bounding, pattern gating (`h` + non-hex not a candidate).
- [ ] Integration tests (existing pg test harness in `db_pg_schema_registry_tests.rs`): legacy orphan reaped, live legacy kept, registry-absent DB, interrupted-batch resume.
- [ ] Manual verification: dry-run against production snapshot; compare candidate list with `pg_namespace` count (expect ≈16.9k − live workspaces).

## Rollback Plan

Config kill-switch: set `storage.orphan_reaper_enabled=false` (existing) or the new `storage.orphan_reaper_legacy_enabled=false` to stop the legacy phase; already-dropped schemas are recoverable only from DB backups, hence the pre-deploy snapshot recommendation in Rollout Notes.
