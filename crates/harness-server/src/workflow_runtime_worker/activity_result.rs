use harness_core::types::{Item, ThreadId, TurnId, TurnStatus};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, RuntimeJob,
};
use serde::Serialize;
use serde_json::json;
use std::path::Path;

use super::data_helpers::activity_name;
use super::prompt_packet::workflow_prompt_artifact;

pub(super) fn activity_result_from_turn(
    job: &RuntimeJob,
    status: &TurnStatus,
    items: &[Item],
    thread_id: &ThreadId,
    turn_id: &TurnId,
    agent_name: &str,
    project_root: &Path,
    prompt_packet_digest: &str,
) -> ActivityResult {
    let activity = activity_name(job);
    let summary = last_agent_summary(items).unwrap_or_else(|| match status {
        TurnStatus::Completed => "Agent turn completed.".to_string(),
        TurnStatus::Cancelled => "Agent turn was cancelled.".to_string(),
        TurnStatus::Failed => "Agent turn failed.".to_string(),
        TurnStatus::Running => "Agent turn is still running after lifecycle returned.".to_string(),
    });
    let envelope = activity_result_envelope_from_turn(status, items, &activity, summary);
    match envelope.outcome {
        ActivityResultEnvelopeOutcome::MissingStructuredOutput => {
            tracing::warn!(
                runtime_job_id = %job.id,
                activity = %activity,
                agent = %agent_name,
                "activity completed without harness-activity-result fenced block; \
                 marking failed to prevent silent state-machine no-progress loops"
            );
        }
        ActivityResultEnvelopeOutcome::InvalidStructuredOutput => {
            if let Some(error) = envelope.extraction_error.as_deref() {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    activity = %activity,
                    agent = %agent_name,
                    "activity result block invalid: {error}"
                );
            }
        }
        ActivityResultEnvelopeOutcome::TurnFailed => {
            if let Some(error) = envelope.extraction_error.as_deref() {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    activity = %activity,
                    agent = %agent_name,
                    turn_status = ?status,
                    items = items.len(),
                    "runtime turn failed: {error}"
                );
            }
        }
        ActivityResultEnvelopeOutcome::Accepted | ActivityResultEnvelopeOutcome::TurnCancelled => {}
    }
    let envelope_artifact = envelope.to_artifact();
    let result = envelope.into_final_result();
    result
        .with_artifact(envelope_artifact)
        .with_artifact(workflow_prompt_artifact(prompt_packet_digest))
        .with_artifact(ActivityArtifact::new(
            "runtime_turn",
            json!({
                "thread_id": thread_id.as_str(),
                "turn_id": turn_id.as_str(),
                "agent": agent_name,
                "project_root": project_root.display().to_string(),
            }),
        ))
        .with_signal(ActivitySignal::new(
            "RuntimeTurnCompleted",
            json!({
                "status": status,
                "runtime_job_id": job.id.as_str(),
            }),
        ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ActivityResultExtractionStrategy {
    FencedActivityResult,
    NotAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ActivityResultEnvelopeOutcome {
    Accepted,
    MissingStructuredOutput,
    InvalidStructuredOutput,
    TurnCancelled,
    TurnFailed,
}

#[derive(Debug, Clone, PartialEq)]
struct ActivityResultEnvelope {
    extraction_strategy: ActivityResultExtractionStrategy,
    outcome: ActivityResultEnvelopeOutcome,
    raw_status: TurnStatus,
    extracted_activity: Option<String>,
    extraction_error: Option<String>,
    final_result: ActivityResult,
}

impl ActivityResultEnvelope {
    fn accepted(
        raw_status: TurnStatus,
        extraction_strategy: ActivityResultExtractionStrategy,
        result: ActivityResult,
    ) -> Self {
        Self {
            extraction_strategy,
            outcome: ActivityResultEnvelopeOutcome::Accepted,
            raw_status,
            extracted_activity: Some(result.activity.clone()),
            extraction_error: None,
            final_result: result,
        }
    }

    fn missing_structured_output(
        raw_status: TurnStatus,
        activity: String,
        summary: String,
    ) -> Self {
        let error = "agent emitted no harness-activity-result fenced JSON block".to_string();
        Self {
            extraction_strategy: ActivityResultExtractionStrategy::FencedActivityResult,
            outcome: ActivityResultEnvelopeOutcome::MissingStructuredOutput,
            raw_status,
            extracted_activity: None,
            extraction_error: Some(error.clone()),
            final_result: ActivityResult::failed(activity, summary, error)
                .with_error_kind(ActivityErrorKind::Configuration),
        }
    }

    fn invalid_structured_output(
        raw_status: TurnStatus,
        activity: String,
        error: String,
        extracted_activity: Option<String>,
    ) -> Self {
        Self {
            extraction_strategy: ActivityResultExtractionStrategy::FencedActivityResult,
            outcome: ActivityResultEnvelopeOutcome::InvalidStructuredOutput,
            raw_status,
            extracted_activity,
            extraction_error: Some(error.clone()),
            final_result: ActivityResult::failed(
                activity,
                "Structured activity result was invalid.",
                error,
            )
            .with_error_kind(ActivityErrorKind::Configuration),
        }
    }

    fn cancelled(raw_status: TurnStatus, activity: String, summary: String) -> Self {
        Self {
            extraction_strategy: ActivityResultExtractionStrategy::NotAttempted,
            outcome: ActivityResultEnvelopeOutcome::TurnCancelled,
            raw_status,
            extracted_activity: None,
            extraction_error: None,
            final_result: ActivityResult::cancelled(activity, summary),
        }
    }

    fn failed(raw_status: TurnStatus, activity: String, summary: String, error: String) -> Self {
        let mut result = ActivityResult::failed(activity, summary, error.clone());
        if turn_error_is_timeout(&error) {
            result = result.with_error_kind(ActivityErrorKind::Timeout);
        } else if turn_error_is_non_retryable_agent_limit(&error) {
            result = result.with_error_kind(ActivityErrorKind::Configuration);
        }
        Self {
            extraction_strategy: ActivityResultExtractionStrategy::NotAttempted,
            outcome: ActivityResultEnvelopeOutcome::TurnFailed,
            raw_status,
            extracted_activity: None,
            extraction_error: Some(error),
            final_result: result,
        }
    }

    fn to_artifact(&self) -> ActivityArtifact {
        ActivityArtifact::new(
            "activity_result_envelope",
            json!({
                "schema": "harness.runtime.activity_result_envelope.v1",
                "extraction_strategy": self.extraction_strategy,
                "outcome": self.outcome,
                "raw_status": self.raw_status,
                "extracted_activity": self.extracted_activity,
                "extraction_error": self.extraction_error,
                "final_result": {
                    "activity": self.final_result.activity,
                    "status": self.final_result.status,
                    "error_kind": self.final_result.error_kind,
                }
            }),
        )
    }

    fn into_final_result(self) -> ActivityResult {
        self.final_result
    }
}

fn activity_result_envelope_from_turn(
    status: &TurnStatus,
    items: &[Item],
    activity: &str,
    summary: String,
) -> ActivityResultEnvelope {
    match status {
        TurnStatus::Completed => match structured_activity_result(items, activity) {
            StructuredActivityResult::Parsed(result) => ActivityResultEnvelope::accepted(
                *status,
                ActivityResultExtractionStrategy::FencedActivityResult,
                result,
            ),
            StructuredActivityResult::Missing => ActivityResultEnvelope::missing_structured_output(
                *status,
                activity.to_string(),
                summary,
            ),
            StructuredActivityResult::Invalid {
                error,
                extracted_activity,
            } => ActivityResultEnvelope::invalid_structured_output(
                *status,
                activity.to_string(),
                error,
                extracted_activity,
            ),
        },
        TurnStatus::Cancelled => {
            ActivityResultEnvelope::cancelled(*status, activity.to_string(), summary)
        }
        TurnStatus::Failed | TurnStatus::Running => {
            let error = last_error(items).unwrap_or_else(|| "agent turn failed".to_string());
            ActivityResultEnvelope::failed(*status, activity.to_string(), summary, error)
        }
    }
}

fn turn_error_is_timeout(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("timed out") || normalized.contains("timeout reached")
}

fn turn_error_is_non_retryable_agent_limit(error: &str) -> bool {
    harness_core::error::is_quota_failure_message(error)
        || harness_core::error::is_billing_failure_message(error)
}

enum StructuredActivityResult {
    Missing,
    Parsed(ActivityResult),
    Invalid {
        error: String,
        extracted_activity: Option<String>,
    },
}

fn structured_activity_result(items: &[Item], expected_activity: &str) -> StructuredActivityResult {
    if let Some(block) = latest_activity_result_block(items) {
        return parse_activity_result_block(block, expected_activity);
    }

    StructuredActivityResult::Missing
}

fn parse_activity_result_block(block: &str, expected_activity: &str) -> StructuredActivityResult {
    match parse_activity_result_json(block, expected_activity) {
        Ok(result) => StructuredActivityResult::Parsed(result),
        Err(error) => StructuredActivityResult::Invalid {
            error: error.error,
            extracted_activity: error.extracted_activity,
        },
    }
}

fn parse_activity_result_json(
    block: &str,
    expected_activity: &str,
) -> Result<ActivityResult, StructuredActivityResultError> {
    match serde_json::from_str::<ActivityResult>(block) {
        Ok(result) if result.activity == expected_activity => Ok(result),
        Ok(result) => Err(StructuredActivityResultError {
            error: format!(
                "activity result block reported activity `{}`, expected `{expected_activity}`",
                result.activity
            ),
            extracted_activity: Some(result.activity),
        }),
        Err(error) => Err(StructuredActivityResultError {
            error: format!("activity result block is invalid JSON: {error}"),
            extracted_activity: None,
        }),
    }
}

struct StructuredActivityResultError {
    error: String,
    extracted_activity: Option<String>,
}

fn latest_activity_result_block(items: &[Item]) -> Option<&str> {
    items.iter().rev().find_map(|item| match item {
        Item::AgentReasoning { content } => {
            extract_fenced_block(content, "harness-activity-result")
        }
        _ => None,
    })
}

fn extract_fenced_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let mut result = None;
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n');
    while let Some(line_with_end) = lines.next() {
        let line = line_with_end.trim_end_matches('\n').trim_end_matches('\r');
        if !opening_fence_matches(line, lang) {
            offset += line_with_end.len();
            continue;
        }
        let content_start = offset + line_with_end.len();
        let mut content_end = text.len();
        let mut inner_offset = content_start;
        for inner_line_with_end in lines.by_ref() {
            let inner_line = inner_line_with_end
                .trim_end_matches('\n')
                .trim_end_matches('\r');
            if inner_line.trim().starts_with("```") {
                content_end = inner_offset;
                inner_offset += inner_line_with_end.len();
                break;
            }
            inner_offset += inner_line_with_end.len();
        }
        result = Some(text[content_start..content_end].trim());
        offset = inner_offset;
    }
    result
}

fn opening_fence_matches(line: &str, lang: &str) -> bool {
    let trimmed = line.trim();
    let Some(after_ticks) = trimmed.strip_prefix("```") else {
        return false;
    };
    !after_ticks.starts_with('`') && after_ticks.trim() == lang
}

pub(super) fn last_agent_summary(items: &[Item]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        Item::AgentReasoning { content } if !content.trim().is_empty() => {
            Some(truncate_summary(content.trim()))
        }
        _ => None,
    })
}

pub(super) fn last_error(items: &[Item]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        Item::Error { message, .. } if !message.trim().is_empty() => {
            Some(truncate_summary(message.trim()))
        }
        _ => None,
    })
}

fn truncate_summary(value: &str) -> String {
    const LIMIT: usize = 1200;
    if value.len() <= LIMIT {
        return value.to_string();
    }
    let mut boundary = LIMIT;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}...", &value[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{ActivityStatus, RuntimeKind};

    #[test]
    fn activity_result_from_turn_fails_when_no_fenced_block_present() {
        // P0-1: a completed agent turn that emits no `harness-activity-result`
        // fenced block must NOT be silently treated as success. Returning
        // succeeded here historically caused state-machine no-progress loops
        // for `repo_backlog` workflows (claude returns prose, reducer falls
        // back to `finish_repo_backlog_scan`, state cycles back to idle, next
        // tick re-dispatches, ad infinitum).
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::ClaudeCode,
            "claude-default",
            json!({
                "activity": "poll_repo_backlog"
            }),
        );
        let items = vec![Item::AgentReasoning {
            content: "I scanned the repo and saw no new issues. Done.".to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "claude",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "poll_repo_backlog");
        assert_eq!(
            result.status,
            ActivityStatus::Failed,
            "missing structured result MUST surface as failed"
        );
        assert_eq!(
            result.error_kind,
            Some(ActivityErrorKind::Configuration),
            "missing structured result is a configuration-class failure (prompt or agent contract issue)"
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|e| e.contains("harness-activity-result")),
            "error message MUST mention the missing block so operators can diagnose"
        );
        let envelope = envelope_artifact(&result);
        assert_eq!(envelope["outcome"], "missing_structured_output");
        assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
        assert_eq!(envelope["raw_status"], "completed");
    }

    #[test]
    fn activity_result_from_turn_parses_structured_activity_result_block() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let items = vec![Item::AgentReasoning {
            content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#
                .to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert_eq!(result.summary, "Implementation completed.");
        let pr_artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "pull_request")
            .expect("structured pull request artifact should be preserved");
        assert_eq!(pr_artifact.artifact["pr_number"], 77);
        assert_eq!(
            pr_artifact.artifact["pr_url"],
            "https://github.com/owner/repo/pull/77"
        );
        let prompt_artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "runtime_prompt_packet")
            .expect("runtime prompt artifact should be appended");
        assert_eq!(prompt_artifact.artifact["digest"], "digest-1");
        let turn_artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "runtime_turn")
            .expect("runtime turn artifact should be appended");
        assert_eq!(turn_artifact.artifact["thread_id"], "thread-1");
        assert_eq!(turn_artifact.artifact["turn_id"], "turn-1");
        let envelope = envelope_artifact(&result);
        assert_eq!(envelope["outcome"], "accepted");
        assert_eq!(envelope["extracted_activity"], "implement_issue");
        assert_eq!(envelope["final_result"]["status"], "succeeded");
        assert!(result
            .signals
            .iter()
            .any(|signal| signal.signal_type == "RuntimeTurnCompleted"));
    }

    #[test]
    fn activity_result_from_turn_rejects_generic_json_activity_result_block() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let items = vec![Item::AgentReasoning {
            content: r#"Work completed.

```json
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed from generic JSON.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":88,"pr_url":"https://github.com/owner/repo/pull/88"}}]}
```"#
                .to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(
            result.error_kind,
            Some(ActivityErrorKind::Configuration),
            "generic JSON fences must not satisfy the final output contract"
        );
        assert!(!result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        let envelope = envelope_artifact(&result);
        assert_eq!(envelope["outcome"], "missing_structured_output");
        assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
        assert!(envelope["extracted_activity"].is_null());
        assert!(envelope["extraction_error"]
            .as_str()
            .is_some_and(|error| error.contains("harness-activity-result")));
    }

    #[test]
    fn activity_result_from_turn_does_not_repair_ambiguous_json_block() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let items = vec![Item::AgentReasoning {
            content: r#"Work completed.

```json
{"summary":"Done, but this is not an ActivityResult."}
```"#
                .to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        let envelope = envelope_artifact(&result);
        assert_eq!(envelope["outcome"], "missing_structured_output");
        assert_eq!(envelope["extraction_strategy"], "fenced_activity_result");
    }

    #[test]
    fn activity_result_from_turn_fails_mismatched_structured_activity() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let content = r#"Wrong activity result.

```harness-activity-result
{"activity":"replan_issue","status":"succeeded","summary":"Wrong activity.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#;
        let items = vec![Item::AgentReasoning {
            content: content.to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.summary, "Structured activity result was invalid.");
        assert_eq!(
            result.error.as_deref(),
            Some("activity result block reported activity `replan_issue`, expected `implement_issue`")
        );
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        assert!(!result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        let envelope = envelope_artifact(&result);
        assert_eq!(envelope["outcome"], "invalid_structured_output");
        assert_eq!(envelope["extracted_activity"], "replan_issue");
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_turn"));
    }

    #[test]
    fn activity_result_from_turn_fails_latest_malformed_structured_activity() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let items = vec![Item::AgentReasoning {
            content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":
```"#
                .to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.summary, "Structured activity result was invalid.");
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("activity result block is invalid JSON:")));
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        assert!(!result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        let envelope = envelope_artifact(&result);
        assert_eq!(envelope["outcome"], "invalid_structured_output");
        assert!(envelope["extracted_activity"].is_null());
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_turn"));
    }

    #[test]
    fn activity_result_from_turn_classifies_timeout_error_kind() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let items = vec![Item::Error {
            code: 1,
            message: "Agent turn timed out after 30s".to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Failed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Timeout));
        assert_eq!(
            result.error.as_deref(),
            Some("Agent turn timed out after 30s")
        );
        let envelope = envelope_artifact(&result);
        assert_eq!(envelope["outcome"], "turn_failed");
        assert_eq!(envelope["extraction_strategy"], "not_attempted");
    }

    fn envelope_artifact(result: &ActivityResult) -> &serde_json::Value {
        &result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "activity_result_envelope")
            .expect("activity result envelope artifact should be appended")
            .artifact
    }
}

#[cfg(test)]
#[path = "activity_result_limit_tests.rs"]
mod limit_tests;
