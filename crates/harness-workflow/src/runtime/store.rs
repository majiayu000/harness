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
];

pub struct WorkflowRuntimeStore {
    pool: PgPool,
    sequence_lock: tokio::sync::Mutex<()>,
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
        Ok(Self {
            pool,
            sequence_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, WORKFLOW_RUNTIME_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            sequence_lock: tokio::sync::Mutex::new(()),
        })
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

    pub async fn append_event(
        &self,
        workflow_id: &str,
        event_type: &str,
        source: &str,
        payload: Value,
    ) -> anyhow::Result<WorkflowEvent> {
        let _guard = self.sequence_lock.lock().await;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_one(&self.pool)
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
        .execute(&self.pool)
        .await?;
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

    pub async fn enqueue_command(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
    ) -> anyhow::Result<String> {
        let data = to_jsonb_string(command)?;
        let command_type = enum_str(&command.command_type)?;
        let row: Option<(String,)> = sqlx::query_as(
            "INSERT INTO workflow_commands
                (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
             VALUES ($1, $2, $3, $4, $5, 'pending', $6::jsonb)
             ON CONFLICT (workflow_id, dedupe_key) DO NOTHING
             RETURNING id",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(workflow_id)
        .bind(decision_id)
        .bind(&command_type)
        .bind(&command.dedupe_key)
        .bind(&data)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((id,)) = row {
            return Ok(id);
        }

        let (id,): (String,) = sqlx::query_as(
            "SELECT id FROM workflow_commands
             WHERE workflow_id = $1 AND dedupe_key = $2",
        )
        .bind(workflow_id)
        .bind(&command.dedupe_key)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn commands_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let rows: Vec<(
            String,
            String,
            Option<String>,
            String,
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, data::text, created_at, updated_at
                 FROM workflow_commands
                 WHERE workflow_id = $1
                 ORDER BY created_at ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(id, workflow_id, decision_id, status, data, created_at, updated_at)| {
                    Ok(WorkflowCommandRecord {
                        id,
                        workflow_id,
                        decision_id,
                        status,
                        command: serde_json::from_str(&data)?,
                        created_at,
                        updated_at,
                    })
                },
            )
            .collect()
    }

    pub async fn get_command(
        &self,
        command_id: &str,
    ) -> anyhow::Result<Option<WorkflowCommandRecord>> {
        let row: Option<(
            String,
            String,
            Option<String>,
            String,
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE id = $1",
        )
        .bind(command_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(
            |(id, workflow_id, decision_id, status, data, created_at, updated_at)| {
                Ok(WorkflowCommandRecord {
                    id,
                    workflow_id,
                    decision_id,
                    status,
                    command: serde_json::from_str(&data)?,
                    created_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    pub async fn pending_commands(&self, limit: i64) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<(
            String,
            String,
            Option<String>,
            String,
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE status = 'pending'
             ORDER BY created_at ASC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(id, workflow_id, decision_id, status, data, created_at, updated_at)| {
                    Ok(WorkflowCommandRecord {
                        id,
                        workflow_id,
                        decision_id,
                        status,
                        command: serde_json::from_str(&data)?,
                        created_at,
                        updated_at,
                    })
                },
            )
            .collect()
    }

    pub async fn mark_command_status(&self, command_id: &str, status: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $1, updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(status)
        .bind(command_id)
        .execute(&self.pool)
        .await?;
        Ok(())
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
                (id, command_id, runtime_kind, runtime_profile, status, data)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
        )
        .bind(&job.id)
        .bind(&job.command_id)
        .bind(&runtime_kind)
        .bind(&job.runtime_profile)
        .bind(&status)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(job)
    }

    pub async fn claim_next_runtime_job(
        &self,
        owner: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE status = 'pending'
               AND CASE
                   WHEN data ? 'not_before'
                   THEN (data->>'not_before')::timestamptz <= CURRENT_TIMESTAMP
                   ELSE TRUE
               END
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
             SET status = $1, data = $2::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $3",
        )
        .bind(&status)
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
        let _guard = self.sequence_lock.lock().await;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM runtime_events WHERE runtime_job_id = $1",
        )
        .bind(runtime_job_id)
        .fetch_one(&self.pool)
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
        .execute(&self.pool)
        .await?;
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
             SET status = $1, data = $2::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $3",
        )
        .bind(&status)
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

    pub async fn runtime_turns_started_for_workflow(
        &self,
        workflow_id: &str,
        exclude_runtime_job_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM runtime_jobs r
             JOIN workflow_commands c ON c.id = r.command_id
             WHERE c.workflow_id = $1
               AND r.status <> 'pending'
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
