use super::helpers::update_status;
use super::local_review_completion::{
    complete_after_local_review_without_hosted_bot, fail_missing_local_review_gate,
    fail_review_provider_gate, LocalReviewPrChecks, LocalReviewPrHead, LocalReviewPrState,
    LocalReviewReadyToMergeFeedback,
};
use super::pr_detection::{detect_repo_slug, find_existing_pr_for_issue_with_token};
pub(super) use super::run_policy::{
    effective_agent_review_round_limit, effective_hosted_review_round_limit,
    initial_hosted_review_wait_secs, local_review_pr_check_timeout_secs, review_repo_slug,
    should_run_issue_triage,
};
use super::{
    agent_review, agent_review_provider_gate, gates, implement_pipeline, non_implementation,
    review_loop, triage_pipeline,
};
use crate::task_executor::SharedTurnInterceptors;
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, TaskId, TaskKind, TaskStatus, TaskStore,
};
use anyhow::Context;
use harness_core::agent::CodeAgent;
use harness_core::config::agents::ReviewStrategy;
use harness_core::review::ReviewGateDecision;
use harness_core::{config::project::load_project_config, prompts};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration, Instant};

/// RAII guard that removes the per-task Cargo target directory on drop.
/// This ensures cleanup regardless of how `run_task` exits (success, error,
/// or timeout), preventing disk exhaustion from accumulated build artifacts.
struct TaskTargetDir(PathBuf);

impl Drop for TaskTargetDir {
    fn drop(&mut self) {
        if self.0.exists() {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                tracing::warn!(
                    path = %self.0.display(),
                    "failed to remove per-task cargo target dir: {e}"
                );
            }
        }
    }
}

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: Option<&dyn CodeAgent>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: SharedTurnInterceptors,
    req: &CreateTaskRequest,
    project: PathBuf,
    // Canonical project root used for project_id derivation in issue workflow records.
    // Distinct from `project` when workspace isolation is active (worktree != canonical root).
    project_root: PathBuf,
    server_config: &harness_core::config::HarnessConfig,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    // Accumulated turn count from previous transient-retry attempts.
    // Ensures the max_turns budget is global across the full task lifecycle,
    // not reset on each retry (fix for budget-reset-on-retry bug).
    turns_used_acc: &mut u32,
) -> anyhow::Result<()> {
    let task_start = Instant::now();

    if !project.exists() {
        anyhow::bail!("project_root does not exist: {}", project.display());
    }

    // Set CARGO_TARGET_DIR to a per-task temp path so parallel agents running
    // cargo check/test simultaneously do not contend on the same build directory.
    // A per-project path caused `.cargo-lock` contention and build failures when
    // two tasks targeted the same project concurrently (issue #488).
    let task_target = std::env::temp_dir()
        .join("harness-cargo-targets")
        .join(task_id.as_str());
    let cargo_env: HashMap<String, String> = [(
        "CARGO_TARGET_DIR".to_string(),
        task_target.display().to_string(),
    )]
    .into();
    // Guard ensures the directory is removed when run_task exits, regardless of
    // the exit path (success, validation failure, timeout, or review exhaustion).
    let _task_target_guard = TaskTargetDir(task_target);

    let project_config = load_project_config(&project).with_context(|| {
        format!(
            "failed to load project config for task {} at {}",
            task_id.as_str(),
            project.display()
        )
    })?;
    let resolved = harness_core::config::resolve::resolve_config(server_config, &project_config);
    let git = Some(&project_config.git);
    let repo_slug = detect_repo_slug(&project)
        .await
        .unwrap_or_else(|| "{owner}/{repo}".to_string());
    let task_kind = store
        .get(task_id)
        .map(|state| state.task_kind)
        .unwrap_or_else(|| req.task_kind());
    let effective_max_turns: Option<u32> = req.max_turns.or(server_config.concurrency.max_turns);
    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);

    if matches!(task_kind, TaskKind::Review | TaskKind::Planner) {
        let mut turns_used = *turns_used_acc;
        non_implementation::run_non_implementation_task(
            store,
            task_id,
            task_kind,
            agent,
            req,
            &project,
            server_config,
            interceptors.as_ref(),
            &events,
            &skills,
            &cargo_env,
            turn_timeout,
            effective_max_turns,
            &mut turns_used,
            turns_used_acc,
            task_start,
        )
        .await?;
        return Ok(());
    }

    // --- Checkpoint-based resume detection ---
    // Load checkpoint and task state to determine if we can skip phases.
    // This is the duplicate-PR prevention gate: if the task already has a PR,
    // we skip triage/plan/implement and jump directly to agent review.
    //
    // Check task store for an existing pr_url first — this survives checkpoint
    // read failures (e.g. transient SQLite contention) and lets us safely
    // resume review even when the checkpoint row is temporarily unreadable.
    let task_state = store.get(task_id);
    let task_pr_url: Option<String> = task_state.as_ref().and_then(|t| t.pr_url.clone());
    let checkpoint = match store.load_checkpoint(task_id).await {
        Ok(cp) => cp,
        Err(e) => {
            if task_pr_url.is_some() {
                // Task state already records a pr_url — safe to resume review
                // without the checkpoint; log the failure for observability.
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "checkpoint load failed but task already has pr_url; resuming review without checkpoint"
                );
                None
            } else {
                // No pr_url in task state — fail closed to prevent duplicate PR.
                return Err(e).with_context(|| {
                    format!(
                        "failed to load checkpoint for task {}; aborting to prevent duplicate PR",
                        task_id
                    )
                });
            }
        }
    };
    let resumed_review_prep = task_state
        .as_ref()
        .and_then(|task| gates::review_prep_from_rounds(&task.rounds))
        .or_else(|| {
            checkpoint
                .as_ref()
                .and_then(|c| gates::review_prep_from_checkpoint_phase(&c.last_phase))
        });
    let resumed_pr_url: Option<String> =
        task_pr_url.or_else(|| checkpoint.as_ref().and_then(|c| c.pr_url.clone()));
    // Capture before `resumed_pr_url` is moved into run_implement_phase.
    // Also covers fresh pr:N tasks from webhook (req.pr is set but no checkpoint pr_url yet).
    let mut was_resumed_pr = resumed_pr_url.is_some() || req.pr.is_some();
    let resumed_plan: Option<String> = checkpoint.as_ref().and_then(|c| c.plan_output.clone());

    // --- Pipeline: Triage → Plan → Implement ---
    // For issue-based tasks without an existing PR, run triage first.
    // Triage decides whether to skip planning or go through a plan phase.
    // Checkpoint overrides: if a plan was saved, skip the pipeline entirely.
    let (plan_output, triage_complexity, pipeline_turns) = if resumed_pr_url.is_some() {
        // PR already exists — skip triage/plan entirely.
        (None, prompts::TriageComplexity::Medium, 0u32)
    } else if let Some(plan) = resumed_plan {
        // Plan checkpoint found — use saved plan, skip triage/plan pipeline.
        tracing::info!(task_id = %task_id, "checkpoint resume: using saved plan, skipping triage/plan");
        (Some(plan), prompts::TriageComplexity::Medium, 0u32)
    } else if let Some(issue) = req.issue {
        // Only triage fresh issues (no existing PR to continue).
        let has_existing_pr = find_existing_pr_for_issue_with_token(
            &project,
            issue,
            server_config.server.github_token.as_deref(),
        )
        .await
        .with_context(|| format!("failed to check for an existing PR for issue #{issue}"))?
        .is_some();
        if has_existing_pr {
            // Fresh issue task reusing an existing PR — treat as resumed for conflict gating.
            was_resumed_pr = true;
            (None, prompts::TriageComplexity::Medium, 0u32)
        } else if !should_run_issue_triage(req.skip_triage, has_existing_pr) {
            tracing::info!(
                task_id = %task_id,
                issue,
                "issue request opted to skip triage/plan pipeline"
            );
            (None, prompts::TriageComplexity::Medium, 0u32)
        } else {
            match triage_pipeline::run_triage_plan_pipeline(
                agent,
                store,
                task_id,
                issue,
                &repo_slug,
                server_config.server.github_token.as_deref(),
                project_config.triage.as_ref(),
                &cargo_env,
                &project,
                req,
                &skills,
                &events,
            )
            .await?
            {
                triage_pipeline::TriagePlanPipelineOutcome::Continue {
                    plan_output,
                    complexity,
                    turns,
                } => (plan_output, complexity, turns),
                triage_pipeline::TriagePlanPipelineOutcome::Skipped => return Ok(()),
            }
        }
    } else {
        // Planning gate (task_runner) may have forced TaskPhase::Plan for a
        // complex prompt-only task.  Check the stored phase so the gate has
        // real effect rather than silently falling through to Implement.
        let forced_plan = store
            .get(task_id)
            .map(|s| s.phase == crate::task_runner::TaskPhase::Plan)
            .unwrap_or(false);
        if forced_plan && req.issue.is_none() && req.pr.is_none() {
            // Set to Planning so operators can see the agent is actively working.
            // Planning is in resumable_statuses, so a crash here will be caught by
            // startup recovery: no pr_url/plan checkpoint → mark failed, re-queue manually.
            update_status(store, task_id, TaskStatus::Planning, 0).await?;
            triage_pipeline::run_plan_for_prompt(
                agent, store, task_id, &cargo_env, &project, req, &skills, &events,
            )
            .await?
        } else {
            (None, prompts::TriageComplexity::Medium, 0u32)
        }
    };

    // Derive dynamic parameters from triage complexity.
    // Triage provides a default only; explicit request max_rounds always wins.
    // Project review_max_rounds is scoped to hosted review-bot polling so Gemini
    // pacing overrides do not cap local agent review.
    // Low complexity no longer skips agent review to preserve the review gate.
    let (triage_default_rounds, skip_agent_review) = match triage_complexity {
        prompts::TriageComplexity::Low => (2u32, false),
        prompts::TriageComplexity::Medium => (8u32, false),
        prompts::TriageComplexity::High => (8u32, false),
    };
    let agent_review_max_rounds = effective_agent_review_round_limit(
        req.max_rounds,
        resolved.review.max_rounds,
        triage_default_rounds,
    );
    let hosted_review_max_rounds = effective_hosted_review_round_limit(
        req.max_rounds,
        resolved.review_max_rounds,
        resolved.review.max_rounds,
        triage_default_rounds,
    );
    let effective_max_rounds = hosted_review_max_rounds;
    let mut effective_review_config = resolved.review.clone();
    effective_review_config.max_rounds = agent_review_max_rounds;
    if let Some(max_rounds) = req.max_rounds {
        effective_review_config.codex_agent_review.max_rounds = max_rounds;
    }
    let review_config = &effective_review_config;
    // max_turns: per-request override wins; global config is the fallback.
    // Counts every agent API call (impl + validation retries + review rounds).
    // Start from accumulated turns (prior transient-retry attempts + pipeline phases)
    // so the budget is global across the full task lifecycle.
    let mut turns_used: u32 = *turns_used_acc + pipeline_turns;
    *turns_used_acc = turns_used;
    let jaccard_threshold = server_config.concurrency.loop_jaccard_threshold;
    tracing::info!(
        task_id = %task_id,
        ?triage_complexity,
        agent_review_max_rounds,
        hosted_review_max_rounds,
        skip_agent_review,
        ?effective_max_turns,
        "triage complexity applied"
    );

    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);

    if let (Some(workflows), Some(issue_number)) = (issue_workflow_store.as_ref(), req.issue) {
        let project_id = project_root.to_string_lossy().into_owned();
        if let Err(e) = workflows
            .record_implement_started(&project_id, req.repo.as_deref(), issue_number, &task_id.0)
            .await
        {
            tracing::warn!(
                issue = issue_number,
                task_id = %task_id.0,
                "issue workflow implement-start tracking failed: {e}"
            );
        }
    }

    let mut current_plan_output = plan_output;
    let mut replan_attempted = false;
    let (pr_url, pr_num, implementation_pushed_commit, review_prep, context_items) = loop {
        let outcome = implement_pipeline::run_implement_phase(
            store,
            task_id,
            agent,
            req,
            server_config,
            &project_config,
            review_config,
            &interceptors,
            &events,
            &skills,
            &cargo_env,
            git,
            &repo_slug,
            &project,
            &project_root,
            current_plan_output.clone(),
            resumed_pr_url.clone(),
            resumed_review_prep.clone(),
            issue_workflow_store.clone(),
            workflow_runtime_store.clone(),
            turn_timeout,
            effective_max_turns,
            &mut turns_used,
            turns_used_acc,
            task_start,
        )
        .await?;

        match outcome {
            implement_pipeline::ImplementOutcome::Done => return Ok(()),
            implement_pipeline::ImplementOutcome::Proceed {
                pr_url,
                pr_num,
                implementation_pushed_commit,
                review_prep,
                context_items,
                ..
            } => {
                break (
                    pr_url,
                    pr_num,
                    implementation_pushed_commit,
                    review_prep,
                    context_items,
                )
            }
            implement_pipeline::ImplementOutcome::Replan {
                issue,
                plan_issue,
                prior_plan,
            } => {
                let workflow_cfg = harness_core::config::workflow::load_workflow_config(&project)
                    .unwrap_or_default();
                let turn_budget_exhausted =
                    effective_max_turns.is_some_and(|max| turns_used >= max);
                let workflow_action = crate::workflow_runtime_plan_issue::decide_plan_issue(
                    workflow_runtime_store.clone(),
                    crate::workflow_runtime_plan_issue::PlanIssueRuntimeContext {
                        project_root: &project_root,
                        repo: req.repo.as_deref(),
                        issue_number: issue,
                        task_id,
                        plan_issue: &plan_issue,
                        force_execute: req.force_execute,
                        auto_replan_on_plan_issue: workflow_cfg
                            .issue_workflow
                            .auto_replan_on_plan_issue,
                        replan_already_attempted: replan_attempted,
                        turn_budget_exhausted,
                    },
                )
                .await;

                match workflow_action {
                    crate::workflow_runtime_plan_issue::PlanIssueRuntimeAction::Block { error } => {
                        mutate_and_persist(store, task_id, |s| {
                            s.status = TaskStatus::Failed;
                            s.error = Some(error.clone());
                        })
                        .await?;
                        return Ok(());
                    }
                    crate::workflow_runtime_plan_issue::PlanIssueRuntimeAction::ForceContinue => {
                        let forced_plan = match prior_plan.clone().or(current_plan_output.clone()) {
                            Some(plan) => format!(
                                "{plan}\n\nExecution override: this issue is force_execute.\n\
                                 Previous plan concern:\n{}",
                                prompts::wrap_external_data(&plan_issue)
                            ),
                            None => format!(
                                "Execution override: this issue is force_execute.\n\
                                 Previous plan concern:\n{}",
                                prompts::wrap_external_data(&plan_issue)
                            ),
                        };
                        current_plan_output = Some(forced_plan);
                    }
                    crate::workflow_runtime_plan_issue::PlanIssueRuntimeAction::RunReplan => {
                        let new_plan = triage_pipeline::run_replan_for_issue(
                            agent,
                            store,
                            task_id,
                            issue,
                            &repo_slug,
                            server_config.server.github_token.as_deref(),
                            prior_plan.as_deref().or(current_plan_output.as_deref()),
                            &plan_issue,
                            &cargo_env,
                            &project,
                            req,
                            &skills,
                            &events,
                        )
                        .await?;
                        turns_used += 1;
                        *turns_used_acc = turns_used;
                        crate::workflow_runtime_plan_issue::record_replan_completed(
                            workflow_runtime_store.clone(),
                            &project_root,
                            req.repo.as_deref(),
                            issue,
                            task_id,
                        )
                        .await;
                        current_plan_output = Some(new_plan);
                    }
                }
                replan_attempted = true;
            }
        }
    };

    // Gate A: require pr_url when the implement phase extracted a pr_num.
    // A null pr_url means URL parsing failed; mark Failed so the dedup index
    // is not poisoned with a task that never produced a usable PR reference.
    if pr_url.is_none() {
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Failed;
            s.error = Some(format!(
                "pr:{pr_num} produced no detectable pr_url; dedup unblocked"
            ));
        })
        .await?;
        return Ok(());
    }

    let mut rebase_pushed = match gates::review_entry_decision(pr_num, review_prep.as_ref()) {
        gates::ReviewEntryDecision::Proceed { rebase_pushed } => rebase_pushed,
        gates::ReviewEntryDecision::FailConflict { error, paths_csv } => {
            gates::fail_rebase_conflict(store, task_id, &events, pr_num, error, paths_csv).await?;
            tracing::info!(
                task_id = %task_id,
                status = "failed",
                turns = turns_used,
                pr = pr_num,
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
            return Ok(());
        }
    };

    if review_prep.is_none() && was_resumed_pr {
        match gates::run_resumed_pr_conflict_gate(
            store,
            task_id,
            agent,
            req,
            &project,
            pr_num,
            &repo_slug,
            &context_items,
            interceptors.as_ref(),
            &events,
            &cargo_env,
            turn_timeout,
            effective_max_turns,
            &mut turns_used,
            turns_used_acc,
        )
        .await?
        {
            gates::ConflictGateOutcome::Clean => {
                rebase_pushed = false;
            }
            gates::ConflictGateOutcome::RebasePushed => {
                rebase_pushed = true;
            }
            gates::ConflictGateOutcome::Failed => return Ok(()),
        }
    }

    let repo_slug_for_review = review_repo_slug(pr_url.as_deref(), &repo_slug);
    let requires_hosted_review =
        agent_review_provider_gate::requires_hosted_review_loop(review_config);
    let should_probe_pr_head = !requires_hosted_review && pr_num > 0;
    let local_review_head_before = if should_probe_pr_head {
        Some(
            review_loop::fetch_pr_head_sha_for_gate(
                &repo_slug_for_review,
                pr_num,
                server_config.server.github_token.as_deref(),
            )
            .await,
        )
    } else {
        None
    };

    // Agent review loop (if enabled and reviewer available, and not skipped by triage complexity)
    let mut agent_pushed_commit = false;
    let mut local_review_head_approved: Option<Result<String, String>> = None;
    let mut local_review_reports = Vec::new();
    let enforce_provider_gate = review_config.enabled
        && req.source.as_deref() != Some("gc_adopt")
        && review_config.strategy != ReviewStrategy::LegacyHostedBot;
    if enforce_provider_gate
        && !skip_agent_review
        && agent_review_provider_gate::should_run_codex_agent_review(review_config)
    {
        if let Some(reviewer) = reviewer {
            tracing::info!(pr_url = %pr_url.as_deref().unwrap_or(""), "starting agent review");
            let codex_agent_review_config =
                agent_review_provider_gate::codex_agent_review_config(review_config);
            let review_head_probe =
                should_probe_pr_head.then_some(agent_review::ReviewHeadProbe::GitHub {
                    repo_slug: &repo_slug_for_review,
                    pr_num,
                    github_token: server_config.server.github_token.as_deref(),
                });
            let (review_ok, fix_requires_pr_head_advance, approved_review_head, provider_report) =
                agent_review::run_agent_review(
                    store,
                    task_id,
                    agent,
                    reviewer,
                    &codex_agent_review_config,
                    &context_items,
                    &project,
                    interceptors.as_ref(),
                    turn_timeout,
                    pr_url.as_deref().unwrap_or(""),
                    project_config.review_type.as_str(),
                    &events,
                    &skills,
                    &cargo_env,
                    effective_max_turns,
                    review_head_probe,
                    &mut turns_used,
                )
                .await?;
            if let Some(report) = provider_report {
                local_review_reports.push(report);
            }
            *turns_used_acc = turns_used;
            if !review_ok {
                return Ok(());
            }
            agent_pushed_commit = fix_requires_pr_head_advance;
            local_review_head_approved = approved_review_head;
        } else {
            tracing::warn!("agent review enabled but no reviewer agent configured; skipping");
        }
    }
    if enforce_provider_gate
        && agent_review_provider_gate::should_run_codex_cli_review(review_config)
    {
        let provider_report = agent_review_provider_gate::run_codex_cli_review_provider(
            store,
            task_id,
            agent_review_provider_gate::CodexCliReviewProviderRequest {
                review_config,
                context_items: &context_items,
                project: &project,
                pr_url: pr_url.as_deref().unwrap_or(""),
                project_type: project_config.review_type.as_str(),
                cargo_env: &cargo_env,
            },
            &mut turns_used,
        )
        .await?;
        local_review_reports.push(provider_report);
        *turns_used_acc = turns_used;
    }
    if enforce_provider_gate {
        let review_gate = if requires_hosted_review {
            agent_review_provider_gate::evaluate_configured_local_review_gate(
                &local_review_reports,
                review_config,
            )
        } else {
            agent_review_provider_gate::evaluate_configured_review_gate(
                &local_review_reports,
                review_config,
            )
        };
        if review_gate.decision != ReviewGateDecision::Approved {
            fail_review_provider_gate(store, task_id, turns_used, pr_url.as_deref(), &review_gate)
                .await?;
            return Ok(());
        }
        if !requires_hosted_review && local_review_head_approved.is_none() && should_probe_pr_head {
            local_review_head_approved = Some(
                review_loop::fetch_pr_head_sha_for_gate(
                    &repo_slug_for_review,
                    pr_num,
                    server_config.server.github_token.as_deref(),
                )
                .await,
            );
        }
        crate::workflow_runtime_pr_feedback::record_local_review_passed(
            workflow_runtime_store.as_deref(),
            crate::workflow_runtime_pr_feedback::LocalReviewPassedRuntimeContext {
                project_root: &project_root,
                repo: req.repo.as_deref(),
                issue_number: req.issue,
                task_id,
                pr_number: pr_num,
                pr_url: pr_url.as_deref(),
                summary: "Configured local review providers approved the PR.",
            },
        )
        .await;
    }
    let local_fix_requires_pr_head_advance = agent_pushed_commit;
    agent_pushed_commit |= implementation_pushed_commit;

    let wait_secs = resolved.review_wait_secs.unwrap_or(req.wait_secs);
    let review_wait_started_at = Instant::now();

    // Skip hosted review bot wait when auto-trigger is disabled, but keep a
    // validation gate so local review cannot mark a red PR complete.
    if !requires_hosted_review {
        if !enforce_provider_gate {
            fail_missing_local_review_gate(store, task_id, turns_used, pr_url.as_deref()).await?;
            return Ok(());
        }
        let (before_review_sha, before_review_error) = match local_review_head_before.as_ref() {
            Some(Ok(sha)) => (Some(sha.as_str()), None),
            Some(Err(error)) => (None, Some(error.as_str())),
            None => (None, None),
        };
        let (approved_review_sha, approved_review_error) = match local_review_head_approved.as_ref()
        {
            Some(Ok(sha)) => (Some(sha.as_str()), None),
            Some(Err(error)) => (None, Some(error.as_str())),
            None => (None, None),
        };
        // This is a GitHub PR polling budget, not a local-review retry budget.
        // Keep project review_max_rounds effective here so slow CI can use a
        // longer wait without also capping local Codex review rounds.
        let pr_check_timeout_secs =
            local_review_pr_check_timeout_secs(wait_secs, hosted_review_max_rounds);
        let pr_check_poll_interval_secs = wait_secs.clamp(5, 30);
        complete_after_local_review_without_hosted_bot(
            store,
            task_id,
            Some(LocalReviewReadyToMergeFeedback {
                issue_workflow_store: issue_workflow_store.as_deref(),
                workflow_runtime_store: workflow_runtime_store.as_deref(),
                project_root: &project_root,
                req,
                pr_num,
                pr_checks: LocalReviewPrChecks::GitHub {
                    repo_slug: &repo_slug_for_review,
                    github_token: server_config.server.github_token.as_deref(),
                    timeout_secs: pr_check_timeout_secs,
                    poll_interval_secs: pr_check_poll_interval_secs,
                },
                pr_head: LocalReviewPrHead::GitHub {
                    repo_slug: &repo_slug_for_review,
                    github_token: server_config.server.github_token.as_deref(),
                    before_review_sha,
                    before_review_error,
                    approved_review_sha,
                    approved_review_error,
                    local_fix_requires_pr_head_advance,
                },
                pr_state: LocalReviewPrState::GitHub {
                    repo_slug: &repo_slug_for_review,
                    github_token: server_config.server.github_token.as_deref(),
                },
            }),
            &project,
            &project_config,
            &cargo_env,
            turns_used,
            pr_url.as_deref(),
            task_start,
        )
        .await?;
        return Ok(());
    }

    // Wait for external review bot.
    // Use a local counter instead of querying the store to derive waiting_count —
    // task execution is sequential within a single tokio task, so a plain u32 suffices.
    let mut waiting_count: u32 = 0;
    waiting_count += 1;
    update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;

    let initial_wait_secs =
        initial_hosted_review_wait_secs(wait_secs, review_config.review_wait_budget_secs);
    tracing::info!("waiting {initial_wait_secs}s for review bot on PR #{pr_num}");
    sleep(Duration::from_secs(initial_wait_secs)).await;

    review_loop::run_review_loop(
        store,
        task_id,
        agent,
        review_config,
        &project_config,
        issue_workflow_store.as_deref(),
        workflow_runtime_store.as_deref(),
        &project_root,
        req,
        &events,
        &interceptors,
        &context_items,
        &project,
        &cargo_env,
        pr_url,
        pr_num,
        effective_max_turns,
        effective_max_rounds,
        wait_secs,
        hosted_review_max_rounds,
        agent_pushed_commit,
        rebase_pushed,
        turn_timeout,
        &mut turns_used,
        turns_used_acc,
        task_start,
        review_wait_started_at,
        repo_slug_for_review,
        jaccard_threshold,
        server_config.server.github_token.as_deref(),
    )
    .await
}
