use harness_core::types::{Item, ThreadId, TurnId, TurnStatus};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, RuntimeJob,
};
use serde::Serialize;
use serde_json::json;
use std::path::Path;

use super::activity_status_contract::{
    enforce_activity_status_contract, status_contract_blockers_from_result,
};
use super::data_helpers::activity_name;
use super::prompt_packet::workflow_prompt_artifact;

const CODEX_SKILL_BUDGET_WARNING: &str =
    "Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.";
const CODEX_SKILL_BUDGET_WARNING_ADVICE: &str =
    " Disable unused skills or plugins to leave more room for the rest.";
const CODEX_SKILL_BUDGET_AGENT_ERROR: &str =
    "agent execution failed: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.";
const CODEX_SKILL_BUDGET_STRUCTURED_AGENT_ERROR: &str =
    "agent execution failed: codex structured error: Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill, but some descriptions are shorter.";
const CODEX_SKILL_BUDGET_STRUCTURED_AGENT_ERROR_PREFIX: &str =
    "agent execution failed: codex structured error: exit ";

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
        ActivityResultEnvelopeOutcome::ZeroOutputSpawnFailure => {
            tracing::error!(
                runtime_job_id = %job.id,
                activity = %activity,
                agent = %agent_name,
                items = items.len(),
                "activity completed with no observable agent activity; classified as spawn failure"
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
        ActivityResultEnvelopeOutcome::AcceptedWithTurnWarning => {
            if let Some(error) = envelope.extraction_error.as_deref() {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    activity = %activity,
                    agent = %agent_name,
                    turn_status = ?status,
                    items = items.len(),
                    "accepted structured activity result despite non-fatal turn warning: {error}"
                );
            }
        }
        ActivityResultEnvelopeOutcome::StatusContractDowngraded => {
            let blocker_signals = status_contract_blockers_from_result(&envelope.final_result);
            tracing::error!(
                runtime_job_id = %job.id,
                activity = %activity,
                agent = %agent_name,
                claimed_status = "succeeded",
                effective_status = "blocked",
                blocker_signals = ?blocker_signals,
                "activity result claimed succeeded while reporting blockers; downgraded to blocked"
            );
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
    AcceptedWithTurnWarning,
    MissingStructuredOutput,
    InvalidStructuredOutput,
    ZeroOutputSpawnFailure,
    TurnCancelled,
    TurnFailed,
    StatusContractDowngraded,
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
        let (downgraded, result) = enforce_activity_status_contract(result);
        let outcome = if downgraded {
            ActivityResultEnvelopeOutcome::StatusContractDowngraded
        } else {
            ActivityResultEnvelopeOutcome::Accepted
        };
        Self {
            extraction_strategy,
            outcome,
            raw_status,
            extracted_activity: Some(result.activity.clone()),
            extraction_error: None,
            final_result: result,
        }
    }

    fn accepted_with_turn_warning(
        raw_status: TurnStatus,
        result: ActivityResult,
        warning: String,
    ) -> Self {
        let (downgraded, result) = enforce_activity_status_contract(result);
        let outcome = if downgraded {
            ActivityResultEnvelopeOutcome::StatusContractDowngraded
        } else {
            ActivityResultEnvelopeOutcome::AcceptedWithTurnWarning
        };
        Self {
            extraction_strategy: ActivityResultExtractionStrategy::FencedActivityResult,
            outcome,
            raw_status,
            extracted_activity: Some(result.activity.clone()),
            extraction_error: Some(warning),
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

    fn zero_output_spawn_failure(
        raw_status: TurnStatus,
        activity: String,
        activity_summary: AgentActivitySummary,
    ) -> Self {
        let error = "agent completed with no observable activity: zero assistant messages, zero tool invocations, and no structured activity result".to_string();
        Self {
            extraction_strategy: ActivityResultExtractionStrategy::FencedActivityResult,
            outcome: ActivityResultEnvelopeOutcome::ZeroOutputSpawnFailure,
            raw_status,
            extracted_activity: None,
            extraction_error: Some(error.clone()),
            final_result: ActivityResult::failed(
                activity,
                "Agent turn completed without observable activity.",
                error,
            )
            .with_error_kind(ActivityErrorKind::SpawnFailure)
            .with_artifact(ActivityArtifact::new(
                "agent_activity_gate",
                json!({
                    "schema": "harness.runtime.agent_activity_gate.v1",
                    "classification": "spawn_failure",
                    "assistant_messages": activity_summary.assistant_messages,
                    "tool_invocations": activity_summary.tool_invocations,
                    "structured_result_artifacts": activity_summary.structured_result_artifacts,
                    "total_items": activity_summary.total_items,
                }),
            ))
            .with_signal(ActivitySignal::new(
                "AgentZeroOutputSpawnFailure",
                json!({
                    "classification": "spawn_failure",
                    "assistant_messages": activity_summary.assistant_messages,
                    "tool_invocations": activity_summary.tool_invocations,
                    "structured_result_artifacts": activity_summary.structured_result_artifacts,
                    "total_items": activity_summary.total_items,
                }),
            )),
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
            StructuredActivityResult::Missing => {
                let activity_summary = agent_activity_summary(items);
                if activity_summary.is_zero_output() {
                    ActivityResultEnvelope::zero_output_spawn_failure(
                        *status,
                        activity.to_string(),
                        activity_summary,
                    )
                } else {
                    ActivityResultEnvelope::missing_structured_output(
                        *status,
                        activity.to_string(),
                        summary,
                    )
                }
            }
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
        TurnStatus::Failed => {
            let error = failed_turn_error(items);
            if let Some(warning) = failed_turn_warning_allows_structured_result(items) {
                match structured_activity_result(items, activity) {
                    StructuredActivityResult::Parsed(result) => {
                        return ActivityResultEnvelope::accepted_with_turn_warning(
                            *status, result, warning,
                        );
                    }
                    StructuredActivityResult::Invalid {
                        error: parse_error,
                        extracted_activity,
                    } => {
                        return ActivityResultEnvelope::invalid_structured_output(
                            *status,
                            activity.to_string(),
                            parse_error,
                            extracted_activity,
                        );
                    }
                    StructuredActivityResult::Missing => {}
                }
            }
            ActivityResultEnvelope::failed(*status, activity.to_string(), summary, error)
        }
        TurnStatus::Running => {
            let error = last_error(items).unwrap_or_else(|| "agent turn failed".to_string());
            ActivityResultEnvelope::failed(*status, activity.to_string(), summary, error)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentActivitySummary {
    assistant_messages: usize,
    tool_invocations: usize,
    structured_result_artifacts: usize,
    total_items: usize,
}

impl AgentActivitySummary {
    fn is_zero_output(&self) -> bool {
        self.assistant_messages == 0
            && self.tool_invocations == 0
            && self.structured_result_artifacts == 0
    }
}

fn agent_activity_summary(items: &[Item]) -> AgentActivitySummary {
    AgentActivitySummary {
        assistant_messages: items
            .iter()
            .filter(
                |item| matches!(item, Item::AgentReasoning { content } if !content.trim().is_empty()),
            )
            .count(),
        tool_invocations: items.iter().filter(|item| item_is_tool_activity(item)).count(),
        structured_result_artifacts: usize::from(latest_activity_result_block(items).is_some()),
        total_items: items.len(),
    }
}

fn item_is_tool_activity(item: &Item) -> bool {
    matches!(
        item,
        Item::ShellCommand { .. }
            | Item::FileEdit { .. }
            | Item::FileRead { .. }
            | Item::ToolCall { .. }
            | Item::ApprovalRequest { .. }
    )
}

fn turn_error_is_timeout(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("timed out") || normalized.contains("timeout reached")
}

fn turn_error_is_non_retryable_agent_limit(error: &str) -> bool {
    harness_core::error::is_quota_failure_message(error)
        || harness_core::error::is_billing_failure_message(error)
}

fn failed_turn_error(items: &[Item]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Error { message, .. } if !failed_turn_error_allows_structured_result(message) => {
                Some(truncate_summary(message.trim()))
            }
            _ => None,
        })
        .next_back()
        .or_else(|| last_error(items))
        .unwrap_or_else(|| "agent turn failed".to_string())
}

fn failed_turn_warning_allows_structured_result(items: &[Item]) -> Option<String> {
    let mut warning = None;
    for item in items {
        if let Item::Error { message, .. } = item {
            if !failed_turn_error_allows_structured_result(message) {
                return None;
            }
            warning = Some(message.clone());
        }
    }
    warning
}

fn failed_turn_error_allows_structured_result(error: &str) -> bool {
    let error = error.trim();
    let error = error
        .strip_suffix(CODEX_SKILL_BUDGET_WARNING_ADVICE)
        .unwrap_or(error);
    error == CODEX_SKILL_BUDGET_WARNING
        || error == CODEX_SKILL_BUDGET_AGENT_ERROR
        || error == CODEX_SKILL_BUDGET_STRUCTURED_AGENT_ERROR
        || codex_structured_skill_budget_error_allows_structured_result(error)
}

fn codex_structured_skill_budget_error_allows_structured_result(error: &str) -> bool {
    let Some(rest) = error.strip_prefix(CODEX_SKILL_BUDGET_STRUCTURED_AGENT_ERROR_PREFIX) else {
        return false;
    };
    let Some(status_prefix) = rest.strip_suffix(CODEX_SKILL_BUDGET_WARNING) else {
        return false;
    };
    let Some(status_text) = status_prefix.strip_suffix(": ") else {
        return false;
    };
    !status_text.trim().is_empty() && !status_text.contains(['\n', '\r'])
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
#[path = "activity_result_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "activity_result_limit_tests.rs"]
mod limit_tests;
