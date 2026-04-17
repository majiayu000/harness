use harness_core::db::Migration;

/// Maximum artifact content size in bytes before truncation.
pub(super) const ARTIFACT_MAX_BYTES: usize = 65_536;

/// Column list for `query_as::<_, TaskRow>` — single source of truth.
///
/// Every SELECT that maps to `TaskRow` MUST use this constant.
/// When adding a field to `TaskRow`, add the column here once and all queries
/// pick it up automatically.  The `task_row_columns_match_struct` test below
/// will fail if this list drifts from the struct definition.
pub(super) const TASK_ROW_COLUMNS: &str = "id, status, turn, pr_url, rounds, error, source, external_id, parent_id, created_at, repo, depends_on, project, priority, phase, description, request_settings";

/// Versioned migrations for the tasks table.
///
/// v1 – baseline schema (all columns including those added in later iterations)
/// v2/v3/v4 – additive ALTER TABLE for databases that predate v1 tracking;
///   duplicate-column errors are silently ignored by the Migrator.
/// v5 – add task_artifacts table for persisting agent output per task turn.
/// v10 – add composite index on (project, status, updated_at).
/// v11 – add task_checkpoints table for phase recovery.
/// v12 – add priority column for priority-based task scheduling.
/// v13 – add phase column for consistent phase persistence across cache/DB paths.
/// v14 – add description column for task observability after restart.
pub(super) static TASK_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create tasks table",
        sql: "CREATE TABLE IF NOT EXISTS tasks (
            id          TEXT PRIMARY KEY,
            status      TEXT NOT NULL DEFAULT 'pending',
            turn        INTEGER NOT NULL DEFAULT 0,
            pr_url      TEXT,
            rounds      TEXT NOT NULL DEFAULT '[]',
            error       TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
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
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id       TEXT NOT NULL,
            turn          INTEGER NOT NULL DEFAULT 0,
            artifact_type TEXT NOT NULL,
            content       TEXT NOT NULL,
            created_at    TEXT NOT NULL DEFAULT (datetime('now'))
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
            updated_at    TEXT NOT NULL
        )",
    },
    Migration {
        version: 12,
        description: "add priority column for task scheduling",
        sql: "ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
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
        description: "add request_settings column for per-task guardrail recovery",
        sql: "ALTER TABLE tasks ADD COLUMN request_settings TEXT",
    },
];
