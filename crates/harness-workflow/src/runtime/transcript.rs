use super::model::{ActivityArtifact, ActivityResult, RuntimeJob};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const RUNTIME_TRANSCRIPT_ARTIFACT: &str = "runtime_transcript";
pub const RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT: &str = "runtime_transcript_source";
pub const RUNTIME_TRANSCRIPT_SCHEMA: &str = "harness.runtime.transcript.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeTranscriptReference {
    pub artifact_ref: String,
    pub size_bytes: u64,
    pub checksum: String,
    pub producer_runtime_job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeTranscriptRecord {
    pub schema: String,
    pub reference: RuntimeTranscriptReference,
    pub workflow_id: String,
    pub content: String,
    #[serde(default)]
    pub source: Value,
    #[serde(default)]
    pub reconstructed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconstructed_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeTranscriptRead {
    Verified(RuntimeTranscriptRecord),
    Missing,
    InvalidMetadata { detail: String },
    ChecksumMismatch { expected: String, actual: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingRuntimeTranscript {
    pub record: RuntimeTranscriptRecord,
}

pub fn runtime_transcript_artifact_ref(runtime_job_id: &str) -> String {
    format!("runtime-transcript:{runtime_job_id}")
}

pub fn runtime_transcript_checksum(content: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(content.as_bytes()))
}

pub fn prepare_runtime_transcript(
    job: &RuntimeJob,
    mut result: ActivityResult,
) -> anyhow::Result<(ActivityResult, Option<PendingRuntimeTranscript>)> {
    let mut source = None;
    let mut source_count = 0_u8;
    result
        .artifacts
        .retain(|artifact| match artifact.artifact_type.as_str() {
            RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT => {
                source_count = source_count.saturating_add(1);
                source = Some(artifact.artifact.clone());
                false
            }
            RUNTIME_TRANSCRIPT_ARTIFACT => false,
            _ => true,
        });
    anyhow::ensure!(
        source_count <= 1,
        "activity result contains multiple runtime transcript sources"
    );
    let Some(mut source) = source else {
        return Ok((result, None));
    };
    let content = source
        .get_mut("content")
        .map(Value::take)
        .and_then(|content| content.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| anyhow::anyhow!("runtime transcript source is missing string content"))?;
    if let Some(source) = source.as_object_mut() {
        source.remove("content");
    }
    let size_bytes = u64::try_from(content.len())
        .map_err(|_| anyhow::anyhow!("runtime transcript is too large to address"))?;
    let reference = RuntimeTranscriptReference {
        artifact_ref: runtime_transcript_artifact_ref(&job.id),
        size_bytes,
        checksum: runtime_transcript_checksum(&content),
        producer_runtime_job_id: job.id.clone(),
    };
    result.artifacts.push(ActivityArtifact::new(
        RUNTIME_TRANSCRIPT_ARTIFACT,
        serde_json::to_value(&reference)?,
    ));
    let workflow_id = job
        .input
        .get("workflow_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("runtime transcript source requires workflow_id"))?;
    Ok((
        result,
        Some(PendingRuntimeTranscript {
            record: RuntimeTranscriptRecord {
                schema: RUNTIME_TRANSCRIPT_SCHEMA.to_string(),
                reference,
                workflow_id: workflow_id.to_string(),
                content,
                source,
                reconstructed: false,
                reconstructed_by: None,
            },
        }),
    ))
}

pub(crate) fn verify_runtime_transcript_record(
    record: RuntimeTranscriptRecord,
) -> RuntimeTranscriptRead {
    if record.schema != RUNTIME_TRANSCRIPT_SCHEMA {
        return RuntimeTranscriptRead::InvalidMetadata {
            detail: format!("unsupported transcript schema `{}`", record.schema),
        };
    }
    if record.reference.artifact_ref.trim().is_empty()
        || record.reference.producer_runtime_job_id.trim().is_empty()
        || record.workflow_id.trim().is_empty()
    {
        return RuntimeTranscriptRead::InvalidMetadata {
            detail: "transcript identity metadata is incomplete".to_string(),
        };
    }
    let actual_size = match u64::try_from(record.content.len()) {
        Ok(size) => size,
        Err(_) => {
            return RuntimeTranscriptRead::InvalidMetadata {
                detail: "transcript size cannot be represented".to_string(),
            }
        }
    };
    if actual_size != record.reference.size_bytes {
        return RuntimeTranscriptRead::InvalidMetadata {
            detail: format!(
                "transcript size mismatch: expected {}, read {actual_size}",
                record.reference.size_bytes
            ),
        };
    }
    let actual = runtime_transcript_checksum(&record.content);
    if actual != record.reference.checksum {
        return RuntimeTranscriptRead::ChecksumMismatch {
            expected: record.reference.checksum.clone(),
            actual,
        };
    }
    RuntimeTranscriptRead::Verified(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimeKind, RuntimeProfile};
    use serde_json::json;

    fn job() -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexExec,
            RuntimeProfile::new("codex", RuntimeKind::CodexExec).name,
            json!({"workflow_id": "workflow-1"}),
        )
    }

    #[test]
    fn transcript_preparation_replaces_source_with_stable_reference() -> anyhow::Result<()> {
        let job = job();
        let result = ActivityResult::succeeded("implement", "done")
            .with_artifact(ActivityArtifact::new(
                RUNTIME_TRANSCRIPT_ARTIFACT,
                json!({"artifact_ref": "runtime-transcript:forged"}),
            ))
            .with_artifact(ActivityArtifact::new(
                RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
                json!({"content": "exact bytes", "turn_id": "turn-1"}),
            ));

        let (result, pending) = prepare_runtime_transcript(&job, result)?;
        let pending = pending.expect("transcript should be prepared");

        assert_eq!(pending.record.content, "exact bytes");
        assert_eq!(pending.record.source["turn_id"], "turn-1");
        assert_eq!(
            pending.record.reference.artifact_ref,
            runtime_transcript_artifact_ref(&job.id)
        );
        assert!(result
            .artifacts
            .iter()
            .all(|artifact| artifact.artifact_type != RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT));
        let references = result
            .artifacts
            .iter()
            .filter(|artifact| artifact.artifact_type == RUNTIME_TRANSCRIPT_ARTIFACT)
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 1);
        assert_eq!(
            references[0].artifact["artifact_ref"],
            runtime_transcript_artifact_ref(&job.id)
        );
        Ok(())
    }

    #[test]
    fn transcript_preparation_discards_unverified_reference_without_source() -> anyhow::Result<()> {
        let result =
            ActivityResult::succeeded("implement", "done").with_artifact(ActivityArtifact::new(
                RUNTIME_TRANSCRIPT_ARTIFACT,
                json!({"artifact_ref": "runtime-transcript:forged"}),
            ));

        let (result, pending) = prepare_runtime_transcript(&job(), result)?;

        assert!(pending.is_none());
        assert!(result
            .artifacts
            .iter()
            .all(|artifact| artifact.artifact_type != RUNTIME_TRANSCRIPT_ARTIFACT));
        Ok(())
    }

    #[test]
    fn transcript_preparation_rejects_multiple_sources() {
        let result = ActivityResult::succeeded("implement", "done")
            .with_artifact(ActivityArtifact::new(
                RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
                json!({"content": "first"}),
            ))
            .with_artifact(ActivityArtifact::new(
                RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
                json!({"content": "second"}),
            ));

        let error = prepare_runtime_transcript(&job(), result)
            .expect_err("multiple transcript sources must be rejected");
        assert!(error
            .to_string()
            .contains("multiple runtime transcript sources"));
    }

    #[test]
    fn transcript_verification_rejects_checksum_mismatch() {
        let reference = RuntimeTranscriptReference {
            artifact_ref: "runtime-transcript:job-1".to_string(),
            size_bytes: 7,
            checksum: runtime_transcript_checksum("before"),
            producer_runtime_job_id: "job-1".to_string(),
        };
        let record = RuntimeTranscriptRecord {
            schema: RUNTIME_TRANSCRIPT_SCHEMA.to_string(),
            reference,
            workflow_id: "workflow-1".to_string(),
            content: "changed".to_string(),
            source: json!({}),
            reconstructed: false,
            reconstructed_by: None,
        };

        assert!(matches!(
            verify_runtime_transcript_record(record),
            RuntimeTranscriptRead::ChecksumMismatch { .. }
        ));
    }
}
