use super::turn::ExecutedImplementation;
use super::{
    contains_worktree_collision_sentinel, parse_implementation_outcome, resolve_pushed_commit_flag,
    task_needs_pr_url, ImplementationOutcome,
};
use crate::task_executor::helpers::build_task_event;
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskFailureKind, TaskId, TaskStatus,
    TaskStore,
};
use harness_core::prompts;
use harness_core::types::Decision;
use std::path::Path;
use tokio::time::Instant;

pub(super) enum ProcessedImplementation {
    Done,
    Replan {
        issue: u64,
        plan_issue: String,
        prior_plan: Option<String>,
    },
    Proceed {
        pr_url: Option<String>,
        pr_num: u64,
        pushed_commit: bool,
        review_prep: Option<prompts::PrReviewPrepOutcome>,
    },
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn process_implementation_output(
    store: &TaskStore,
    task_id: &TaskId,
    req: &CreateTaskRequest,
    events: &harness_observe::event_store::EventStore,
    project: &Path,
    project_root: &Path,
    plan_output: Option<String>,
    issue_workflow_store: Option<&harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    workflow_runtime_store: Option<&harness_workflow::runtime::WorkflowRuntimeStore>,
    executed: ExecutedImplementation,
    task_start: Instant,
) -> anyhow::Result<ProcessedImplementation> {
    let ExecutedImplementation {
        output,
        telemetry: impl_telemetry,
    } = executed;
    // Fast-fail: if the agent observed a stale worktree managed by another harness
    // session, abort immediately. This prevents the task from pushing commits to the
    // wrong PR (issue #799).
    if contains_worktree_collision_sentinel(&output) {
        // If the agent already pushed to a PR before we detected the collision,
        // capture the URL so there is a tracked handle for cleanup.
        let collision_pr_url = match parse_implementation_outcome(&output) {
            Ok(ImplementationOutcome::ParsedPr { pr_url, .. }) => pr_url,
            _ => None,
        };
        tracing::error!(
            task_id = %task_id,
            "WorktreeCollision: agent observed stale worktree; aborting task"
        );
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Failed;
            s.failure_kind = Some(TaskFailureKind::WorkspaceLifecycle);
            s.turn = 1;
            s.pr_url = collision_pr_url.clone();
            s.error = Some(
                "WorktreeCollision: agent observed worktree managed by another harness session; reconciliation required"
                    .into(),
            );
            s.rounds.push(RoundResult::new(
                1,
                "implement",
                "worktree_collision",
                if output.is_empty() {
                    None
                } else {
                    Some(output.clone())
                },
                Some(impl_telemetry.clone()),
                None,
            ));
        })
        .await?;
        let event = build_task_event(
            task_id,
            1,
            "implement",
            "task_implement",
            Decision::Block,
            Some("worktree collision detected".to_string()),
            collision_pr_url.clone().map(|url| format!("pr_url={url}")),
            Some(impl_telemetry.clone()),
            None,
            if output.is_empty() {
                None
            } else {
                Some(output.clone())
            },
        );
        if let Err(error) = events.log(&event).await {
            tracing::warn!("failed to log task_implement event: {error}");
        }
        tracing::info!(
            task_id = %task_id,
            status = "failed",
            turns = 1,
            pr_url = ?collision_pr_url,
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(ProcessedImplementation::Done);
    }

    // Review-only tasks produce a report, not a PR.
    // Persist the output and return immediately — no PR parsing or review loop.
    let is_review_task = store.get(task_id).is_some_and(|s| {
        matches!(
            s.source.as_deref(),
            Some("periodic_review") | Some("sprint_planner")
        )
    });

    if is_review_task {
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 1;
            s.rounds.push(RoundResult::new(
                1,
                "review",
                "completed",
                if output.is_empty() {
                    None
                } else {
                    Some(output.clone())
                },
                Some(impl_telemetry.clone()),
                None,
            ));
        })
        .await?;
        let event = build_task_event(
            task_id,
            1,
            "review",
            "task_review",
            Decision::Complete,
            Some("review-only task completed".to_string()),
            None,
            Some(impl_telemetry.clone()),
            None,
            if output.is_empty() {
                None
            } else {
                Some(output.clone())
            },
        );
        if let Err(error) = events.log(&event).await {
            tracing::warn!("failed to log task_review event: {error}");
        }
        store.log_event(crate::event_replay::TaskEvent::Completed {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
        });
        tracing::info!(
            task_id = %task_id,
            status = "done",
            turns = 1,
            pr_url = tracing::field::Empty,
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(ProcessedImplementation::Done);
    }

    let (pr_url, pr_num, created_issue_num, pushed_commit, review_prep) =
        match parse_implementation_outcome(&output) {
            Ok(ImplementationOutcome::PlanIssue(plan_issue)) => {
                if let (Some(workflows), Some(issue_number)) =
                    (issue_workflow_store.as_ref(), req.issue)
                {
                    let project_id = project_root.to_string_lossy().into_owned();
                    if let Err(e) = workflows
                        .record_plan_issue_detected(
                            &project_id,
                            req.repo.as_deref(),
                            issue_number,
                            &task_id.0,
                            &plan_issue,
                        )
                        .await
                    {
                        tracing::warn!(
                            issue = issue_number,
                            task_id = %task_id.0,
                            "issue workflow PLAN_ISSUE tracking failed: {e}"
                        );
                    }
                }
                if let Some(issue_number) = req.issue {
                    return Ok(ProcessedImplementation::Replan {
                        issue: issue_number,
                        plan_issue,
                        prior_plan: plan_output.clone(),
                    });
                }
                tracing::error!(
                    task_id = %task_id,
                    plan_issue = %plan_issue,
                    "implementation returned PLAN_ISSUE; marking task failed"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.turn = 2;
                    s.error = Some(plan_issue.clone());
                    s.rounds.push(RoundResult::new(
                        1,
                        "implement",
                        "plan_issue",
                        if output.is_empty() {
                            None
                        } else {
                            Some(output.clone())
                        },
                        Some(impl_telemetry.clone()),
                        None,
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    1,
                    "implement",
                    "task_implement",
                    Decision::Block,
                    Some(plan_issue.clone()),
                    None,
                    Some(impl_telemetry.clone()),
                    None,
                    if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    },
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log task_implement event: {error}");
                }
                tracing::info!(
                    task_id = %task_id,
                    status = "failed",
                    turns = 2,
                    pr_url = tracing::field::Empty,
                    total_elapsed_secs = task_start.elapsed().as_secs(),
                    "task_completed"
                );
                return Ok(ProcessedImplementation::Done);
            }
            Ok(ImplementationOutcome::ParsedPr {
                pr_url,
                pr_num,
                created_issue_num,
                pushed_commit,
                review_prep,
            }) => (
                pr_url,
                pr_num,
                created_issue_num,
                pushed_commit,
                review_prep,
            ),
            Err(parse_err) => {
                tracing::error!(
                    task_id = %task_id,
                    parse_error = %parse_err,
                    "implementation returned malformed structured output"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.turn = 2;
                    s.error = Some(format!(
                        "implementation returned malformed structured output: {parse_err}"
                    ));
                    s.rounds.push(RoundResult::new(
                        1,
                        "implement",
                        "malformed_output",
                        if output.is_empty() {
                            None
                        } else {
                            Some(output.clone())
                        },
                        Some(impl_telemetry.clone()),
                        None,
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    1,
                    "implement",
                    "task_implement",
                    Decision::Block,
                    Some(format!("malformed structured output: {parse_err}")),
                    None,
                    Some(impl_telemetry.clone()),
                    None,
                    if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    },
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log task_implement event: {error}");
                }
                tracing::info!(
                    task_id = %task_id,
                    status = "failed",
                    turns = 2,
                    pr_url = tracing::field::Empty,
                    total_elapsed_secs = task_start.elapsed().as_secs(),
                    "task_completed"
                );
                return Ok(ProcessedImplementation::Done);
            }
        };

    if let Some(pr_number) = req.pr {
        let Some(review_prep) = review_prep.as_ref() else {
            let error = format!("PR #{pr_number} preparation produced no rebase outcome sentinel");
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error = Some(error.clone());
                s.rounds.push(RoundResult::new(
                    1,
                    "implement",
                    "missing_rebase_outcome",
                    if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    },
                    Some(impl_telemetry.clone()),
                    None,
                ));
            })
            .await?;
            let event = build_task_event(
                task_id,
                1,
                "implement",
                "task_implement",
                Decision::Block,
                Some(error.clone()),
                None,
                Some(impl_telemetry.clone()),
                None,
                if output.is_empty() {
                    None
                } else {
                    Some(output.clone())
                },
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log task_implement event: {error}");
            }
            store.log_event(crate::event_replay::TaskEvent::Failed {
                task_id: task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
                reason: "missing_rebase_outcome".to_string(),
            });
            tracing::info!(
            task_id = %task_id,
            status = "failed",
            turns = 2,
            pr_url = tracing::field::Empty,
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
            );
            return Ok(ProcessedImplementation::Done);
        };
        tracing::debug!(
            pr = pr_number,
            ?review_prep,
            "parsed pr review prep outcome"
        );
    }

    let pushed_commit =
        match resolve_pushed_commit_flag(req.pr.is_some(), pushed_commit, review_prep.as_ref()) {
            Ok(pushed_commit) => pushed_commit,
            Err(parse_err) => {
                tracing::error!(
                    task_id = %task_id,
                    parse_error = %parse_err,
                    "implementation omitted required PR-check structured output"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.turn = 2;
                    s.error = Some(parse_err.clone());
                    s.rounds.push(RoundResult::new(
                        1,
                        "implement",
                        "malformed_output",
                        if output.is_empty() {
                            None
                        } else {
                            Some(output.clone())
                        },
                        Some(impl_telemetry.clone()),
                        None,
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    1,
                    "implement",
                    "task_implement",
                    Decision::Block,
                    Some(parse_err.clone()),
                    None,
                    Some(impl_telemetry.clone()),
                    None,
                    if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    },
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log task_implement event: {error}");
                }
                tracing::info!(
                    task_id = %task_id,
                    status = "failed",
                    turns = 2,
                    pr_url = tracing::field::Empty,
                    total_elapsed_secs = task_start.elapsed().as_secs(),
                    "task_completed"
                );
                return Ok(ProcessedImplementation::Done);
            }
        };

    mutate_and_persist(store, task_id, |s| {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult::new(
            1,
            "implement",
            if let Some(review_prep) = review_prep.as_ref() {
                match review_prep {
                    prompts::PrReviewPrepOutcome::RebasePushed => "rebase_pushed",
                    prompts::PrReviewPrepOutcome::RebaseSkipped => "rebase_skipped",
                    prompts::PrReviewPrepOutcome::RebaseConflict { .. } => "rebase_conflict",
                }
            } else if pr_num.is_some() {
                "pr_created"
            } else {
                "no_pr"
            },
            if output.is_empty() {
                None
            } else {
                Some(output.clone())
            },
            Some(impl_telemetry.clone()),
            None,
        ));
    })
    .await?;

    // Back-fill (or correct) external_id for auto-fix tasks that created a
    // GitHub issue.  Dedup layers 1-3 match on external_id; without this,
    // intake re-creates a duplicate task when it receives the webhook for the
    // newly created issue.
    //
    // We compare against the current in-memory value rather than checking
    // IS NULL so that a post-run self-correction (agent emitted CREATED_ISSUE=20
    // after an earlier CREATED_ISSUE=10) is always written even when the
    // streaming path already stored the stale value.
    if let Some(issue_num) = created_issue_num {
        let final_eid = format!("issue:{issue_num}");
        let needs_backfill = store.get(task_id).is_some_and(|s| {
            s.source.as_deref() == Some("auto-fix") && s.external_id.as_deref() != Some(&final_eid)
        });
        if needs_backfill {
            if let Err(e) = store
                .overwrite_external_id_auto_fix(task_id, &final_eid)
                .await
            {
                tracing::warn!(task_id = %task_id, "failed to back-fill external_id: {e}");
            } else {
                tracing::info!(task_id = %task_id, external_id = %final_eid, "back-filled external_id for auto-fix task");
            }
        }
    }

    // Emit PrDetected event so crash recovery can reconstruct pr_url.
    if let Some(pr_url_str) = pr_url.as_deref() {
        if let (Some(workflows), Some(issue_number), Some(pr_number)) =
            (issue_workflow_store.as_ref(), req.issue, pr_num)
        {
            let project_id = project.to_string_lossy().into_owned();
            if let Err(e) = workflows
                .record_pr_detected(
                    &project_id,
                    req.repo.as_deref(),
                    issue_number,
                    &task_id.0,
                    pr_number,
                    pr_url_str,
                )
                .await
            {
                tracing::warn!(
                    issue = issue_number,
                    pr = pr_number,
                    task_id = %task_id.0,
                    "issue workflow PR binding failed: {e}"
                );
            }
        }
        if let (Some(workflow_runtime), Some(issue_number), Some(pr_number)) =
            (workflow_runtime_store.as_ref(), req.issue, pr_num)
        {
            crate::workflow_runtime_pr_feedback::record_pr_detected(
                Some(workflow_runtime),
                crate::workflow_runtime_pr_feedback::PrDetectedRuntimeContext {
                    project_root,
                    repo: req.repo.as_deref(),
                    issue_number,
                    task_id,
                    pr_number,
                    pr_url: pr_url_str,
                },
            )
            .await;
        }
        store.log_event(crate::event_replay::TaskEvent::PrDetected {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
            pr_url: pr_url_str.to_string(),
        });
    }

    // Write PR checkpoint immediately after pr_url is persisted.
    // This is the most critical checkpoint — it prevents duplicate PR creation on resume.
    if let Some(pr_url_str) = pr_url.as_deref() {
        let checkpoint_phase = match review_prep.as_ref() {
            Some(prompts::PrReviewPrepOutcome::RebasePushed) => "rebase_pushed",
            Some(prompts::PrReviewPrepOutcome::RebaseSkipped) => "rebase_skipped",
            Some(prompts::PrReviewPrepOutcome::RebaseConflict { .. }) => "rebase_conflict",
            None => "pr_created",
        };
        if let Err(e) = store
            .write_checkpoint(task_id, None, None, Some(pr_url_str), checkpoint_phase)
            .await
        {
            tracing::warn!(task_id = %task_id, "failed to write pr checkpoint: {e}");
        }
    }

    let Some(pr_num) = pr_num else {
        if output.trim().is_empty() {
            tracing::warn!(
                task_id = %task_id,
                "empty agent output: no PR created; marking failed"
            );
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error = Some("empty agent output: no PR created and no output".to_string());
            })
            .await?;
            let event = build_task_event(
                task_id,
                1,
                "implement",
                "task_implement",
                Decision::Block,
                Some("implementation produced no output".to_string()),
                None,
                Some(impl_telemetry.clone()),
                None,
                None,
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log task_implement event: {error}");
            }
            store.log_event(crate::event_replay::TaskEvent::Completed {
                task_id: task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
            });
            tracing::info!(
                task_id = %task_id,
                status = "failed",
                turns = 2,
                pr_url = tracing::field::Empty,
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
            return Ok(ProcessedImplementation::Done);
        }
        // Issue tasks and pr:N tasks must produce a PR URL. Mark Failed so the issue
        // is removed from the dispatched set by on_task_complete and can be re-queued.
        // Generic prompt-only implementation tasks may legitimately finish without a PR.
        if task_needs_pr_url(req) {
            tracing::warn!(
                task_id = %task_id,
                issue = req.issue,
                pr = req.pr,
                "no PR number found in agent output for issue/pr task; marking failed to allow requeue"
            );
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error =
                    Some("no PR number found in agent output; task requires PR_URL".to_string());
            })
            .await?;
            let event = build_task_event(
                task_id,
                1,
                "implement",
                "task_implement",
                Decision::Block,
                Some("task required PR_URL but none was produced".to_string()),
                None,
                Some(impl_telemetry.clone()),
                None,
                Some(output.clone()),
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log task_implement event: {error}");
            }
        } else {
            tracing::warn!("no PR number found in agent output; skipping review");
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = 2;
            })
            .await?;
            let event = build_task_event(
                task_id,
                1,
                "implement",
                "task_implement",
                Decision::Complete,
                Some("implementation completed without PR".to_string()),
                None,
                Some(impl_telemetry.clone()),
                None,
                Some(output.clone()),
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log task_implement event: {error}");
            }
        }
        store.log_event(crate::event_replay::TaskEvent::Completed {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
        });
        tracing::info!(
            task_id = %task_id,
            status = "done",
            turns = 2,
            pr_url = tracing::field::Empty,
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(ProcessedImplementation::Done);
    };

    let event = build_task_event(
        task_id,
        1,
        "implement",
        "task_implement",
        Decision::Complete,
        Some("implementation completed".to_string()),
        Some(format!("pr={pr_num}")),
        Some(impl_telemetry.clone()),
        None,
        if output.is_empty() {
            None
        } else {
            Some(output.clone())
        },
    );
    if let Err(error) = events.log(&event).await {
        tracing::warn!("failed to log task_implement event: {error}");
    }

    Ok(ProcessedImplementation::Proceed {
        pr_url,
        pr_num,
        pushed_commit,
        review_prep,
    })
}
