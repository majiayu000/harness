use crate::http::AppState;
use harness_workflow::runtime::{
    completion_evidence::{server_validation_digest_passed, ARTIFACT_EVAL_BASE_CHECKOUT},
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus, RuntimeJob,
    QUALITY_GATE_ACTIVITY,
};
use serde_json::json;
use std::sync::Arc;

pub(crate) async fn apply_remote_completion_evidence(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    result: ActivityResult,
) -> anyhow::Result<ActivityResult> {
    let workflow = super::job_context::workflow_for_job(state, job).await?;
    if super::pr_feedback_inspection::is_server_owned_pr_feedback_inspection(job) {
        return Ok(
            super::pr_feedback_inspection::execute_pr_feedback_inspection(
                state,
                job,
                workflow.as_ref(),
            )
            .await,
        );
    }

    let result = super::merge_completion::verify_merge_completion_if_needed(
        state,
        job,
        workflow.as_ref(),
        result,
    )
    .await;
    let result = super::completion_evidence_integration::apply_external_completion_evidence(
        state,
        job,
        workflow.as_ref(),
        result,
    )
    .await;
    if super::data_helpers::activity_name(job) != QUALITY_GATE_ACTIVITY
        || result.status != ActivityStatus::Succeeded
    {
        return Ok(result);
    }

    if !state
        .core
        .server
        .config
        .workflow
        .completion_evidence_enforced
    {
        return Ok(result);
    }

    let revision_bound = remote_quality_gate_has_revision_bound_verification(&result);
    Ok(if revision_bound {
        result
    } else {
        remote_quality_gate_requires_revision_bound_verification(result)
    })
}

fn remote_quality_gate_has_revision_bound_verification(result: &ActivityResult) -> bool {
    result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == ARTIFACT_EVAL_BASE_CHECKOUT)
        && server_validation_digest_passed(result)
}

fn remote_quality_gate_requires_revision_bound_verification(
    mut result: ActivityResult,
) -> ActivityResult {
    let error = "remote quality-gate success requires server verification against the exact remote revision; no revision-bound server workspace is available";
    result.status = ActivityStatus::Failed;
    result.summary = "Remote quality-gate completion could not be independently verified.".into();
    result.error = Some(error.into());
    result.error_kind = Some(ActivityErrorKind::Configuration);
    result.artifacts.push(ActivityArtifact::new(
        "remote_quality_gate_verification",
        json!({
            "verified": false,
            "reason": "revision_bound_workspace_unavailable",
        }),
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_quality_gate_success_fails_closed_without_revision_bound_workspace() {
        let result = remote_quality_gate_requires_revision_bound_verification(
            ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "Remote host reported success."),
        );

        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "remote_quality_gate_verification"
                && artifact.artifact["verified"] == false
                && artifact.artifact["reason"] == "revision_bound_workspace_unavailable"
        }));
    }

    #[test]
    fn remote_quality_gate_accepts_revision_bound_host_validation() {
        let result = ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "validated")
            .with_artifact(ActivityArtifact::new(
                ARTIFACT_EVAL_BASE_CHECKOUT,
                json!({"requested_commit": "a", "observed_commit": "a"}),
            ))
            .with_artifact(ActivityArtifact::new(
                harness_workflow::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST,
                json!({"commands": [{"command": "cargo check", "exit_code": 0}]}),
            ));

        assert!(remote_quality_gate_has_revision_bound_verification(&result));
    }

    #[test]
    fn remote_quality_gate_rejects_validation_without_checkout_binding() {
        let result =
            ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "validated")
                .with_artifact(ActivityArtifact::new(
                harness_workflow::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST,
                json!({"commands": [{"command": "cargo check", "exit_code": 0}]}),
            ));

        assert!(!remote_quality_gate_has_revision_bound_verification(
            &result
        ));
    }
}
