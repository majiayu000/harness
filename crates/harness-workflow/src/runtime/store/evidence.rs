use super::*;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const WORKFLOW_RUN_EVIDENCE_SCHEMA: &str = "harness.workflow.run_evidence.v1";
pub const WORKFLOW_RUN_EVIDENCE_EXPORT_SCHEMA: &str = "harness.workflow.run_evidence_export.v1";
pub const WORKFLOW_RUN_EVIDENCE_DEFAULT_LIMIT: i64 = 100;
pub const WORKFLOW_RUN_EVIDENCE_MAX_LIMIT: i64 = 500;
pub const WORKFLOW_RUN_EVIDENCE_PAYLOAD_MAX_BYTES: usize = 64 * 1024;
pub const WORKFLOW_RUN_EVIDENCE_RETENTION_MAX_BATCH: i64 = 1000;
const WORKFLOW_RUN_EVIDENCE_PAYLOAD_RETENTION_DAYS: i64 = 30;
const WORKFLOW_RUN_EVIDENCE_COMPLETION_TRUST: &str = "server_persisted_runtime_completion";

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
        let mut tx = self.pool.begin().await?;
        let record = record_workflow_run_evidence_tx(&mut tx, input).await?;
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
                    CASE
                        WHEN $7::boolean
                         AND payload_expired_at IS NULL
                         AND (payload_expires_at IS NULL OR payload_expires_at > CURRENT_TIMESTAMP)
                        THEN payload::text
                        ELSE NULL
                    END AS payload_json,
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

pub(in crate::runtime::store) async fn record_workflow_run_evidence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    input: WorkflowRunEvidenceInput,
) -> anyhow::Result<WorkflowRunEvidence> {
    validate_run_evidence_input(&input)?;
    let id = input
        .id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let command_id = resolve_run_evidence_command_link_tx(
        tx,
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
    let inserted: Option<WorkflowRunEvidenceRow> = sqlx::query_as(
        "INSERT INTO workflow_run_evidence
                (id, workflow_id, command_id, runtime_job_id, project_id, commit_sha,
                 stack, suite, baseline, decision, evidence_schema, digest, trust,
                 location, retention_class, payload, payload_expires_at)
             VALUES
                ($1, $2, $3, $4, $5, $6,
                 $7, $8, $9, $10, $11, $12, $13,
                 $14::jsonb, $15, $16::jsonb, $17)
             ON CONFLICT (id) DO NOTHING
             RETURNING id, workflow_id, command_id, runtime_job_id, project_id, commit_sha,
                       stack, suite, baseline, decision, evidence_schema, digest, trust,
                       location::text AS location_json, retention_class,
                       payload::text AS payload_json, payload_expires_at, payload_expired_at,
                       created_at, updated_at",
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
    .fetch_optional(&mut **tx)
    .await?;

    if let Some(row) = inserted {
        return WorkflowRunEvidence::try_from(row);
    }

    let record = select_workflow_run_evidence_by_id_tx(tx, &id, true)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow run evidence was not persisted"))?;
    ensure_idempotent_run_evidence_replay(&record, &input, command_id.as_deref())?;
    Ok(record)
}

pub(in crate::runtime::store) async fn record_runtime_completion_evidence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow: &WorkflowInstance,
    command: &WorkflowCommandRecord,
    job: &RuntimeJob,
    event: &WorkflowEvent,
    result: &ActivityResult,
    decision: Option<&WorkflowDecisionRecord>,
) -> anyhow::Result<WorkflowRunEvidence> {
    let prompt = latest_runtime_prompt_packet_tx(tx, &job.id).await?;
    let payload =
        bounded_runtime_completion_payload(event, result, decision, prompt.digest.as_deref())?;
    let digest = evidence_payload_digest(&json!({
        "schema": WORKFLOW_RUN_EVIDENCE_SCHEMA,
        "workflow_id": workflow.id.as_str(),
        "command_id": command.id.as_str(),
        "runtime_job_id": job.id.as_str(),
        "activity_result": result,
        "decision": decision,
    }))?;
    record_workflow_run_evidence_tx(
        tx,
        WorkflowRunEvidenceInput {
            id: Some(format!("runtime-completion:{}", job.id)),
            workflow_id: workflow.id.clone(),
            command_id: Some(command.id.clone()),
            runtime_job_id: Some(job.id.clone()),
            project_id: project_id_for_completion_evidence(workflow, job, prompt.packet.as_ref()),
            commit_sha: commit_sha_for_completion_evidence(workflow, job, result),
            stack: job.runtime_profile.clone(),
            suite: completion_suite(job, result),
            baseline: baseline_for_completion_evidence(workflow, job, prompt.packet.as_ref()),
            decision: decision
                .map(|record| record.decision.decision.clone())
                .unwrap_or_else(|| "no_runtime_decision".to_string()),
            evidence_schema: WORKFLOW_RUN_EVIDENCE_SCHEMA.to_string(),
            digest,
            trust: WORKFLOW_RUN_EVIDENCE_COMPLETION_TRUST.to_string(),
            location: runtime_completion_location(
                event,
                command,
                job,
                decision,
                prompt.digest.as_deref(),
            ),
            retention_class: "runtime_completion".to_string(),
            payload: Some(payload),
            payload_expires_at: event.created_at.checked_add_signed(chrono::Duration::days(
                WORKFLOW_RUN_EVIDENCE_PAYLOAD_RETENTION_DAYS,
            )),
        },
    )
    .await
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
        if let Some(command_id) = command_id {
            anyhow::ensure!(
                command_id == job_command_id,
                "runtime job `{runtime_job_id}` belongs to command `{job_command_id}`, not `{command_id}`"
            );
        }
        return Ok(Some(job_command_id));
    } else if let Some(command_id) = command_id {
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
        return Ok(Some(command_id.to_string()));
    }
    Ok(None)
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
    let payload_expires_at_matches = existing
        .payload_expires_at
        .map(|value| value.timestamp_micros())
        == input
            .payload_expires_at
            .map(|value| value.timestamp_micros());
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
            && payload_expires_at_matches
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

#[derive(Debug, Default)]
struct RuntimePromptPacketEvidence {
    digest: Option<String>,
    packet: Option<Value>,
}

async fn latest_runtime_prompt_packet_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    runtime_job_id: &str,
) -> anyhow::Result<RuntimePromptPacketEvidence> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT data #>> '{event,prompt_packet_digest}',
                (data #> '{event,prompt_packet}')::text
         FROM runtime_events
         WHERE runtime_job_id = $1
           AND event_type = 'RuntimePromptPrepared'
         ORDER BY sequence DESC
         LIMIT 1",
    )
    .bind(runtime_job_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((digest, packet_json)) = row else {
        return Ok(RuntimePromptPacketEvidence::default());
    };
    let packet = packet_json
        .filter(|raw| raw != "null")
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?;
    Ok(RuntimePromptPacketEvidence { digest, packet })
}

fn runtime_completion_location(
    event: &WorkflowEvent,
    command: &WorkflowCommandRecord,
    job: &RuntimeJob,
    decision: Option<&WorkflowDecisionRecord>,
    prompt_packet_digest: Option<&str>,
) -> Value {
    let mut location = json!({
        "kind": "runtime_completion",
        "workflow_event_id": event.id.as_str(),
        "runtime_job_id": job.id.as_str(),
        "command_id": command.id.as_str(),
        "runtime_job_output": {
            "table": "runtime_jobs",
            "column": "data.output"
        }
    });
    if let Some(decision) = decision {
        location["decision_id"] = json!(decision.id.as_str());
        location["decision_accepted"] = json!(decision.accepted);
    }
    if let Some(prompt_packet_digest) = prompt_packet_digest {
        location["prompt_packet_digest"] = json!(prompt_packet_digest);
    }
    location
}

fn bounded_runtime_completion_payload(
    event: &WorkflowEvent,
    result: &ActivityResult,
    decision: Option<&WorkflowDecisionRecord>,
    prompt_packet_digest: Option<&str>,
) -> anyhow::Result<Value> {
    let full = json!({
        "workflow_event_id": event.id.as_str(),
        "prompt_packet_digest": prompt_packet_digest,
        "decision_id": decision.map(|record| record.id.as_str()),
        "decision_accepted": decision.map(|record| record.accepted),
        "activity_result": result,
    });
    if serde_json::to_vec(&full)?.len() <= WORKFLOW_RUN_EVIDENCE_PAYLOAD_MAX_BYTES {
        return Ok(full);
    }
    Ok(json!({
        "workflow_event_id": event.id.as_str(),
        "prompt_packet_digest": prompt_packet_digest,
        "decision_id": decision.map(|record| record.id.as_str()),
        "decision_accepted": decision.map(|record| record.accepted),
        "activity": result.activity.as_str(),
        "status": result.status,
        "summary": result.summary.as_str(),
        "artifact_count": result.artifacts.len(),
        "signal_count": result.signals.len(),
        "validation_count": result.validation.len(),
        "payload_truncated": true,
    }))
}

fn evidence_payload_digest(value: &Value) -> anyhow::Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value)?)
    ))
}

fn project_id_for_completion_evidence(
    workflow: &WorkflowInstance,
    job: &RuntimeJob,
    prompt_packet: Option<&Value>,
) -> String {
    string_field(&workflow.data, "project_id")
        .or_else(|| string_field(&job.input, "project_id"))
        .or_else(|| string_path(&job.input, &["command", "project_id"]))
        .or_else(|| prompt_packet.and_then(|packet| string_path(packet, &["project", "root"])))
        .unwrap_or_else(|| workflow.id.clone())
}

fn commit_sha_for_completion_evidence(
    workflow: &WorkflowInstance,
    job: &RuntimeJob,
    result: &ActivityResult,
) -> Option<String> {
    result
        .artifacts
        .iter()
        .find_map(|artifact| {
            string_any_field(
                &artifact.artifact,
                &["head_sha", "head_oid", "commit_sha", "merge_commit_sha"],
            )
        })
        .or_else(|| string_any_field(&workflow.data, &["head_sha", "pr_head_sha", "commit_sha"]))
        .or_else(|| string_any_field(&job.input, &["head_sha", "pr_head_sha", "commit_sha"]))
        .or_else(|| {
            job.input.get("command").and_then(|command| {
                string_any_field(command, &["head_sha", "pr_head_sha", "commit_sha"])
            })
        })
}

fn completion_suite(job: &RuntimeJob, result: &ActivityResult) -> String {
    if !result.activity.trim().is_empty() {
        return result.activity.clone();
    }
    string_field(&job.input, "activity").unwrap_or_else(|| "workflow_activity".to_string())
}

fn baseline_for_completion_evidence(
    workflow: &WorkflowInstance,
    job: &RuntimeJob,
    prompt_packet: Option<&Value>,
) -> Option<String> {
    baseline_string(&workflow.data)
        .or_else(|| baseline_string(&job.input))
        .or_else(|| job.input.get("command").and_then(baseline_string))
        .or_else(|| prompt_packet.and_then(prompt_packet_base_ref))
}

fn baseline_string(value: &Value) -> Option<String> {
    string_any_field(
        value,
        &[
            "baseline",
            "base_ref",
            "target_base_ref",
            "expected_base_ref",
        ],
    )
}

fn prompt_packet_base_ref(prompt_packet: &Value) -> Option<String> {
    let base = prompt_packet.pointer("/workflow_file/config/base")?;
    let remote = string_field(base, "remote").unwrap_or_else(|| "origin".to_string());
    let branch = string_field(base, "branch")?;
    Some(format!("{remote}/{branch}"))
}

fn string_any_field(value: &Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| string_field(value, field))
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn string_path(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for segment in path {
        cursor = cursor.get(*segment)?;
    }
    cursor
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
