use harness_workflow::runtime::{ActivityArtifact, ActivityResult};
use serde_json::json;

pub(super) fn combine_activity_result_with_runtime_workspace_finalization(
    activity_result: anyhow::Result<ActivityResult>,
    finish_result: anyhow::Result<()>,
) -> anyhow::Result<ActivityResult> {
    match (activity_result, finish_result) {
        (Ok(result), Err(error)) => {
            if result.status == harness_workflow::runtime::ActivityStatus::Succeeded {
                Ok(activity_result_failed_by_runtime_workspace_finalization(
                    result, &error,
                ))
            } else {
                Ok(result.with_artifact(runtime_workspace_finalization_warning_artifact(&error)))
            }
        }
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Err(finish_error)) => Err(error.context(format!(
            "runtime workspace finalization also failed: {finish_error}"
        ))),
        (Err(error), Ok(())) => Err(error),
    }
}

fn activity_result_failed_by_runtime_workspace_finalization(
    mut result: ActivityResult,
    error: &anyhow::Error,
) -> ActivityResult {
    result.status = harness_workflow::runtime::ActivityStatus::Failed;
    result.summary =
        "Runtime workspace finalization failed after the activity completed.".to_string();
    result.error = Some(format!("runtime workspace finalization failed: {error}"));
    result.error_kind = Some(harness_workflow::runtime::ActivityErrorKind::Retryable);
    result.with_artifact(runtime_workspace_finalization_warning_artifact(error))
}

fn runtime_workspace_finalization_warning_artifact(error: &anyhow::Error) -> ActivityArtifact {
    ActivityArtifact::new(
        "runtime_workspace_finalization_warning",
        json!({ "error": error.to_string() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_workspace_finalization_failure_marks_activity_failed() {
        let result =
            ActivityResult::succeeded("implement_issue", "Created a pull request.").with_artifact(
                ActivityArtifact::new("pull_request", json!({ "pr_number": 42 })),
            );
        let result = combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        )
        .expect("finalization failure should be returned as a failed activity result");
        assert_eq!(result.activity, "implement_issue");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Retryable)
        );
        assert!(result
            .summary
            .contains("Runtime workspace finalization failed"));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("after_run hook failed"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "runtime_workspace_finalization_warning"
                && artifact.artifact["error"] == "after_run hook failed"
        }));
    }

    #[test]
    fn runtime_workspace_finalization_failure_preserves_failed_activity_result() {
        let result = ActivityResult::failed(
            "address_pr_feedback",
            "Structured output was invalid.",
            "fatal",
        )
        .with_error_kind(harness_workflow::runtime::ActivityErrorKind::Fatal)
        .with_artifact(ActivityArtifact::new(
            "activity_result_parse_error",
            json!({ "field": "status" }),
        ));
        let result = combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        )
        .expect("failed activity result should be preserved");
        assert_eq!(result.activity, "address_pr_feedback");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(result.summary, "Structured output was invalid.");
        assert_eq!(result.error.as_deref(), Some("fatal"));
        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Fatal)
        );
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "activity_result_parse_error"));
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "runtime_workspace_finalization_warning"
                && artifact.artifact["error"] == "after_run hook failed"
        }));
    }
}
