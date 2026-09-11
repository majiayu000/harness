//! Server-side verification of agent-claimed PR bindings (GH-1766, B-005).
//!
//! When an implementation activity claims a pull request, the server resolves
//! the claim through the existing GraphQL snapshot client before the reducer
//! may mint a `BindPr` command. The verdict is attached to the
//! [`ActivityResult`] as a server-reserved artifact; the reducer converts a
//! verified artifact into `verified_pr_binding` decision evidence and a
//! failure artifact into a typed blocked decision.

use crate::github_pr_snapshot::{fetch_github_pr_snapshot, GitHubPrSnapshotTarget};
use crate::http::AppState;
use harness_workflow::runtime::completion_evidence::{
    ARTIFACT_PR_BINDING_VERIFICATION_FAILED, ARTIFACT_VERIFIED_PR_BINDING,
};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus, RuntimeJob,
    WorkflowInstance,
};
use serde_json::{json, Value};
use std::sync::Arc;

const PR_BINDING_VERIFICATION_ATTEMPTS: u32 = 3;
const PR_BINDING_RETRY_DELAY_MS: u64 = 500;

/// Activities whose successful results may claim a new PR binding for a
/// `github_issue_pr` workflow in `implementing`.
const GITHUB_ISSUE_PR_BINDING_ACTIVITIES: [&str; 2] = ["implement_issue", "promote_candidate_pr"];

pub(super) fn result_claims_pr_binding(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    result: &ActivityResult,
) -> bool {
    let Some(workflow) = workflow else {
        return false;
    };
    if workflow.state != "implementing"
        || result.status != ActivityStatus::Succeeded
        || claimed_pull_request(result).is_none()
    {
        return false;
    }
    let activity = super::data_helpers::activity_name(job);
    match workflow.definition_id.as_str() {
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID => {
            GITHUB_ISSUE_PR_BINDING_ACTIVITIES.contains(&activity.as_str())
        }
        // GH-2050: prompt_task BindPr must receive the same server verification
        // attach path as github_issue_pr (GH-1766).
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID => {
            activity == harness_workflow::runtime::PROMPT_TASK_IMPLEMENT_ACTIVITY
        }
        _ => false,
    }
}

/// Verify the claimed PR against GitHub and attach the verdict artifact.
///
/// Definitive negative answers (missing PR, closed PR, wrong repository,
/// mismatched head) attach a failure artifact that the reducer turns into a
/// typed blocked decision. Transport errors after bounded retries rewrite
/// the result as a retryable external-dependency failure so the standard
/// retry policy bounds the outage, rather than blocking the workflow.
pub(super) async fn attach_pr_binding_verification(
    state: &Arc<AppState>,
    workflow: &WorkflowInstance,
    result: ActivityResult,
) -> ActivityResult {
    let Some((pr_number, pr_url)) = claimed_pull_request(&result) else {
        return result;
    };
    let Some(repo_slug) = resolve_expected_repo_slug(workflow).await else {
        return result.with_artifact(ActivityArtifact::new(
            ARTIFACT_PR_BINDING_VERIFICATION_FAILED,
            json!({
                "outcome": "repository_unresolvable",
                "detail": "the workflow records no trusted repository slug; refusing to treat the claimed pr_url as ownership proof",
                "claimed_pr_number": pr_number,
                "claimed_pr_url": pr_url,
            }),
        ));
    };
    let target = match GitHubPrSnapshotTarget::new(repo_slug.clone(), pr_number) {
        Ok(target) => target,
        Err(error) => {
            return result.with_artifact(ActivityArtifact::new(
                ARTIFACT_PR_BINDING_VERIFICATION_FAILED,
                json!({
                    "outcome": "invalid_target",
                    "detail": error.to_string(),
                    "claimed_pr_number": pr_number,
                    "claimed_pr_url": pr_url,
                }),
            ));
        }
    };

    let token = state.core.server.config.server.github_token.clone();
    let mut last_error = String::new();
    for attempt in 0..PR_BINDING_VERIFICATION_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(
                PR_BINDING_RETRY_DELAY_MS * u64::from(attempt),
            ))
            .await;
        }
        match fetch_github_pr_snapshot(&target, token.as_deref()).await {
            Ok(artifacts) => {
                let verdict = evaluate_pr_binding_snapshot(
                    pr_number,
                    &repo_slug,
                    expected_head_ref(workflow).as_deref(),
                    &artifacts.normalized_snapshot,
                );
                return match verdict {
                    PrBindingVerdict::Verified(payload) => result.with_artifact(
                        ActivityArtifact::new(ARTIFACT_VERIFIED_PR_BINDING, payload),
                    ),
                    PrBindingVerdict::Rejected(payload) => result.with_artifact(
                        ActivityArtifact::new(ARTIFACT_PR_BINDING_VERIFICATION_FAILED, payload),
                    ),
                };
            }
            Err(error) => {
                last_error = error.to_string();
                // "Could not resolve" from GraphQL is a definitive negative,
                // not a transport failure: the PR does not exist.
                if last_error
                    .to_ascii_lowercase()
                    .contains("could not resolve")
                {
                    return result.with_artifact(ActivityArtifact::new(
                        ARTIFACT_PR_BINDING_VERIFICATION_FAILED,
                        json!({
                            "outcome": "pr_not_found",
                            "detail": last_error,
                            "claimed_pr_number": pr_number,
                            "claimed_pr_url": pr_url,
                            "repo": repo_slug,
                        }),
                    ));
                }
            }
        }
    }

    let mut failed = result;
    failed.status = ActivityStatus::Failed;
    failed.summary = format!(
        "Server could not verify the claimed pull request binding for {repo_slug}#{pr_number} \
         after {PR_BINDING_VERIFICATION_ATTEMPTS} attempts."
    );
    failed.error = Some(format!(
        "pr binding verification transport failure: {last_error}"
    ));
    failed.error_kind = Some(ActivityErrorKind::ExternalDependency);
    failed
}

pub(super) enum PrBindingVerdict {
    Verified(Value),
    Rejected(Value),
}

/// Pure verdict over a normalized server snapshot: exists, open, right
/// repository, and (when the workflow records an expected branch) a matching
/// head ref.
pub(super) fn evaluate_pr_binding_snapshot(
    claimed_pr_number: u64,
    expected_repo: &str,
    expected_head_ref: Option<&str>,
    snapshot: &Value,
) -> PrBindingVerdict {
    let snapshot_number = snapshot.get("pr_number").and_then(Value::as_u64);
    if snapshot_number != Some(claimed_pr_number) {
        return PrBindingVerdict::Rejected(json!({
            "outcome": "pr_number_mismatch",
            "detail": format!(
                "snapshot resolved PR {snapshot_number:?} but the activity claimed {claimed_pr_number}"
            ),
            "claimed_pr_number": claimed_pr_number,
        }));
    }
    let snapshot_repo = snapshot.get("repo").and_then(Value::as_str).unwrap_or("");
    if !snapshot_repo.eq_ignore_ascii_case(expected_repo) {
        return PrBindingVerdict::Rejected(json!({
            "outcome": "repository_mismatch",
            "detail": format!(
                "snapshot repository `{snapshot_repo}` does not match expected `{expected_repo}`"
            ),
            "claimed_pr_number": claimed_pr_number,
        }));
    }
    let state = snapshot.get("state").and_then(Value::as_str).unwrap_or("");
    if !state.eq_ignore_ascii_case("open") {
        return PrBindingVerdict::Rejected(json!({
            "outcome": "pr_not_open",
            "detail": format!("pull request state is `{state}`, expected OPEN"),
            "claimed_pr_number": claimed_pr_number,
            "state": state,
        }));
    }
    let head_ref = snapshot.get("head_ref").and_then(Value::as_str);
    if let Some(expected) = expected_head_ref {
        if head_ref != Some(expected) {
            return PrBindingVerdict::Rejected(json!({
                "outcome": "head_ref_mismatch",
                "detail": format!(
                    "pull request head `{}` does not match the workflow branch `{expected}`",
                    head_ref.unwrap_or("unknown")
                ),
                "claimed_pr_number": claimed_pr_number,
            }));
        }
    }
    PrBindingVerdict::Verified(json!({
        "pr_number": claimed_pr_number,
        "repo": expected_repo,
        "head_oid": snapshot.get("head_oid").cloned().unwrap_or(Value::Null),
        "head_ref": head_ref,
        "head_ref_verified": expected_head_ref.is_some(),
        "observed_at": snapshot.get("observed_at").cloned().unwrap_or(Value::Null),
        "snapshot_source": snapshot
            .get("snapshot_source")
            .cloned()
            .unwrap_or(Value::Null),
    }))
}

fn claimed_pull_request(result: &ActivityResult) -> Option<(u64, String)> {
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

/// Trusted repository ownership for PR-binding verification (GH-2054).
///
/// Only server-trusted sources are accepted: `workflow.data.repo`, or the
/// project filesystem remote resolved from server-owned `project_id`.
/// Parsing the agent-claimed `pr_url` must not supply the expected slug.
async fn resolve_expected_repo_slug(workflow: &WorkflowInstance) -> Option<String> {
    if let Some(repo) = expected_repo_slug(workflow) {
        return Some(repo);
    }
    let project_id = workflow.data.get("project_id").and_then(Value::as_str)?;
    crate::workflow_runtime_pr_feedback::pr_detection::detect_repo_slug(std::path::Path::new(
        project_id,
    ))
    .await
}

fn expected_repo_slug(workflow: &WorkflowInstance) -> Option<String> {
    workflow
        .data
        .get("repo")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(str::to_string)
}

fn expected_head_ref(workflow: &WorkflowInstance) -> Option<String> {
    ["workspace_branch", "branch", "head_ref"]
        .iter()
        .find_map(|key| workflow.data.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .filter(|branch| !branch.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(pr_number: u64, repo: &str, state: &str, head_ref: &str) -> Value {
        json!({
            "pr_number": pr_number,
            "repo": repo,
            "state": state,
            "head_ref": head_ref,
            "head_oid": "abc123",
            "observed_at": "2026-07-26T00:00:00Z",
            "snapshot_source": "server_github_graphql",
        })
    }

    #[test]
    fn valid_open_pr_verifies_with_binding_payload() {
        let verdict = evaluate_pr_binding_snapshot(
            42,
            "octo/repo",
            Some("feat/gh42"),
            &snapshot(42, "octo/repo", "OPEN", "feat/gh42"),
        );
        let PrBindingVerdict::Verified(payload) = verdict else {
            panic!("expected verified verdict");
        };
        assert_eq!(payload["pr_number"], 42);
        assert_eq!(payload["head_oid"], "abc123");
        assert_eq!(payload["head_ref_verified"], true);
        assert_eq!(payload["snapshot_source"], "server_github_graphql");
    }

    #[test]
    fn closed_pr_is_rejected() {
        let verdict = evaluate_pr_binding_snapshot(
            42,
            "octo/repo",
            None,
            &snapshot(42, "octo/repo", "CLOSED", "feat/gh42"),
        );
        let PrBindingVerdict::Rejected(payload) = verdict else {
            panic!("expected rejected verdict");
        };
        assert_eq!(payload["outcome"], "pr_not_open");
    }

    #[test]
    fn wrong_repository_is_rejected() {
        let verdict = evaluate_pr_binding_snapshot(
            42,
            "octo/repo",
            None,
            &snapshot(42, "fork/repo", "OPEN", "feat/gh42"),
        );
        let PrBindingVerdict::Rejected(payload) = verdict else {
            panic!("expected rejected verdict");
        };
        assert_eq!(payload["outcome"], "repository_mismatch");
    }

    #[test]
    fn mismatched_head_is_rejected_only_when_branch_known() {
        let verdict = evaluate_pr_binding_snapshot(
            42,
            "octo/repo",
            Some("feat/gh42"),
            &snapshot(42, "octo/repo", "OPEN", "unrelated-branch"),
        );
        let PrBindingVerdict::Rejected(payload) = verdict else {
            panic!("expected rejected verdict");
        };
        assert_eq!(payload["outcome"], "head_ref_mismatch");

        let verdict = evaluate_pr_binding_snapshot(
            42,
            "octo/repo",
            None,
            &snapshot(42, "octo/repo", "OPEN", "unrelated-branch"),
        );
        let PrBindingVerdict::Verified(payload) = verdict else {
            panic!("unknown branch cannot fail the head check");
        };
        assert_eq!(payload["head_ref_verified"], false);
    }

    #[test]
    fn pr_number_mismatch_is_rejected() {
        let verdict = evaluate_pr_binding_snapshot(
            42,
            "octo/repo",
            None,
            &snapshot(41, "octo/repo", "OPEN", "feat/gh42"),
        );
        let PrBindingVerdict::Rejected(payload) = verdict else {
            panic!("expected rejected verdict");
        };
        assert_eq!(payload["outcome"], "pr_number_mismatch");
    }

    #[test]
    fn expected_repo_slug_requires_trusted_workflow_repo() {
        let mut wf = workflow(
            harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
            "implementing",
        );
        assert_eq!(expected_repo_slug(&wf), None);

        wf = wf.with_server_data(json!({ "repo": "octo/repo" }));
        assert_eq!(expected_repo_slug(&wf), Some("octo/repo".to_string()));

        wf = workflow(
            harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
            "implementing",
        )
        .with_server_data(json!({ "repo": "   " }));
        assert_eq!(expected_repo_slug(&wf), None);
    }

    #[tokio::test]
    async fn resolve_expected_repo_slug_uses_project_filesystem_when_repo_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).expect("mkdir .git");
        std::fs::write(
            git.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/octo/from-project.git\n",
        )
        .expect("write git config");

        let wf = workflow(
            harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
            "implementing",
        )
        .with_server_data(json!({ "project_id": dir.path().to_string_lossy() }));

        assert_eq!(
            resolve_expected_repo_slug(&wf).await,
            Some("octo/from-project".to_string())
        );
    }

    #[tokio::test]
    async fn resolve_expected_repo_slug_prefers_persisted_repo_over_project_detect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).expect("mkdir .git");
        std::fs::write(
            git.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/octo/from-project.git\n",
        )
        .expect("write git config");

        let wf = workflow(
            harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
            "implementing",
        )
        .with_server_data(json!({
            "repo": "octo/persisted",
            "project_id": dir.path().to_string_lossy(),
        }));

        assert_eq!(
            resolve_expected_repo_slug(&wf).await,
            Some("octo/persisted".to_string())
        );
    }

    fn runtime_job(activity: &str) -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "test",
            json!({ "activity": activity }),
        )
    }

    fn workflow(definition_id: &str, state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            definition_id,
            1,
            state,
            harness_workflow::runtime::WorkflowSubject::new("subject", "key-1"),
        )
        .with_id("workflow-1")
    }

    fn pull_request_result(activity: &str) -> ActivityResult {
        ActivityResult::succeeded(activity, "claimed a PR").with_artifact(
            harness_workflow::runtime::ActivityArtifact::new(
                "pull_request",
                json!({
                    "pr_number": 42,
                    "pr_url": "https://github.com/octo/repo/pull/42",
                }),
            ),
        )
    }

    #[test]
    fn result_claims_pr_binding_covers_prompt_task_implement_prompt() {
        let job = runtime_job("implement_prompt");
        let wf = workflow(
            harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
            "implementing",
        );
        let result = pull_request_result("implement_prompt");
        assert!(result_claims_pr_binding(&job, Some(&wf), &result));
    }

    #[test]
    fn result_claims_pr_binding_still_covers_github_issue_implement_issue() {
        let job = runtime_job("implement_issue");
        let wf = workflow(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            "implementing",
        );
        let result = pull_request_result("implement_issue");
        assert!(result_claims_pr_binding(&job, Some(&wf), &result));
    }

    #[test]
    fn result_claims_pr_binding_ignores_unrelated_definitions() {
        let job = runtime_job("implement_prompt");
        let wf = workflow("quality_gate", "implementing");
        let result = pull_request_result("implement_prompt");
        assert!(!result_claims_pr_binding(&job, Some(&wf), &result));
    }
}
