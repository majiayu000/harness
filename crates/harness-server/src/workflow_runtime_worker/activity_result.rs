use harness_core::types::{Item, ThreadId, TurnId, TurnStatus};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, RuntimeJob,
};
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
    let result = match status {
        TurnStatus::Completed => match structured_activity_result(items, &activity) {
            StructuredActivityResult::Parsed(result) => result,
            StructuredActivityResult::Missing => {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    activity = %activity,
                    agent = %agent_name,
                    "activity completed without harness-activity-result fenced block; \
                     marking failed to prevent silent state-machine no-progress loops"
                );
                ActivityResult::failed(
                    activity,
                    summary,
                    "agent emitted no harness-activity-result fenced JSON block",
                )
                .with_error_kind(ActivityErrorKind::Configuration)
            }
            StructuredActivityResult::Invalid(error) => {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    activity = %activity,
                    agent = %agent_name,
                    "activity result block invalid: {error}"
                );
                ActivityResult::failed(activity, "Structured activity result was invalid.", error)
            }
        },
        TurnStatus::Cancelled => ActivityResult::cancelled(activity, summary),
        TurnStatus::Failed | TurnStatus::Running => {
            let error = last_error(items).unwrap_or_else(|| "agent turn failed".to_string());
            tracing::warn!(
                runtime_job_id = %job.id,
                activity = %activity,
                agent = %agent_name,
                turn_status = ?status,
                items = items.len(),
                "runtime turn failed: {error}"
            );
            let mut result = ActivityResult::failed(activity, summary, error.clone());
            if turn_error_is_timeout(&error) {
                result = result.with_error_kind(ActivityErrorKind::Timeout);
            }
            result
        }
    };
    result
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

fn turn_error_is_timeout(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("timed out") || normalized.contains("timeout reached")
}

enum StructuredActivityResult {
    Missing,
    Parsed(ActivityResult),
    Invalid(String),
}

fn structured_activity_result(items: &[Item], expected_activity: &str) -> StructuredActivityResult {
    let Some(block) = latest_activity_result_block(items) else {
        return StructuredActivityResult::Missing;
    };

    match serde_json::from_str::<ActivityResult>(block) {
        Ok(result) if result.activity == expected_activity => {
            StructuredActivityResult::Parsed(result)
        }
        Ok(result) => StructuredActivityResult::Invalid(format!(
            "activity result block reported activity `{}`, expected `{expected_activity}`",
            result.activity
        )),
        Err(error) => StructuredActivityResult::Invalid(format!(
            "activity result block is invalid JSON: {error}"
        )),
    }
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
        assert!(result
            .signals
            .iter()
            .any(|signal| signal.signal_type == "RuntimeTurnCompleted"));
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
        assert!(!result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
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
        assert!(!result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
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
    }
}
