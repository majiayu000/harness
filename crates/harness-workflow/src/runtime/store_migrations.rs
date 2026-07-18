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
    Migration {
        version: 14,
        description: "guard workflow runtime graph references",
        sql: "DO $$
              BEGIN
                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_events_workflow_id_fkey'
                    AND conrelid = 'workflow_events'::regclass
                ) THEN
                  ALTER TABLE workflow_events
                    ADD CONSTRAINT workflow_events_workflow_id_fkey
                    FOREIGN KEY (workflow_id)
                    REFERENCES workflow_instances(id)
                    ON DELETE CASCADE
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_decisions_workflow_id_fkey'
                    AND conrelid = 'workflow_decisions'::regclass
                ) THEN
                  ALTER TABLE workflow_decisions
                    ADD CONSTRAINT workflow_decisions_workflow_id_fkey
                    FOREIGN KEY (workflow_id)
                    REFERENCES workflow_instances(id)
                    ON DELETE CASCADE
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_decisions_event_id_fkey'
                    AND conrelid = 'workflow_decisions'::regclass
                ) THEN
                  ALTER TABLE workflow_decisions
                    ADD CONSTRAINT workflow_decisions_event_id_fkey
                    FOREIGN KEY (event_id)
                    REFERENCES workflow_events(id)
                    ON DELETE SET NULL
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_commands_workflow_id_fkey'
                    AND conrelid = 'workflow_commands'::regclass
                ) THEN
                  ALTER TABLE workflow_commands
                    ADD CONSTRAINT workflow_commands_workflow_id_fkey
                    FOREIGN KEY (workflow_id)
                    REFERENCES workflow_instances(id)
                    ON DELETE CASCADE
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_commands_decision_id_fkey'
                    AND conrelid = 'workflow_commands'::regclass
                ) THEN
                  ALTER TABLE workflow_commands
                    ADD CONSTRAINT workflow_commands_decision_id_fkey
                    FOREIGN KEY (decision_id)
                    REFERENCES workflow_decisions(id)
                    ON DELETE SET NULL
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'runtime_jobs_command_id_fkey'
                    AND conrelid = 'runtime_jobs'::regclass
                ) THEN
                  ALTER TABLE runtime_jobs
                    ADD CONSTRAINT runtime_jobs_command_id_fkey
                    FOREIGN KEY (command_id)
                    REFERENCES workflow_commands(id)
                    ON DELETE CASCADE
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'runtime_events_runtime_job_id_fkey'
                    AND conrelid = 'runtime_events'::regclass
                ) THEN
                  ALTER TABLE runtime_events
                    ADD CONSTRAINT runtime_events_runtime_job_id_fkey
                    FOREIGN KEY (runtime_job_id)
                    REFERENCES runtime_jobs(id)
                    ON DELETE CASCADE
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_artifacts_workflow_id_fkey'
                    AND conrelid = 'workflow_artifacts'::regclass
                ) THEN
                  ALTER TABLE workflow_artifacts
                    ADD CONSTRAINT workflow_artifacts_workflow_id_fkey
                    FOREIGN KEY (workflow_id)
                    REFERENCES workflow_instances(id)
                    ON DELETE CASCADE
                    NOT VALID;
                END IF;

                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'workflow_artifacts_runtime_job_id_fkey'
                    AND conrelid = 'workflow_artifacts'::regclass
                ) THEN
                  ALTER TABLE workflow_artifacts
                    ADD CONSTRAINT workflow_artifacts_runtime_job_id_fkey
                    FOREIGN KEY (runtime_job_id)
                    REFERENCES runtime_jobs(id)
                    ON DELETE SET NULL
                    NOT VALID;
                END IF;
              END $$",
    },
    Migration {
        version: 15,
        description: "create remote fact snapshots",
        sql: "CREATE TABLE IF NOT EXISTS remote_fact_snapshots (
            id              UUID PRIMARY KEY,
            provider        TEXT NOT NULL,
            repo            TEXT NOT NULL,
            subject_type    TEXT NOT NULL,
            subject_number  BIGINT NOT NULL,
            subject_url     TEXT,
            head_sha        TEXT,
            state           TEXT NOT NULL,
            fact_hash       TEXT NOT NULL,
            facts           JSONB NOT NULL,
            fetched_at      TIMESTAMPTZ NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE (provider, repo, subject_type, subject_number)
        );
        CREATE INDEX IF NOT EXISTS idx_remote_fact_snapshots_repo_subject
          ON remote_fact_snapshots (repo, subject_type, subject_number);
        CREATE INDEX IF NOT EXISTS idx_remote_fact_snapshots_fact_hash
          ON remote_fact_snapshots (fact_hash)",
    },
    Migration {
        version: 16,
        description: "index aged workflow wait-state scans",
        sql: "CREATE INDEX IF NOT EXISTS idx_workflow_instances_state_updated_at
              ON workflow_instances (state, updated_at)",
    },
    Migration {
        version: 17,
        description: "create workflow repo memory records",
        sql: "CREATE TABLE IF NOT EXISTS workflow_repo_memory (
            id              UUID PRIMARY KEY,
            repo            TEXT NOT NULL,
            activity_class  TEXT NOT NULL,
            outcome         TEXT NOT NULL,
            kind            TEXT NOT NULL,
            payload_json    JSONB NOT NULL,
            evidence_ref    TEXT,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            last_used_at    TIMESTAMPTZ,
            use_count       BIGINT NOT NULL DEFAULT 0,
            CONSTRAINT workflow_repo_memory_repo_not_blank
                CHECK (btrim(repo) <> ''),
            CONSTRAINT workflow_repo_memory_activity_class_not_blank
                CHECK (btrim(activity_class) <> ''),
            CONSTRAINT workflow_repo_memory_outcome_check
                CHECK (outcome IN ('done', 'failed')),
            CONSTRAINT workflow_repo_memory_kind_check
                CHECK (kind IN (
                    'validation_command',
                    'failure_lesson',
                    'reviewer_pattern',
                    'environment_note'
                )),
            CONSTRAINT workflow_repo_memory_use_count_nonnegative
                CHECK (use_count >= 0)
        );
        CREATE INDEX IF NOT EXISTS idx_workflow_repo_memory_repo_created
          ON workflow_repo_memory (repo, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_workflow_repo_memory_repo_activity_created
          ON workflow_repo_memory (repo, activity_class, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_workflow_repo_memory_repo_outcome_kind
          ON workflow_repo_memory (repo, outcome, kind)",
    },
    Migration {
        version: 18,
        description: "create workflow runtime usage records",
        sql: "CREATE TABLE IF NOT EXISTS runtime_usage_events (
            id                          TEXT PRIMARY KEY,
            runtime_job_id              TEXT NOT NULL,
            usage_key                   TEXT NOT NULL,
            command_id                  TEXT NOT NULL,
            workflow_id                 TEXT NOT NULL,
            turn_id                     TEXT,
            runtime_kind                TEXT NOT NULL,
            runtime_profile             TEXT NOT NULL,
            agent                       TEXT NOT NULL,
            model                       TEXT NOT NULL,
            project                     TEXT NOT NULL,
            task_id                     TEXT,
            candidate_group_id          TEXT,
            candidate_id                TEXT,
            candidate_index             BIGINT,
            candidate_count             BIGINT,
            input_tokens                BIGINT NOT NULL,
            output_tokens               BIGINT NOT NULL,
            cache_read_input_tokens     BIGINT NOT NULL,
            cache_creation_input_tokens BIGINT NOT NULL,
            reported_total_tokens       BIGINT,
            reported_at                 TIMESTAMPTZ NOT NULL,
            created_at                  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at                  TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE (runtime_job_id, usage_key),
            CONSTRAINT runtime_usage_events_tokens_nonnegative
                CHECK (
                    input_tokens >= 0
                    AND output_tokens >= 0
                    AND cache_read_input_tokens >= 0
                    AND cache_creation_input_tokens >= 0
                    AND (reported_total_tokens IS NULL OR reported_total_tokens >= 0)
                )
        );
        CREATE INDEX IF NOT EXISTS idx_runtime_usage_events_reported_at
          ON runtime_usage_events (reported_at DESC);
        CREATE INDEX IF NOT EXISTS idx_runtime_usage_events_runtime_job
          ON runtime_usage_events (runtime_job_id);
        CREATE INDEX IF NOT EXISTS idx_runtime_usage_events_command
          ON runtime_usage_events (command_id);
        CREATE INDEX IF NOT EXISTS idx_runtime_usage_events_workflow
          ON runtime_usage_events (workflow_id)",
    },
    Migration {
        version: 19,
        description: "persist deferred workflow command dispatch evidence",
        sql: "ALTER TABLE workflow_commands
              ADD COLUMN IF NOT EXISTS dispatch_not_before TIMESTAMPTZ;
              ALTER TABLE workflow_commands
              ADD COLUMN IF NOT EXISTS dispatch_attempt_count BIGINT NOT NULL DEFAULT 0;
              ALTER TABLE workflow_commands
              ADD COLUMN IF NOT EXISTS dispatch_claim_generation BIGINT NOT NULL DEFAULT 0;
              ALTER TABLE workflow_commands
              ADD COLUMN IF NOT EXISTS dispatch_barrier JSONB;
              ALTER TABLE workflow_commands
              DROP CONSTRAINT IF EXISTS workflow_commands_status_check;
              ALTER TABLE workflow_commands
              ADD CONSTRAINT workflow_commands_status_check
              CHECK (status IN (
                'pending', 'dispatching', 'deferred', 'dispatched', 'handled_inline',
                'completed', 'failed', 'blocked', 'cancelled', 'skipped'
              ));
              ALTER TABLE workflow_commands
              DROP CONSTRAINT IF EXISTS workflow_commands_dispatch_claim_generation_check;
              ALTER TABLE workflow_commands
              ADD CONSTRAINT workflow_commands_dispatch_claim_generation_check
              CHECK (dispatch_claim_generation >= 0);
              DROP INDEX IF EXISTS idx_workflow_commands_dispatch_claim;
              CREATE INDEX idx_workflow_commands_dispatch_claim
              ON workflow_commands (
                status, dispatch_not_before, dispatch_lease_expires_at, created_at
              )
              WHERE status IN ('pending', 'dispatching', 'deferred')",
    },
    Migration {
        version: 20,
        description: "create runtime job lease renewal receipts",
        sql: "CREATE TABLE IF NOT EXISTS runtime_job_lease_renewal_receipts (
            runtime_job_id       TEXT NOT NULL,
            renewal_id           UUID NOT NULL,
            owner                TEXT NOT NULL,
            lease_generation     BIGINT NOT NULL,
            previous_expires_at  TIMESTAMPTZ NOT NULL,
            renewed_expires_at   TIMESTAMPTZ NOT NULL,
            lease_secs           BIGINT NOT NULL,
            created_at           TIMESTAMPTZ NOT NULL,
            PRIMARY KEY (runtime_job_id, lease_generation, renewal_id),
            CONSTRAINT runtime_job_lease_renewal_receipts_job_fkey
                FOREIGN KEY (runtime_job_id)
                REFERENCES runtime_jobs(id)
                ON DELETE CASCADE,
            CONSTRAINT runtime_job_lease_renewal_receipts_generation_nonnegative
                CHECK (lease_generation >= 0),
            CONSTRAINT runtime_job_lease_renewal_receipts_lease_secs_positive
                CHECK (lease_secs > 0)
        );
        CREATE INDEX IF NOT EXISTS idx_runtime_job_lease_receipts_expiry
          ON runtime_job_lease_renewal_receipts (runtime_job_id, renewed_expires_at)",
    },
    Migration {
        version: 21,
        description: "persist workflow runtime usage costs",
        sql: "ALTER TABLE runtime_usage_events
              ADD COLUMN IF NOT EXISTS cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0;
              DO $$
              BEGIN
                IF NOT EXISTS (
                  SELECT 1 FROM pg_constraint
                  WHERE conname = 'runtime_usage_events_cost_nonnegative'
                    AND conrelid = 'runtime_usage_events'::regclass
                ) THEN
                  ALTER TABLE runtime_usage_events
                    ADD CONSTRAINT runtime_usage_events_cost_nonnegative
                    CHECK (cost_usd >= 0);
                END IF;
              END $$",
    },
];
