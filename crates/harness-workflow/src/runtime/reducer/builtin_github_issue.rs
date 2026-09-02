use super::support::{
    event_field_string, non_empty_json_string, runtime_blocked_command, runtime_completion_evidence,
};
use super::{
    GITHUB_ISSUE_PR_DEFINITION_ID, ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL,
    ISSUE_STATE_ARTIFACT,
};
use crate::runtime::completion_evidence::{
    github_pr_identity, pr_binding_verification_failure,
    transition_evidence_enforced_with_registry, verified_issue_state_artifact,
    verified_pr_binding_artifact, ARTIFACT_MERGE_COMPLETION_VERIFICATION, EVIDENCE_GITHUB_TERMINAL,
    EVIDENCE_VERIFIED_PR_BINDING, MERGE_COMPLETION_VERIFICATION_SCHEMA,
    REASON_PR_BINDING_VERIFICATION_FAILED,
};
use crate::runtime::model::{
    ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvent,
    WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::reason_class::STOP_REASON_INVALID_AGENT_OUTPUT;
use crate::runtime::{WorkflowDefinitionRegistry, SERVER_PR_SNAPSHOT_ARTIFACT};
use serde_json::{json, Value};

pub(super) fn issue_implementation_missing_result_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        "implement_issue",
    ) {
        return None;
    }

    let reason = "implement_issue succeeded without a pull_request artifact, closed-issue evidence, or another validated terminal signal";
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "block_missing_implementation_result",
            "blocked",
            reason,
        )
        .with_command(runtime_blocked_command(
            reason,
            Some(STOP_REASON_INVALID_AGENT_OUTPUT),
            format!(
                "runtime-completion:{}:missing-implementation:block",
                event.id
            ),
            event,
            result,
        ))
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::RequestOperatorAttention,
            format!(
                "runtime-completion:{}:missing-implementation:operator",
                event.id
            ),
            json!({
                "reason": reason,
                "activity": result.activity,
                "runtime_job_id": event_field_string(event, "runtime_job_id"),
            }),
        ))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

pub(super) fn github_issue_closed_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        || !github_issue_state_can_finish_closed(instance.state.as_str())
    {
        return None;
    }

    let verified_issue = verified_issue_state_for_instance(instance, result);
    let closed_issue = closed_issue_evidence_from_activity_result(result).or_else(|| {
        verified_issue.map(|verified| ClosedIssueEvidence {
            summary: format!(
                "server verified issue {} is closed",
                verified
                    .get("issue_number")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
            ),
            payload: verified.clone(),
        })
    })?;
    let reason = format!(
        "{} reported structured evidence that the GitHub issue is already closed",
        result.activity
    );
    let terminal_evidence = if let Some(verified_issue) = verified_issue {
        WorkflowEvidence::runtime_observed(
            EVIDENCE_GITHUB_TERMINAL,
            format!("verified_closed_issue: {verified_issue}"),
            "server_verified_issue_state",
            Some(event.id.clone()),
        )
    } else {
        WorkflowEvidence::new(
            EVIDENCE_GITHUB_TERMINAL,
            format!("closed_issue: {}", closed_issue.summary),
        )
    };
    let terminal_payload = verified_issue
        .cloned()
        .unwrap_or_else(|| closed_issue.payload.clone());
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "finish_closed_issue",
            "done",
            &reason,
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::MarkDone,
            format!("runtime-completion:{}:closed-issue:done", event.id),
            json!({
                "reason": reason,
                "activity": result.activity,
                "runtime_job_id": event_field_string(event, "runtime_job_id"),
                "closed_issue_evidence": terminal_payload,
            }),
        ))
        .with_evidence(terminal_evidence)
        .with_evidence(WorkflowEvidence::new("closed_issue", closed_issue.summary))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

fn verified_issue_state_for_instance<'a>(
    instance: &WorkflowInstance,
    result: &'a ActivityResult,
) -> Option<&'a Value> {
    let verified = verified_issue_state_artifact(result)?;
    let expected_issue_number = instance
        .data
        .get("issue_number")
        .and_then(Value::as_u64)
        .or_else(|| instance.subject.subject_key.parse().ok())?;
    if verified.get("issue_number").and_then(Value::as_u64) != Some(expected_issue_number)
        || verified.get("state").and_then(Value::as_str) != Some("closed")
    {
        return None;
    }
    if let Some(expected_repo) = instance.data.get("repo").and_then(Value::as_str) {
        if !verified
            .get("repo")
            .and_then(Value::as_str)
            .is_some_and(|repo| repo.eq_ignore_ascii_case(expected_repo))
        {
            return None;
        }
    }
    Some(verified)
}

fn github_issue_state_can_finish_closed(state: &str) -> bool {
    matches!(
        state,
        "implementing"
            | "pr_open"
            | "awaiting_feedback"
            | "addressing_feedback"
            | "local_review_gate"
            | "quality_gate_pending"
            | "ready_to_merge"
    )
}

pub(super) fn terminal_pr_snapshot_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        || matches!(instance.state.as_str(), "done" | "cancelled" | "failed")
    {
        return None;
    }

    let snapshot = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == SERVER_PR_SNAPSHOT_ARTIFACT)
        .map(|artifact| &artifact.artifact)
        .find(|snapshot| {
            crate::runtime::pr_feedback::server_pr_snapshot_matches_instance(instance, snapshot)
        })?;
    let pr_number = snapshot.get("pr_number").and_then(Value::as_u64)?;
    let pr_url = snapshot.get("pr_url").and_then(Value::as_str)?;
    let state = snapshot.get("state").and_then(Value::as_str)?;
    let (decision, next_state, command_type, action) = match state {
        "MERGED" => (
            "finish_server_observed_pr_merge",
            "done",
            WorkflowCommandType::MarkDone,
            "merged",
        ),
        "CLOSED" => (
            "cancel_server_observed_closed_pr",
            "cancelled",
            WorkflowCommandType::MarkCancelled,
            "closed without merge",
        ),
        _ => return None,
    };
    let reason = format!("server-owned GitHub snapshot observed pull request {action}");

    Some(
        WorkflowDecision::new(&instance.id, &instance.state, decision, next_state, &reason)
            .with_command(WorkflowCommand::new(
                command_type,
                format!(
                    "runtime-completion:{}:server-pr-terminal:{pr_number}",
                    event.id
                ),
                json!({
                    "reason": reason,
                    "activity": result.activity,
                    "runtime_job_id": event_field_string(event, "runtime_job_id"),
                    "pr_number": pr_number,
                    "pr_url": pr_url,
                    "server_pr_snapshot": snapshot,
                }),
            ))
            .with_evidence(WorkflowEvidence::runtime_observed(
                EVIDENCE_GITHUB_TERMINAL,
                format!("server_pr_snapshot: pr={pr_number} state={state}"),
                "server_pr_snapshot",
                Some(event.id.clone()),
            ))
            .with_evidence(runtime_completion_evidence(event, result))
            .high_confidence(),
    )
}

pub(super) fn bind_pr_from_activity_result(
    registry: &WorkflowDefinitionRegistry,
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        "implement_issue",
    ) {
        return None;
    }
    let (pr_number, pr_url) = pull_request_artifact(result)?;
    let binding =
        match verified_pr_binding_evidence_with_registry(registry, result, pr_number, &pr_url) {
            Ok(binding) => binding,
            Err(reason) => {
                return Some(pr_binding_verification_blocked_decision(
                    instance, event, result, &reason,
                ));
            }
        };
    let pr_url = binding.canonical_pr_url;
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "bind_pr",
            "pr_open",
            "implementation activity returned a structured pull request artifact",
        )
        .with_command(WorkflowCommand::bind_pr(
            pr_number,
            pr_url.clone(),
            format!("runtime-completion:{}:bind-pr:{pr_number}", event.id),
        ))
        .with_evidence(WorkflowEvidence::new("pull_request", pr_url))
        .with_evidence(binding.evidence)
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

/// The `verified_pr_binding` decision evidence for a claimed PR binding
/// (GH-1766, B-005/B-006): the server-attached verification artifact, or a
/// recorded waiver when enforcement is disabled. A server-recorded
/// verification failure, a PR-number mismatch, or a missing verification
/// while enforcement is active all fail closed.
pub(crate) struct VerifiedPrBindingEvidence {
    pub(crate) evidence: WorkflowEvidence,
    pub(crate) canonical_pr_url: String,
}

pub(crate) fn verified_pr_binding_evidence_with_registry(
    registry: &WorkflowDefinitionRegistry,
    result: &ActivityResult,
    claimed_pr_number: u64,
    claimed_pr_url: &str,
) -> Result<VerifiedPrBindingEvidence, String> {
    if let Some(failure) = pr_binding_verification_failure(result) {
        return Err(format!(
            "server verification of the claimed pull request failed: {failure}"
        ));
    }
    let Some((claimed_repo, url_pr_number)) = github_pr_identity(claimed_pr_url) else {
        return Err(format!(
            "the activity claimed an invalid GitHub pull request URL: {claimed_pr_url}"
        ));
    };
    if url_pr_number != claimed_pr_number {
        return Err(format!(
            "the activity claimed pull request number {claimed_pr_number} but its URL identifies #{url_pr_number}"
        ));
    }
    if let Some(verified) = verified_pr_binding_artifact(result) {
        let verified_number = verified.get("pr_number").and_then(Value::as_u64);
        let verified_repo = verified
            .get("repo")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|repo| !repo.is_empty());
        let Some(verified_repo) = verified_repo else {
            return Err(format!(
                "server verified pull request {verified_number:?} without a repository identity"
            ));
        };
        if verified_number != Some(claimed_pr_number)
            || !verified_repo.eq_ignore_ascii_case(&claimed_repo)
        {
            return Err(format!(
                "server verified pull request {verified_number:?} in {verified_repo} but the activity claimed {claimed_pr_number} in {claimed_repo}"
            ));
        }
        return Ok(VerifiedPrBindingEvidence {
            evidence: WorkflowEvidence::runtime_observed(
                EVIDENCE_VERIFIED_PR_BINDING,
                verified.to_string(),
                "server_verified_pr_binding_artifact",
                None,
            ),
            canonical_pr_url: format!(
                "https://github.com/{verified_repo}/pull/{claimed_pr_number}"
            ),
        });
    }
    if !transition_evidence_enforced_with_registry(
        registry,
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        "pr_open",
        EVIDENCE_VERIFIED_PR_BINDING,
    ) {
        // The transition table no longer demands it, so neither does this
        // reducer: one authority, no drift.
        return Ok(VerifiedPrBindingEvidence {
            evidence: WorkflowEvidence::new(
                EVIDENCE_VERIFIED_PR_BINDING,
                "enforcement_lifted_by_deployment_config",
            ),
            canonical_pr_url: format!("https://github.com/{claimed_repo}/pull/{claimed_pr_number}"),
        });
    }
    Err(
        "the activity claimed a pull request but the server recorded no binding verification"
            .to_string(),
    )
}

pub(crate) fn pr_binding_verification_blocked_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    detail: &str,
) -> WorkflowDecision {
    let reason = format!("{REASON_PR_BINDING_VERIFICATION_FAILED}: {detail}");
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        REASON_PR_BINDING_VERIFICATION_FAILED,
        "blocked",
        &reason,
    )
    .with_command(runtime_blocked_command(
        &reason,
        Some(STOP_REASON_INVALID_AGENT_OUTPUT),
        format!(
            "runtime-completion:{}:pr-binding-verification:block",
            event.id
        ),
        event,
        result,
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        format!(
            "runtime-completion:{}:pr-binding-verification:operator",
            event.id
        ),
        json!({
            "reason": reason,
            "activity": result.activity,
            "runtime_job_id": event_field_string(event, "runtime_job_id"),
        }),
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

pub(super) fn merged_pr_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (GITHUB_ISSUE_PR_DEFINITION_ID, "merging", "merge_pr")
    {
        return None;
    }
    let merged = merged_pull_request_artifact(result)?;
    let reason = "merge_pr returned structured evidence that the pull request was merged";
    let terminal_evidence = if let Some(source) =
        merge_completion_trust_source(instance, result, merged.pr_number, &merged.pr_url)
    {
        WorkflowEvidence::runtime_observed(
            EVIDENCE_GITHUB_TERMINAL,
            format!(
                "merged_pull_request: pr={} head={} merge_commit={}",
                merged.pr_number,
                merged.head_sha.as_deref().unwrap_or("unknown"),
                merged.merge_commit_sha.as_deref().unwrap_or("unknown"),
            ),
            source,
            Some(event.id.clone()),
        )
    } else {
        WorkflowEvidence::new(
            EVIDENCE_GITHUB_TERMINAL,
            format!(
                "agent_reported_merged_pull_request: pr={}",
                merged.pr_number
            ),
        )
    };
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "record_pr_merged",
            "done",
            reason,
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::MarkDone,
            format!(
                "runtime-completion:{}:merged-pr:{}:done",
                event.id, merged.pr_number
            ),
            json!({
                "reason": reason,
                "activity": result.activity,
                "runtime_job_id": event_field_string(event, "runtime_job_id"),
                "pr_number": merged.pr_number,
                "pr_url": merged.pr_url,
                "merge_commit_sha": merged.merge_commit_sha,
                "head_sha": merged.head_sha,
                "pull_request_evidence": merged.payload,
            }),
        ))
        .with_evidence(terminal_evidence)
        .with_evidence(WorkflowEvidence::new(
            "github_pr_merged",
            format!("pr={} url={}", merged.pr_number, merged.pr_url),
        ))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

fn merge_completion_trust_source<'a>(
    instance: &WorkflowInstance,
    result: &'a ActivityResult,
    pr_number: u64,
    pr_url: &str,
) -> Option<&'a str> {
    let expected_repo = instance
        .data
        .get("repo")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|repo| !repo.is_empty())?;
    let (url_repo, url_pr_number) = github_pr_identity(pr_url)?;
    if url_pr_number != pr_number || !url_repo.eq_ignore_ascii_case(expected_repo) {
        return None;
    }
    result.artifacts.iter().find_map(|artifact| {
        if artifact.artifact_type != ARTIFACT_MERGE_COMPLETION_VERIFICATION
            || artifact.artifact.get("schema").and_then(Value::as_str)
                != Some(MERGE_COMPLETION_VERIFICATION_SCHEMA)
        {
            return None;
        }
        if artifact.artifact.get("pr_number").and_then(Value::as_u64) != Some(pr_number)
            || !artifact
                .artifact
                .get("repo")
                .and_then(Value::as_str)
                .is_some_and(|repo| repo.eq_ignore_ascii_case(expected_repo))
        {
            return None;
        }
        let verified = artifact.artifact.get("verified").and_then(Value::as_bool) == Some(true)
            && artifact
                .artifact
                .get("observed_merged")
                .and_then(Value::as_bool)
                == Some(true);
        if verified {
            return Some("github_pr_merged_result");
        }
        None
    })
}

#[derive(Debug, Clone)]
struct MergedPullRequestEvidence {
    pr_number: u64,
    pr_url: String,
    merge_commit_sha: Option<String>,
    head_sha: Option<String>,
    payload: Value,
}

fn merged_pull_request_artifact(result: &ActivityResult) -> Option<MergedPullRequestEvidence> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "pull_request")
        .find_map(|artifact| {
            if !pull_request_artifact_is_merged(&artifact.artifact) {
                return None;
            }
            let pr_number = artifact.artifact.get("pr_number")?.as_u64()?;
            let pr_url = artifact
                .artifact
                .get("pr_url")
                .or_else(|| artifact.artifact.get("url"))?
                .as_str()
                .filter(|value| !value.trim().is_empty())?
                .to_string();
            let merge_commit_sha = artifact
                .artifact
                .get("merge_commit_sha")
                .or_else(|| artifact.artifact.get("mergeCommitOid"))
                .and_then(non_empty_json_string);
            let head_sha = artifact
                .artifact
                .get("head_sha")
                .or_else(|| artifact.artifact.get("headRefOid"))
                .and_then(non_empty_json_string);
            Some(MergedPullRequestEvidence {
                pr_number,
                pr_url,
                merge_commit_sha,
                head_sha,
                payload: artifact.artifact.clone(),
            })
        })
}

fn pull_request_artifact_is_merged(value: &Value) -> bool {
    value.get("merged").and_then(Value::as_bool) == Some(true)
        || value
            .get("state")
            .and_then(non_empty_json_string)
            .is_some_and(|state| state.eq_ignore_ascii_case("merged"))
}

fn pull_request_artifact(result: &ActivityResult) -> Option<(u64, String)> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "pull_request")
        .find_map(|artifact| {
            let pr_number = artifact.artifact.get("pr_number")?.as_u64()?;
            let pr_url = artifact
                .artifact
                .get("pr_url")?
                .as_str()
                .filter(|value| !value.trim().is_empty())?
                .to_string();
            Some((pr_number, pr_url))
        })
}

#[derive(Debug, Clone)]
pub(super) struct ClosedIssueEvidence {
    summary: String,
    payload: Value,
}

pub(super) fn closed_issue_evidence_from_activity_result(
    result: &ActivityResult,
) -> Option<ClosedIssueEvidence> {
    result
        .signals
        .iter()
        .find_map(|signal| match signal.signal_type.as_str() {
            ISSUE_CLOSED_SIGNAL | ISSUE_ALREADY_RESOLVED_SIGNAL => {
                closed_issue_evidence_from_value(&signal.signal, &signal.signal_type)
            }
            _ => None,
        })
        .or_else(|| {
            result
                .artifacts
                .iter()
                .filter(|artifact| artifact.artifact_type == ISSUE_STATE_ARTIFACT)
                .find_map(|artifact| {
                    if issue_state_is_closed(&artifact.artifact) {
                        closed_issue_evidence_from_value(&artifact.artifact, ISSUE_STATE_ARTIFACT)
                    } else {
                        None
                    }
                })
        })
}

pub(super) fn closed_issue_evidence_from_activity_result_value(
    value: &Value,
) -> Option<ClosedIssueEvidence> {
    value
        .get("signals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|signal| {
            let signal_type = signal.get("signal_type").and_then(non_empty_json_string)?;
            match signal_type.as_str() {
                ISSUE_CLOSED_SIGNAL | ISSUE_ALREADY_RESOLVED_SIGNAL => signal
                    .get("signal")
                    .and_then(|payload| closed_issue_evidence_from_value(payload, &signal_type)),
                _ => None,
            }
        })
        .or_else(|| {
            value
                .get("artifacts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|artifact| {
                    artifact.get("artifact_type").and_then(Value::as_str)
                        == Some(ISSUE_STATE_ARTIFACT)
                })
                .find_map(|artifact| {
                    artifact
                        .get("artifact")
                        .filter(|payload| issue_state_is_closed(payload))
                        .and_then(|payload| {
                            closed_issue_evidence_from_value(payload, ISSUE_STATE_ARTIFACT)
                        })
                })
        })
}

pub(super) fn closed_issue_evidence_from_value(
    value: &Value,
    source: &str,
) -> Option<ClosedIssueEvidence> {
    let issue_number = value.get("issue_number").and_then(Value::as_u64);
    let issue_url = value
        .get("issue_url")
        .or_else(|| value.get("html_url"))
        .or_else(|| value.get("url"))
        .and_then(non_empty_json_string);
    let state = value.get("state").and_then(non_empty_json_string);
    let closed = issue_state_is_closed(value);

    if !closed || (issue_number.is_none() && issue_url.is_none()) {
        return None;
    }

    let mut facts = vec![format!("source={source}")];
    if let Some(issue_number) = issue_number {
        facts.push(format!("issue_number={issue_number}"));
    }
    if let Some(issue_url) = issue_url.clone() {
        facts.push(format!("issue_url={issue_url}"));
    }
    if let Some(state) = state.clone() {
        facts.push(format!("state={state}"));
    }
    if closed {
        facts.push("closed=true".to_string());
    }

    Some(ClosedIssueEvidence {
        summary: facts.join(" "),
        payload: json!({
            "source": source,
            "issue_number": issue_number,
            "issue_url": issue_url,
            "state": state,
            "closed": closed,
        }),
    })
}

fn issue_state_is_closed(value: &Value) -> bool {
    value
        .get("closed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| {
                state.trim().eq_ignore_ascii_case("closed")
                    || state.trim().eq_ignore_ascii_case("resolved")
            })
        || value
            .get("is_closed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}
