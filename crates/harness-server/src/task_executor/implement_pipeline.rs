/// Implementation phase pipeline module.
///
/// Builds the implementation prompt, executes the agent with post-execution
/// validation and auto-retry, and returns the resulting PR URL and number.
///
/// **Note:** `cargo check` / `cargo test` are agent-side operations driven by
/// harness prompts — this module spawns no Cargo subprocesses directly.
use crate::task_runner::{mutate_and_persist, RoundResult, TaskStatus};
use harness_core::agent::{AgentRequest, AgentResponse};
use harness_core::interceptor::ToolUseEvent;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{ContextItem, Decision, Event, ExecutionPhase, SessionId};
use harness_core::{lang_detect, prompts};
use tokio::time::{sleep, timeout, Duration, Instant};

use super::helpers::{
    collect_context_items, detect_modified_files, inject_skills_into_prompt,
    matched_skills_for_prompt, run_on_error, run_post_execute, run_post_tool_use, run_pre_execute,
    truncate_validation_error,
};
use super::pr_detection::find_existing_pr_for_issue;
use super::{
    compute_backoff_ms, parse_implementation_outcome, prepend_constitution, restricted_tools,
    run_agent_streaming, ImplementationOutcome, TaskContext,
};
use harness_core::config::agents::CapabilityProfile;

/// Outcome returned by the implementation pipeline to the orchestrator.
pub(crate) enum ImplementOutcome {
    /// Task was handled internally (marked Done or Failed). The orchestrator
    /// should return `Ok(())` immediately.
    Handled,
    /// A PR is ready for review.
    PrReady {
        pr_url: Option<String>,
        pr_num: u64,
        /// Context items collected from skills / project; passed to review phases.
        context_items: Vec<ContextItem>,
        /// Whether the agent committed code during this phase (used to set
        /// `prev_fixed` for the external review bot loop).
        agent_pushed_commit: bool,
    },
}

/// Run the implementation phase.
///
/// * `plan_output` — optional plan text from the triage pipeline.
/// * `resumed_pr_url` — if `Some`, skip the agent entirely and return the
///   existing PR URL directly (checkpoint resume).
/// * `effective_max_turns` — optional turn budget.
/// * `turns_used` — accumulated turn counter; updated in-place.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    ctx: &TaskContext<'_>,
    plan_output: Option<String>,
    resumed_pr_url: Option<String>,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
) -> anyhow::Result<ImplementOutcome> {
    // --- Checkpoint resume: PR already exists, skip agent entirely ---
    if let Some(pr_str) = resumed_pr_url {
        tracing::info!(
            task_id = %ctx.task_id,
            pr_url = %pr_str,
            "checkpoint resume: PR exists, skipping implement phase"
        );
        let Some(pr_num) = prompts::extract_pr_number(&pr_str) else {
            tracing::error!(
                task_id = %ctx.task_id,
                pr_url = %pr_str,
                "checkpoint resume: cannot parse PR number, marking failed"
            );
            mutate_and_persist(ctx.store, ctx.task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 1;
                s.error = Some(format!("checkpoint resume: no PR number in {pr_str}"));
            })
            .await?;
            return Ok(ImplementOutcome::Handled);
        };
        mutate_and_persist(ctx.store, ctx.task_id, |s| {
            s.pr_url = Some(pr_str.clone());
            s.rounds.push(RoundResult {
                turn: 1,
                action: "implement".into(),
                result: "resumed_checkpoint".into(),
                detail: None,
                first_token_latency_ms: None,
            });
        })
        .await?;
        // context_items not needed for checkpoint-resumed tasks (review loop
        // uses them but the checkpoint case already had them in a prior run).
        // Pass an empty vec; the review phases will re-collect as needed.
        return Ok(ImplementOutcome::PrReady {
            pr_url: Some(pr_str),
            pr_num,
            context_items: Vec::new(),
            agent_pushed_commit: false,
        });
    }

    // --- Build implementation prompt ---
    let git = Some(&ctx.project_config.git);
    let first_prompt = build_first_prompt(ctx, plan_output.as_deref(), git).await?;

    // Inject language-detected validation instructions when no explicit
    // validation config is set.
    let first_prompt = if ctx.project_config.validation.pre_commit.is_empty()
        && ctx.project_config.validation.pre_push.is_empty()
    {
        let lang = lang_detect::detect_language(&ctx.project);
        let instructions = lang_detect::validation_prompt_instructions(lang, &ctx.project);
        if instructions.is_empty() {
            first_prompt
        } else {
            format!("{first_prompt}\n\n{instructions}")
        }
    } else {
        first_prompt
    };

    // Inject sibling-awareness context when other agents are working on the same project.
    let first_prompt = {
        let canonical_project = ctx
            .store
            .get(ctx.task_id)
            .and_then(|s| s.project_root)
            .unwrap_or_else(|| ctx.project.clone());
        let siblings = ctx.store.list_siblings(&canonical_project, ctx.task_id);
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
            let sibling_ctx = prompts::sibling_task_context(&sibling_tasks);
            format!("{first_prompt}\n\n{sibling_ctx}")
        }
    };

    // Prepend the Golden Principles constitution when enabled.
    let first_prompt =
        prepend_constitution(first_prompt, ctx.server_config.server.constitution_enabled);

    // Inject skill content directly into the prompt text.
    let matched_skills = matched_skills_for_prompt(&ctx.skills, &first_prompt).await;
    let skill_additions = inject_skills_into_prompt(&ctx.skills, &first_prompt).await;
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
            ctx.task_id.as_str(),
            skill_id.as_str()
        ));
        if let Err(err) = ctx.events.log(&skill_event).await {
            tracing::warn!(error = %err, "failed to log skill_used event");
        }
    }

    let context_items = collect_context_items(&ctx.skills, &ctx.project, &first_prompt).await;

    tracing::info!(
        task_id = %ctx.task_id,
        prompt_len = first_prompt.len(),
        prompt_empty = first_prompt.is_empty(),
        source = ?ctx.req.source,
        "run_task: prompt constructed"
    );
    if first_prompt.is_empty() {
        tracing::error!(task_id = %ctx.task_id, "run_task: prompt is empty — agent will fail");
    }

    // Periodic review tasks need Bash but not unrestricted write access.
    let (initial_allowed_tools, capability_prompt_note) = if matches!(
        ctx.req.source.as_deref(),
        Some("periodic_review") | Some("sprint_planner")
    ) {
        (
            Some(restricted_tools(CapabilityProfile::Standard)?),
            CapabilityProfile::Standard.prompt_note(),
        )
    } else {
        (None, None)
    };

    // Prepend capability restriction note.
    let first_prompt = if let Some(note) = capability_prompt_note {
        format!("{note}\n\n{first_prompt}")
    } else {
        first_prompt
    };

    let impl_phase_start = Instant::now();

    let initial_req = AgentRequest {
        prompt: first_prompt,
        project_root: ctx.project.clone(),
        context: context_items.clone(),
        max_budget_usd: ctx.req.max_budget_usd,
        execution_phase: Some(ExecutionPhase::Planning),
        allowed_tools: initial_allowed_tools,
        env_vars: ctx.cargo_env.clone(),
        ..Default::default()
    };

    let first_req = run_pre_execute(&ctx.interceptors, initial_req).await?;

    // Execute implementation turn with post-execution validation and auto-retry.
    let max_validation_retries: u32 = ctx
        .interceptors
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
        let raw = timeout(
            ctx.turn_timeout,
            run_agent_streaming(ctx.agent, impl_req.clone(), ctx.task_id, ctx.store, 1),
        )
        .await;
        *turns_used += 1;
        match raw {
            Ok(Ok((r, impl_first_token_ms))) => {
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
                let hook_err = {
                    let modified = detect_modified_files(&ctx.project).await;
                    if modified.is_empty() {
                        None
                    } else {
                        let hook_event = ToolUseEvent {
                            tool_name: "file_write".to_string(),
                            affected_files: modified,
                            session_id: None,
                        };
                        run_post_tool_use(&ctx.interceptors, &hook_event, &ctx.project).await
                    }
                };
                let post_err = run_post_execute(&ctx.interceptors, &impl_req, &r).await;
                let combined_err = violation_err.or(hook_err).or(post_err);
                if let Some(err) = combined_err {
                    if validation_attempt < max_validation_retries {
                        validation_attempt += 1;
                        let backoff_ms = compute_backoff_ms(
                            ctx.req.retry_base_backoff_ms,
                            ctx.req.retry_max_backoff_ms,
                            validation_attempt,
                        );
                        tracing::warn!(
                            attempt = validation_attempt,
                            max = max_validation_retries,
                            backoff_ms,
                            error = %err,
                            "post-execution validation failed; backing off before retry"
                        );
                        let truncated = truncate_validation_error(&err, 2000);
                        impl_req.prompt = prompts::validation_retry_error_context(
                            &first_req.prompt,
                            &truncated,
                            validation_attempt,
                            max_validation_retries,
                        );
                        sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    } else {
                        tracing::error!(
                            max = max_validation_retries,
                            error = %err,
                            "post-execution validation failed after max retries; aborting task"
                        );
                        run_on_error(&ctx.interceptors, &impl_req, &err).await;
                        return Err(anyhow::anyhow!(
                            "Post-execution validation failed after {} attempts: {}",
                            max_validation_retries,
                            err
                        ));
                    }
                }
                break (r, impl_first_token_ms);
            }
            Ok(Err(e)) => {
                run_on_error(&ctx.interceptors, &impl_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Implementation turn timed out after {}s",
                    ctx.turn_timeout.as_secs()
                );
                run_on_error(&ctx.interceptors, &impl_req, &msg).await;
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
        impl_first_token_ms,
    ) = resp;

    tracing::info!(
        task_id = %ctx.task_id,
        phase = "implementing",
        elapsed_secs = impl_phase_start.elapsed().as_secs(),
        "phase_completed"
    );
    {
        let preview: String = output.chars().take(200).collect();
        tracing::info!(
            task_id = %ctx.task_id,
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

    // Review-only tasks produce a report, not a PR.
    let is_review_task = ctx.store.get(ctx.task_id).is_some_and(|s| {
        matches!(
            s.source.as_deref(),
            Some("periodic_review") | Some("sprint_planner")
        )
    });

    if is_review_task {
        mutate_and_persist(ctx.store, ctx.task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 1;
            s.rounds.push(RoundResult {
                turn: 1,
                action: "review".into(),
                result: "completed".into(),
                detail: if output.is_empty() {
                    None
                } else {
                    Some(output.clone())
                },
                first_token_latency_ms: impl_first_token_ms,
            });
        })
        .await?;
        ctx.store
            .log_event(crate::event_replay::TaskEvent::Completed {
                task_id: ctx.task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
            });
        tracing::info!(
            task_id = %ctx.task_id,
            status = "done",
            turns = 1,
            pr_url = tracing::field::Empty,
            total_elapsed_secs = ctx.task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(ImplementOutcome::Handled);
    }

    let (pr_url, pr_num, created_issue_num) = match parse_implementation_outcome(&output) {
        ImplementationOutcome::PlanIssue(plan_issue) => {
            tracing::error!(
                task_id = %ctx.task_id,
                plan_issue = %plan_issue,
                "implementation returned PLAN_ISSUE; marking task failed"
            );
            mutate_and_persist(ctx.store, ctx.task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error = Some(plan_issue.clone());
                s.rounds.push(RoundResult {
                    turn: 1,
                    action: "implement".into(),
                    result: "plan_issue".into(),
                    detail: if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    },
                    first_token_latency_ms: impl_first_token_ms,
                });
            })
            .await?;
            tracing::info!(
                task_id = %ctx.task_id,
                status = "failed",
                turns = 2,
                pr_url = tracing::field::Empty,
                total_elapsed_secs = ctx.task_start.elapsed().as_secs(),
                "task_completed"
            );
            return Ok(ImplementOutcome::Handled);
        }
        ImplementationOutcome::ParsedPr {
            pr_url,
            pr_num,
            created_issue_num,
        } => (pr_url, pr_num, created_issue_num),
    };

    mutate_and_persist(ctx.store, ctx.task_id, |s| {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult {
            turn: 1,
            action: "implement".into(),
            result: if pr_num.is_some() {
                "pr_created".into()
            } else {
                "no_pr".into()
            },
            detail: if output.is_empty() {
                None
            } else {
                Some(output.clone())
            },
            first_token_latency_ms: impl_first_token_ms,
        });
    })
    .await?;

    // Back-fill external_id for auto-fix tasks that created a GitHub issue.
    if let Some(issue_num) = created_issue_num {
        let final_eid = format!("issue:{issue_num}");
        let needs_backfill = ctx.store.get(ctx.task_id).is_some_and(|s| {
            s.source.as_deref() == Some("auto-fix") && s.external_id.as_deref() != Some(&final_eid)
        });
        if needs_backfill {
            if let Err(e) = ctx
                .store
                .overwrite_external_id_auto_fix(ctx.task_id, &final_eid)
                .await
            {
                tracing::warn!(task_id = %ctx.task_id, "failed to back-fill external_id: {e}");
            } else {
                tracing::info!(
                    task_id = %ctx.task_id,
                    external_id = %final_eid,
                    "back-filled external_id for auto-fix task"
                );
            }
        }
    }

    // Emit PrDetected event so crash recovery can reconstruct pr_url.
    if let Some(pr_url_str) = pr_url.as_deref() {
        ctx.store
            .log_event(crate::event_replay::TaskEvent::PrDetected {
                task_id: ctx.task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
                pr_url: pr_url_str.to_string(),
            });
    }

    // Write PR checkpoint — prevents duplicate PR creation on resume.
    if let Some(pr_url_str) = pr_url.as_deref() {
        if let Err(e) = ctx
            .store
            .write_checkpoint(ctx.task_id, None, None, Some(pr_url_str), "pr_created")
            .await
        {
            tracing::warn!(task_id = %ctx.task_id, "failed to write pr checkpoint: {e}");
        }
    }

    // Log implementation event.
    let mut ev = Event::new(
        SessionId::new(),
        "task_implement",
        "task_runner",
        harness_core::types::Decision::Complete,
    );
    ev.detail = pr_num.map(|n| format!("pr={n}"));
    if let Err(e) = ctx.events.log(&ev).await {
        tracing::warn!("failed to log task_implement event: {e}");
    }

    let Some(pr_num) = pr_num else {
        tracing::warn!("no PR number found in agent output; skipping review");
        mutate_and_persist(ctx.store, ctx.task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 2;
        })
        .await?;
        ctx.store
            .log_event(crate::event_replay::TaskEvent::Completed {
                task_id: ctx.task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
            });
        tracing::info!(
            task_id = %ctx.task_id,
            status = "done",
            turns = 2,
            pr_url = tracing::field::Empty,
            total_elapsed_secs = ctx.task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(ImplementOutcome::Handled);
    };

    Ok(ImplementOutcome::PrReady {
        pr_url,
        pr_num,
        context_items,
        agent_pushed_commit: false,
    })
}

/// Build the base first-turn prompt before skill injection and constitution prepend.
async fn build_first_prompt(
    ctx: &TaskContext<'_>,
    plan_output: Option<&str>,
    git: Option<&harness_core::config::project::GitConfig>,
) -> anyhow::Result<String> {
    if let Some(issue) = ctx.req.issue {
        let base = match find_existing_pr_for_issue(&ctx.project, issue).await {
            Ok(Some((pr_num, branch, pr_url))) => {
                tracing::info!(
                    "reusing existing PR #{pr_num} on branch `{branch}` for issue #{issue}"
                );
                let slug = prompts::repo_slug_from_pr_url(Some(&pr_url));
                prompts::continue_existing_pr(issue, pr_num, &branch, &slug)
            }
            Ok(None) => prompts::implement_from_issue(issue, git, plan_output).to_prompt_string(),
            Err(e) => {
                tracing::warn!("failed to check for existing PR for issue #{issue}: {e}");
                prompts::implement_from_issue(issue, git, plan_output).to_prompt_string()
            }
        };
        let prompt = if let Some(hint) = ctx.req.prompt.as_deref().filter(|s| !s.is_empty()) {
            format!(
                "{base}\n\nAdditional context from caller:\n{}",
                prompts::wrap_external_data(hint)
            )
        } else {
            base
        };
        Ok(prompt)
    } else if let Some(pr) = ctx.req.pr {
        Ok(prompts::check_existing_pr(
            pr,
            &ctx.review_config.review_bot_command,
            &ctx.repo_slug,
            &ctx.review_config.reviewer_name,
            false,
        ))
    } else if matches!(
        ctx.req.source.as_deref(),
        Some("periodic_review") | Some("sprint_planner")
    ) {
        Ok(ctx.req.prompt.clone().unwrap_or_default())
    } else {
        Ok(prompts::implement_from_prompt(
            ctx.req.prompt.as_deref().unwrap_or_default(),
            git,
        ))
    }
}
