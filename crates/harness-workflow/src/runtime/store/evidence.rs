use super::*;
use uuid::Uuid;

pub const WORKFLOW_RUN_EVIDENCE_SCHEMA: &str = "harness.workflow.run_evidence.v1";
pub const WORKFLOW_RUN_EVIDENCE_EXPORT_SCHEMA: &str = "harness.workflow.run_evidence_export.v1";
pub const WORKFLOW_RUN_EVIDENCE_DEFAULT_LIMIT: i64 = 100;
pub const WORKFLOW_RUN_EVIDENCE_MAX_LIMIT: i64 = 500;
pub const WORKFLOW_RUN_EVIDENCE_PAYLOAD_MAX_BYTES: usize = 64 * 1024;
pub const WORKFLOW_RUN_EVIDENCE_RETENTION_MAX_BATCH: i64 = 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRunEvidence {
    pub id: String,
    pub workflow_id: String,
    pub command_id: Option<String>,
    pub runtime_job_id: Option<String>,
    pub project_id: String,
    pub commit_sha: Option<String>,
    pub stack: String,
    pub suite: String,
    pub baseline: Option<String>,
    pub decision: String,
    pub evidence_schema: String,
    pub digest: String,
    pub trust: String,
    pub location: Value,
    pub retention_class: String,
    pub payload: Option<Value>,
    pub payload_expires_at: Option<DateTime<Utc>>,
    pub payload_expired_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRunEvidenceInput {
    pub id: Option<String>,
    pub workflow_id: String,
    pub command_id: Option<String>,
    pub runtime_job_id: Option<String>,
    pub project_id: String,
    pub commit_sha: Option<String>,
    pub stack: String,
    pub suite: String,
    pub baseline: Option<String>,
    pub decision: String,
    pub evidence_schema: String,
    pub digest: String,
    pub trust: String,
    pub location: Value,
    pub retention_class: String,
    pub payload: Option<Value>,
    pub payload_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRunEvidenceQuery {
    pub project_id: Option<String>,
    pub commit_sha: Option<String>,
    pub suite: Option<String>,
    pub decision: Option<String>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub include_payload: bool,
    pub limit: i64,
}

impl Default for WorkflowRunEvidenceQuery {
    fn default() -> Self {
        Self {
            project_id: None,
            commit_sha: None,
            suite: None,
            decision: None,
            created_after: None,
            created_before: None,
            include_payload: false,
            limit: WORKFLOW_RUN_EVIDENCE_DEFAULT_LIMIT,
        }
    }
}

impl WorkflowRunEvidenceQuery {
    fn bounded_limit(&self) -> i64 {
        if self.limit <= 0 {
            WORKFLOW_RUN_EVIDENCE_DEFAULT_LIMIT
        } else {
            self.limit.min(WORKFLOW_RUN_EVIDENCE_MAX_LIMIT)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRunEvidenceExport {
    pub schema: String,
    pub generated_at: DateTime<Utc>,
    pub limit: i64,
    pub records: Vec<WorkflowRunEvidence>,
}

#[derive(sqlx::FromRow)]
struct WorkflowRunEvidenceRow {
    id: String,
    workflow_id: String,
    command_id: Option<String>,
    runtime_job_id: Option<String>,
    project_id: String,
    commit_sha: Option<String>,
    stack: String,
    suite: String,
    baseline: Option<String>,
    decision: String,
    evidence_schema: String,
    digest: String,
    trust: String,
    location_json: String,
    retention_class: String,
    payload_json: Option<String>,
    payload_expires_at: Option<DateTime<Utc>>,
    payload_expired_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl WorkflowRuntimeStore {
    pub async fn record_workflow_run_evidence(
        &self,
        input: WorkflowRunEvidenceInput,
    ) -> anyhow::Result<WorkflowRunEvidence> {
        validate_run_evidence_input(&input)?;
        let id = input
            .id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut tx = self.pool.begin().await?;
        let command_id = resolve_run_evidence_command_link_tx(
            &mut tx,
            &input.workflow_id,
            input.command_id.as_deref(),
            input.runtime_job_id.as_deref(),
        )
        .await?;
        let location = serde_json::to_string(&input.location)?;
        let payload = input
            .payload
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let inserted = sqlx::query(
            "INSERT INTO workflow_run_evidence
                (id, workflow_id, command_id, runtime_job_id, project_id, commit_sha,
                 stack, suite, baseline, decision, evidence_schema, digest, trust,
                 location, retention_class, payload, payload_expires_at)
             VALUES
                ($1, $2, $3, $4, $5, $6,
                 $7, $8, $9, $10, $11, $12, $13,
                 $14::jsonb, $15, $16::jsonb, $17)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id)
        .bind(&input.workflow_id)
        .bind(&command_id)
        .bind(&input.runtime_job_id)
        .bind(&input.project_id)
        .bind(&input.commit_sha)
        .bind(&input.stack)
        .bind(&input.suite)
        .bind(&input.baseline)
        .bind(&input.decision)
        .bind(&input.evidence_schema)
        .bind(&input.digest)
        .bind(&input.trust)
        .bind(&location)
        .bind(&input.retention_class)
        .bind(&payload)
        .bind(input.payload_expires_at)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        let record = select_workflow_run_evidence_by_id_tx(&mut tx, &id, true)
            .await?
            .ok_or_else(|| anyhow::anyhow!("workflow run evidence was not persisted"))?;
        if !inserted {
            ensure_idempotent_run_evidence_replay(&record, &input, command_id.as_deref())?;
        }
        tx.commit().await?;
        Ok(record)
    }

    pub async fn query_workflow_run_evidence(
        &self,
        query: WorkflowRunEvidenceQuery,
    ) -> anyhow::Result<Vec<WorkflowRunEvidence>> {
        let limit = query.bounded_limit();
        let rows: Vec<WorkflowRunEvidenceRow> = sqlx::query_as(
            "SELECT id, workflow_id, command_id, runtime_job_id, project_id, commit_sha,
                    stack, suite, baseline, decision, evidence_schema, digest, trust,
                    location::text AS location_json, retention_class,
                    CASE WHEN $7::boolean THEN payload::text ELSE NULL END AS payload_json,
                    payload_expires_at, payload_expired_at, created_at, updated_at
             FROM workflow_run_evidence
             WHERE ($1::text IS NULL OR project_id = $1)
               AND ($2::text IS NULL OR commit_sha = $2)
               AND ($3::text IS NULL OR suite = $3)
               AND ($4::text IS NULL OR decision = $4)
               AND ($5::timestamptz IS NULL OR created_at >= $5)
               AND ($6::timestamptz IS NULL OR created_at <= $6)
             ORDER BY created_at DESC, id DESC
             LIMIT $8",
        )
        .bind(blank_to_none(query.project_id.as_deref()))
        .bind(blank_to_none(query.commit_sha.as_deref()))
        .bind(blank_to_none(query.suite.as_deref()))
        .bind(blank_to_none(query.decision.as_deref()))
        .bind(query.created_after)
        .bind(query.created_before)
        .bind(query.include_payload)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(WorkflowRunEvidence::try_from)
            .collect()
    }

    pub async fn export_workflow_run_evidence(
        &self,
        query: WorkflowRunEvidenceQuery,
    ) -> anyhow::Result<WorkflowRunEvidenceExport> {
        let limit = query.bounded_limit();
        let records = self.query_workflow_run_evidence(query).await?;
        Ok(WorkflowRunEvidenceExport {
            schema: WORKFLOW_RUN_EVIDENCE_EXPORT_SCHEMA.to_string(),
            generated_at: Utc::now(),
            limit,
            records,
        })
    }

    pub async fn expire_workflow_run_evidence_payloads(
        &self,
        as_of: DateTime<Utc>,
        limit: i64,
    ) -> anyhow::Result<usize> {
        let limit = if limit <= 0 {
            WORKFLOW_RUN_EVIDENCE_RETENTION_MAX_BATCH
        } else {
            limit.min(WORKFLOW_RUN_EVIDENCE_RETENTION_MAX_BATCH)
        };
        let result = sqlx::query(
            "UPDATE workflow_run_evidence
             SET payload = NULL,
                 payload_expired_at = COALESCE(payload_expired_at, $1),
                 updated_at = CURRENT_TIMESTAMP
             WHERE id IN (
                 SELECT id
                 FROM workflow_run_evidence
                 WHERE payload IS NOT NULL
                   AND payload_expires_at IS NOT NULL
                   AND payload_expires_at <= $1
                 ORDER BY payload_expires_at ASC, id ASC
                 LIMIT $2
                 FOR UPDATE SKIP LOCKED
             )",
        )
        .bind(as_of)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() as usize)
    }
}

impl TryFrom<WorkflowRunEvidenceRow> for WorkflowRunEvidence {
    type Error = anyhow::Error;

    fn try_from(row: WorkflowRunEvidenceRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            workflow_id: row.workflow_id,
            command_id: row.command_id,
            runtime_job_id: row.runtime_job_id,
            project_id: row.project_id,
            commit_sha: row.commit_sha,
            stack: row.stack,
            suite: row.suite,
            baseline: row.baseline,
            decision: row.decision,
            evidence_schema: row.evidence_schema,
            digest: row.digest,
            trust: row.trust,
            location: serde_json::from_str(&row.location_json)?,
            retention_class: row.retention_class,
            payload: row
                .payload_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            payload_expires_at: row.payload_expires_at,
            payload_expired_at: row.payload_expired_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

async fn resolve_run_evidence_command_link_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    command_id: Option<&str>,
    runtime_job_id: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let mut resolved_command_id = command_id.map(str::to_string);
    if let Some(runtime_job_id) = runtime_job_id {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT command.workflow_id, job.command_id
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE job.id = $1",
        )
        .bind(runtime_job_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((job_workflow_id, job_command_id)) = row else {
            anyhow::bail!("runtime job `{runtime_job_id}` was not found");
        };
        anyhow::ensure!(
            job_workflow_id == workflow_id,
            "runtime job `{runtime_job_id}` belongs to workflow `{job_workflow_id}`, not `{workflow_id}`"
        );
        if let Some(command_id) = &resolved_command_id {
            anyhow::ensure!(
                command_id == &job_command_id,
                "runtime job `{runtime_job_id}` belongs to command `{job_command_id}`, not `{command_id}`"
            );
        } else {
            resolved_command_id = Some(job_command_id);
        }
    }
    if let Some(command_id) = &resolved_command_id {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT workflow_id FROM workflow_commands WHERE id = $1")
                .bind(command_id)
                .fetch_optional(&mut **tx)
                .await?;
        let Some((command_workflow_id,)) = row else {
            anyhow::bail!("workflow command `{command_id}` was not found");
        };
        anyhow::ensure!(
            command_workflow_id == workflow_id,
            "workflow command `{command_id}` belongs to workflow `{command_workflow_id}`, not `{workflow_id}`"
        );
    }
    Ok(resolved_command_id)
}

async fn select_workflow_run_evidence_by_id_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
    include_payload: bool,
) -> anyhow::Result<Option<WorkflowRunEvidence>> {
    let row: Option<WorkflowRunEvidenceRow> = sqlx::query_as(
        "SELECT id, workflow_id, command_id, runtime_job_id, project_id, commit_sha,
                stack, suite, baseline, decision, evidence_schema, digest, trust,
                location::text AS location_json, retention_class,
                CASE WHEN $2::boolean THEN payload::text ELSE NULL END AS payload_json,
                payload_expires_at, payload_expired_at, created_at, updated_at
         FROM workflow_run_evidence
         WHERE id = $1",
    )
    .bind(id)
    .bind(include_payload)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(WorkflowRunEvidence::try_from).transpose()
}

fn validate_run_evidence_input(input: &WorkflowRunEvidenceInput) -> anyhow::Result<()> {
    for (name, value) in [
        ("workflow_id", input.workflow_id.as_str()),
        ("project_id", input.project_id.as_str()),
        ("stack", input.stack.as_str()),
        ("suite", input.suite.as_str()),
        ("decision", input.decision.as_str()),
        ("evidence_schema", input.evidence_schema.as_str()),
        ("digest", input.digest.as_str()),
        ("trust", input.trust.as_str()),
        ("retention_class", input.retention_class.as_str()),
    ] {
        anyhow::ensure!(!value.trim().is_empty(), "{name} is required");
    }
    if let Some(payload) = &input.payload {
        let bytes = serde_json::to_vec(payload)?.len();
        anyhow::ensure!(
            bytes <= WORKFLOW_RUN_EVIDENCE_PAYLOAD_MAX_BYTES,
            "workflow run evidence payload exceeds {WORKFLOW_RUN_EVIDENCE_PAYLOAD_MAX_BYTES} bytes"
        );
    }
    Ok(())
}

fn ensure_idempotent_run_evidence_replay(
    existing: &WorkflowRunEvidence,
    input: &WorkflowRunEvidenceInput,
    command_id: Option<&str>,
) -> anyhow::Result<()> {
    let payload_matches =
        existing.payload == input.payload || existing.payload_expired_at.is_some();
    anyhow::ensure!(
        existing.workflow_id == input.workflow_id
            && existing.command_id.as_deref() == command_id
            && existing.runtime_job_id == input.runtime_job_id
            && existing.project_id == input.project_id
            && existing.commit_sha == input.commit_sha
            && existing.stack == input.stack
            && existing.suite == input.suite
            && existing.baseline == input.baseline
            && existing.decision == input.decision
            && existing.evidence_schema == input.evidence_schema
            && existing.digest == input.digest
            && existing.trust == input.trust
            && existing.location == input.location
            && existing.retention_class == input.retention_class
            && existing.payload_expires_at == input.payload_expires_at
            && payload_matches,
        "workflow run evidence id `{}` already exists with different metadata",
        existing.id
    );
    Ok(())
}

fn blank_to_none(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then_some(value)
    })
}
