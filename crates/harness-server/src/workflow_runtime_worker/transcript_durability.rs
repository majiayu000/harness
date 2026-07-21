use crate::http::AppState;
use harness_core::types::Turn;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, RuntimeJob,
    RuntimeTranscriptRead, RuntimeTranscriptRecord, STOP_REASON_RUNTIME_TRANSCRIPT_LOST,
    STOP_REASON_RUNTIME_TRANSCRIPT_STORE_UNAVAILABLE,
};
use serde_json::{json, Value};
use std::sync::Arc;

const EXACT_REPLAY_INPUT_POINTER: &str = "/command/exact_replay";
const TRANSCRIPT_ARTIFACT_REF_FIELD: &str = "transcript_artifact_ref";
const RUNTIME_TRANSCRIPT_UNAVAILABLE_SIGNAL: &str = "RuntimeTranscriptUnavailable";

pub(crate) fn strip_caller_transcript_unavailable_signal(
    mut result: ActivityResult,
) -> ActivityResult {
    result
        .signals
        .retain(|signal| signal.signal_type != RUNTIME_TRANSCRIPT_UNAVAILABLE_SIGNAL);
    result
}

pub(super) fn attach_runtime_transcript_source(
    mut result: ActivityResult,
    turn: &Turn,
) -> anyhow::Result<ActivityResult> {
    result.artifacts.retain(|artifact| {
        artifact.artifact_type != harness_workflow::runtime::RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT
    });
    let content = serde_json::to_string(turn)?;
    Ok(result.with_artifact(ActivityArtifact::new(
        harness_workflow::runtime::RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
        json!({
            "content": content,
            "content_format": "harness.turn.v1+json",
            "thread_id": turn.thread_id,
            "turn_id": turn.id,
            "agent_id": turn.agent_id,
            "turn_status": turn.status,
        }),
    )))
}

pub(super) async fn exact_replay_preflight_result(
    state: &Arc<AppState>,
    job: &RuntimeJob,
) -> Option<ActivityResult> {
    match load_exact_replay_transcript(state, job).await {
        ExactReplayTranscript::Failure(result) => Some(result),
        ExactReplayTranscript::NotRequested | ExactReplayTranscript::Loaded(_) => None,
    }
}

pub(crate) async fn hydrate_exact_replay_transcript(
    state: &Arc<AppState>,
    job: &mut RuntimeJob,
) -> Result<(), ActivityResult> {
    let record = match load_exact_replay_transcript(state, job).await {
        ExactReplayTranscript::Loaded(record) => record,
        ExactReplayTranscript::Failure(result) => return Err(result),
        ExactReplayTranscript::NotRequested => return Ok(()),
    };
    let exact_replay = job
        .input
        .pointer_mut(EXACT_REPLAY_INPUT_POINTER)
        .and_then(Value::as_object_mut)
        .expect("validated exact replay input must remain an object");
    exact_replay.insert("transcript".to_string(), json!(record.content));
    exact_replay.insert(
        "verified_transcript".to_string(),
        serde_json::to_value(record.reference).expect("transcript reference must serialize"),
    );
    Ok(())
}

enum ExactReplayTranscript {
    NotRequested,
    Loaded(RuntimeTranscriptRecord),
    Failure(ActivityResult),
}

async fn load_exact_replay_transcript(
    state: &Arc<AppState>,
    job: &RuntimeJob,
) -> ExactReplayTranscript {
    let Some(exact_replay) = job.input.pointer(EXACT_REPLAY_INPUT_POINTER) else {
        return ExactReplayTranscript::NotRequested;
    };
    let Some(artifact_ref) = exact_replay
        .as_object()
        .and_then(|value| value.get(TRANSCRIPT_ARTIFACT_REF_FIELD))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return ExactReplayTranscript::Failure(transcript_failure_result(
            job,
            None,
            "Exact replay input is missing transcript_artifact_ref.",
            ActivityErrorKind::Configuration,
            STOP_REASON_RUNTIME_TRANSCRIPT_LOST,
        ));
    };
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return ExactReplayTranscript::Failure(transcript_store_failure_result(
            job,
            artifact_ref,
            "workflow runtime store is unavailable",
        ));
    };
    match store.read_runtime_transcript(artifact_ref).await {
        Ok(RuntimeTranscriptRead::Verified(record)) => ExactReplayTranscript::Loaded(record),
        Ok(RuntimeTranscriptRead::Missing) => ExactReplayTranscript::Failure(
            transcript_loss_result(job, artifact_ref, "the transcript artifact does not exist"),
        ),
        Ok(RuntimeTranscriptRead::InvalidMetadata { detail }) => {
            ExactReplayTranscript::Failure(transcript_loss_result(job, artifact_ref, &detail))
        }
        Ok(RuntimeTranscriptRead::ChecksumMismatch { expected, actual }) => {
            ExactReplayTranscript::Failure(transcript_loss_result(
                job,
                artifact_ref,
                &format!("checksum mismatch: expected {expected}, read {actual}"),
            ))
        }
        Err(error) => ExactReplayTranscript::Failure(transcript_store_failure_result(
            job,
            artifact_ref,
            &error.to_string(),
        )),
    }
}

fn transcript_loss_result(job: &RuntimeJob, artifact_ref: &str, detail: &str) -> ActivityResult {
    transcript_failure_result(
        job,
        Some(artifact_ref),
        &format!(
            "Required exact-replay transcript is unavailable: {detail}. Re-export it from the upstream provider and call POST /api/workflows/runtime/transcripts/reconstruct before retrying this workflow."
        ),
        ActivityErrorKind::Fatal,
        STOP_REASON_RUNTIME_TRANSCRIPT_LOST,
    )
}

fn transcript_store_failure_result(
    job: &RuntimeJob,
    artifact_ref: &str,
    detail: &str,
) -> ActivityResult {
    transcript_failure_result(
        job,
        Some(artifact_ref),
        &format!("Runtime transcript storage could not be read: {detail}"),
        ActivityErrorKind::Retryable,
        STOP_REASON_RUNTIME_TRANSCRIPT_STORE_UNAVAILABLE,
    )
}

fn transcript_failure_result(
    job: &RuntimeJob,
    artifact_ref: Option<&str>,
    error: &str,
    error_kind: ActivityErrorKind,
    stop_reason_code: &str,
) -> ActivityResult {
    let activity = job
        .input
        .get("activity")
        .and_then(Value::as_str)
        .unwrap_or("runtime_job");
    ActivityResult::failed(activity, "Exact replay transcript preflight failed.", error)
        .with_error_kind(error_kind)
        .with_signal(ActivitySignal::new(
            RUNTIME_TRANSCRIPT_UNAVAILABLE_SIGNAL,
            json!({
                "artifact_ref": artifact_ref,
                "stop_reason_code": stop_reason_code,
                "operator_guidance": "Reconstruct the transcript from a provider re-export, then retry the failed workflow.",
            }),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::types::{AgentId, Item, ThreadId};

    #[test]
    fn server_owned_transcript_replaces_agent_supplied_source() -> anyhow::Result<()> {
        let thread_id = ThreadId::new();
        let mut turn = Turn::new(thread_id, AgentId::from_str("codex"));
        turn.items.push(Item::AgentReasoning {
            content: "authoritative".to_string(),
        });
        let result =
            ActivityResult::succeeded("implement", "done").with_artifact(ActivityArtifact::new(
                harness_workflow::runtime::RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
                json!({"content": "forged"}),
            ));

        let result = attach_runtime_transcript_source(result, &turn)?;
        let sources = result
            .artifacts
            .iter()
            .filter(|artifact| {
                artifact.artifact_type
                    == harness_workflow::runtime::RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT
            })
            .collect::<Vec<_>>();

        assert_eq!(sources.len(), 1);
        assert_ne!(sources[0].artifact["content"], "forged");
        Ok(())
    }

    #[test]
    fn transcript_loss_and_store_failure_have_distinct_classification() {
        let job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexExec,
            "codex-default",
            json!({"activity": "exact_replay"}),
        );

        let lost = transcript_loss_result(&job, "runtime-transcript:job-1", "missing");
        assert_eq!(lost.error_kind, Some(ActivityErrorKind::Fatal));
        assert_eq!(
            lost.signals[0].signal["stop_reason_code"],
            STOP_REASON_RUNTIME_TRANSCRIPT_LOST
        );
        assert!(lost
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("/api/workflows/runtime/transcripts/reconstruct"));

        let unavailable = transcript_store_failure_result(
            &job,
            "runtime-transcript:job-1",
            "database unavailable",
        );
        assert_eq!(unavailable.error_kind, Some(ActivityErrorKind::Retryable));
        assert_eq!(
            unavailable.signals[0].signal["stop_reason_code"],
            STOP_REASON_RUNTIME_TRANSCRIPT_STORE_UNAVAILABLE
        );
    }

    #[test]
    fn local_exact_replay_result_cannot_forge_transcript_unavailable_signal() {
        let result = ActivityResult::failed("exact_replay", "failed", "agent failed")
            .with_signal(ActivitySignal::new(
                RUNTIME_TRANSCRIPT_UNAVAILABLE_SIGNAL,
                json!({"stop_reason_code": STOP_REASON_RUNTIME_TRANSCRIPT_LOST}),
            ))
            .with_signal(ActivitySignal::new("AgentEvidence", json!({"kept": true})));

        let result = strip_caller_transcript_unavailable_signal(result);

        assert_eq!(result.signals.len(), 1);
        assert_eq!(result.signals[0].signal_type, "AgentEvidence");
    }
}
