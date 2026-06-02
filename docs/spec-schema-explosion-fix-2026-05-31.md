# SPEC — Eliminate Postgres Schema Explosion

Status: Draft for review · Owner: TBD · Created: 2026-05-31
Related: storage RFC #1206/#1213/#1216 · WorkStream #416, #426

This spec follows the four-element contract (Goal / Context / Constraints / Done-when)
and separates Fact / Inference / Suggestion per W-11. No code changed yet.

---

## 1. Goal

Stop the unbounded growth of Postgres schemas (currently 13,618 schemas / 22 GB) and
prevent recurrence, by migrating every `PathDerivedSchema` store off path-hashed
schemas and onto either a bounded shared schema (durable data) or a local SQLite file
(ephemeral data), then cleaning up the existing orphans and wiring a background safety
reaper.

Success = schema count stays bounded by **project count (tens)**, not run/test count
(thousands); query planning latency and the resulting event-stream backpressure recover.

## 2. Context (Fact — evidenced)

- [source: psql] 13,618 schemas; 13,602 match `^h[0-9a-f]{16}$`; DB 22 GB. Largest
  tables in the one live schema `h136534546a051d3d`. The other ~13.6k are full store
  schemas (2,314 `tasks` stores ×5 tables, 1,802 `events`, 909 workflow runtime ×10
  tables, 1,249 `threads`, …), 11,006 with `schema_migrations` (fully initialized).
- [source: code] `pg_schema_for_path` (db_pg.rs:312) hashes a store-identity path →
  `h<16hex>` schema. Inherited from the SQLite era ("one file per path"). `CREATE SCHEMA
  IF NOT EXISTS` runs on open; **no DROP anywhere except the reaper**; workspace cleanup
  does not drop schemas.
- [source: code] `StoreLocation` (store_backend.rs:34) already defines the target model
  and names `PathDerivedSchema` "the source of the schema explosion. Retained only until
  callers migrate to `SharedSchema` or `LocalFile`." The seam exists (#426).
- [source: code] tests resolve `resolve_database_url(None)` → the **same** local DB
  (`postgres://harness:harness@localhost:5432/harness`) and open stores at `tempdir()`
  paths → each leaks a schema, never dropped.
- [source: fs check] of 1,195 registry rows with `owner_path`, only **8 exist on disk**;
  1,187 point to gone `…/T/.tmp*` test temp dirs.
- [source: code] `reap_orphaned_path_schemas` (db_pg_schema_registry.rs:452) is the only
  DROP path, scans **only** `owner_kind='path_derived_store'`, and is **not wired into
  the server background** (CLI-only). Two divergent classifiers exist (`reap_*` vs
  `pg_schema_cleanup_plan`) — RS-12.

### Root cause (Inference, high confidence)
The path→schema mapping makes *any* store opened at a transient path leak a permanent
schema. The dominant producer is the **test suite** (thousands of tempdir store-opens
against the shared local DB); production uses fixed data dirs and is bounded. A reaper
is a janitor, not a fix. The real fix is to remove `PathDerivedSchema` from the hot
paths, per the model the code already documents.

## 3. Target architecture (Suggestion)

Route every store open through `StoreLocation`, choosing per call site:

| Store class | Today | Target | Rationale |
|---|---|---|---|
| Durable server stores (tasks, events, threads, plan, workflow_runtime, project_registry, q_value, runtime_state, review) | `PathDerivedSchema(data_dir/...)` | **`SharedSchema(name)`** | bounded: one schema per store-type (or per project), not per path |
| Ephemeral per-workspace/per-job scratch stores (if any) | `PathDerivedSchema(workspace/...)` | **`LocalFile(workspace/...)`** | SQLite under the worktree → deleted with the dir, self-cleaning like the SQLite era |
| Test stores | `PathDerivedSchema(tempdir/...)` | **`LocalFile(tempdir/...)`** (default) or per-test ephemeral schema with teardown drop | zero pollution of the shared PG |

**Decision D1 (locked 2026-05-31): one global `SharedSchema` partitioned by a
`project_id` column.** Bounded at 1 durable schema. This is the clean long-term
relational model; per-project schemas were considered and rejected in favor of a single
namespace. Consequence: every durable store table gains/uses a `project_id` column and a
matching index, and reads/writes must scope by `project_id`. The migration (P2) folds
the existing per-path live schema(s) into this one global schema, stamping `project_id`
from the originating data dir.

## 4. Implementation plan (phased)

### P0 — Stop the bleeding: test isolation (highest leverage, lowest risk)
- Route test store construction through `StoreLocation::LocalFile` (SQLite) so `cargo
  test` never creates PG schemas. Touch points: `harness-server/src/test_helpers.rs`,
  per-crate test store builders, any `PgStoreContext::from_path` under `#[cfg(test)]`.
- Alternative if a test specifically needs PG semantics: open an ephemeral schema and
  `DROP SCHEMA CASCADE` in the test's teardown/`Drop`.
- Done-when: after a full `cargo test` run, `SELECT count(*)` of `^h[0-9a-f]{16}$`
  schemas does not increase.

### P1 — One-off cleanup of the existing 13.6k — ✅ DONE 2026-05-31
- **Executed** via `scripts/pg-orphan-cleanup.sh --confirm` with the server stopped.
  Dual-signal keep-list (recomputed at apply time): keep any `h*` schema with write
  activity in a fresh 70s window OR an existing on-disk `owner_path`, plus
  `workflow_runtime`/`harness_admin`/system. Backed up the keep-set first
  (`pg_dump -n <keep…>`, 1.4 GB) — a whole-catalog dump was impossible (13k schemas
  blow `max_locks_per_transaction`), and the dropped schemas are dead by definition.
- **Result**: dropped 13,597 dead schemas; kept exactly 8 live `h*` + `workflow_runtime`
  + `harness_admin`. `VACUUM (FULL, ANALYZE)` then reclaimed catalog bloat.
  **DB 22 GB → 706 MB** (97% reduction). Live data verified intact post-vacuum
  (h136.threads 4619, workflow_runtime.workflow_events 72,920, runtime_jobs 33,819).
- Backup retained at `/tmp/harness-keepset-predrop-20260531153628.sql`.
- ⚠️ Server must be restarted by the operator (Claude Code cannot start it).

### P2 — Migrate durable stores off PathDerivedSchema (the real fix)
- For each durable caller (registry.rs / storage.rs / engines.rs builders, task_db.rs,
  event_store, thread_db, plan_db, runtime/store.rs, project_registry, q_value_store,
  runtime_state_store, review_store): replace `PgStoreContext::from_path(path)` with a
  single `StoreLocation::SharedSchema("<global name>")` open (D1).
- Add a `project_id` column (+ index) to durable store tables that are currently
  isolated only by their schema; scope all queries by `project_id`.
- One-time data migration: copy each existing live `h*` schema's tables into the global
  schema, stamping `project_id` from the originating data dir. Must run before first
  boot on the new code; idempotent and re-runnable.
- Keep `PathDerivedSchema` as a deprecated variant for one release for rollback; remove
  after.
- Done-when: no production code path constructs `PathDerivedSchema`; `rg
  "PathDerivedSchema|from_path" crates --glob '!*test*'` returns only the deprecated def.

### P3 — Background safety reaper (defense in depth) — ✅ IMPLEMENTED 2026-05-31
- **Done**: `reap_orphaned_path_schemas` is now wired into the server background via a new
  `crates/harness-server/src/http/orphan_reaper.rs` (spawned in `http/mod.rs` startup
  alongside the other pollers), closing the U-26 gap (was CLI-only). Gated by new config
  `storage.orphan_reaper_enabled` (default true) / `storage.orphan_reaper_interval_secs`
  (default 3600) on `WorkflowStoragePolicy`. Uses the existing conservative
  parent-dir-NotFound oracle; pool sourced from `workflow_runtime_store.pool()`.
- Verified: `cargo check -p harness-core -p harness-server` clean;
  `RUSTFLAGS=-Dwarnings cargo check --all-targets` clean; 19 config + schema-registry
  tests pass; `cargo fmt --all -- --check` clean. Takes effect on next server restart.
- **Still open (P3.2)**: unify the two divergent classifiers and widen beyond
  `owner_kind='path_derived_store'` to also reap `path_derived_schema` /
  `path_derived_directory_store`. Untracked schemas (no registry row) remain out of
  scope for the background reaper and are handled by the one-off P1 cleanup.

## 5. Migration & rollback

- P1 dump-first; P2 keep the deprecated variant + a documented downgrade for one
  release; data migration is idempotent and re-runnable.
- Rollback: restore from `pg_dump`; revert the StoreLocation routing commit; the
  deprecated `PathDerivedSchema` path still compiles and opens old schemas.

## 6. Risks

- **Dropping a live schema** (P1/P3): mitigated by keep-list from active data dirs,
  dump-first, and the conservative parent-dir-NotFound oracle.
- **Data migration correctness** (P2): the live `threads`/`workflow_events`/
  `runtime_events` rows must survive the schema move — verify row counts pre/post; W-42
  fidelity check (this is a multi-step artifact migration).
- **Shared-schema contention**: moving from per-path isolation to a shared schema with
  `project_id` adds query predicates; ensure indexes on `project_id`.
- **Behavior change in hot workspace path** (ephemeral → LocalFile): confirm no two
  in-flight jobs depend on sharing a path-derived schema.

## 7. Done-when (acceptance)

1. `cargo test` (full) adds **zero** new `h*` schemas. [P0]
2. Schema count after cleanup ≈ live durable schemas; DB size materially reduced. [P1]
3. No production code constructs `PathDerivedSchema`. [P2]
4. Live data (threads/events/runtime_jobs row counts) preserved across migration. [P2]
5. Background reaper tick visible in logs; orphan injected → reaped next interval;
   live-schema-never-dropped regression test green. [P3]
6. Slow-statement WARN rate and event-stream-lag errors drop after P1+P2 (compare
   against the 2026-05-31 baseline in `runtime-health-2026-05-31.md`).
7. `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`,
   `cargo test --workspace`, `cargo fmt --all -- --check` all clean.

## 8. Open decisions for the operator

- ~~D1: durable stores granularity~~ — **RESOLVED 2026-05-31: one global SharedSchema +
  `project_id` column** (see §3).
- D2: P1 cleanup now (recover 22 GB) before P2, or do P2 first then clean once?
  (Recommend P1 now — it is the active perf drag feeding backpressure.)
- D3: grace period before an untracked `h*` schema is treated as reapable in P3.
- D4: ship P0–P3 as one storage release or incrementally (P0 first is independently shippable).

## 9. Suggested sequencing
P0 (test isolation) → P1 (cleanup, recover) → P2 (durable migration) → P3 (background
reaper). P0 and P1 are independently valuable and low-risk; P2 is the structural fix;
P3 is the long-term guard. Each phase is independently verifiable per §7.
