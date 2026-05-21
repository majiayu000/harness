use super::{review_loop, run_test_gate};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use harness_workflow::runtime::PrFeedbackOutcome;
use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use std::sync::{Arc, Mutex};
use tokio::time::Instant;

pub(crate) struct LocalReviewReadyToMergeFeedback<'a> {
    pub(crate) issue_workflow_store:
        Option<&'a harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    pub(crate) workflow_runtime_store: Option<&'a harness_workflow::runtime::WorkflowRuntimeStore>,
    pub(crate) project_root: &'a Path,
    pub(crate) req: &'a CreateTaskRequest,
    pub(crate) pr_num: u64,
    pub(crate) pr_checks: LocalReviewPrChecks<'a>,
    pub(crate) pr_head: LocalReviewPrHead<'a>,
    pub(crate) pr_state: LocalReviewPrState<'a>,
}

pub(crate) enum LocalReviewPrChecks<'a> {
    GitHub {
        repo_slug: &'a str,
        github_token: Option<&'a str>,
        timeout_secs: u64,
        poll_interval_secs: u64,
    },
    #[cfg(test)]
    Passed,
    #[cfg(test)]
    Failed(&'a str),
}

impl LocalReviewPrChecks<'_> {
    async fn verify(&self, pr_num: u64) -> Result<(), String> {
        match self {
            LocalReviewPrChecks::GitHub {
                repo_slug,
                github_token,
                timeout_secs,
                poll_interval_secs,
            } => {
                review_loop::verify_pr_checks_green(
                    repo_slug,
                    pr_num,
                    *github_token,
                    *timeout_secs,
                    *poll_interval_secs,
                )
                .await
            }
            #[cfg(test)]
            LocalReviewPrChecks::Passed => Ok(()),
            #[cfg(test)]
            LocalReviewPrChecks::Failed(reason) => Err((*reason).to_string()),
        }
    }
}

pub(crate) enum LocalReviewPrHead<'a> {
    GitHub {
        repo_slug: &'a str,
        github_token: Option<&'a str>,
        before_review_sha: Option<&'a str>,
        before_review_error: Option<&'a str>,
        approved_review_sha: Option<&'a str>,
        approved_review_error: Option<&'a str>,
        local_fix_attempted: bool,
    },
    #[cfg(test)]
    Verified,
    #[cfg(test)]
    Failed(&'a str),
    #[cfg(test)]
    FinalFailed(&'a str),
}

impl LocalReviewPrHead<'_> {
    async fn verify_local_fix(&self, pr_num: u64) -> Result<(), String> {
        match self {
            LocalReviewPrHead::GitHub {
                repo_slug,
                before_review_sha,
                before_review_error,
                approved_review_sha,
                approved_review_error,
                local_fix_attempted,
                ..
            } => {
                if let Some(error) = before_review_error {
                    return Err(format!(
                        "GitHub PR head baseline lookup failed before local review fix: {error}"
                    ));
                }
                let before_review_sha = before_review_sha.ok_or_else(|| {
                    "GitHub PR head baseline is missing before local review fix".to_string()
                })?;
                if let Some(error) = approved_review_error {
                    return Err(format!(
                        "GitHub PR head approval lookup failed after local review: {error}"
                    ));
                }
                let approved_review_sha = approved_review_sha.ok_or_else(|| {
                    "GitHub PR head approval SHA is missing after local review".to_string()
                })?;
                if !*local_fix_attempted && approved_review_sha != before_review_sha {
                    return Err(format!(
                        "GitHub PR head changed during local review without a local fix for \
                         {repo_slug}#{pr_num}; before {before_review_sha}, approved {approved_review_sha}"
                    ));
                }
                Ok(())
            }
            #[cfg(test)]
            LocalReviewPrHead::Verified => Ok(()),
            #[cfg(test)]
            LocalReviewPrHead::Failed(reason) => Err((*reason).to_string()),
            #[cfg(test)]
            LocalReviewPrHead::FinalFailed(_) => Ok(()),
        }
    }

    async fn verify_still_reviewed_head(&self, pr_num: u64) -> Result<(), String> {
        match self {
            LocalReviewPrHead::GitHub {
                repo_slug,
                github_token,
                approved_review_sha,
                approved_review_error,
                ..
            } => {
                if let Some(error) = approved_review_error {
                    return Err(format!(
                        "GitHub PR head approval lookup failed after local review: {error}"
                    ));
                }
                let approved_review_sha = approved_review_sha.ok_or_else(|| {
                    "GitHub PR head approval SHA is missing after local review".to_string()
                })?;
                let current_sha =
                    review_loop::fetch_pr_head_sha_for_gate(repo_slug, pr_num, *github_token)
                        .await?;
                if current_sha != approved_review_sha {
                    return Err(format!(
                        "GitHub PR head changed after local review approval for \
                         {repo_slug}#{pr_num}; approved {approved_review_sha}, current {current_sha}"
                    ));
                }
                Ok(())
            }
            #[cfg(test)]
            LocalReviewPrHead::Verified => Ok(()),
            #[cfg(test)]
            LocalReviewPrHead::Failed(reason) => Err((*reason).to_string()),
            #[cfg(test)]
            LocalReviewPrHead::FinalFailed(reason) => Err((*reason).to_string()),
        }
    }
}

pub(crate) enum LocalReviewPrState<'a> {
    GitHub {
        repo_slug: &'a str,
        github_token: Option<&'a str>,
    },
    #[cfg(test)]
    Open,
    #[cfg(test)]
    Merged,
    #[cfg(test)]
    NotOpen(&'a str),
    #[cfg(test)]
    Sequence(Arc<Mutex<Vec<Result<review_loop::PrOpenOrMergedState, String>>>>),
}

impl LocalReviewPrState<'_> {
    async fn verify_open_or_merged(
        &self,
        pr_num: u64,
    ) -> Result<review_loop::PrOpenOrMergedState, String> {
        match self {
            LocalReviewPrState::GitHub {
                repo_slug,
                github_token,
            } => review_loop::verify_pr_open_or_merged(repo_slug, pr_num, *github_token).await,
            #[cfg(test)]
            LocalReviewPrState::Open => Ok(review_loop::PrOpenOrMergedState::Open),
            #[cfg(test)]
            LocalReviewPrState::Merged => Ok(review_loop::PrOpenOrMergedState::Merged),
            #[cfg(test)]
            LocalReviewPrState::NotOpen(reason) => Err((*reason).to_string()),
            #[cfg(test)]
            LocalReviewPrState::Sequence(states) => states
                .lock()
                .expect("test PR state sequence lock should not be poisoned")
                .remove(0),
        }
    }
}

async fn complete_merged_external_pr(
    store: &TaskStore,
    task_id: &TaskId,
    feedback: &LocalReviewReadyToMergeFeedback<'_>,
    turns_used: u32,
    pr_url: Option<&str>,
    task_start: Instant,
    validation_result: &'static str,
    runtime_summary: &'static str,
) -> anyhow::Result<()> {
    review_loop::record_runtime_pr_merged(
        feedback.issue_workflow_store,
        feedback.workflow_runtime_store,
        feedback.project_root,
        feedback.req,
        task_id,
        feedback.pr_num,
        pr_url,
        runtime_summary,
    )
    .await;
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Done;
        s.turn = turns_used;
        s.error =
            Some("PR merged externally; local review completion gate exited as done.".to_string());
        s.rounds.push(RoundResult::new(
            turns_used,
            "local_review_validation",
            validation_result,
            None,
            None,
            None,
        ));
        s.rounds.push(RoundResult::new(
            turns_used,
            "pr_open_gate",
            "merged",
            Some("PR was already merged externally.".to_string()),
            None,
            None,
        ));
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::Completed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
    });
    tracing::info!(
        task_id = %task_id,
        status = "done",
        turns = turns_used,
        pr_url = pr_url.unwrap_or(""),
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}

async fn fail_local_review_completion_gate(
    store: &TaskStore,
    task_id: &TaskId,
    feedback: &LocalReviewReadyToMergeFeedback<'_>,
    turns_used: u32,
    pr_url: Option<&str>,
    task_start: Instant,
    action: &'static str,
    user_error: String,
    failure: String,
    feedback_summary: &'static str,
    record_pr_feedback: bool,
) -> anyhow::Result<()> {
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.turn = turns_used;
        s.error = Some(user_error);
        s.rounds.push(RoundResult::new(
            turns_used,
            action,
            "failed",
            Some(failure.clone()),
            None,
            None,
        ));
    })
    .await?;
    if record_pr_feedback {
        review_loop::record_runtime_pr_feedback(
            feedback.issue_workflow_store,
            feedback.workflow_runtime_store,
            feedback.project_root,
            feedback.req,
            task_id,
            feedback.pr_num,
            pr_url,
            PrFeedbackOutcome::BlockingFeedback,
            feedback_summary,
        )
        .await;
    }
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: failure,
    });
    tracing::warn!(
        task_id = %task_id,
        status = "failed",
        turns = turns_used,
        pr_url = pr_url.unwrap_or(""),
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}

fn pr_ci_failure_is_actionable_feedback(failure: &str) -> bool {
    failure.contains("GitHub PR checks are not green")
}

fn pr_head_failure_is_actionable_feedback(failure: &str) -> bool {
    failure.contains("GitHub PR head did not advance") || failure.contains("GitHub PR head changed")
}

fn pr_head_after_approval_user_error(failure: &str) -> String {
    if failure.contains("GitHub PR head changed") {
        format!(
            "Local review passed, but the PR head changed after local approval while hosted review was disabled: {failure}"
        )
    } else {
        format!(
            "Local review passed, but Harness could not verify the PR head after approval while hosted review was disabled: {failure}"
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_after_local_review_without_hosted_bot(
    store: &TaskStore,
    task_id: &TaskId,
    feedback: Option<LocalReviewReadyToMergeFeedback<'_>>,
    project: &Path,
    project_config: &harness_core::config::project::ProjectConfig,
    cargo_env: &HashMap<String, String>,
    turns_used: u32,
    pr_url: Option<&str>,
    task_start: Instant,
) -> anyhow::Result<()> {
    tracing::info!("review_bot_auto_trigger disabled; running validation gate before completion");
    match run_test_gate(
        project,
        &project_config.validation.pre_push,
        project_config.validation.test_gate_timeout_secs,
        cargo_env,
    )
    .await
    {
        Ok(()) => {
            if let Some(feedback) = feedback {
                let pr_state = match feedback
                    .pr_state
                    .verify_open_or_merged(feedback.pr_num)
                    .await
                {
                    Ok(state) => state,
                    Err(failure) => {
                        fail_local_review_completion_gate(
                            store,
                            task_id,
                            &feedback,
                            turns_used,
                            pr_url,
                            task_start,
                            "pr_open_gate",
                            format!(
                                "Local review passed, but the PR is not open while hosted review was disabled: {failure}"
                            ),
                            failure,
                            "Local agent review approved, but the PR is not open.",
                            false,
                        )
                        .await?;
                        return Ok(());
                    }
                };
                if pr_state == review_loop::PrOpenOrMergedState::Merged {
                    complete_merged_external_pr(
                        store,
                        task_id,
                        &feedback,
                        turns_used,
                        pr_url,
                        task_start,
                        "passed",
                        "Local agent review approved, validation passed, and PR was already merged externally.",
                    )
                    .await?;
                    return Ok(());
                }
                if let Err(failure) = feedback.pr_head.verify_local_fix(feedback.pr_num).await {
                    fail_local_review_completion_gate(
                        store,
                        task_id,
                        &feedback,
                        turns_used,
                        pr_url,
                        task_start,
                        "pr_head_gate",
                        format!(
                            "Local review passed, but Harness could not prove the reviewed PR head is valid while hosted review was disabled: {failure}"
                        ),
                        failure.clone(),
                        "Local agent review approved, but Harness could not prove the reviewed PR head is valid.",
                        pr_head_failure_is_actionable_feedback(&failure),
                    )
                    .await?;
                    return Ok(());
                }
                if let Err(failure) = feedback.pr_checks.verify(feedback.pr_num).await {
                    let pr_state = match feedback
                        .pr_state
                        .verify_open_or_merged(feedback.pr_num)
                        .await
                    {
                        Ok(state) => state,
                        Err(state_failure) => {
                            fail_local_review_completion_gate(
                                store,
                                task_id,
                                &feedback,
                                turns_used,
                                pr_url,
                                task_start,
                                "pr_open_gate",
                                format!(
                                    "Local review passed, but the PR is not open after CI polling while hosted review was disabled: {state_failure}"
                                ),
                                state_failure,
                                "Local agent review approved, but the PR is not open after CI polling.",
                                false,
                            )
                            .await?;
                            return Ok(());
                        }
                    };
                    if pr_state == review_loop::PrOpenOrMergedState::Merged {
                        complete_merged_external_pr(
                            store,
                            task_id,
                            &feedback,
                            turns_used,
                            pr_url,
                            task_start,
                            "passed",
                            "Local agent review approved, validation passed, and PR was already merged externally.",
                        )
                        .await?;
                        return Ok(());
                    }
                    fail_local_review_completion_gate(
                        store,
                        task_id,
                        &feedback,
                        turns_used,
                        pr_url,
                        task_start,
                        "pr_ci_gate",
                        format!(
                            "Local review passed, but PR CI checks are not green while hosted review was disabled: {failure}"
                        ),
                        failure.clone(),
                        "Local agent review approved, but PR CI checks are not green.",
                        pr_ci_failure_is_actionable_feedback(&failure),
                    )
                    .await?;
                    return Ok(());
                }
                let pr_state = match feedback
                    .pr_state
                    .verify_open_or_merged(feedback.pr_num)
                    .await
                {
                    Ok(state) => state,
                    Err(failure) => {
                        fail_local_review_completion_gate(
                            store,
                            task_id,
                            &feedback,
                            turns_used,
                            pr_url,
                            task_start,
                            "pr_open_gate",
                            format!(
                                "Local review passed, but the PR is not open after CI polling while hosted review was disabled: {failure}"
                            ),
                            failure,
                            "Local agent review approved, but the PR is not open after CI polling.",
                            false,
                        )
                        .await?;
                        return Ok(());
                    }
                };
                if pr_state == review_loop::PrOpenOrMergedState::Merged {
                    complete_merged_external_pr(
                        store,
                        task_id,
                        &feedback,
                        turns_used,
                        pr_url,
                        task_start,
                        "passed",
                        "Local agent review approved, validation passed, and PR was already merged externally.",
                    )
                    .await?;
                    return Ok(());
                }
                if let Err(failure) = feedback
                    .pr_head
                    .verify_still_reviewed_head(feedback.pr_num)
                    .await
                {
                    fail_local_review_completion_gate(
                        store,
                        task_id,
                        &feedback,
                        turns_used,
                        pr_url,
                        task_start,
                        "pr_head_gate",
                        pr_head_after_approval_user_error(&failure),
                        failure.clone(),
                        "Local agent review approved, but the PR head changed after approval.",
                        pr_head_failure_is_actionable_feedback(&failure),
                    )
                    .await?;
                    return Ok(());
                }
                review_loop::record_runtime_pr_feedback(
                    feedback.issue_workflow_store,
                    feedback.workflow_runtime_store,
                    feedback.project_root,
                    feedback.req,
                    task_id,
                    feedback.pr_num,
                    pr_url,
                    PrFeedbackOutcome::ReadyToMerge,
                    "Local agent review approved, validation passed, and PR checks are green.",
                )
                .await;
            }
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = turns_used;
                s.rounds.push(RoundResult::new(
                    turns_used,
                    "local_review_validation",
                    "passed",
                    None,
                    None,
                    None,
                ));
            })
            .await?;
            store.log_event(crate::event_replay::TaskEvent::Completed {
                task_id: task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
            });
            tracing::info!(
                task_id = %task_id,
                status = "done",
                turns = turns_used,
                pr_url = pr_url.unwrap_or(""),
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
        }
        Err(failure) => {
            let feedback = if let Some(feedback) = feedback {
                if feedback
                    .pr_state
                    .verify_open_or_merged(feedback.pr_num)
                    .await
                    .is_ok_and(|state| state == review_loop::PrOpenOrMergedState::Merged)
                {
                    complete_merged_external_pr(
                        store,
                        task_id,
                        &feedback,
                        turns_used,
                        pr_url,
                        task_start,
                        "skipped_merged",
                        "Local agent review approved and PR was already merged externally; local validation failed after merge.",
                    )
                    .await?;
                    return Ok(());
                }
                Some(feedback)
            } else {
                None
            };
            let error = format!(
                "Local review passed, but validation failed while hosted review was disabled: {failure}"
            );
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = turns_used;
                s.error = Some(error.clone());
                s.rounds.push(RoundResult::new(
                    turns_used,
                    "local_review_validation",
                    "failed",
                    Some(failure.clone()),
                    None,
                    None,
                ));
            })
            .await?;
            store.log_event(crate::event_replay::TaskEvent::Failed {
                task_id: task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
                reason: error,
            });
            if let Some(feedback) = feedback {
                review_loop::record_runtime_pr_feedback(
                    feedback.issue_workflow_store,
                    feedback.workflow_runtime_store,
                    feedback.project_root,
                    feedback.req,
                    task_id,
                    feedback.pr_num,
                    pr_url,
                    PrFeedbackOutcome::BlockingFeedback,
                    "Local agent review approved, but local validation failed.",
                )
                .await;
            }
            tracing::warn!(
                task_id = %task_id,
                status = "failed",
                turns = turns_used,
                pr_url = pr_url.unwrap_or(""),
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
        }
    }
    Ok(())
}

pub(crate) async fn fail_missing_local_review_gate(
    store: &TaskStore,
    task_id: &TaskId,
    turns_used: u32,
    pr_url: Option<&str>,
) -> anyhow::Result<()> {
    let error = "Hosted review is disabled, but no local agent review approved the PR. Enable agents.review with a registered reviewer_agent or set review_bot_auto_trigger=true.".to_string();
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.turn = turns_used;
        s.error = Some(error.clone());
        s.rounds.push(RoundResult::new(
            turns_used,
            "review_gate_config",
            "failed",
            Some(format!("pr={}", pr_url.unwrap_or(""))),
            None,
            None,
        ));
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: error,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pr_head_gate_allows_noop_local_fix_round() -> anyhow::Result<()> {
        let gate = LocalReviewPrHead::GitHub {
            repo_slug: "owner/repo",
            github_token: None,
            before_review_sha: Some("same-sha"),
            before_review_error: None,
            approved_review_sha: Some("same-sha"),
            approved_review_error: None,
            local_fix_attempted: true,
        };

        assert!(gate.verify_local_fix(77).await.is_ok());

        Ok(())
    }
}
