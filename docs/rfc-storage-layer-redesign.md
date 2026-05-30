# RFC: Storage Layer Redesign — Decoupled, Bounded, File-First for Ephemeral State

Status: Draft
Date: 2026-05-31
Author: (design review)

## 1. Summary

Harness currently maps **every store-identity path to its own PostgreSQL schema**
(`pg_schema_for_path` → `h{hash}`). Because store paths embed per-task /
per-thread / per-workspace identifiers, and schemas are **never dropped**, the
production database has accumulated **~539k schemas and ~9M `pg_class` objects**
(DB size ~79 GB) while only ~10 workspaces exist on disk. This catalog bloat is
the dominant live performance drag (slow cross-schema scans, 5 GB+ RSS, stuck
repo-backlog scans).

This RFC proposes:
1. **Decouple persistence behind a `Backend` / `Repository` abstraction** so domain
   code no longer depends on `PgStoreContext`/`PgPool` directly.
2. **Stop schema-per-path.** Durable orchestration state lives in a small fixed
   set of shared Postgres schemas, row-keyed by id — never one schema per entity.
3. **Move ephemeral per-workspace/per-run scratch to local files** (SQLite or
   JSONL) placed *inside the workspace directory*, so they are created cheaply and
   removed together with the workspace's existing worktree cleanup (see §6.3 for
   the exact lifecycle requirement) — mirroring `~/.claude/` and `~/.codex/`.

**Core decision (one line):** decouple a store's *logical identity* from its
*physical location* — durable orchestration state → a small fixed set of shared
Postgres schemas (row-keyed, never one-schema-per-entity); ephemeral per-run
scratch → workspace-local files. No store path is ever hashed into its own schema.

## 2. Problem & Evidence (facts)

- `pg_schema_for_path(path)` hashes a normalized path → `h{16hex}` schema and
  `CREATE SCHEMA IF NOT EXISTS` on first use. `crates/harness-core/src/db_pg.rs:311`.
- There is **no `DROP SCHEMA` anywhere** in the codebase; `finish_runtime_workspace`
  removes the git worktree only. `crates/harness-server/src/workflow_runtime_worker/workspace.rs:146`.
- Storage is **Postgres-only** (`sqlx` features = `postgres` only) with hand-written
  Postgres SQL per store (`$1`, `TIMESTAMPTZ`, `ON CONFLICT`). No backend abstraction.
- Empirical (live DB, 2026-05-30): `information_schema.schemata` h-schemas ≈ 539k;
  `workflow_instances` tables ≈ 28k; `pg_class` ≈ 9.0M; DB size 79 GB; on-disk
  workspaces = 10. h-schema table signatures: ~115k tasks/task_artifacts/…,
  ~111k events/scan_watermarks, ~85k projects, ~82k threads, ~26k full
  workflow-runtime sets.
- Canonical/live data is small and already lives in fixed-name schemas
  (`workflow_runtime` has ~1195 instances; plus `workflow_issue`, `workflow_project`,
  `workflow_codex_runtime_*`).

### Two distinct store usages (inference, high confidence; verify exact call-path)
- **Server-side shared stores** (opened once at startup at fixed `data_dir/<name>`
  paths): a small, bounded set — the server's own orchestration state.
- **Per-workspace stores** (the source of the 539k explosion): a `harness` process
  running with `project_root = <workspace>` (the spawned agent and/or per-run
  worker) opens its own task/thread/event/etc. stores rooted at the workspace path,
  yielding a fresh schema set per workspace that is never reclaimed.
  > Open question O-1: confirm the exact process + call path that opens stores with
  > a workspace-scoped `project_root`. The redesign targets this path regardless.

## 3. Goals / Non-Goals

**Goals**
- Bound the number of Postgres schemas to O(store-types), not O(entities).
- Decouple domain logic from the concrete Postgres backend (testability,
  swappability, leaf dependency).
- Tie ephemeral state lifetime to the workspace lifetime (auto-GC, no leak).
- Preserve all durable orchestration semantics (cross-restart recovery, the
  workflow runtime event ledger, idempotency keys).

**Non-Goals**
- Rewriting the workflow runtime data model (it already uses a single shared
  `workflow_runtime` schema — keep it).
- Supporting arbitrary external databases.
- Changing agent/CLI behavior beyond where it opens stores.

## 4. Reference Designs (facts, observed on this machine)

| | Ephemeral session/task state | Structured state | DB objects |
|---|---|---|---|
| **Claude Code** (`~/.claude/`) | one append-only JSONL per session: `projects/<slug>/<uuid>.jsonl` + `sessions-index.json` | none (flat JSON/Markdown) | 0 |
| **Codex** (`~/.codex/`) | one JSONL rollout per session: `sessions/YYYY/MM/DD/rollout-*.jsonl` | a few **fixed** shared SQLite DBs: `goals_1`, `logs_2`, `memories_1`, `state_5` | ~4 |
| **harness (today)** | **a PG schema set per workspace/task/thread** | same | **~539k schemas / 9M** |

Principle both tools share: ephemeral state → append-only local files; any DB is a
**small fixed set** of shared databases keyed by id columns. Neither creates a
database/schema per entity.

## 5. Root Cause

The Postgres backend preserved the legacy SQLite "one `.db` file per store-identity
path" model by mapping each path → a Postgres **schema**. SQLite files are cheap and
GC by file deletion; Postgres schemas are catalog objects that must be explicitly
dropped. Combining schema-per-path with per-workspace paths and zero cleanup
produced unbounded catalog growth. The defect is **identity→schema coupling**, not
"using a database."

## 6. Proposed Architecture

### 6.1 Storage taxonomy → routing

Classify each store by (durability, scope) and route it:

| Store | Class | Target backend |
|---|---|---|
| workflow runtime (instances/events/decisions/commands/jobs/artifacts/prompt_payloads) | durable, global orchestration | **Postgres, shared `workflow_runtime` schema** (already) |
| project_registry | durable, global | Postgres, shared schema |
| issue_workflow / project_workflow | durable, global | Postgres, shared schema (row-keyed by project_id) |
| review_findings | durable, global | Postgres, shared schema (row-keyed by project_root) |
| q_value / rule_experiences | durable, global (cross-run learning) | Postgres, shared schema |
| event_store (events + scan_watermarks) | durable audit, global | Postgres, shared schema (row-keyed by project/session) — OR local JSONL per project (see §6.4) |
| runtime_state snapshot | durable, single-row global | Postgres, shared schema |
| **per-workspace task scratch / threads / per-run events** | **ephemeral, per-workspace** | **Local file under the workspace dir / `~/.harness/workspaces/<id>/`** |

Rule of thumb: **global, cross-restart, cross-entity-queried → shared Postgres
schema (row-keyed). Per-workspace, recreated-per-run, scoped-to-one-id → local
file.**

### 6.2 Decoupling: Backend + Repository traits

Introduce a dedicated leaf crate `harness-store` (or a module in `harness-core`)
that domain crates depend on via traits — not `PgStoreContext`/`PgPool`.

```rust
/// A logical store location, independent of physical backend.
pub enum StoreLocation {
    /// Durable, shared: a named table-group inside a fixed shared schema.
    Shared { schema: &'static str },        // e.g. "workflow_runtime"
    /// Ephemeral, local: a file rooted at a workspace/project dir.
    LocalFile { path: PathBuf },            // e.g. ~/.harness/workspaces/<id>/threads.sqlite
}

#[async_trait]
pub trait Backend: Send + Sync {
    /// Open (and migrate) a connection/handle for a store at `loc`.
    /// `migrations` matches the existing API: a slice of the `Migration` struct
    /// already defined in `crates/harness-core/src/db.rs` (passed as `&[Migration]`).
    async fn open(&self, loc: &StoreLocation, migrations: &[Migration]) -> Result<StoreHandle>;
}

/// Generic typed repository over a JSON-blob entity (most stores already are KV-JSON).
/// Note: the existing `DbEntity` trait (`crates/harness-core/src/db.rs`) has no
/// `Filter` associated type, so the generic repository deliberately exposes only
/// id-keyed CRUD + unfiltered `list`. Filtered/aggregate queries live on
/// store-specific repository traits (see below), which is where cross-entity
/// query needs actually exist (e.g. TaskRepository, ReviewRepository).
#[async_trait]
pub trait Repository<E: DbEntity>: Send + Sync {
    async fn get(&self, id: &str) -> Result<Option<E>>;
    async fn put(&self, e: &E) -> Result<()>;
    async fn list(&self) -> Result<Vec<E>>;
    async fn delete(&self, id: &str) -> Result<()>;
}

/// Store-specific traits add the filtered/aggregate queries each store needs,
/// keeping the generic `Repository<E>` minimal and dialect-agnostic. Example:
#[async_trait]
pub trait TaskRepository: Repository<Task> {
    async fn list_summaries(&self, filter: TaskSummaryFilter) -> Result<Vec<TaskSummary>>;
}
```

- Domain code depends on `Repository<E>` (and store-specific traits like
  `TaskRepository`, `ThreadRepository`), never on `PgPool`.
- The existing service traits (`ProjectService`, `TaskService`, `ExecutionService`)
  are extended/used as the seam; AppState holds `Arc<dyn …Repository>`.
- Two `Backend` impls: `PostgresBackend` (shared schema) and `LocalFileBackend`
  (SQLite file or JSONL). Backend chosen by **store type + config**, not by hashing
  a path.

### 6.3 `~/.harness/` layout (aligned with `~/.claude`, `~/.codex`)

```
~/.harness/
  config.toml                      # server config (optional)
  projects/<project-slug>/         # durable-but-local per-project state, if any
  workspaces/<workspace-id>/       # EPHEMERAL per-run scratch — dies with the workspace
    threads.sqlite
    tasks.sqlite                   # per-run task scratch (NOT the global task store)
    events.jsonl                   # append-only per-run event log
  logs/                            # already used
```

- **Auto-GC requires the files to live *inside* the workspace directory.** Today
  `remove_workspace` (`crates/harness-server/src/workspace.rs:285`) only runs
  `git worktree remove` on the worktree path — it does **not** touch any mirrored
  `~/.harness/workspaces/<id>/` directory. Therefore:
  - **Default (recommended):** place per-workspace store files *under the workspace
    dir itself* (e.g. `<workspace>/.harness/threads.sqlite`), so the existing
    worktree removal deletes them with no new code.
  - **If** a mirrored `~/.harness/workspaces/<id>/` layout is preferred (e.g. to
    keep scratch out of the git worktree), then Phase 3 **must** extend
    `remove_workspace`/`finish_runtime_workspace` to explicitly delete the mirrored
    directory. This is an explicit lifecycle requirement, not free behavior.
  Either way: no central catalog, no per-entity schema.

### 6.4 Backend implementations

- **PostgresBackend (shared schema)**: one fixed schema per *domain group*
  (e.g., `workflow_runtime`, `harness_core`). Entities carry `project_id` /
  `workspace_id` columns for scoping; cross-entity queries use indexes on those
  columns. Bounded schema count = number of domain groups (single digits).
- **LocalFileBackend**: SQLite file (preferred — keeps SQL semantics; `sqlx`
  `sqlite` feature) or append-only JSONL for pure event logs. Files are created on
  demand and removed with the workspace. SQLite dialect differences from Postgres
  for these simple KV tables are small (`$1`→`?`, `TIMESTAMPTZ`→`TEXT`/INTEGER,
  `ON CONFLICT` supported by both).

### 6.5 Lifecycle integration

- Ephemeral store handles are created under the workspace dir; their lifetime ==
  workspace lifetime. Removal is automatic (worktree/dir deletion).
- For the interim (before files land), add a **stopgap**: when a workspace is
  removed, drop the Postgres schemas derived from its store paths (the band-aid
  ① fix). This is removed once ephemeral state is file-backed.

## 7. Decoupling Seams & Dependency Direction

- Today: `harness-core::db` is a leaf, but every crate imports `PgStoreContext`
  directly; ~85% of access is via concrete AppState fields, ~15% via the three
  service traits. Coupling is high and Postgres-specific.
- Target dependency direction: `domain crates → harness-store traits →
  {PostgresBackend | LocalFileBackend}`. Concrete backends are wired only at
  startup (the five `build_*` phases in `http/init.rs`), which is the single
  construction choke point (Seam A/E from the wiring analysis).
- Lowest-churn rollout: introduce traits + back them with the *existing* Postgres
  code first (no behavior change), then swap ephemeral stores to `LocalFileBackend`.

## 8. Migration & Rollout (phased)

1. **Stopgap (in progress)**: one-time cleanup of the 539k orphan schemas
   (data-bearing schemas backed up via `pg_dump`, then batch `DROP SCHEMA CASCADE`);
   plus ① drop-schema-on-workspace-finish to stop new accumulation.
2. **Abstraction**: add `harness-store` crate with `Backend`/`Repository` traits;
   implement `PostgresBackend` wrapping the current code. Re-point stores through
   traits with zero behavior change. Add `sqlx` `sqlite` feature.
3. **Route ephemeral → files**: implement `LocalFileBackend`; migrate per-workspace
   scratch stores (threads/per-run tasks/per-run events) to files under the
   workspace dir. No data migration needed (scratch is recreated per run).
4. **Consolidate durable → shared schema**: ensure all durable stores use fixed
   shared schemas, row-keyed; remove any remaining per-path schema usage.
5. **Remove `pg_schema_for_path`-per-path** and confirm no new `h*` schemas are
   created (assert in a test / startup check).
6. **Docs**: update architecture docs + `RELEASING.md`; add a guard that fails CI
   if schema count grows unbounded (observability hook).

## 9. Risks & Tradeoffs

- **SQLite dialect work** for the file backend (bounded: ~5 simple KV stores).
- **Dual-backend testing**: tests currently force Postgres + a global lock; the
  abstraction enables faster file/in-memory tests (a benefit) but needs coverage.
- **Cross-restart semantics**: ensure nothing currently *relies* on a per-workspace
  schema persisting across runs (verify via O-1). Reusable workspaces keep their
  files on disk (not removed until terminal), preserving reuse.
- **Supabase pooler**: shared-schema design is friendlier to pgbouncer/Supavisor
  than schema-per-path (fewer search_path variants → fewer pool objects).
- **Big-bang risk**: mitigated by phasing — traits-over-existing-Postgres first.

## 10. Open Questions

- **O-1 (largely answered — confirm exact line in Phase 2):** Empirically, the
  exploded schemas are keyed by **per-runtime-job workspace paths** under
  `~/Library/Application Support/harness-codex-runtime/workspaces/` — e.g. a
  `threads` row's `cwd` = `.../harness-codex-runtime/workspaces/runtime-job-<hash>`,
  and a `tasks` row's `workspace_path` = `.../workspaces/<hash>__<repo>__issue_N`.
  The per-job workspace id is minted at
  `crates/harness-server/src/workflow_runtime_worker/workspace.rs:244`
  (`runtime-job-{stable_hash_8(job.id)}`). So it is the **harness Codex-runtime
  path (server-side), not the external agent**, that opens ThreadDb/TaskDb/EventStore
  rooted at the per-job workspace → one schema-set per runtime job. Remaining
  confirmation for Phase 2: pin the exact store-open call site in the Codex
  adapter / per-job harness init (data dir `harness-codex-runtime`) — that is the
  file-backend injection point.
- **O-2**: Should `event_store` audit be local JSONL per project (CC/Codex style)
  or a shared Postgres table? Depends on cross-project query needs.
- **O-3**: Do any "durable per-workspace" stores (per inventory) actually need
  cross-restart survival, or are they scratch? (e.g., per-run task scratch vs the
  global task store.)

## 11. Alternatives Considered

- **A. Keep Postgres, single shared schema + `workspace_id` column (no files).**
  Eliminates the explosion without SQLite work, but keeps ephemeral scratch in the
  shared DB (more write load, no auto-GC-with-workspace) and does not match the
  `~/.harness/` file-first direction. Viable fallback if SQLite cost is undesirable.
- **B. Periodic GC sweep that drops orphan schemas.** Treats the symptom; schemas
  still churn and bloat the catalog between sweeps. Rejected as the primary fix
  (kept only as the interim stopgap).
- **C. Status quo + raise Postgres limits.** Does not scale; catalog operations
  degrade regardless. Rejected.
