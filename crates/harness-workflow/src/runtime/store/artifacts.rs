use super::*;
use crate::runtime::transcript::{
    runtime_transcript_artifact_ref, runtime_transcript_checksum, verify_runtime_transcript_record,
    PendingRuntimeTranscript, RuntimeTranscriptRead, RuntimeTranscriptRecord,
    RuntimeTranscriptReference, RUNTIME_TRANSCRIPT_ARTIFACT, RUNTIME_TRANSCRIPT_SCHEMA,
};

impl WorkflowRuntimeStore {
    pub async fn read_runtime_transcript(
        &self,
        artifact_ref: &str,
    ) -> anyhow::Result<RuntimeTranscriptRead> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT workflow_id, artifact_type, data::text
             FROM workflow_artifacts
             WHERE id = $1",
        )
        .bind(artifact_ref)
        .fetch_optional(&self.pool)
        .await?;
        let Some((workflow_id, artifact_type, data)) = row else {
            return Ok(RuntimeTranscriptRead::Missing);
        };
        if artifact_type != RUNTIME_TRANSCRIPT_ARTIFACT {
            return Ok(RuntimeTranscriptRead::InvalidMetadata {
                detail: format!("artifact `{artifact_ref}` is not a runtime transcript"),
            });
        }
        let record: RuntimeTranscriptRecord = match serde_json::from_str(&data) {
            Ok(record) => record,
            Err(error) => {
                return Ok(RuntimeTranscriptRead::InvalidMetadata {
                    detail: format!("transcript metadata is unreadable: {error}"),
                })
            }
        };
        if record.workflow_id != workflow_id || record.reference.artifact_ref != artifact_ref {
            return Ok(RuntimeTranscriptRead::InvalidMetadata {
                detail: "transcript row identity does not match its metadata".to_string(),
            });
        }
        Ok(verify_runtime_transcript_record(record))
    }

    pub async fn reconstruct_runtime_transcript(
        &self,
        workflow_id: &str,
        runtime_job_id: &str,
        content: &str,
        expected_checksum: Option<&str>,
        actor: &str,
    ) -> anyhow::Result<RuntimeTranscriptRecord> {
        anyhow::ensure!(!workflow_id.trim().is_empty(), "workflow_id is required");
        anyhow::ensure!(
            !runtime_job_id.trim().is_empty(),
            "runtime_job_id is required"
        );
        anyhow::ensure!(!actor.trim().is_empty(), "reconstruction actor is required");
        let checksum = runtime_transcript_checksum(content);
        if let Some(expected) = expected_checksum {
            anyhow::ensure!(
                expected == checksum,
                "re-exported transcript checksum does not match expected checksum"
            );
        }
        let size_bytes = u64::try_from(content.len())
            .map_err(|_| anyhow::anyhow!("runtime transcript is too large to address"))?;
        let record = RuntimeTranscriptRecord {
            schema: RUNTIME_TRANSCRIPT_SCHEMA.to_string(),
            reference: RuntimeTranscriptReference {
                artifact_ref: runtime_transcript_artifact_ref(runtime_job_id),
                size_bytes,
                checksum,
                producer_runtime_job_id: runtime_job_id.to_string(),
            },
            workflow_id: workflow_id.to_string(),
            content: content.to_string(),
            source: json!({"kind": "provider_reexport"}),
            reconstructed: true,
            reconstructed_by: Some(actor.to_string()),
        };
        let mut tx = self.pool.begin().await?;
        ensure_runtime_job_belongs_to_workflow_tx(&mut tx, workflow_id, runtime_job_id).await?;
        let persisted_record = upsert_reconstructed_transcript_tx(&mut tx, &record).await?;
        tx.commit().await?;
        Ok(persisted_record)
    }
}

pub(super) async fn insert_runtime_transcript_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    runtime_job_id: &str,
    pending: &PendingRuntimeTranscript,
) -> anyhow::Result<()> {
    let record = &pending.record;
    anyhow::ensure!(
        record.workflow_id == workflow_id,
        "transcript workflow mismatch"
    );
    anyhow::ensure!(
        record.reference.producer_runtime_job_id == runtime_job_id,
        "transcript producer runtime job mismatch"
    );
    anyhow::ensure!(
        matches!(
            verify_runtime_transcript_record(record.clone()),
            RuntimeTranscriptRead::Verified(_)
        ),
        "runtime transcript failed integrity validation before persistence"
    );
    let existing: Option<(String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT workflow_id, runtime_job_id, artifact_type, data::text
         FROM workflow_artifacts
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(&record.reference.artifact_ref)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((existing_workflow_id, existing_runtime_job_id, artifact_type, data)) = existing {
        anyhow::ensure!(
            existing_workflow_id == workflow_id
                && existing_runtime_job_id.as_deref() == Some(runtime_job_id)
                && artifact_type == RUNTIME_TRANSCRIPT_ARTIFACT,
            "runtime transcript identity already exists with different ownership"
        );
        let existing: RuntimeTranscriptRecord = serde_json::from_str(&data)?;
        anyhow::ensure!(
            existing == *record,
            "runtime transcript identity already exists with different content"
        );
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO workflow_artifacts
            (id, workflow_id, runtime_job_id, artifact_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind(&record.reference.artifact_ref)
    .bind(workflow_id)
    .bind(runtime_job_id)
    .bind(RUNTIME_TRANSCRIPT_ARTIFACT)
    .bind(to_jsonb_string(record)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn reconcile_runtime_transcript_dependencies_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM workflow_artifact_dependencies AS dependency
         WHERE dependency.workflow_id = $1
           AND NOT EXISTS (
               SELECT 1
               FROM workflow_commands AS command
               WHERE command.workflow_id = $1
                 AND jsonb_typeof(
                     command.data #> '{command,exact_replay,transcript_artifact_ref}'
                 ) = 'string'
                 AND btrim(
                     command.data #>> '{command,exact_replay,transcript_artifact_ref}'
                 ) = dependency.artifact_ref
           )
           AND NOT EXISTS (
               SELECT 1
               FROM runtime_jobs AS job
               JOIN workflow_commands AS command ON command.id = job.command_id
               WHERE command.workflow_id = $1
                 AND jsonb_typeof(
                     job.data #> '{input,command,exact_replay,transcript_artifact_ref}'
                 ) = 'string'
                 AND btrim(
                     job.data #>> '{input,command,exact_replay,transcript_artifact_ref}'
                 ) = dependency.artifact_ref
           )",
    )
    .bind(workflow_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO workflow_artifact_dependencies (artifact_ref, workflow_id)
         SELECT DISTINCT refs.artifact_ref, $1
         FROM (
             SELECT btrim(
                 command.data #>> '{command,exact_replay,transcript_artifact_ref}'
             ) AS artifact_ref
             FROM workflow_commands AS command
             WHERE command.workflow_id = $1
               AND jsonb_typeof(
                   command.data #> '{command,exact_replay,transcript_artifact_ref}'
               ) = 'string'
             UNION
             SELECT btrim(
                 job.data #>> '{input,command,exact_replay,transcript_artifact_ref}'
             ) AS artifact_ref
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE command.workflow_id = $1
               AND jsonb_typeof(
                   job.data #> '{input,command,exact_replay,transcript_artifact_ref}'
               ) = 'string'
         ) AS refs
         WHERE refs.artifact_ref <> ''
         ON CONFLICT (artifact_ref, workflow_id) DO NOTHING",
    )
    .bind(workflow_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn ensure_runtime_job_belongs_to_workflow_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    runtime_job_id: &str,
) -> anyhow::Result<()> {
    let (belongs,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (
            SELECT 1
            FROM runtime_jobs AS job
            JOIN workflow_commands AS command ON command.id = job.command_id
            WHERE job.id = $1 AND command.workflow_id = $2
        )",
    )
    .bind(runtime_job_id)
    .bind(workflow_id)
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(
        belongs,
        "runtime job does not belong to the requested workflow"
    );
    Ok(())
}

async fn upsert_reconstructed_transcript_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &RuntimeTranscriptRecord,
) -> anyhow::Result<RuntimeTranscriptRecord> {
    let existing: Option<(String, String, String)> = sqlx::query_as(
        "SELECT workflow_id, artifact_type, data::text
         FROM workflow_artifacts
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(&record.reference.artifact_ref)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((workflow_id, artifact_type, data)) = existing {
        anyhow::ensure!(
            workflow_id == record.workflow_id && artifact_type == RUNTIME_TRANSCRIPT_ARTIFACT,
            "existing artifact identity does not match the transcript reconstruction target"
        );
        if let Ok(existing) = serde_json::from_str::<RuntimeTranscriptRecord>(&data) {
            if let RuntimeTranscriptRead::Verified(existing) =
                verify_runtime_transcript_record(existing)
            {
                anyhow::ensure!(
                    existing.reference.checksum == record.reference.checksum,
                    "a verified transcript already exists with different content"
                );
                return Ok(existing);
            }
        }
    }
    sqlx::query(
        "INSERT INTO workflow_artifacts
            (id, workflow_id, runtime_job_id, artifact_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)
         ON CONFLICT (id) DO UPDATE
         SET workflow_id = EXCLUDED.workflow_id,
             runtime_job_id = EXCLUDED.runtime_job_id,
             artifact_type = EXCLUDED.artifact_type,
             data = EXCLUDED.data,
             created_at = CURRENT_TIMESTAMP",
    )
    .bind(&record.reference.artifact_ref)
    .bind(&record.workflow_id)
    .bind(&record.reference.producer_runtime_job_id)
    .bind(RUNTIME_TRANSCRIPT_ARTIFACT)
    .bind(to_jsonb_string(record)?)
    .execute(&mut **tx)
    .await?;
    Ok(record.clone())
}
