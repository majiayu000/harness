# Orphan Postgres Schema — Remediation Research

Decision-enabling research (no code changed). Goal: decide how deep to go on the
13.6k-schema / 22 GB problem. Fact / Inference / Suggestion separated per W-11.

## 1. Current state (Fact)

- [source: psql] 13,618 schemas, 13,602 match `^h[0-9a-f]{16}$`; DB 22 GB.
- [source: `harness_admin.schema_ownership`] registry breakdown:

  | owner_kind | retention_class | rows |
  |---|---|---|
  | `path_derived_schema` | `unattributed_path_derived` | 1,359 |
  | `path_derived_store` | `path_derived` | 1,146 |
  | `path_derived_directory_store` | `path_derived` | 49 |

  Total registry rows: 2,554. **Untracked workspace schemas (no registry row): 11,048.**
- [source: fs check] of 1,195 rows with a non-null `owner_path`, only **8 still exist**
  on disk; 1,187 point to gone `…/T/.tmp*` macOS temp dirs (test residue).

## 2. What the existing tooling actually does (Fact, from code)

There are **two cleanup paths with different philosophies** (this inconsistency is
itself a finding — RS-12):

### a. `reap_orphaned_path_schemas` (db_pg_schema_registry.rs:452) — the "#1216 reaper"
- Scans **only** `owner_kind = 'path_derived_store'` (1,146 rows). Ignores
  `path_derived_schema` (1,359) and `path_derived_directory_store` (49).
- Orphan signal: the owner_path's **parent directory** is missing, and only on a
  definitive `NotFound` (IO errors → keep). Conservative; "never drops a schema that
  could still be reopened."
- **NOT wired into the server background.** Despite the commit title "automatic …
  reaper", the only caller is the CLI `harness-pg-schema-cleanup --reap-orphans`
  (`--confirm-drop` to apply). → U-26 declaration-execution gap: declared automatic,
  not actually scheduled.

### b. `pg_schema_cleanup_plan` / `apply_pg_schema_cleanup` (same file) — the planner
- `classify_schema_cleanup_candidate` decisions:
  - unregistered → **Blocked** ("requires explicit allowlist")
  - `owner_kind == path_derived_store` → **Keep** ("owner_path is an identity key,
    not a liveness check") — note: the *opposite* stance from path (a) above.
  - other registered + owner_path missing → **DropCandidate**
  - owner_path exists / none → Keep
- Drops require explicit `--schema X` or `--allow-unregistered X` + `--confirm-drop`,
  one name at a time.

### Net reach of running the tools today
- `--reap-orphans` would drop ≈ the path_derived_store rows whose parent dir is gone
  (subset of 1,146). It would **not** touch the 1,359 path_derived_schema, the 49
  directory stores, or the 11,048 unregistered.
- Estimated residue after the existing reaper: **~12,400 schemas still present.**

## 3. Why orphans exist at all (Fact + Inference)

- [source: code] CREATE SCHEMA sites: `pg_create_schema_if_not_exists`, `ensure_schema`,
  `pg_schema_for_path` (a schema is the SHA of a store-identity path). [source: code]
  workspace cleanup has **no DROP SCHEMA**; the only DROP in the codebase is the reaper.
- [inference, high] Two independent leak sources, neither fixed by a reaper:
  1. **Production**: every per-job/workspace store does CREATE SCHEMA, nothing DROPs on
     teardown → growth is bounded only by how often the reaper is run.
  2. **Tests**: the suite opens stores at `tempdir()` paths → each hashes to a unique
     schema in the **shared local Postgres**, never dropped. Every `cargo test` against
     this DB regenerates a batch (origin of the 1,187 `.tmp*` orphans).

## 4. Answer to "will orphans never happen again?" (Inference, high)

**No — not from cleanup alone.** A reaper is a janitor, not a leak fix. Achievable
end-states:

| Action | End-state |
|---|---|
| One-off cleanup | current 13.6k cleared; **recurs** |
| Cleanup + reaper wired to background, widened to all owner_kinds + untracked | **self-healing, bounded** — orphans created then reaped on a cycle; never 0, but never 22 GB |
| + drop-on-teardown in workspace cleanup + isolated/ephemeral test DB | orphans largely **not created** at the source (true fix; architectural) |

## 5. Options & risks (Suggestion)

### Layer 1 — one-off cleanup (low risk, reversible-ish)
- [assumption: temp-dir owners are truly gone] `harness-pg-schema-cleanup --reap-orphans
  --confirm-drop` clears the path_derived_store orphans. For the 11,048 untracked + 1,359
  path_derived_schema, either widen the tool or script targeted `DROP SCHEMA CASCADE`
  for `^h[0-9a-f]{16}$` schemas not in `schema_ownership` **and** not the live data_dir
  schema (the ~8 live ones, e.g. `h136534546a051d3d`).
- Risk: dropping a schema that *is* live. Mitigation: exclude schemas whose registry
  owner_path (or, for untracked, a derived liveness check) still resolves; take a
  `pg_dump --schema-only` of the catalog first; run with server paused.

### Layer 2 — make the reaper real (medium)
- [assumption: bounded growth is acceptable] Wire `reap_orphaned_path_schemas` into a
  background interval (it is currently CLI-only), and reconcile the two divergent
  classifiers (a vs b) into one liveness oracle. Widen the WHERE clause beyond
  `path_derived_store`, and add an untracked-schema enumerator gated by the same
  parent-dir-missing rule.
- Risk: the untracked schemas have **no owner_path**, so the parent-dir oracle doesn't
  apply — need a different liveness signal (e.g. derive expected path, or treat any
  `^h[0-9a-f]{16}$` not in the registry and not referenced by an active workspace as
  reapable after a grace period). Getting this wrong drops live data. Needs a
  regression guard test before enabling.

### Layer 3 — stop creating them (high effort, true fix)
- [assumption: per-job schemas are the design] Add DROP SCHEMA on workspace
  `cleanup: on_terminal`; OR move per-job stores off dedicated schemas (storage RFC).
- Tests: point the suite at an **isolated/ephemeral DB** (per-run database, or
  `DROP SCHEMA` in test teardown) so `cargo test` stops polluting the shared local PG.
- Risk: behavior change in the hot workspace path; must not drop a schema another
  in-flight job shares.

## 6. Recommendation (Suggestion)

1. **Now**: Layer 1 one-off cleanup (paused server, dump-first) to get off 22 GB and
   restore query speed — this is what's choking COMMIT/UPDATE and feeding the event-
   stream lag.
2. **Next**: Layer 2 — wire the reaper into the background + unify the two classifiers,
   so it stays bounded without manual runs. This is the smallest change with lasting
   effect and matches the storage-RFC direction (#1206/#1215/#1216).
3. **Then, if recurrence still annoys**: Layer 3 test-DB isolation (kills the largest
   observed orphan class cheaply) before the heavier drop-on-teardown work.

Open questions for the operator:
- Is per-job-schema the intended long-term design, or should stores share one schema?
  (Determines whether Layer 3 is "drop on teardown" or "stop per-job schemas".)
- Acceptable grace period before an untracked `h*` schema is considered reapable?
