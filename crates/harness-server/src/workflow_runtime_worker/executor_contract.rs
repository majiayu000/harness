use super::child_workflow::execute_start_child_workflow;
use super::data_helpers::activity_name;
use super::executor::{is_internal_non_agent_activity, ServerRuntimeJobExecutor};
use super::pr_feedback_inspection::execute_pr_feedback_inspection;
use super::prompt_packet::PromptPacketConfigurationError;
use super::server_merge::{execute_server_merge, server_merge_execution_enabled};
use super::transcript_durability::{
    exact_replay_preflight_result, hydrate_exact_replay_transcript,
    strip_transcript_unavailable_signal,
};
use async_trait::async_trait;
use harness_core::config::workflow::WorkflowClassifierPolicy;
use harness_core::types::Item;
use harness_workflow::runtime::{
    stable_remote_fact_hash, ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus,
    RuntimeJob, RuntimeJobExecutor, RuntimeKind, CLASSIFIER_ASSESSMENT_ARTIFACT,
    CLASSIFIER_ASSESSMENT_SCHEMA, CLASSIFIER_JOB_SCHEMA, CLASSIFIER_OUTPUT_ARTIFACT,
    CLASSIFIER_OUTPUT_SCHEMA,
};
use serde::Deserialize;
use serde_json::{json, Value};

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

impl ServerRuntimeJobExecutor<'_> {
    pub(super) async fn execute_server_owned_activity(
        &self,
        job: &RuntimeJob,
        parent: Option<&harness_workflow::runtime::WorkflowInstance>,
    ) -> anyhow::Result<Option<ActivityResult>> {
        match activity_name(job).as_str() {
            "start_child_workflow" => Ok(Some(
                execute_start_child_workflow(self.state, job, parent).await?,
            )),
            activity if activity == harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY => Ok(
                Some(execute_pr_feedback_inspection(self.state, job, parent).await),
            ),
            "merge_pr" if server_merge_execution_enabled(self.state, job, parent) => {
                Ok(Some(execute_server_merge(self.state, job, parent).await))
            }
            _ => Ok(None),
        }
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
        .downcast_ref::<PromptPacketConfigurationError>()
        .is_some()
    {
        result.with_error_kind(ActivityErrorKind::Configuration)
    } else {
        result
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifierOutput {
    schema: String,
    verdict: String,
    rationale: String,
    #[serde(default)]
    evidence_refs: Vec<String>,
}

pub(super) fn classifier_policy_for_job(
    job: &RuntimeJob,
) -> anyhow::Result<Option<WorkflowClassifierPolicy>> {
    let Some(snapshot) = job.input.get("classifier").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if snapshot.get("schema").and_then(Value::as_str) != Some(CLASSIFIER_JOB_SCHEMA) {
        anyhow::bail!(
            "runtime job {} has an invalid classifier snapshot schema",
            job.id
        );
    }
    if snapshot.get("activity").and_then(Value::as_str)
        != job.input.get("activity").and_then(Value::as_str)
    {
        anyhow::bail!(
            "runtime job {} classifier activity does not match job activity",
            job.id
        );
    }
    let policy_value = snapshot.get("policy").ok_or_else(|| {
        anyhow::anyhow!(
            "runtime job {} classifier snapshot is missing policy",
            job.id
        )
    })?;
    let expected_hash = stable_remote_fact_hash(policy_value);
    if snapshot.get("policy_sha256").and_then(Value::as_str) != Some(expected_hash.as_str()) {
        anyhow::bail!(
            "runtime job {} classifier policy digest does not match its snapshot",
            job.id
        );
    }
    let policy: WorkflowClassifierPolicy = serde_json::from_value(policy_value.clone())?;
    policy.validate(
        job.input
            .get("activity")
            .and_then(Value::as_str)
            .unwrap_or("classifier"),
    )?;
    Ok(Some(policy))
}

pub(super) fn validate_classifier_runtime_kind(kind: RuntimeKind) -> anyhow::Result<()> {
    match kind {
        RuntimeKind::CodexExec
        | RuntimeKind::CodexJsonrpc
        | RuntimeKind::ClaudeCode
        | RuntimeKind::AnthropicApi => Ok(()),
        RuntimeKind::OpenCode | RuntimeKind::RemoteHost => anyhow::bail!(
            "runtime kind '{}' cannot attest classifier isolation and model selection",
            kind.as_str()
        ),
    }
}

pub(super) fn classifier_turn_used_tools(items: &[Item]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            Item::ShellCommand { .. }
                | Item::FileEdit { .. }
                | Item::FileRead { .. }
                | Item::ToolCall { .. }
                | Item::ApprovalRequest { .. }
        )
    })
}

pub(super) fn attest_classifier_result(
    policy: &WorkflowClassifierPolicy,
    job: &RuntimeJob,
    requested_model: &str,
    reported_models: &[String],
    tool_use_detected: bool,
    prompt_packet: &Value,
    prompt_packet_digest: &str,
    mut result: ActivityResult,
) -> ActivityResult {
    result.signals.clear();
    result
        .artifacts
        .retain(|artifact| artifact.artifact_type != CLASSIFIER_ASSESSMENT_ARTIFACT);
    if tool_use_detected {
        return rejected_classifier_result(
            result,
            "Classifier attempted to use a tool.",
            "classifier turns must use only the supplied classifier input",
        );
    }
    let (executed_model, model_identity_source) = match job.runtime_kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => {
            (requested_model, "codex_cli_launch_argument")
        }
        RuntimeKind::ClaudeCode | RuntimeKind::AnthropicApi => {
            let Some(reported_model) = reported_models.last() else {
                return rejected_classifier_result(
                    result,
                    "Classifier model identity was not reported by the backend.",
                    "provider-reported model identity is required for this runtime",
                );
            };
            if reported_models.iter().any(|model| model != requested_model) {
                return rejected_classifier_result(
                    result,
                    "Classifier model identity did not match the requested model.",
                    &format!("requested '{requested_model}', backend reported {reported_models:?}"),
                );
            }
            (reported_model.as_str(), "provider_reported")
        }
        RuntimeKind::OpenCode | RuntimeKind::RemoteHost => {
            return rejected_classifier_result(
                result,
                "Classifier runtime cannot attest model selection.",
                job.runtime_kind.as_str(),
            )
        }
    };
    if result.status != ActivityStatus::Succeeded {
        result
            .artifacts
            .retain(|artifact| artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT);
        return result;
    }
    let outputs = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == CLASSIFIER_OUTPUT_ARTIFACT)
        .collect::<Vec<_>>();
    let output = match outputs.as_slice() {
        [artifact] => serde_json::from_value::<ClassifierOutput>(artifact.artifact.clone())
            .map_err(|error| format!("classifier_output is malformed: {error}")),
        [] => Err("classifier_output artifact is missing".to_string()),
        _ => Err("multiple classifier_output artifacts were returned".to_string()),
    }
    .and_then(|output| validate_classifier_output(policy, prompt_packet, output));
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return rejected_classifier_result(
                result,
                "Classifier output failed server validation.",
                &error,
            )
        }
    };
    result
        .artifacts
        .retain(|artifact| artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT);
    let subject = prompt_packet
        .pointer("/classifier_input/subject")
        .cloned()
        .unwrap_or(Value::Null);
    let activity = result.activity.clone();
    result.with_artifact(ActivityArtifact::new(
        CLASSIFIER_ASSESSMENT_ARTIFACT,
        json!({
            "schema": CLASSIFIER_ASSESSMENT_SCHEMA,
            "activity": activity,
            "subject": subject,
            "verdict": output.verdict,
            "rationale": output.rationale,
            "evidence_refs": output.evidence_refs,
            "policy_sha256": job.input.pointer("/classifier/policy_sha256"),
            "prompt_packet_sha256": prompt_packet_digest,
            "runtime_job_id": job.id,
            "runtime_profile": job.runtime_profile,
            "requested_model": requested_model,
            "executed_model": executed_model,
            "model_identity_source": model_identity_source,
            "tool_use_detected": false,
            "workspace_isolation": "ephemeral_empty_read_only",
        }),
    ))
}

fn validate_classifier_output(
    policy: &WorkflowClassifierPolicy,
    prompt_packet: &Value,
    mut output: ClassifierOutput,
) -> Result<ClassifierOutput, String> {
    if output.schema != CLASSIFIER_OUTPUT_SCHEMA {
        return Err(format!(
            "classifier output schema must be '{CLASSIFIER_OUTPUT_SCHEMA}'"
        ));
    }
    output.verdict = output.verdict.trim().to_string();
    output.rationale = output.rationale.trim().to_string();
    if !policy
        .verdicts
        .iter()
        .any(|verdict| verdict == &output.verdict)
    {
        return Err(format!(
            "classifier verdict '{}' is not declared by policy",
            output.verdict
        ));
    }
    if output.rationale.is_empty() {
        return Err("classifier rationale must not be empty".to_string());
    }
    for evidence_ref in &mut output.evidence_refs {
        *evidence_ref = evidence_ref.trim().to_string();
        if !evidence_ref.starts_with("/classifier_input/")
            || prompt_packet.pointer(evidence_ref).is_none()
        {
            return Err(format!(
                "classifier evidence_ref '{evidence_ref}' is not a valid classifier-input JSON pointer"
            ));
        }
    }
    Ok(output)
}

fn rejected_classifier_result(
    mut result: ActivityResult,
    summary: &str,
    error: &str,
) -> ActivityResult {
    result.artifacts.retain(|artifact| {
        artifact.artifact_type != CLASSIFIER_OUTPUT_ARTIFACT
            && artifact.artifact_type != CLASSIFIER_ASSESSMENT_ARTIFACT
    });
    ActivityResult::failed(&result.activity, summary, error)
        .with_error_kind(ActivityErrorKind::Fatal)
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

    fn classifier_policy() -> WorkflowClassifierPolicy {
        WorkflowClassifierPolicy {
            verdicts: vec!["allow".to_string(), "needs_human".to_string()],
            instructions: vec!["Judge only supplied facts.".to_string()],
        }
    }

    #[test]
    fn classifier_output_becomes_one_server_assessment() -> anyhow::Result<()> {
        let policy = classifier_policy();
        let policy_value = serde_json::to_value(&policy)?;
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::AnthropicApi,
            "classifier",
            json!({
                "classifier": {"policy_sha256": stable_remote_fact_hash(&policy_value)}
            }),
        );
        let result = ActivityResult::succeeded("classify", "classified").with_artifact(
            ActivityArtifact::new(
                CLASSIFIER_OUTPUT_ARTIFACT,
                json!({
                    "schema": CLASSIFIER_OUTPUT_SCHEMA,
                    "verdict": "allow",
                    "rationale": "Facts match.",
                    "evidence_refs": ["/classifier_input/facts/example"]
                }),
            ),
        );

        let result = attest_classifier_result(
            &policy,
            &job,
            "claude-test",
            &["claude-test".to_string()],
            false,
            &json!({
                "classifier_input": {
                    "subject": {"kind": "test", "identity": "1"},
                    "facts": {"example": true}
                }
            }),
            "sha256:prompt",
            result,
        );

        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert!(result.signals.is_empty());
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(
            result.artifacts[0].artifact_type,
            CLASSIFIER_ASSESSMENT_ARTIFACT
        );
        assert_eq!(result.artifacts[0].artifact["verdict"], "allow");
        assert_eq!(
            result.artifacts[0].artifact["model_identity_source"],
            "provider_reported"
        );
        Ok(())
    }

    #[test]
    fn codex_classifier_attests_the_explicit_cli_model_argument() -> anyhow::Result<()> {
        let policy = classifier_policy();
        let policy_value = serde_json::to_value(&policy)?;
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexExec,
            "classifier",
            json!({
                "classifier": {"policy_sha256": stable_remote_fact_hash(&policy_value)}
            }),
        );
        let result = attest_classifier_result(
            &policy,
            &job,
            "gpt-5.6-sol",
            &[],
            false,
            &json!({
                "classifier_input": {
                    "subject": {"kind": "test", "identity": "1"},
                    "facts": {"example": true}
                }
            }),
            "sha256:prompt",
            ActivityResult::succeeded("classify", "classified").with_artifact(
                ActivityArtifact::new(
                    CLASSIFIER_OUTPUT_ARTIFACT,
                    json!({
                        "schema": CLASSIFIER_OUTPUT_SCHEMA,
                        "verdict": "allow",
                        "rationale": "Facts match.",
                        "evidence_refs": ["/classifier_input/facts/example"]
                    }),
                ),
            ),
        );

        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert_eq!(
            result.artifacts[0].artifact["executed_model"],
            "gpt-5.6-sol"
        );
        assert_eq!(
            result.artifacts[0].artifact["model_identity_source"],
            "codex_cli_launch_argument"
        );
        Ok(())
    }

    #[test]
    fn classifier_missing_model_identity_fails_closed() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::AnthropicApi,
            "classifier",
            json!({}),
        );
        let result = attest_classifier_result(
            &classifier_policy(),
            &job,
            "claude-test",
            &[],
            false,
            &json!({}),
            "sha256:prompt",
            ActivityResult::succeeded("classify", "classified"),
        );

        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Fatal));
        assert!(result.artifacts.is_empty());
    }
}
