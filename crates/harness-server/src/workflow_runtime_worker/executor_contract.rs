use super::data_helpers::activity_name;
use super::executor::{is_internal_non_agent_activity, ServerRuntimeJobExecutor};
use super::prompt_packet::PromptPacketConfigurationError;
use super::transcript_durability::{
    exact_replay_preflight_result, hydrate_exact_replay_transcript,
    strip_transcript_unavailable_signal,
};
use async_trait::async_trait;
use harness_workflow::runtime::{
    ActivityErrorKind, ActivityResult, RuntimeJob, RuntimeJobExecutor,
};

#[async_trait]
impl RuntimeJobExecutor for ServerRuntimeJobExecutor<'_> {
    fn consumes_runtime_turn(&self, job: &RuntimeJob) -> bool {
        !is_internal_non_agent_activity(job)
    }

    async fn preflight_result(&self, job: &RuntimeJob) -> Option<ActivityResult> {
        // Internal server-owned activities do not run a user agent. They must keep
        // flowing even when the runtime worker is disabled, otherwise disabling the
        // worker would strand workflows or prevent server-owned PR snapshots.
        if is_internal_non_agent_activity(job) {
            return None;
        }
        if let Some(result) = exact_replay_preflight_result(self.state, job).await {
            return Some(result);
        }
        self.runtime_worker_disabled_result(job).await
    }

    async fn execute(&self, mut job: RuntimeJob) -> ActivityResult {
        if let Err(result) = hydrate_exact_replay_transcript(self.state, &mut job).await {
            return *result;
        }
        let activity = activity_name(&job);
        match self.execute_inner(job).await {
            Ok(result) => postprocess_local_execution_result(result),
            Err(error) => execution_error_result(activity, error),
        }
    }

    async fn cancel_execution(&self, _job: &RuntimeJob) {
        // The turn loop watches this notification and interrupts the agent,
        // which terminates the child process and lets the workspace cleanup
        // run before execute returns (GH-1877).
        self.cancel_lease_lost();
    }
}

fn postprocess_local_execution_result(result: ActivityResult) -> ActivityResult {
    strip_transcript_unavailable_signal(result)
}

fn execution_error_result(activity: String, error: anyhow::Error) -> ActivityResult {
    let result = ActivityResult::failed(
        activity,
        "Runtime job execution failed before the agent completed.",
        error.to_string(),
    );
    if error
        .downcast_ref::<super::agent_contract_job::AgentContractExecutionError>()
        .is_some()
    {
        result.with_error_kind(ActivityErrorKind::Fatal)
    } else if error
        .downcast_ref::<PromptPacketConfigurationError>()
        .is_some()
    {
        result.with_error_kind(ActivityErrorKind::Configuration)
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_packet_provenance_errors_are_configuration_failures() {
        let error = anyhow::Error::new(PromptPacketConfigurationError::new(
            "unclassified workflow.data field `/summary`",
        ));

        let result = execution_error_result("implement_issue".to_string(), error);

        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unclassified workflow.data")));
    }

    #[test]
    fn agent_contract_infrastructure_errors_are_fatal() {
        let job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexExec,
            "codex-contract",
            serde_json::json!({
                "activity": "classify_scope",
                "command": {"agent_contract": null}
            }),
        );
        let error = super::super::agent_contract_job::pinned_agent_contract_for_execution(&job)
            .expect_err("malformed present contract must fail extraction");

        let result = execution_error_result("classify_scope".to_string(), error);

        assert_eq!(result.error_kind, Some(ActivityErrorKind::Fatal));
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unparseable agent_contract payload")));
    }

    #[test]
    fn local_executor_retains_server_attached_completion_evidence() {
        let result = ActivityResult::succeeded("merge_pr", "verified merge")
            .with_artifact(harness_workflow::runtime::ActivityArtifact::new(
            harness_workflow::runtime::completion_evidence::ARTIFACT_MERGE_COMPLETION_VERIFICATION,
            serde_json::json!({"verified": true, "observed_merged": true}),
        ));

        let result = postprocess_local_execution_result(result);

        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(
            result.artifacts[0].artifact_type,
            harness_workflow::runtime::completion_evidence::ARTIFACT_MERGE_COMPLETION_VERIFICATION
        );
    }
}
