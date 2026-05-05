use super::model::{
    ActivityResult, RuntimeEvent, RuntimeJob, RuntimeJobStatus, RuntimeKind, WorkflowCommand,
    WorkflowCommandRecord, WorkflowDecisionRecord, WorkflowDefinition, WorkflowEvent,
    WorkflowInstance,
};
use chrono::{DateTime, Utc};
use harness_core::db::{Migration, PgStoreContext};
use serde::Serialize;
use serde_json::Value;
use sqlx::postgres::PgPool;
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

static WORKFLOW_RUNTIME_MIGRATIONS: &[Migration] = &[
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
];

pub struct WorkflowRuntimeStore {
    pool: PgPool,
}

type WorkflowCommandRecordRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
);

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeJobEnqueueOutcome {
    Enqueued(RuntimeJob),
    AlreadyExists(RuntimeJob),
    CommandNotPending { status: String },
}

fn workflow_command_record_from_row(
    (
        id,
        workflow_id,
        decision_id,
        status,
        dispatch_owner,
        dispatch_lease_expires_at,
        data,
        created_at,
        updated_at,
    ): WorkflowCommandRecordRow,
) -> anyhow::Result<WorkflowCommandRecord> {
    Ok(WorkflowCommandRecord {
        id,
        workflow_id,
        decision_id,
        status,
        dispatch_owner,
        dispatch_lease_expires_at,
        command: serde_json::from_str(&data)?,
        created_at,
        updated_at,
    })
}

impl WorkflowRuntimeStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_path(path, configured_database_url)?;
        let pool = context
            .open_migrated_pool(WORKFLOW_RUNTIME_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, WORKFLOW_RUNTIME_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn upsert_definition(&self, definition: &WorkflowDefinition) -> anyhow::Result<()> {
        let data = to_jsonb_string(definition)?;
        sqlx::query(
            "INSERT INTO workflow_definitions (id, version, data, active)
             VALUES ($1, $2, $3::jsonb, $4)
             ON CONFLICT (id, version) DO UPDATE SET
                data = EXCLUDED.data,
                active = EXCLUDED.active,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&definition.id)
        .bind(definition.version as i64)
        .bind(&data)
        .bind(definition.active)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_definition(
        &self,
        id: &str,
        version: u32,
    ) -> anyhow::Result<Option<WorkflowDefinition>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_definitions
             WHERE id = $1 AND version = $2",
        )
        .bind(id)
        .bind(version as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn upsert_instance(&self, instance: &WorkflowInstance) -> anyhow::Result<()> {
        let data = to_jsonb_string(instance)?;
        sqlx::query(
            "INSERT INTO workflow_instances
                (id, definition_id, state, subject_type, subject_key, parent_workflow_id, data, version)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
             ON CONFLICT (id) DO UPDATE SET
                definition_id = EXCLUDED.definition_id,
                state = EXCLUDED.state,
                subject_type = EXCLUDED.subject_type,
                subject_key = EXCLUDED.subject_key,
                parent_workflow_id = EXCLUDED.parent_workflow_id,
                data = EXCLUDED.data,
                version = EXCLUDED.version,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&instance.id)
        .bind(&instance.definition_id)
        .bind(&instance.state)
        .bind(&instance.subject.subject_type)
        .bind(&instance.subject.subject_key)
        .bind(&instance.parent_workflow_id)
        .bind(&data)
        .bind(instance.version as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_instance(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1")
                .bind(workflow_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn get_instance_by_task_id(
        &self,
        task_id: &str,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE data->'data'->>'task_id' = $1
                OR data->'data'->'task_ids' ? $1
             ORDER BY updated_at DESC
             LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn list_instances_by_state(
        &self,
        definition_id: &str,
        state: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND state = $2
             ORDER BY updated_at ASC
             LIMIT $3",
        )
        .bind(definition_id)
        .bind(state)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn touch_instance(&self, workflow_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE workflow_instances SET updated_at = clock_timestamp() WHERE id = $1")
            .bind(workflow_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_instances(
        &self,
        project_id: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE ($1::text IS NULL OR data->'data'->>'project_id' = $1)
             ORDER BY updated_at DESC
             LIMIT $2",
        )
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn list_instances_by_definition(
        &self,
        definition_id: &str,
        project_id: Option<&str>,
        limit: Option<i64>,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.map(|value| value.clamp(1, 500));
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND ($2::text IS NULL OR data->'data'->>'project_id' = $2)
             ORDER BY updated_at DESC
             LIMIT COALESCE($3, 2147483647)",
        )
        .bind(definition_id)
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn append_event(
        &self,
        workflow_id: &str,
        event_type: &str,
        source: &str,
        payload: Value,
    ) -> anyhow::Result<WorkflowEvent> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("workflow_events:{workflow_id}"))
            .execute(&mut *tx)
            .await?;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_one(&mut *tx)
        .await?;
        let event = WorkflowEvent::new(workflow_id, next_sequence as u64, event_type, source)
            .with_payload(payload);
        let data = to_jsonb_string(&event)?;
        sqlx::query(
            "INSERT INTO workflow_events
                (id, workflow_id, sequence, event_type, source, data)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
        )
        .bind(&event.id)
        .bind(&event.workflow_id)
        .bind(event.sequence as i64)
        .bind(&event.event_type)
        .bind(&event.source)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn events_for(&self, workflow_id: &str) -> anyhow::Result<Vec<WorkflowEvent>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_events
             WHERE workflow_id = $1
             ORDER BY sequence ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn events_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowEvent>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT workflow_id, data::text FROM workflow_events
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, sequence ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for (workflow_id, data) in rows {
            by_workflow
                .entry(workflow_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_workflow)
    }

    pub async fn record_decision(&self, record: &WorkflowDecisionRecord) -> anyhow::Result<()> {
        let data = to_jsonb_string(record)?;
        sqlx::query(
            "INSERT INTO workflow_decisions
                (id, workflow_id, event_id, accepted, data, rejection_reason)
             VALUES ($1, $2, $3, $4, $5::jsonb, $6)
             ON CONFLICT (id) DO UPDATE SET
                accepted = EXCLUDED.accepted,
                data = EXCLUDED.data,
                rejection_reason = EXCLUDED.rejection_reason",
        )
        .bind(&record.id)
        .bind(&record.workflow_id)
        .bind(&record.event_id)
        .bind(record.accepted)
        .bind(&data)
        .bind(&record.rejection_reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn decisions_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowDecisionRecord>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_decisions
             WHERE workflow_id = $1
             ORDER BY created_at ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn decisions_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowDecisionRecord>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT workflow_id, data::text FROM workflow_decisions
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, created_at ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for (workflow_id, data) in rows {
            by_workflow
                .entry(workflow_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_workflow)
    }

    pub async fn enqueue_command(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
    ) -> anyhow::Result<String> {
        self.enqueue_command_with_status(workflow_id, decision_id, command, "pending")
            .await
    }

    pub async fn enqueue_command_with_status(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
        status: &str,
    ) -> anyhow::Result<String> {
        let data = to_jsonb_string(command)?;
        let command_type = enum_str(&command.command_type)?;
        let (id,): (String,) = sqlx::query_as(
            "INSERT INTO workflow_commands
                (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)
             ON CONFLICT (workflow_id, dedupe_key) DO UPDATE SET
                status = CASE
                    WHEN workflow_commands.status = 'pending' THEN EXCLUDED.status
                    ELSE workflow_commands.status
                END,
                updated_at = CASE
                    WHEN workflow_commands.status = 'pending'
                         AND workflow_commands.status <> EXCLUDED.status
                    THEN CURRENT_TIMESTAMP
                    ELSE workflow_commands.updated_at
                END
             RETURNING id",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(workflow_id)
        .bind(decision_id)
        .bind(&command_type)
        .bind(&command.dedupe_key)
        .bind(status)
        .bind(&data)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn commands_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
                 FROM workflow_commands
                 WHERE workflow_id = $1
                 ORDER BY created_at ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(workflow_command_record_from_row)
            .collect()
    }

    pub async fn commands_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowCommandRecord>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, created_at ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for row in rows {
            let record = workflow_command_record_from_row(row)?;
            by_workflow
                .entry(record.workflow_id.clone())
                .or_insert_with(Vec::new)
                .push(record);
        }
        Ok(by_workflow)
    }

    pub async fn get_command(
        &self,
        command_id: &str,
    ) -> anyhow::Result<Option<WorkflowCommandRecord>> {
        let row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE id = $1",
        )
        .bind(command_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(workflow_command_record_from_row).transpose()
    }

    pub async fn pending_commands(&self, limit: i64) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE status = 'pending'
             ORDER BY created_at ASC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(workflow_command_record_from_row)
            .collect()
    }

    pub async fn claim_pending_commands(
        &self,
        owner: &str,
        expires_at: DateTime<Utc>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "WITH candidates AS (
                 SELECT id
                 FROM workflow_commands
                 WHERE status = 'pending'
                    OR (
                        status = 'dispatching'
                        AND COALESCE(dispatch_lease_expires_at, '-infinity'::timestamptz)
                            <= CURRENT_TIMESTAMP
                    )
                 ORDER BY created_at ASC
                 LIMIT $3
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE workflow_commands AS command
             SET status = 'dispatching',
                 dispatch_owner = $1,
                 dispatch_lease_expires_at = $2,
                 updated_at = CURRENT_TIMESTAMP
             FROM candidates
             WHERE command.id = candidates.id
             RETURNING command.id, command.workflow_id, command.decision_id, command.status,
                       command.dispatch_owner, command.dispatch_lease_expires_at,
                       command.data::text, command.created_at, command.updated_at",
        )
        .bind(owner)
        .bind(expires_at)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut records: Vec<_> = rows
            .into_iter()
            .map(workflow_command_record_from_row)
            .collect::<anyhow::Result<_>>()?;
        records.sort_by_key(|record| record.created_at);
        Ok(records)
    }

    pub async fn mark_command_status(&self, command_id: &str, status: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(status)
        .bind(command_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_pending_command_status(
        &self,
        command_id: &str,
        status: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2 AND status = 'pending'",
        )
        .bind(status)
        .bind(command_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn enqueue_runtime_job(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
    ) -> anyhow::Result<RuntimeJob> {
        self.enqueue_runtime_job_with_not_before(
            command_id,
            runtime_kind,
            runtime_profile,
            input,
            None,
        )
        .await
    }

    pub async fn enqueue_runtime_job_with_not_before(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJob> {
        let mut job = RuntimeJob::pending(command_id, runtime_kind, runtime_profile, input);
        job.not_before = not_before;
        let data = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        let runtime_kind = enum_str(&job.runtime_kind)?;
        sqlx::query(
            "INSERT INTO runtime_jobs
                (id, command_id, runtime_kind, runtime_profile, status, not_before, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
        )
        .bind(&job.id)
        .bind(&job.command_id)
        .bind(&runtime_kind)
        .bind(&job.runtime_profile)
        .bind(&status)
        .bind(job.not_before)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(job)
    }

    pub async fn enqueue_runtime_job_for_pending_command(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        self.enqueue_runtime_job_for_command(
            command_id,
            None,
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
    }

    pub async fn enqueue_runtime_job_for_claimed_command(
        &self,
        command_id: &str,
        dispatch_owner: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        self.enqueue_runtime_job_for_command(
            command_id,
            Some(dispatch_owner),
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
    }

    async fn enqueue_runtime_job_for_command(
        &self,
        command_id: &str,
        dispatch_owner: Option<&str>,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        let mut job = RuntimeJob::pending(command_id, runtime_kind, runtime_profile, input);
        job.not_before = not_before;
        let data = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        let runtime_kind = enum_str(&job.runtime_kind)?;
        let mut tx = self.pool.begin().await?;
        let command_row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT status, dispatch_owner FROM workflow_commands WHERE id = $1 FOR UPDATE",
        )
        .bind(command_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((command_status, command_dispatch_owner)) = command_row else {
            anyhow::bail!("workflow command not found: {command_id}");
        };

        let eligible = match dispatch_owner {
            Some(owner) => {
                command_status == "dispatching" && command_dispatch_owner.as_deref() == Some(owner)
            }
            None => command_status == "pending",
        };

        if !eligible {
            let existing = runtime_job_for_command_tx(&mut tx, command_id).await?;
            tx.commit().await?;
            return Ok(match existing {
                Some(runtime_job) => RuntimeJobEnqueueOutcome::AlreadyExists(runtime_job),
                None => RuntimeJobEnqueueOutcome::CommandNotPending {
                    status: command_status,
                },
            });
        }

        if let Some(existing) = runtime_job_for_command_tx(&mut tx, command_id).await? {
            sqlx::query(
                "UPDATE workflow_commands
                 SET status = 'dispatched',
                     dispatch_owner = NULL,
                     dispatch_lease_expires_at = NULL,
                     updated_at = CURRENT_TIMESTAMP
                 WHERE id = $1",
            )
            .bind(command_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(RuntimeJobEnqueueOutcome::AlreadyExists(existing));
        }

        sqlx::query(
            "INSERT INTO runtime_jobs
                (id, command_id, runtime_kind, runtime_profile, status, not_before, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
        )
        .bind(&job.id)
        .bind(&job.command_id)
        .bind(&runtime_kind)
        .bind(&job.runtime_profile)
        .bind(&status)
        .bind(job.not_before)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE workflow_commands
             SET status = 'dispatched',
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1",
        )
        .bind(command_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RuntimeJobEnqueueOutcome::Enqueued(job))
    }

    pub async fn claim_next_runtime_job(
        &self,
        owner: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE (
                 status = 'pending'
                 AND (not_before IS NULL OR not_before <= CURRENT_TIMESTAMP)
             ) OR (
                 status = 'running'
                 AND data ? 'lease'
                 AND (data->'lease' ? 'expires_at')
                 AND (data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
             )
             ORDER BY created_at ASC
             LIMIT 1
             FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some((id, data)) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        job.claim(owner, expires_at);
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn record_runtime_event(
        &self,
        runtime_job_id: &str,
        event_type: &str,
        payload: Value,
    ) -> anyhow::Result<RuntimeEvent> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("runtime_events:{runtime_job_id}"))
            .execute(&mut *tx)
            .await?;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM runtime_events WHERE runtime_job_id = $1",
        )
        .bind(runtime_job_id)
        .fetch_one(&mut *tx)
        .await?;
        let event = RuntimeEvent::new(runtime_job_id, next_sequence as u64, event_type, payload);
        let data = to_jsonb_string(&event)?;
        sqlx::query(
            "INSERT INTO runtime_events
                (id, runtime_job_id, sequence, event_type, data)
             VALUES ($1, $2, $3, $4, $5::jsonb)",
        )
        .bind(&event.id)
        .bind(&event.runtime_job_id)
        .bind(event.sequence as i64)
        .bind(&event.event_type)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn runtime_events_for(
        &self,
        runtime_job_id: &str,
    ) -> anyhow::Result<Vec<RuntimeEvent>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM runtime_events
             WHERE runtime_job_id = $1
             ORDER BY sequence ASC",
        )
        .bind(runtime_job_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn runtime_events_for_jobs(
        &self,
        runtime_job_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeEvent>>> {
        if runtime_job_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT runtime_job_id, data::text FROM runtime_events
             WHERE runtime_job_id = ANY($1::text[])
             ORDER BY runtime_job_id ASC, sequence ASC",
        )
        .bind(runtime_job_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_job = BTreeMap::new();
        for (runtime_job_id, data) in rows {
            by_job
                .entry(runtime_job_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_job)
    }

    pub async fn complete_runtime_job(
        &self,
        runtime_job_id: &str,
        result: &ActivityResult,
    ) -> anyhow::Result<RuntimeJob> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((data,)) = row else {
            anyhow::bail!("runtime job not found: {runtime_job_id}");
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        job.complete(result)?;
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(runtime_job_id)
        .execute(&self.pool)
        .await?;
        Ok(job)
    }

    pub async fn get_runtime_job(
        &self,
        runtime_job_id: &str,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn runtime_jobs_for_command(
        &self,
        command_id: &str,
    ) -> anyhow::Result<Vec<RuntimeJob>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM runtime_jobs
             WHERE command_id = $1
             ORDER BY created_at ASC",
        )
        .bind(command_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn cancel_command_and_unfinished_runtime_jobs(
        &self,
        command_id: &str,
        activity: &str,
        summary: &str,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let _command_row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM workflow_commands WHERE id = $1 FOR UPDATE")
                .bind(command_id)
                .fetch_optional(&mut *tx)
                .await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE command_id = $1
               AND status IN ('pending', 'running')
             FOR UPDATE",
        )
        .bind(command_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut cancelled = 0usize;
        for (id, data) in rows {
            let mut job: RuntimeJob = serde_json::from_str(&data)?;
            job.complete(&ActivityResult::cancelled(activity, summary))?;
            let updated = to_jsonb_string(&job)?;
            let status = enum_str(&job.status)?;
            sqlx::query(
                "UPDATE runtime_jobs
                 SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
                 WHERE id = $4",
            )
            .bind(&status)
            .bind(job.not_before)
            .bind(&updated)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            cancelled += 1;
        }
        sqlx::query(
            "UPDATE workflow_commands
             SET status = 'cancelled', updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(command_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(cancelled)
    }

    pub async fn runtime_jobs_for_commands(
        &self,
        command_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeJob>>> {
        if command_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT command_id, data::text FROM runtime_jobs
             WHERE command_id = ANY($1::text[])
             ORDER BY command_id ASC, created_at ASC",
        )
        .bind(command_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_command = BTreeMap::new();
        for (command_id, data) in rows {
            by_command
                .entry(command_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_command)
    }

    pub async fn runtime_turns_started_for_workflow(
        &self,
        workflow_id: &str,
        exclude_runtime_job_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM runtime_events e
             JOIN runtime_jobs r ON r.id = e.runtime_job_id
             JOIN workflow_commands c ON c.id = r.command_id
             WHERE c.workflow_id = $1
               AND e.event_type = 'RuntimeTurnStarted'
               AND ($2::text IS NULL OR r.id <> $2)",
        )
        .bind(workflow_id)
        .bind(exclude_runtime_job_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    pub async fn runtime_job_count_by_status(
        &self,
        status: RuntimeJobStatus,
    ) -> anyhow::Result<i64> {
        let status = enum_str(&status)?;
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM runtime_jobs WHERE status = $1")
                .bind(&status)
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }
}

fn to_jsonb_string(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?.replace("\\u0000", ""))
}

fn enum_str(value: &impl Serialize) -> anyhow::Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("serialized enum did not produce a string"))
}

async fn runtime_job_for_command_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
) -> anyhow::Result<Option<RuntimeJob>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT data::text FROM runtime_jobs
         WHERE command_id = $1
         ORDER BY created_at ASC
         LIMIT 1",
    )
    .bind(command_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}
