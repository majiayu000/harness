use harness_core::db::Migration;

/// Versioned migrations for the tasks table.
///
/// v1 – baseline schema (all columns including those added in later iterations)
/// v2/v3/v4 – additive ALTER TABLE for databases that predate v1 tracking;
///   duplicate-column errors are silently ignored by PgMigrator.
/// v5 – add task_artifacts table for persisting agent output per task turn.
/// v10 – add composite index on (project, status, updated_at).
/// v11 – add task_checkpoints table for phase recovery.
/// v12 – add priority column for priority-based task scheduling.
/// v13 – add phase column for consistent phase persistence across cache/DB paths.
/// v14 – add description column for task observability after restart.
/// v15 – add (status, updated_at DESC) index for project-agnostic terminal queries.
/// v18 – add version column for optimistic locking.
/// v19 – add task_kind column for first-class task lifecycle dispatch.
/// v20 – add system_input column for restart-safe system prompt bundles.
/// v21 – add workspace lifecycle ownership and failure classification columns.
/// v22 – add scheduler_state column as the single authoritative ownership/recovery contract.
/// v23 – add indexes for server-side task list filters.
/// v24 – add persisted workspace lease rows for the per-project worktree pool.
/// v25 – add process start-time proof to persisted workspace leases.
/// v26 – scope task-owned tables within a fixed shared schema using store_key.
pub(super) static TASK_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create tasks table",
        sql: "CREATE TABLE IF NOT EXISTS tasks (
            id          TEXT PRIMARY KEY,
            status      TEXT NOT NULL DEFAULT 'pending',
            turn        BIGINT NOT NULL DEFAULT 0,
            pr_url      TEXT,
            rounds      TEXT NOT NULL DEFAULT '[]',
            error       TEXT,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "add source column",
        sql: "ALTER TABLE tasks ADD COLUMN source TEXT",
    },
    Migration {
        version: 3,
        description: "add external_id column",
        sql: "ALTER TABLE tasks ADD COLUMN external_id TEXT",
    },
    Migration {
        version: 4,
        description: "add parent_id column",
        sql: "ALTER TABLE tasks ADD COLUMN parent_id TEXT",
    },
    Migration {
        version: 5,
        description: "create task_artifacts table",
        sql: "CREATE TABLE IF NOT EXISTS task_artifacts (
            id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            task_id       TEXT NOT NULL,
            turn          BIGINT NOT NULL DEFAULT 0,
            artifact_type TEXT NOT NULL,
            content       TEXT NOT NULL,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 6,
        description: "add repo column",
        sql: "ALTER TABLE tasks ADD COLUMN repo TEXT",
    },
    Migration {
        version: 7,
        description: "add depends_on column",
        sql: "ALTER TABLE tasks ADD COLUMN depends_on TEXT NOT NULL DEFAULT '[]'",
    },
    Migration {
        version: 8,
        description: "add project column for task observability",
        sql: "ALTER TABLE tasks ADD COLUMN project TEXT",
    },
    Migration {
        version: 10,
        description: "add index on tasks(project, status, updated_at) for dashboard queries",
        sql: "CREATE INDEX IF NOT EXISTS idx_tasks_project_status_updated \
              ON tasks(project, status, updated_at DESC)",
    },
    Migration {
        version: 11,
        description: "create task_checkpoints table for phase recovery",
        sql: "CREATE TABLE IF NOT EXISTS task_checkpoints (
            task_id       TEXT PRIMARY KEY,
            triage_output TEXT,
            plan_output   TEXT,
            pr_url        TEXT,
            last_phase    TEXT NOT NULL,
            updated_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 12,
        description: "add priority column for task scheduling",
        sql: "ALTER TABLE tasks ADD COLUMN priority BIGINT NOT NULL DEFAULT 0",
    },
    Migration {
        version: 13,
        description: "add phase column for consistent phase persistence",
        sql: r#"ALTER TABLE tasks ADD COLUMN phase TEXT NOT NULL DEFAULT '"implement"'"#,
    },
    Migration {
        version: 14,
        description: "add description column for task observability",
        sql: "ALTER TABLE tasks ADD COLUMN description TEXT",
    },
    Migration {
        version: 15,
        description: "add index on tasks(status, updated_at) for project-agnostic terminal queries",
        sql: "CREATE INDEX IF NOT EXISTS idx_tasks_status_updated \
              ON tasks(status, updated_at DESC)",
    },
    Migration {
        version: 16,
        description: "add request_settings column for execution limit recovery",
        sql: "ALTER TABLE tasks ADD COLUMN request_settings TEXT",
    },
    Migration {
        version: 17,
        description: "create task_prompts table for per-turn prompt persistence",
        sql: "CREATE TABLE IF NOT EXISTS task_prompts (
            id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            task_id    TEXT NOT NULL,
            turn       BIGINT NOT NULL,
            phase      TEXT NOT NULL,
            prompt     TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(task_id, turn, phase)
        ); CREATE INDEX IF NOT EXISTS idx_task_prompts_task_id
           ON task_prompts(task_id, turn)",
    },
    Migration {
        version: 18,
        description: "add version column for optimistic locking",
        sql: "ALTER TABLE tasks ADD COLUMN version INTEGER NOT NULL DEFAULT 0",
    },
    Migration {
        version: 19,
        description: "add task_kind column for explicit task lifecycle dispatch",
        sql: "ALTER TABLE tasks ADD COLUMN task_kind TEXT NOT NULL DEFAULT 'legacy'",
    },
    Migration {
        version: 20,
        description: "add system_input column for restart-safe system prompt bundles",
        sql: "ALTER TABLE tasks ADD COLUMN system_input TEXT",
    },
    Migration {
        version: 21,
        description: "add workspace lifecycle ownership and failure classification columns",
        sql: "ALTER TABLE tasks \
              ADD COLUMN failure_kind TEXT, \
              ADD COLUMN workspace_path TEXT, \
              ADD COLUMN workspace_owner TEXT, \
              ADD COLUMN run_generation BIGINT NOT NULL DEFAULT 0",
    },
    Migration {
        version: 22,
        description: "add scheduler_state column for unified task ownership and recovery",
        sql: "ALTER TABLE tasks ADD COLUMN scheduler_state TEXT NOT NULL DEFAULT \
              '{\"authority_state\":\"queued\",\"run_generation\":0,\"recovery_generation\":0}'",
    },
    Migration {
        version: 23,
        description: "add indexes for task list filters",
        sql: "CREATE INDEX IF NOT EXISTS idx_tasks_scheduler_state_created \
              ON tasks((scheduler_state::jsonb ->> 'authority_state'), created_at DESC); \
              CREATE INDEX IF NOT EXISTS idx_tasks_repo_created \
              ON tasks(repo, created_at DESC); \
              CREATE INDEX IF NOT EXISTS idx_tasks_source_created \
              ON tasks(source, created_at DESC); \
              CREATE INDEX IF NOT EXISTS idx_tasks_kind_created \
              ON tasks(task_kind, created_at DESC)",
    },
    Migration {
        version: 24,
        description: "create workspace leases table",
        sql: crate::workspace_lease_store::WORKSPACE_LEASES_TABLE_V24_SQL,
    },
    Migration {
        version: 25,
        description: "add process start-time proof to workspace leases",
        sql: "ALTER TABLE workspace_leases ADD COLUMN process_started_at BIGINT NOT NULL DEFAULT 0",
    },
    Migration {
        version: 26,
        description: "scope task db rows within shared schema",
        sql: "ALTER TABLE tasks ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE tasks
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE tasks ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE tasks ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE tasks DROP CONSTRAINT IF EXISTS tasks_pkey;
              ALTER TABLE tasks ADD CONSTRAINT tasks_pkey PRIMARY KEY (store_key, id);
              CREATE INDEX IF NOT EXISTS idx_tasks_store_created
              ON tasks(store_key, created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_tasks_store_status_updated
              ON tasks(store_key, status, updated_at DESC);
              CREATE INDEX IF NOT EXISTS idx_tasks_store_project_status_updated
              ON tasks(store_key, project, status, updated_at DESC);
              CREATE INDEX IF NOT EXISTS idx_tasks_store_repo_created
              ON tasks(store_key, repo, created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_tasks_store_source_created
              ON tasks(store_key, source, created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_tasks_store_kind_created
              ON tasks(store_key, task_kind, created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_tasks_store_scheduler_state_created
              ON tasks(store_key, (scheduler_state::jsonb ->> 'authority_state'), created_at DESC);

              ALTER TABLE task_artifacts ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE task_artifacts
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE task_artifacts ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE task_artifacts ALTER COLUMN store_key SET NOT NULL;
              CREATE INDEX IF NOT EXISTS idx_task_artifacts_store_task_id
              ON task_artifacts(store_key, task_id, turn, id);

              ALTER TABLE task_checkpoints ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE task_checkpoints
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE task_checkpoints ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE task_checkpoints ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE task_checkpoints DROP CONSTRAINT IF EXISTS task_checkpoints_pkey;
              ALTER TABLE task_checkpoints
              ADD CONSTRAINT task_checkpoints_pkey PRIMARY KEY (store_key, task_id);

              ALTER TABLE task_prompts ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE task_prompts
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE task_prompts ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE task_prompts ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE task_prompts
              DROP CONSTRAINT IF EXISTS task_prompts_task_id_turn_phase_key;
              CREATE UNIQUE INDEX IF NOT EXISTS idx_task_prompts_store_task_turn_phase
              ON task_prompts(store_key, task_id, turn, phase);
              CREATE INDEX IF NOT EXISTS idx_task_prompts_store_task_id
              ON task_prompts(store_key, task_id, turn);

              ALTER TABLE workspace_leases ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE workspace_leases
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE workspace_leases ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE workspace_leases ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE workspace_leases DROP CONSTRAINT IF EXISTS workspace_leases_pkey;
              ALTER TABLE workspace_leases
              ADD CONSTRAINT workspace_leases_pkey PRIMARY KEY (store_key, project_key, slot_index);
              CREATE INDEX IF NOT EXISTS idx_workspace_leases_store_task_state
              ON workspace_leases(store_key, task_id, state);
              CREATE INDEX IF NOT EXISTS idx_workspace_leases_store_workspace_key_state
              ON workspace_leases(store_key, project_key, workspace_key, state);
              CREATE INDEX IF NOT EXISTS idx_workspace_leases_store_state_owner
              ON workspace_leases(store_key, state, owner_session);

              CREATE TABLE IF NOT EXISTS task_db_legacy_backfills (
                  store_key     TEXT NOT NULL DEFAULT current_schema(),
                  legacy_schema TEXT NOT NULL,
                  copied_rows   BIGINT NOT NULL DEFAULT 0,
                  backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                  PRIMARY KEY (store_key, legacy_schema)
              )",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_lease_process_started_migration_uses_plain_add_column() {
        let create_migration = TASK_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 24)
            .expect("v24 migration should exist");
        let migration = TASK_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 25)
            .expect("v25 migration should exist");

        assert!(
            !create_migration.sql.contains("process_started_at"),
            "v24 should create the legacy workspace_leases shape so v25 can add the column"
        );
        assert!(
            migration
                .sql
                .contains("ALTER TABLE workspace_leases ADD COLUMN process_started_at"),
            "v25 should add process_started_at"
        );
        assert!(
            !migration.sql.to_ascii_uppercase().contains("IF NOT EXISTS"),
            "SQLite rejects ALTER TABLE ADD COLUMN IF NOT EXISTS"
        );
    }

    #[test]
    fn workspace_lease_create_table_includes_process_started_at() {
        assert!(
            crate::workspace_lease_store::WORKSPACE_LEASES_TABLE_SQL
                .contains("process_started_at  BIGINT NOT NULL DEFAULT 0"),
            "new workspace_leases tables should include process_started_at without v25"
        );
    }

    #[test]
    fn shared_schema_migration_scopes_task_owned_tables() {
        let migration = TASK_MIGRATIONS
            .iter()
            .find(|migration| migration.version == 26)
            .expect("v26 migration should exist");

        for table in [
            "tasks",
            "task_artifacts",
            "task_checkpoints",
            "task_prompts",
            "workspace_leases",
        ] {
            let needle = format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS store_key TEXT");
            assert!(
                migration.sql.contains(&needle),
                "v26 should add store_key to {table}"
            );
        }
        assert!(
            migration.sql.contains("task_db_legacy_backfills"),
            "v26 should create legacy backfill markers"
        );
        assert!(
            migration.sql.contains("idx_task_artifacts_store_task_id"),
            "v26 should keep artifacts-by-task reads backed by a scoped index"
        );
        assert!(
            migration
                .sql
                .contains("ON task_artifacts(store_key, task_id, turn, id)"),
            "artifact index should match the store_key + task_id query predicate"
        );
    }
}
