use harness_core::db::Migration;

pub(super) static WORKFLOW_RUNTIME_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create generic workflow runtime tables",
        sql: "CREATE TABLE IF NOT EXISTS workflow_definitions (
            id              TEXT NOT NULL,
            version         BIGINT NOT NULL,
            data            JSONB NOT NULL,
            active          BOOLEAN NOT NULL DEFAULT TRUE,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (id, version)
        );

        CREATE TABLE IF NOT EXISTS workflow_instances (
            id              TEXT PRIMARY KEY,
            definition_id   TEXT NOT NULL,
            state           TEXT NOT NULL,
            subject_type    TEXT NOT NULL,
            subject_key     TEXT NOT NULL,
            parent_workflow_id TEXT,
            data            JSONB NOT NULL,
            version         BIGINT NOT NULL DEFAULT 0,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS workflow_events (
            id              TEXT PRIMARY KEY,
            workflow_id     TEXT NOT NULL,
            sequence        BIGINT NOT NULL,
            event_type      TEXT NOT NULL,
            source          TEXT NOT NULL,
            data            JSONB NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE (workflow_id, sequence)
        );

        CREATE TABLE IF NOT EXISTS workflow_decisions (
            id              TEXT PRIMARY KEY,
            workflow_id     TEXT NOT NULL,
            event_id        TEXT,
            accepted        BOOLEAN NOT NULL,
            data            JSONB NOT NULL,
            rejection_reason TEXT,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS workflow_commands (
            id              TEXT PRIMARY KEY,
            workflow_id     TEXT NOT NULL,
            decision_id     TEXT,
            command_type    TEXT NOT NULL,
            dedupe_key      TEXT NOT NULL,
            status          TEXT NOT NULL,
            data            JSONB NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE (workflow_id, dedupe_key)
        );

        CREATE TABLE IF NOT EXISTS runtime_jobs (
            id              TEXT PRIMARY KEY,
            command_id      TEXT NOT NULL,
            runtime_kind    TEXT NOT NULL,
            runtime_profile TEXT NOT NULL,
            status          TEXT NOT NULL,
            data            JSONB NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS runtime_events (
            id              TEXT PRIMARY KEY,
            runtime_job_id  TEXT NOT NULL,
            sequence        BIGINT NOT NULL,
            event_type      TEXT NOT NULL,
            data            JSONB NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE (runtime_job_id, sequence)
        );

        CREATE TABLE IF NOT EXISTS workflow_artifacts (
            id              TEXT PRIMARY KEY,
            workflow_id     TEXT NOT NULL,
            runtime_job_id  TEXT,
            artifact_type   TEXT NOT NULL,
            data            JSONB NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "index generic workflow runtime lookups",
        sql: "CREATE INDEX IF NOT EXISTS idx_workflow_instances_subject
              ON workflow_instances (definition_id, subject_type, subject_key);
              CREATE INDEX IF NOT EXISTS idx_workflow_events_workflow_sequence
              ON workflow_events (workflow_id, sequence);
              CREATE INDEX IF NOT EXISTS idx_workflow_commands_status
              ON workflow_commands (status, created_at);
              CREATE INDEX IF NOT EXISTS idx_runtime_jobs_status
              ON runtime_jobs (status, created_at);
              CREATE INDEX IF NOT EXISTS idx_runtime_events_job_sequence
              ON runtime_events (runtime_job_id, sequence)",
    },
    Migration {
        version: 3,
        description: "promote runtime job not_before to indexed column",
        sql: "ALTER TABLE runtime_jobs
              ADD COLUMN IF NOT EXISTS not_before TIMESTAMPTZ;
              UPDATE runtime_jobs
              SET not_before = (data->>'not_before')::timestamptz
              WHERE not_before IS NULL
                AND jsonb_typeof(data->'not_before') = 'string'
                AND data->>'not_before' ~ '^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}';
              CREATE INDEX IF NOT EXISTS idx_runtime_jobs_ready
              ON runtime_jobs (status, not_before, created_at)",
    },
    Migration {
        version: 4,
        description: "index runtime workflow handle lookups",
        sql: "CREATE INDEX IF NOT EXISTS idx_workflow_instances_state
              ON workflow_instances (definition_id, state, updated_at);
              CREATE INDEX IF NOT EXISTS idx_workflow_instances_task_id
              ON workflow_instances ((data->'data'->>'task_id'))",
    },
    Migration {
        version: 5,
        description: "index runtime workflow handle history",
        sql: "CREATE INDEX IF NOT EXISTS idx_workflow_instances_task_ids
              ON workflow_instances USING GIN ((data->'data'->'task_ids'))",
    },
    Migration {
        version: 6,
        description: "add workflow command dispatch leases",
        sql: "ALTER TABLE workflow_commands
              ADD COLUMN IF NOT EXISTS dispatch_owner TEXT;
              ALTER TABLE workflow_commands
              ADD COLUMN IF NOT EXISTS dispatch_lease_expires_at TIMESTAMPTZ;
              CREATE INDEX IF NOT EXISTS idx_workflow_commands_dispatch_claim
              ON workflow_commands (status, dispatch_lease_expires_at, created_at)",
    },
    Migration {
        version: 7,
        description: "index runtime issue workflow PR lookups",
        sql: "CREATE INDEX IF NOT EXISTS idx_workflow_instances_project_pr
              ON workflow_instances (
                  definition_id,
                  (data->'data'->>'project_id'),
                  (data->'data'->>'pr_number'),
                  updated_at DESC
              )
              WHERE data->'data'->>'pr_number' IS NOT NULL;
              CREATE INDEX IF NOT EXISTS idx_workflow_instances_project_repo_pr
              ON workflow_instances (
                  definition_id,
                  (data->'data'->>'project_id'),
                  (data->'data'->>'repo'),
                  (data->'data'->>'pr_number'),
                  updated_at DESC
              )
              WHERE data->'data'->>'pr_number' IS NOT NULL",
    },
    Migration {
        version: 8,
        description: "index runtime job command pagination",
        sql: "CREATE INDEX IF NOT EXISTS idx_runtime_jobs_command_created
              ON runtime_jobs (command_id, created_at DESC)",
    },
    Migration {
        version: 9,
        description: "index workflow events by type",
        sql: "CREATE INDEX IF NOT EXISTS idx_workflow_events_workflow_type_sequence
              ON workflow_events (workflow_id, event_type, sequence DESC)",
    },
    Migration {
        version: 10,
        description: "persist workflow runtime prompt payloads",
        sql: "CREATE TABLE IF NOT EXISTS workflow_prompt_payloads (
            prompt_ref TEXT PRIMARY KEY,
            prompt TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 11,
        description: "constrain workflow runtime status vocabularies",
        sql: "DO $$
              BEGIN
                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_commands_status_check'
                    AND conrelid = 'workflow_commands'::regclass
                ) THEN
                  ALTER TABLE workflow_commands
                    ADD CONSTRAINT workflow_commands_status_check
                    CHECK (status IN (
                      'pending', 'dispatching', 'dispatched', 'handled_inline',
                      'completed', 'failed', 'blocked', 'cancelled', 'skipped'
                    ));
                END IF;
                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'runtime_jobs_status_check'
                    AND conrelid = 'runtime_jobs'::regclass
                ) THEN
                  ALTER TABLE runtime_jobs
                    ADD CONSTRAINT runtime_jobs_status_check
                    CHECK (status IN (
                      'pending', 'running', 'succeeded',
                      'failed', 'cancelled', 'expired'
                    ));
                END IF;
              END $$",
    },
    Migration {
        version: 12,
        description: "index explicit runtime submission handles",
        sql: "CREATE INDEX IF NOT EXISTS idx_workflow_instances_submission_id
              ON workflow_instances ((data->'data'->>'submission_id'))",
    },
    Migration {
        version: 13,
        description: "align runtime job status constraint with typed values",
        sql: "ALTER TABLE runtime_jobs
              DROP CONSTRAINT IF EXISTS runtime_jobs_status_check;
              UPDATE runtime_jobs
              SET status = 'failed',
                  data = jsonb_set(data, '{status}', '\"failed\"'::jsonb, false)
              WHERE status = 'expired'
                 OR data->>'status' = 'expired';
              ALTER TABLE runtime_jobs
                ADD CONSTRAINT runtime_jobs_status_check
                CHECK (status IN (
                  'pending', 'running', 'succeeded',
                  'failed', 'cancelled'
                ))",
    },
];
