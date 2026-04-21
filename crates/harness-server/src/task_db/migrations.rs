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
        description: "add issue column for github issue number persistence",
        sql: "ALTER TABLE tasks ADD COLUMN issue BIGINT",
    },
];
