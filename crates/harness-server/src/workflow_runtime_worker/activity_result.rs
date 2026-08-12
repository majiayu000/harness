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
#[path = "activity_result_parser.rs"]
mod activity_result_parser;
use super::prompt_packet::workflow_prompt_artifact;
use activity_result_parser::parse_activity_result_json;

pub(super) fn activity_result_from_turn_with_workflow(
    job: &RuntimeJob,
    status: &TurnStatus,
    items: &[Item],
    thread_id: &ThreadId,
    turn_id: &TurnId,
    agent_name: &str,
    project_root: &Path,
    prompt_packet_digest: &str,
    workflow_definition: Option<&str>,
) -> ActivityResult {
    let activity = activity_name(job);
    let summary = last_agent_summary(items).unwrap_or_else(|| match status {
        TurnStatus::Completed => "Agent turn completed.".to_string(),
        TurnStatus::Cancelled => "Agent turn was cancelled.".to_string(),
        TurnStatus::Failed => "Agent turn failed.".to_string(),
        TurnStatus::Running => "Agent turn is still running after lifecycle returned.".to_string(),
    });
    let envelope =
        activity_result_envelope_from_turn(status, items, &activity, summary, workflow_definition);
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

#[cfg(test)]
fn activity_result_from_turn(
    job: &RuntimeJob,
    status: &TurnStatus,
    items: &[Item],
    thread_id: &ThreadId,
    turn_id: &TurnId,
    agent_name: &str,
    project_root: &Path,
    prompt_packet_digest: &str,
) -> ActivityResult {
    activity_result_from_turn_with_workflow(
        job,
        status,
        items,
        thread_id,
        turn_id,
        agent_name,
        project_root,
        prompt_packet_digest,
        None,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ActivityResultExtractionStrategy {
    FencedActivityResult,
    RawActivityResult,
    NotAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ActivityResultEnvelopeOutcome {
    Accepted,
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
        workflow_definition: Option<&str>,
    ) -> Self {
        let (downgraded, result) = enforce_activity_status_contract(workflow_definition, result);
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
        extraction_strategy: ActivityResultExtractionStrategy,
        activity: String,
        error: String,
        extracted_activity: Option<String>,
    ) -> Self {
        Self {
            extraction_strategy,
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
                    "summary": self.final_result.summary,
                    "error": self.final_result.error,
                    "error_kind": self.final_result.error_kind,
                }
            }),
        )
    }

    fn into_final_result(self) -> ActivityResult {
        self.final_result
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StructuredOutputCorrection {
    pub outcome: String,
    pub error: String,
    pub extracted_activity: Option<String>,
}

pub(super) fn activity_result_envelope_outcome(result: &ActivityResult) -> Option<&str> {
    activity_result_envelope(result)?
        .get("outcome")
        .and_then(serde_json::Value::as_str)
}

pub(super) fn structured_output_correction(
    result: &ActivityResult,
) -> Option<StructuredOutputCorrection> {
    let envelope = activity_result_envelope(result)?;
    let outcome = envelope.get("outcome")?.as_str()?;
    if !matches!(
        outcome,
        "invalid_structured_output" | "missing_structured_output"
    ) {
        return None;
    }
    let error = envelope
        .get("extraction_error")
        .and_then(serde_json::Value::as_str)
        .or(result.error.as_deref())
        .unwrap_or("structured activity result was invalid")
        .to_string();
    let extracted_activity = envelope
        .get("extracted_activity")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Some(StructuredOutputCorrection {
        outcome: outcome.to_string(),
        error,
        extracted_activity,
    })
}

fn activity_result_envelope(result: &ActivityResult) -> Option<&serde_json::Value> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "activity_result_envelope")
        .map(|artifact| &artifact.artifact)
}

fn activity_result_envelope_from_turn(
    status: &TurnStatus,
    items: &[Item],
    activity: &str,
    summary: String,
    workflow_definition: Option<&str>,
) -> ActivityResultEnvelope {
    match status {
        TurnStatus::Completed => match structured_activity_result(items, activity) {
            StructuredActivityResult::Parsed {
                result,
                extraction_strategy,
            } => ActivityResultEnvelope::accepted(
                *status,
                extraction_strategy,
                result,
                workflow_definition,
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
                extraction_strategy,
            } => ActivityResultEnvelope::invalid_structured_output(
                *status,
                extraction_strategy,
                activity.to_string(),
                error,
                extracted_activity,
            ),
        },
        TurnStatus::Cancelled => {
            ActivityResultEnvelope::cancelled(*status, activity.to_string(), summary)
        }
        TurnStatus::Failed => {
            let error = last_error(items).unwrap_or_else(|| "agent turn failed".to_string());
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
        structured_result_artifacts: usize::from(structured_activity_result_candidate(items)),
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

enum StructuredActivityResult {
    Missing,
    Parsed {
        result: ActivityResult,
        extraction_strategy: ActivityResultExtractionStrategy,
    },
    Invalid {
        error: String,
        extracted_activity: Option<String>,
        extraction_strategy: ActivityResultExtractionStrategy,
    },
}

fn structured_activity_result(items: &[Item], expected_activity: &str) -> StructuredActivityResult {
    if let Some(block) = latest_activity_result_block(items) {
        return parse_activity_result_block(
            block,
            expected_activity,
            ActivityResultExtractionStrategy::FencedActivityResult,
        );
    }

    if let Some(raw_json) = latest_raw_activity_result_json(items) {
        return parse_activity_result_block(
            raw_json,
            expected_activity,
            ActivityResultExtractionStrategy::RawActivityResult,
        );
    }

    StructuredActivityResult::Missing
}

fn parse_activity_result_block(
    block: &str,
    expected_activity: &str,
    extraction_strategy: ActivityResultExtractionStrategy,
) -> StructuredActivityResult {
    match parse_activity_result_json(block, expected_activity) {
        Ok(result) => StructuredActivityResult::Parsed {
            result,
            extraction_strategy,
        },
        Err(error) => StructuredActivityResult::Invalid {
            error: error.error,
            extracted_activity: error.extracted_activity,
            extraction_strategy,
        },
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

fn latest_raw_activity_result_json(items: &[Item]) -> Option<&str> {
    items.iter().rev().find_map(|item| match item {
        Item::AgentReasoning { content } => {
            let content = content.trim();
            (content.starts_with('{') && content.ends_with('}')).then_some(content)
        }
        _ => None,
    })
}

fn structured_activity_result_candidate(items: &[Item]) -> bool {
    latest_activity_result_block(items).is_some()
        || latest_raw_activity_result_json(items).is_some()
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
