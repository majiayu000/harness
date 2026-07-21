use super::data_helpers::activity_name;
use super::executor::{is_internal_non_agent_activity, ServerRuntimeJobExecutor};
use super::transcript_durability::{
    exact_replay_preflight_result, hydrate_exact_replay_transcript,
    strip_caller_transcript_unavailable_signal,
};
use async_trait::async_trait;
use harness_workflow::runtime::{ActivityResult, RuntimeJob, RuntimeJobExecutor};

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
            return result;
        }
        let activity = activity_name(&job);
        match self.execute_inner(job).await {
            Ok(result) => strip_caller_transcript_unavailable_signal(result),
            Err(error) => ActivityResult::failed(
                activity,
                "Runtime job execution failed before the agent completed.",
                error.to_string(),
            ),
        }
    }
}
