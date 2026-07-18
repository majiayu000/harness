use crate::http::AppState;
use harness_core::types::{Item, ThreadId, Turn, TurnId};
use harness_workflow::runtime::{ActivityResult, ActivityStatus};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

pub(super) fn ensure_review_queue_limit(state: &Arc<AppState>, project_root: &std::path::Path) {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    state
        .concurrency
        .review_task_queue
        .set_project_limit(&canonical, 1);
}

/// Maximum number of violations inlined into the prompt.
///
/// The full list may contain hundreds of entries; inlining all of them can
/// exceed OS ARG_MAX or the model's context window before the agent even
/// starts.  Only the first N are embedded; the total count is always reported
/// so the agent knows additional findings exist.
pub(super) const MAX_INLINE_VIOLATIONS: usize = 20;

pub(super) fn format_violations_for_prompt(
    violations: &[harness_core::types::Violation],
) -> String {
    if violations.is_empty() {
        return "No violations found.".to_string();
    }
    let total = violations.len();
    let shown = violations.len().min(MAX_INLINE_VIOLATIONS);
    let lines: Vec<String> = violations[..shown]
        .iter()
        .map(|v| {
            let loc = match v.line {
                Some(l) => format!("{}:{l}", v.file.display()),
                None => v.file.display().to_string(),
            };
            format!("[{:?}] {}: {} ({})", v.severity, v.rule_id, v.message, loc)
        })
        .collect();
    let mut out = format!(
        "{total} violation(s) (showing {shown}):\n{}",
        lines.join("\n")
    );
    if total > shown {
        out.push_str(&format!(
            "\n... and {} more violation(s) not shown. Run guard scripts locally for the full list.",
            total - shown
        ));
    }
    out
}

pub(super) fn pick_secondary_review_agent<F>(
    primary_agent: &str,
    candidates: &[String],
    mut is_available: F,
) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    candidates
        .iter()
        .find(|agent| agent.as_str() != primary_agent && is_available(agent.as_str()))
        .cloned()
}

pub(super) enum TerminalResultDisposition {
    ReadRuntimeTurn,
    Return(String),
    Ignore,
    Failed(String),
}

pub(super) fn terminal_result_disposition(result: &ActivityResult) -> TerminalResultDisposition {
    match result.status {
        ActivityStatus::Succeeded => TerminalResultDisposition::ReadRuntimeTurn,
        ActivityStatus::Cancelled => TerminalResultDisposition::Ignore,
        ActivityStatus::Failed | ActivityStatus::Blocked
            if result.summary.trim() == "REVIEW_SKIPPED" =>
        {
            TerminalResultDisposition::Return("REVIEW_SKIPPED".to_string())
        }
        ActivityStatus::Failed | ActivityStatus::Blocked => TerminalResultDisposition::Failed(
            result
                .error
                .clone()
                .unwrap_or_else(|| result.summary.clone()),
        ),
    }
}

pub(super) fn runtime_turn_ref(result: &ActivityResult) -> Option<(ThreadId, TurnId)> {
    let artifact = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "runtime_turn")?;
    let thread_id = artifact.artifact.get("thread_id")?.as_str()?.trim();
    let turn_id = artifact.artifact.get("turn_id")?.as_str()?.trim();
    if thread_id.is_empty() || turn_id.is_empty() {
        return None;
    }
    Some((ThreadId::from_str(thread_id), TurnId::from_str(turn_id)))
}

pub(super) fn review_output_from_turn(turn: &Turn) -> Option<String> {
    let output = turn
        .items
        .iter()
        .filter_map(|item| match item {
            Item::AgentReasoning { content } if !content.trim().is_empty() => {
                Some(content.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!output.is_empty()).then_some(output)
}

/// Poll a workflow-runtime submission until its activity completes, then read
/// the full agent output from the live runtime turn referenced by the durable
/// completion event.
pub(super) async fn poll_task_output(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
    timeout_secs: u64,
) -> Option<String> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        tracing::error!(task_id = %task_id, "poll_task_output: workflow runtime store unavailable");
        return None;
    };
    let poll_interval = Duration::from_secs(15);
    let max_wait = if timeout_secs == 0 {
        Duration::from_secs(999_999)
    } else {
        Duration::from_secs(timeout_secs + 120)
    };
    let start = tokio::time::Instant::now();
    loop {
        sleep(poll_interval).await;
        if start.elapsed() > max_wait {
            tracing::warn!(task_id = %task_id, "poll_task_output: timed out");
            return None;
        }
        let workflow = match store.get_instance_by_submission_id(task_id.as_str()).await {
            Ok(Some(workflow)) => workflow,
            Ok(None) => continue,
            Err(error) => {
                tracing::error!(task_id = %task_id, "poll_task_output: workflow lookup failed: {error}");
                return None;
            }
        };
        let event = match store
            .latest_event_for_type(&workflow.id, "RuntimeJobCompleted")
            .await
        {
            Ok(Some(event)) => event,
            Ok(None) if workflow.is_terminal() => {
                tracing::error!(
                    task_id = %task_id,
                    workflow_id = %workflow.id,
                    "poll_task_output: terminal workflow has no runtime completion event"
                );
                return None;
            }
            Ok(None) => continue,
            Err(error) => {
                tracing::error!(
                    task_id = %task_id,
                    workflow_id = %workflow.id,
                    "poll_task_output: completion event lookup failed: {error}"
                );
                return None;
            }
        };
        let result: ActivityResult = match event.event.get("activity_result").cloned() {
            Some(value) => match serde_json::from_value(value) {
                Ok(result) => result,
                Err(error) => {
                    tracing::error!(task_id = %task_id, "poll_task_output: invalid activity result: {error}");
                    return None;
                }
            },
            None => {
                tracing::error!(task_id = %task_id, "poll_task_output: completion event has no activity result");
                return None;
            }
        };
        match terminal_result_disposition(&result) {
            TerminalResultDisposition::Return(output) => return Some(output),
            TerminalResultDisposition::Ignore => return None,
            TerminalResultDisposition::Failed(error) => {
                tracing::error!(task_id = %task_id, "poll_task_output: runtime activity failed: {error}");
                return None;
            }
            TerminalResultDisposition::ReadRuntimeTurn => {}
        }
        let Some((thread_id, turn_id)) = runtime_turn_ref(&result) else {
            tracing::error!(task_id = %task_id, "poll_task_output: activity result has no runtime turn reference");
            return None;
        };
        let Some(turn) = state
            .core
            .server
            .thread_manager
            .get_turn(&thread_id, &turn_id)
        else {
            tracing::error!(
                task_id = %task_id,
                thread_id = %thread_id,
                turn_id = %turn_id,
                "poll_task_output: referenced runtime turn is unavailable"
            );
            return None;
        };
        let Some(output) = review_output_from_turn(&turn) else {
            tracing::warn!(task_id = %task_id, "poll_task_output: completed but no agent output");
            return None;
        };
        return Some(output);
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewOutput {
    pub(super) findings: Vec<ReviewFinding>,
    pub(super) summary: ReviewSummary,
}

/// Validation-only representation of the finding schema required by the
/// periodic-review prompt. The fields stay private because the persistence and
/// auto-fix consumers were removed with the review store.
#[derive(Debug, Deserialize)]
pub(super) struct ReviewFinding {
    #[serde(rename = "id")]
    _id: String,
    #[serde(rename = "rule_id")]
    _rule_id: String,
    #[serde(rename = "priority")]
    _priority: String,
    #[serde(rename = "impact")]
    _impact: i32,
    #[serde(rename = "confidence")]
    _confidence: i32,
    #[serde(rename = "effort")]
    _effort: i32,
    #[serde(rename = "file")]
    _file: String,
    #[serde(rename = "line")]
    _line: i64,
    #[serde(rename = "title")]
    _title: String,
    #[serde(rename = "description")]
    _description: String,
    #[serde(rename = "action")]
    _action: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewSummary {
    #[serde(rename = "p0_count")]
    _p0_count: i32,
    #[serde(rename = "p1_count")]
    _p1_count: i32,
    #[serde(rename = "p2_count")]
    _p2_count: i32,
    #[serde(rename = "p3_count")]
    _p3_count: i32,
    pub(super) health_score: i32,
}

/// Parse the structured result emitted by a periodic review agent.
///
/// Agents may wrap the JSON in prose, Markdown fences, or explicit review
/// markers. Parsing remains a prerequisite for advancing the review watermark.
pub(super) fn parse_review_output(raw: &str) -> anyhow::Result<ReviewOutput> {
    let trimmed = raw.trim();

    const START_MARKER: &str = "REVIEW_JSON_START";
    const END_MARKER: &str = "REVIEW_JSON_END";
    if let Some(start) = trimmed.find(START_MARKER) {
        let after_marker = &trimmed[start + START_MARKER.len()..];
        if let Some(end) = after_marker.find(END_MARKER) {
            let json_slice = after_marker[..end].trim();
            return serde_json::from_str(json_slice)
                .map_err(|error| anyhow::anyhow!("failed to parse marked review JSON: {error}"));
        }
    }

    let start = trimmed
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in review output"))?;
    let end = trimmed
        .rfind('}')
        .map(|index| index + 1)
        .ok_or_else(|| anyhow::anyhow!("no closing brace found in review output"))?;
    serde_json::from_str(&trimmed[start..end])
        .map_err(|error| anyhow::anyhow!("failed to parse review JSON: {error}"))
}

#[cfg(test)]
mod review_output_tests {
    use super::parse_review_output;

    const EMPTY_REVIEW: &str = r#"{"findings":[],"summary":{"p0_count":0,"p1_count":0,"p2_count":0,"p3_count":0,"health_score":100}}"#;

    #[test]
    fn parses_clean_json() -> anyhow::Result<()> {
        let output = parse_review_output(EMPTY_REVIEW)?;
        assert!(output.findings.is_empty());
        assert_eq!(output.summary.health_score, 100);
        Ok(())
    }

    #[test]
    fn parses_markdown_fenced_json() -> anyhow::Result<()> {
        let output = parse_review_output(&format!("```json\n{EMPTY_REVIEW}\n```"))?;
        assert!(output.findings.is_empty());
        Ok(())
    }

    #[test]
    fn parses_marked_json_before_surrounding_prose() -> anyhow::Result<()> {
        let raw =
            format!("Created issues.\nREVIEW_JSON_START\n{EMPTY_REVIEW}\nREVIEW_JSON_END\nDone.");
        let output = parse_review_output(&raw)?;
        assert_eq!(output.summary.health_score, 100);
        Ok(())
    }

    #[test]
    fn rejects_output_without_json() {
        assert!(parse_review_output("not json at all").is_err());
    }

    #[test]
    fn rejects_findings_that_do_not_match_the_prompt_schema() {
        let malformed = r#"{"findings":[{}],"summary":{"p0_count":0,"p1_count":0,"p2_count":0,"p3_count":0,"health_score":100}}"#;
        assert!(parse_review_output(malformed).is_err());
    }
}
