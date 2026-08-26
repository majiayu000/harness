use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus,
};
use serde_json::json;

pub(super) fn combine(
    activity_result: anyhow::Result<ActivityResult>,
    finish_result: anyhow::Result<()>,
) -> anyhow::Result<ActivityResult> {
    match (activity_result, finish_result) {
        (Ok(result), Err(error)) => {
            if result.status == ActivityStatus::Succeeded {
                Ok(failed_result(result, &error))
            } else {
                Ok(result.with_artifact(warning_artifact(&error)))
            }
        }
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Err(finish_error)) => Err(error.context(format!(
            "runtime workspace finalization also failed: {finish_error}"
        ))),
        (Err(error), Ok(())) => Err(error),
    }
}

fn failed_result(mut result: ActivityResult, error: &anyhow::Error) -> ActivityResult {
    result.status = ActivityStatus::Failed;
    result.summary = "Runtime workspace finalization failed after the activity completed.".into();
    result.error = Some(format!("runtime workspace finalization failed: {error}"));
    result.error_kind = Some(ActivityErrorKind::Retryable);
    result.with_artifact(warning_artifact(error))
}

fn warning_artifact(error: &anyhow::Error) -> ActivityArtifact {
    ActivityArtifact::new(
        "runtime_workspace_finalization_warning",
        json!({ "error": error.to_string() }),
    )
}
