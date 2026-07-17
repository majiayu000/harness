//! Declarative intake routing (GH-1656, T004/T005/T006).
//!
//! Routes an incoming issue to a declarative workflow definition when a
//! project-scoped intake binding matches. Routing is EXCLUSIVE: a matched issue
//! is handled here and never falls through to the default `github_issue_pr`
//! path (B-006). Every non-routed outcome (live-dedupe suppression, cap skip,
//! failure) is audited — nothing is silently dropped (B-009).

use super::{binding::binding_project_key, build_prompt_from_issue, IncomingIssue, IntakeSource};
use crate::http::AppState;
use crate::task_runner::TaskId;
use harness_core::types::{Decision, Event, SessionId};
use harness_workflow::runtime::WorkflowRuntimeStore;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const INTAKE_ROUTING_HOOK: &str = "intake_routing";

/// Attempt to route an incoming issue to a declarative definition.
///
/// Returns `true` if the issue was handled by the declarative path (routed,
/// suppressed as a live duplicate, skipped at cap, or failed) and the caller
/// MUST NOT dispatch it to the default github path. Returns `false` only when no
/// binding matched, so the issue continues down the existing path unchanged.
pub(super) async fn route_declarative_intake(
    state: &Arc<AppState>,
    project_root: &Path,
    source: &Arc<dyn IntakeSource>,
    issue: &IncomingIssue,
) -> bool {
    let project_key = binding_project_key(project_root);
    let Some(matched) = state.intake.intake_bindings.resolve(&project_key, issue) else {
        // Unmatched: the issue falls through to the default path (B-006). The
        // default path has its own coverage-gate logging, so we do not emit a
        // per-issue "unmatched" audit event here to avoid drowning it in noise.
        return false;
    };

    let definition_id = matched.binding.definition_id.clone();
    let cap = matched.binding.max_active_instances;
    let tie = matched.tie_definition_ids.clone();

    let Some(store) = state.core.workflow_runtime_store.as_deref() else {
        audit(
            state,
            issue,
            source.name(),
            &definition_id,
            "failed",
            &tie,
            Some("workflow runtime store unavailable"),
        )
        .await;
        return true;
    };

    // One query serves both the dedupe check and the cap count (B-007, B-008).
    let nonterminal = match store
        .list_nonterminal_instances_by_definition(&definition_id, None, None)
        .await
    {
        Ok(instances) => instances,
        Err(error) => {
            audit(
                state,
                issue,
                source.name(),
                &definition_id,
                "failed",
                &tie,
                Some(&format!("dedupe/cap query failed: {error}")),
            )
            .await;
            // Matched but could not verify: skip this tick without marking
            // dispatched, so the issue is retried next tick.
            return true;
        }
    };

    // Dedupe (B-007): a live (nonterminal) instance keyed by the same external id
    // suppresses. `mark_dispatched` is deliberately NOT called, so once that
    // instance reaches a terminal state the issue becomes eligible again.
    if nonterminal
        .iter()
        .any(|instance| instance.subject.subject_key == issue.external_id)
    {
        audit(
            state,
            issue,
            source.name(),
            &definition_id,
            "suppressed_live",
            &tie,
            None,
        )
        .await;
        return true;
    }

    // Cap (B-008): at the per-binding ceiling, skip without marking dispatched so
    // the issue is naturally retried on a later tick once the count drains.
    if nonterminal.len() as u32 >= cap {
        audit(
            state,
            issue,
            source.name(),
            &definition_id,
            "skipped_at_cap",
            &tie,
            Some(&format!("{} active >= cap {cap}", nonterminal.len())),
        )
        .await;
        return true;
    }

    // Dispatch (B-005, B-011): reuse the operator declarative submission path.
    match dispatch(store, project_root, &definition_id, source, issue).await {
        Ok(task_id) => {
            if let Err(error) = source.mark_dispatched(&issue.external_id, &task_id).await {
                tracing::warn!(
                    source = source.name(),
                    external_id = issue.external_id,
                    definition_id,
                    "intake: declarative instance created but mark_dispatched failed: {error}"
                );
            }
            audit(
                state,
                issue,
                source.name(),
                &definition_id,
                "routed",
                &tie,
                None,
            )
            .await;
        }
        Err(error) => {
            source.unmark_dispatched(&issue.external_id).await;
            audit(
                state,
                issue,
                source.name(),
                &definition_id,
                "failed",
                &tie,
                Some(&error.to_string()),
            )
            .await;
        }
    }
    true
}

/// Create the declarative instance via the same submission function operators
/// use (B-011). Returns the synthetic task id on success.
async fn dispatch(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    definition_id: &str,
    source: &Arc<dyn IntakeSource>,
    issue: &IncomingIssue,
) -> anyhow::Result<TaskId> {
    let task_id = TaskId::new();
    let prompt = build_prompt_from_issue(issue);
    let record = crate::workflow_runtime_submission::record_declarative_submission(
        store,
        crate::workflow_runtime_submission::DeclarativeSubmissionRuntimeContext {
            project_root,
            definition_id,
            task_id: &task_id,
            prompt: &prompt,
            depends_on: &[],
            serialization_depends_on: &[],
            source: Some(source.name()),
            external_id: Some(&issue.external_id),
        },
    )
    .await?;
    if !record.accepted {
        anyhow::bail!(
            "workflow runtime rejected declarative intake submission for '{}': {}",
            definition_id,
            record.rejection_reason.as_deref().unwrap_or("rejected")
        );
    }
    Ok(task_id)
}

/// Record an intake routing outcome to the observe event store (B-009). Every
/// matched-but-not-routed outcome is audited so operators can see diverted,
/// suppressed, and capped issues.
#[allow(clippy::too_many_arguments)]
async fn audit(
    state: &Arc<AppState>,
    issue: &IncomingIssue,
    source: &str,
    definition_id: &str,
    outcome: &str,
    tie_definition_ids: &[String],
    detail: Option<&str>,
) {
    tracing::info!(
        source,
        external_id = issue.external_id,
        identifier = issue.identifier,
        definition_id,
        outcome,
        tie = ?tie_definition_ids,
        "intake: declarative routing outcome"
    );
    let mut event = Event::new(
        SessionId::new(),
        INTAKE_ROUTING_HOOK,
        source,
        Decision::Pass,
    );
    event.reason = Some(outcome.to_string());
    event.detail = Some(
        json!({
            "issue": issue.identifier,
            "external_id": issue.external_id,
            "source": source,
            "definition_id": definition_id,
            "outcome": outcome,
            "tie_definition_ids": tie_definition_ids,
            "detail": detail,
        })
        .to_string(),
    );
    if let Err(error) = state.observability.events.log(&event).await {
        tracing::warn!(
            definition_id,
            outcome,
            "intake: failed to record intake_routing audit event: {error}"
        );
    }
}
