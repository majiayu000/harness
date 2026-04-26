use super::helpers::{
    build_task_event, collect_context_items, detect_modified_files, inject_skills_into_prompt,
    matched_skills_for_prompt, run_agent_streaming, run_on_error, run_post_execute,
    run_post_tool_use, run_pre_execute, telemetry_for_timeout, update_status,
};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, TaskFailureKind, TaskId, TaskStatus, TaskStore,
};
use chrono::Utc;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::interceptor::ToolUseEvent;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{
    ContextItem, Decision, Event, ExecutionPhase, SessionId, TurnFailure, TurnFailureKind,
};
use harness_core::{lang_detect, prompts};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};

/// Compute exponential backoff: `min(base_ms * 2^(attempt-1), max_ms)`.
///
/// - Attempt 1: `base_ms`
/// - Attempt 2: `base_ms * 2`
/// - Attempt 3: `base_ms * 4`
/// - …capped at `max_ms`
pub(crate) fn compute_backoff_ms(base_ms: u64, max_ms: u64, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    base_ms.saturating_mul(1u64 << shift).min(max_ms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImplementationOutcome {
    PlanIssue(String),
    ParsedPr {
        pr_url: Option<String>,
        pr_num: Option<u64>,
        created_issue_num: Option<u64>,
    },
}

pub(crate) fn parse_implementation_outcome(output: &str) -> ImplementationOutcome {
    if let Some(desc) = prompts::parse_plan_issue(output) {
        return ImplementationOutcome::PlanIssue(desc);
    }
    let pr_url = prompts::parse_pr_url(output);
    let pr_num = pr_url.as_deref().and_then(prompts::extract_pr_number);
    let created_issue_num = prompts::parse_created_issue_number(output);
    ImplementationOutcome::ParsedPr {
        pr_url,
        pr_num,
        created_issue_num,
    }
}

/// Returns true if the agent output contains the sentinel string that indicates
/// the agent is operating inside a stale worktree owned by another harness session.
pub(crate) fn contains_worktree_collision_sentinel(output: &str) -> bool {
    output.contains("managed by another harness session")
}

pub(crate) fn prepend_constitution(prompt: String, enabled: bool) -> String {
    const CONSTITUTION: &str = include_str!("../../../../config/constitution.md");
    if enabled {
        format!("{CONSTITUTION}\n\n{prompt}")
    } else {
        prompt
    }
}

/// Returns true when the task type requires the agent to emit a `PR_URL=…` line.
/// Issue-backed tasks and `pr:N` review tasks must produce a PR URL; generic
/// prompt-only implementation tasks may complete without creating one.
pub(crate) fn task_needs_pr_url(req: &CreateTaskRequest) -> bool {
    req.task_kind().requires_pr_url()
}

fn implementation_failure_result(failure: &TurnFailure) -> &'static str {
    match failure.kind {
        TurnFailureKind::Timeout => "timeout",
        TurnFailureKind::Quota => "quota_exhausted",
        TurnFailureKind::Billing => "billing_failed",
        TurnFailureKind::Upstream => "upstream_failure",
        TurnFailureKind::LocalProcess => "local_process_failure",
        TurnFailureKind::Protocol => "protocol_failure",
        TurnFailureKind::Unknown => "failed",
    }
}

/// Outcome of the implement phase.
#[derive(Debug)]
pub(crate) enum ImplementOutcome {
    /// Task is fully complete (success or failure already persisted) — caller returns Ok(()).
    Done,
    /// Implementation reported that the current plan is incomplete and the
    /// caller should choose between replan or force-execute retry.
    Replan {
        issue: u64,
        plan_issue: String,
        prior_plan: Option<String>,
    },
    /// Implementation succeeded — proceed to conflict resolution and review.
    Proceed {
        pr_url: Option<String>,
        pr_num: u64,
        context_items: Vec<ContextItem>,
        #[allow(dead_code)]
        turn_timeout: Duration,
        #[allow(dead_code)]
        initial_allowed_tools: Option<Vec<String>>,
    },
}

/// Construct the initial implementation prompt and execute the implementation agent turn.
///
/// Returns `ImplementOutcome::Done` when the task is fully complete (success or failure
/// already persisted) and the caller should return `Ok(())`. Returns `ImplementOutcome::Proceed`
/// with `(pr_url, pr_num)` when the implement phase succeeded and the caller should proceed
/// to conflict resolution and the review loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_implement_phase(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
    server_config: &harness_core::config::HarnessConfig,
    project_config: &harness_core::config::project::ProjectConfig,
    review_config: &harness_core::config::agents::AgentReviewConfig,
    interceptors: &Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    events: &Arc<harness_observe::event_store::EventStore>,
    skills: &Arc<tokio::sync::RwLock<harness_skills::store::SkillStore>>,
    cargo_env: &HashMap<String, String>,
    git: Option<&harness_core::config::project::GitConfig>,
    repo_slug: &str,
    project: &Path,
    plan_output: Option<String>,
    resumed_pr_url: Option<String>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    turn_timeout: Duration,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
    task_start: Instant,
) -> anyhow::Result<ImplementOutcome> {
    use crate::task_runner::RoundResult;

    // Resume normal flow — update status to Implementing for the main turn.
    update_status(store, task_id, TaskStatus::Implementing, 1).await?;

    let first_prompt = if let Some(issue) = req.issue {
        let base = match super::pr_detection::find_existing_pr_for_issue(project, issue).await {
            Ok(Some((pr_num, branch, pr_url))) => {
                tracing::info!(
                    "reusing existing PR #{pr_num} on branch `{branch}` for issue #{issue}"
                );
                let slug = prompts::repo_slug_from_pr_url(Some(&pr_url));
                prompts::continue_existing_pr(issue, pr_num, &branch, &slug)
            }
            Ok(None) => {
                prompts::implement_from_issue(issue, git, plan_output.as_deref()).to_prompt_string()
            }
            Err(e) => {
                tracing::warn!("failed to check for existing PR for issue #{issue}: {e}");
                prompts::implement_from_issue(issue, git, plan_output.as_deref()).to_prompt_string()
            }
        };
        // If the caller also supplied a description alongside the issue number, include it
        // as additional context. Without this, batch tasks that set both `description` and
        // `issue` would silently discard the description.
        if let Some(hint) = req.prompt.as_deref().filter(|s| !s.is_empty()) {
            format!(
                "{base}\n\nAdditional context from caller:\n{}",
                prompts::wrap_external_data(hint)
            )
        } else {
            base
        }
    } else if let Some(pr) = req.pr {
        prompts::check_existing_pr(
            pr,
            &review_config.review_bot_command,
            repo_slug,
            &review_config.reviewer_name,
            false,
        )
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default(), git)
    };

    // Inject language-detected validation instructions into the prompt when no
    // explicit validation config is set. This delegates validation to the agent
    // instead of running external commands that may not be installed.
    let first_prompt = if project_config.validation.pre_commit.is_empty()
        && project_config.validation.pre_push.is_empty()
    {
        let lang = lang_detect::detect_language(project);
        let instructions = lang_detect::validation_prompt_instructions(lang, project);
        if instructions.is_empty() {
            first_prompt
        } else {
            format!("{first_prompt}\n\n{instructions}")
        }
    } else {
        first_prompt
    };

    // Inject sibling-awareness context when other agents are working on the same project.
    // This prevents parallel agents from over-scoping their changes into each other's files.
    //
    // `project` may be a per-task worktree path when workspace isolation is active, but the
    // sibling cache stores the canonical source repo path (set at spawn time before the worktree
    // is created). Look up the canonical root from the task's own cache entry so that
    // list_siblings() path comparison works correctly in the isolated-worktree case.
    let first_prompt = {
        let canonical_project = store
            .get(task_id)
            .and_then(|s| s.project_root)
            .unwrap_or_else(|| project.to_path_buf());
        let siblings = store.list_siblings(&canonical_project, task_id);
        if siblings.is_empty() {
            first_prompt
        } else {
            let sibling_tasks: Vec<prompts::SiblingTask> = siblings
                .into_iter()
                .filter_map(|s| {
                    s.description.and_then(|description| {
                        if description.is_empty() {
                            None
                        } else {
                            Some(prompts::SiblingTask {
                                issue: s.issue,
                                description,
                            })
                        }
                    })
                })
                .collect();
            let ctx = prompts::sibling_task_context(&sibling_tasks);
            format!("{first_prompt}\n\n{ctx}")
        }
    };

    // Prepend the Golden Principles constitution when enabled.
    let first_prompt =
        prepend_constitution(first_prompt, server_config.server.constitution_enabled);

    // Inject skill content directly into the prompt text.
    // Since harness uses single-turn `claude -p`, context items are not visible
    // to the agent — we must embed skill content in the prompt string itself.
    // Also records usage for any matched skills via record_use().
    let matched_skills = matched_skills_for_prompt(skills, &first_prompt).await;
    let skill_additions = inject_skills_into_prompt(skills, &first_prompt).await;
    let first_prompt = if skill_additions.is_empty() {
        first_prompt
    } else {
        first_prompt + &skill_additions
    };
    for (skill_id, skill_name) in matched_skills {
        let mut skill_event = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        skill_event.reason = Some(skill_name);
        skill_event.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        if let Err(err) = events.log(&skill_event).await {
            tracing::warn!(error = %err, "failed to log skill_used event");
        }
    }

    let context_items = collect_context_items(skills, project, &first_prompt).await;

    let initial_allowed_tools: Option<Vec<String>> = None;
    let capability_prompt_note: Option<&'static str> = None;

    // Prepend capability restriction note so the agent knows which tools are
    // permitted. This is the primary enforcement path now that --allowedTools
    // is not passed to the CLI.
    let first_prompt = if let Some(note) = capability_prompt_note {
        format!("{note}\n\n{first_prompt}")
    } else {
        first_prompt
    };

    tracing::info!(
        task_id = %task_id,
        prompt_len = first_prompt.len(),
        prompt_empty = first_prompt.is_empty(),
        source = ?req.source,
        "run_task: prompt constructed"
    );
    if first_prompt.is_empty() {
        tracing::error!(task_id = %task_id, "run_task: prompt is empty — agent will fail");
    }

    // Duplicate-PR prevention guard: if checkpoint shows PR already created, skip the
    // implement agent entirely to avoid opening a second PR on resume.
    let (pr_url, pr_num): (Option<String>, u64) = 'implement: {
        if let Some(pr_str) = resumed_pr_url {
            tracing::info!(
                task_id = %task_id,
                pr_url = %pr_str,
                "checkpoint resume: PR exists, skipping implement phase"
            );
            let Some(n) = prompts::extract_pr_number(&pr_str) else {
                tracing::error!(
                    task_id = %task_id,
                    pr_url = %pr_str,
                    "checkpoint resume: cannot parse PR number, marking failed"
                );
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.turn = 1;
                    s.error = Some(format!("checkpoint resume: no PR number in {pr_str}"));
                })
                .await?;
                return Ok(ImplementOutcome::Done);
            };
            mutate_and_persist(store, task_id, |s| {
                s.pr_url = Some(pr_str.clone());
                s.rounds.push(RoundResult::new(
                    1,
                    "implement",
                    "resumed_checkpoint",
                    None,
                    None,
                    None,
                ));
            })
            .await?;
            break 'implement (Some(pr_str), n);
        }

        let impl_phase_start = Instant::now();

        let initial_req = AgentRequest {
            prompt: first_prompt,
            project_root: project.to_path_buf(),
            context: context_items.clone(),
            max_budget_usd: req.max_budget_usd,
            execution_phase: Some(ExecutionPhase::Planning),
            allowed_tools: initial_allowed_tools.clone(),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };

        // Run pre_execute interceptors; Block aborts the task.
        let first_req = run_pre_execute(interceptors, initial_req).await?;

        // Execute implementation turn with post-execution validation and auto-retry.
        // Use the largest max_retries declared by any interceptor.
        // A single interceptor returning 0 should not suppress retries for others.
        let max_validation_retries: u32 = interceptors
            .iter()
            .filter_map(|i| i.max_validation_retries())
            .max()
            .unwrap_or(2);
        let mut validation_attempt = 0u32;
        let mut impl_req = first_req.clone();

        let resp = loop {
            if let Some(max) = effective_max_turns {
                if *turns_used >= max {
                    return Err(anyhow::anyhow!(
                        "Turn budget exhausted: used {} of {} allowed turns",
                        turns_used,
                        max
                    ));
                }
            }
            let prompt_built_at = Utc::now();
            let agent_started_at = Utc::now();
            let raw = tokio::time::timeout(
                turn_timeout,
                run_agent_streaming(
                    agent,
                    impl_req.clone(),
                    task_id,
                    store,
                    1,
                    prompt_built_at,
                    agent_started_at,
                ),
            )
            .await;
            *turns_used += 1;
            *turns_used_acc = *turns_used;
            match raw {
                Ok(Ok(success)) => {
                    let mut impl_telemetry = success.telemetry.clone();
                    impl_telemetry.retry_count = Some(validation_attempt);
                    let r = success.response;
                    // Post-execution tool isolation check (defense-in-depth alongside
                    // --allowedTools CLI enforcement). Violations feed into the retry loop
                    // so the agent gets a chance to self-correct (fail-closed, not fail-open).
                    let impl_tools = impl_req.allowed_tools.as_deref().unwrap_or(&[]);
                    let tool_violations = validate_tool_usage(&r.output, impl_tools);
                    let violation_err: Option<String> = if !tool_violations.is_empty() {
                        let msg = format!(
                        "[VALIDATION ERROR] Tool isolation violation: agent used disallowed tools: [{}]. Only [{}] are permitted.",
                        tool_violations.join(", "),
                        impl_tools.join(", ")
                    );
                        tracing::warn!(
                            ?tool_violations,
                            "implementation turn: agent used tools outside allowed list"
                        );
                        Some(msg)
                    } else {
                        None
                    };
                    // PreToolUse / PostToolUse hook injection point:
                    // detect files written during this turn and fire post_tool_use hooks.
                    let hook_err = {
                        let modified = detect_modified_files(project).await;
                        if modified.is_empty() {
                            None
                        } else {
                            let hook_event = ToolUseEvent {
                                tool_name: "file_write".to_string(),
                                affected_files: modified,
                                session_id: None,
                            };
                            run_post_tool_use(interceptors, &hook_event, project).await
                        }
                    };
                    let post_err = run_post_execute(interceptors, &impl_req, &r).await;
                    let combined_err = violation_err.or(hook_err).or(post_err);
                    if let Some(err) = combined_err {
                        if validation_attempt < max_validation_retries {
                            validation_attempt += 1;
                            let backoff_ms = compute_backoff_ms(
                                req.retry_base_backoff_ms,
                                req.retry_max_backoff_ms,
                                validation_attempt,
                            );
                            tracing::warn!(
                                attempt = validation_attempt,
                                max = max_validation_retries,
                                backoff_ms,
                                error = %err,
                                "post-execution validation failed; backing off before retry"
                            );
                            let truncated = super::helpers::truncate_validation_error(&err, 2000);
                            impl_req.prompt = prompts::validation_retry_prompt(
                                &first_req.prompt,
                                validation_attempt,
                                max_validation_retries,
                                &truncated,
                            );
                            sleep(Duration::from_millis(backoff_ms)).await;
                            continue;
                        } else {
                            tracing::error!(
                                max = max_validation_retries,
                                error = %err,
                                "post-execution validation failed after max retries; aborting task"
                            );
                            run_on_error(interceptors, &impl_req, &err).await;
                            let failure = TurnFailure {
                                kind: TurnFailureKind::Protocol,
                                provider: Some(agent.name().to_string()),
                                upstream_status: None,
                                message: Some(err.clone()),
                                body_excerpt: None,
                            };
                            mutate_and_persist(store, task_id, |s| {
                                s.rounds.push(RoundResult::new(
                                    1,
                                    "implement",
                                    implementation_failure_result(&failure),
                                    Some(r.output.clone()),
                                    Some(impl_telemetry.clone()),
                                    Some(failure.clone()),
                                ));
                            })
                            .await?;
                            let event = build_task_event(
                                task_id,
                                1,
                                "implement",
                                "task_implement",
                                Decision::Block,
                                Some("implementation validation failed".to_string()),
                                None,
                                Some(impl_telemetry.clone()),
                                Some(failure),
                                Some(r.output.clone()),
                            );
                            if let Err(log_err) = events.log(&event).await {
                                tracing::warn!("failed to log task_implement event: {log_err}");
                            }
                            return Err(anyhow::anyhow!(
                                "Post-execution validation failed after {} attempts: {}",
                                max_validation_retries,
                                err
                            ));
                        }
                    }
                    break (r, impl_telemetry);
                }
                Ok(Err(failure)) => {
                    let mut telemetry = failure.telemetry.clone();
                    telemetry.retry_count = Some(validation_attempt);
                    run_on_error(interceptors, &impl_req, &failure.error.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.rounds.push(RoundResult::new(
                            1,
                            "implement",
                            implementation_failure_result(&failure.failure),
                            None,
                            Some(telemetry.clone()),
                            Some(failure.failure.clone()),
                        ));
                    })
                    .await?;
                    let event = build_task_event(
                        task_id,
                        1,
                        "implement",
                        "task_implement",
                        Decision::Block,
                        Some("implementation failed".to_string()),
                        None,
                        Some(telemetry),
                        Some(failure.failure.clone()),
                        None,
                    );
                    if let Err(log_err) = events.log(&event).await {
                        tracing::warn!("failed to log task_implement event: {log_err}");
                    }
                    return Err(failure.error.into());
                }
                Err(_) => {
                    let msg = format!(
                        "Implementation turn timed out after {}s",
                        turn_timeout.as_secs()
                    );
                    run_on_error(interceptors, &impl_req, &msg).await;
                    let telemetry = telemetry_for_timeout(
                        prompt_built_at,
                        agent_started_at,
                        Utc::now(),
                        Some(validation_attempt),
                    );
                    let failure = TurnFailure {
                        kind: TurnFailureKind::Timeout,
                        provider: Some(agent.name().to_string()),
                        upstream_status: None,
                        message: Some(msg.clone()),
                        body_excerpt: None,
                    };
                    mutate_and_persist(store, task_id, |s| {
                        s.rounds.push(RoundResult::new(
                            1,
                            "implement",
                            "timeout",
                            None,
                            Some(telemetry.clone()),
                            Some(failure.clone()),
                        ));
                    })
                    .await?;
                    let event = build_task_event(
                        task_id,
                        1,
                        "implement",
                        "task_implement",
                        Decision::Block,
                        Some("implementation timed out".to_string()),
                        None,
                        Some(telemetry),
                        Some(failure),
                        None,
                    );
                    if let Err(log_err) = events.log(&event).await {
                        tracing::warn!("failed to log task_implement event: {log_err}");
                    }
                    return Err(anyhow::anyhow!("{msg}"));
                }
            }
        };

        let (
            AgentResponse {
                output,
                stderr,
                token_usage: impl_token_usage,
                ..
            },
            impl_telemetry,
        ) = resp;

        tracing::info!(
            task_id = %task_id,
            phase = "implementing",
            elapsed_secs = impl_phase_start.elapsed().as_secs(),
            "phase_completed"
        );
        {
            let preview: String = output.chars().take(200).collect();
            tracing::info!(
                task_id = %task_id,
                output_chars = output.len(),
                preview = %preview,
                input_tokens = impl_token_usage.input_tokens,
                output_tokens = impl_token_usage.output_tokens,
                "agent_output_summary"
            );
        }

        if !stderr.is_empty() {
            tracing::warn!(stderr = %stderr, "agent stderr during implementation");
        }

        // Fast-fail: if the agent observed a stale worktree managed by another harness
        // session, abort immediately. This prevents the task from pushing commits to the
        // wrong PR (issue #799).
        if contains_worktree_collision_sentinel(&output) {
            // If the agent already pushed to a PR before we detected the collision,
            // capture the URL so there is a tracked handle for cleanup.
            let collision_pr_url = match parse_implementation_outcome(&output) {
                ImplementationOutcome::ParsedPr { pr_url, .. } => pr_url,
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
            return Ok(ImplementOutcome::Done);
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
            return Ok(ImplementOutcome::Done);
        }

        let (pr_url, pr_num, created_issue_num) = match parse_implementation_outcome(&output) {
            ImplementationOutcome::PlanIssue(plan_issue) => {
                if let (Some(workflows), Some(issue_number)) =
                    (issue_workflow_store.as_ref(), req.issue)
                {
                    let project_id = project.to_string_lossy().into_owned();
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
                    return Ok(ImplementOutcome::Replan {
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
                return Ok(ImplementOutcome::Done);
            }
            ImplementationOutcome::ParsedPr {
                pr_url,
                pr_num,
                created_issue_num,
            } => (pr_url, pr_num, created_issue_num),
        };

        mutate_and_persist(store, task_id, |s| {
            s.pr_url = pr_url.clone();
            s.rounds.push(RoundResult::new(
                1,
                "implement",
                if pr_num.is_some() {
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
                s.source.as_deref() == Some("auto-fix")
                    && s.external_id.as_deref() != Some(&final_eid)
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
            store.log_event(crate::event_replay::TaskEvent::PrDetected {
                task_id: task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
                pr_url: pr_url_str.to_string(),
            });
        }

        // Write PR checkpoint immediately after pr_url is persisted.
        // This is the most critical checkpoint — it prevents duplicate PR creation on resume.
        if let Some(pr_url_str) = pr_url.as_deref() {
            if let Err(e) = store
                .write_checkpoint(task_id, None, None, Some(pr_url_str), "pr_created")
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
                return Ok(ImplementOutcome::Done);
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
                    s.error = Some(
                        "no PR number found in agent output; task requires PR_URL".to_string(),
                    );
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
            return Ok(ImplementOutcome::Done);
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

        (pr_url, pr_num)
    }; // end 'implement

    Ok(ImplementOutcome::Proceed {
        pr_url,
        pr_num,
        context_items,
        turn_timeout,
        initial_allowed_tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_backoff_three_consecutive_retries() {
        let base_ms: u64 = 10_000;
        let max_ms: u64 = 300_000;

        // Attempt 1: base_ms * 2^0 = 10_000 ms (10 s)
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 1), 10_000);
        // Attempt 2: base_ms * 2^1 = 20_000 ms (20 s)
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 2), 20_000);
        // Attempt 3: base_ms * 2^2 = 40_000 ms (40 s)
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 3), 40_000);
    }

    #[test]
    fn exponential_backoff_capped_at_max() {
        let base_ms: u64 = 10_000;
        let max_ms: u64 = 300_000;

        // Attempt 6: 10_000 * 2^5 = 320_000 — should be capped at 300_000
        assert_eq!(compute_backoff_ms(base_ms, max_ms, 6), 300_000);
    }

    #[test]
    fn parse_implementation_outcome_prefers_plan_issue() {
        let output = "PLAN_ISSUE=Plan missed rollback path\nPR_URL=https://github.com/o/r/pull/123";
        let parsed = parse_implementation_outcome(output);
        assert_eq!(
            parsed,
            ImplementationOutcome::PlanIssue("Plan missed rollback path".to_string())
        );
    }

    #[test]
    fn parse_implementation_outcome_extracts_pr_when_no_plan_issue() {
        let output = "Done.\nPR_URL=https://github.com/majiayu000/harness/pull/42";
        let parsed = parse_implementation_outcome(output);
        assert_eq!(
            parsed,
            ImplementationOutcome::ParsedPr {
                pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
                pr_num: Some(42),
                created_issue_num: None,
            }
        );
    }

    #[test]
    fn worktree_collision_sentinel_detected() {
        let collision_output =
            "Pushed. Now clean up the worktree (it's managed by another harness session so I'll skip removing it):\nPR_URL=https://github.com/owner/repo/pull/796";
        assert!(
            contains_worktree_collision_sentinel(collision_output),
            "sentinel should be detected in collision output"
        );
    }

    #[test]
    fn worktree_collision_sentinel_absent_in_normal_output() {
        let normal_output = "Done.\nPR_URL=https://github.com/owner/repo/pull/42";
        assert!(
            !contains_worktree_collision_sentinel(normal_output),
            "sentinel should not be detected in normal output"
        );
    }

    #[test]
    fn constitution_present_when_enabled() {
        let result = prepend_constitution("Do the task.".to_string(), true);
        assert!(result.contains("GP-01"));
        assert!(result.contains("GP-02"));
        assert!(result.contains("GP-03"));
        assert!(result.contains("GP-04"));
        assert!(result.contains("GP-05"));
        assert!(result.ends_with("Do the task."));
    }

    #[test]
    fn constitution_absent_when_disabled() {
        let result = prepend_constitution("Do the task.".to_string(), false);
        assert_eq!(result, "Do the task.");
        assert!(!result.contains("GP-01"));
    }
}
