use crate::task_executor::pipeline::{
    load_task_config, prepend_constitution, resolve_plan, restricted_tools, TaskTargetDir,
};
use crate::task_executor::pr_detection::find_existing_pr_for_issue;
use crate::task_executor::{helpers, lifecycle, review_loop};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::config::agents::CapabilityProfile;
use harness_core::interceptor::ToolUseEvent;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{Decision, Event, ExecutionPhase, SessionId};
use helpers::{
    collect_context_items, detect_modified_files, inject_skills_into_prompt,
    matched_skills_for_prompt, run_on_error, run_post_execute, run_post_tool_use, run_pre_execute,
    truncate_validation_error, update_status,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration, Instant};

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: Option<&dyn CodeAgent>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
    server_config: &harness_core::config::HarnessConfig,
    // Accumulated turn count from previous transient-retry attempts.
    // Ensures the max_turns budget is global across the full task lifecycle,
    // not reset on each retry (fix for budget-reset-on-retry bug).
    turns_used_acc: &mut u32,
) -> anyhow::Result<()> {
    let task_start = Instant::now();

    if !project.exists() {
        anyhow::bail!("project_root does not exist: {}", project.display());
    }

    let task_target = std::env::temp_dir()
        .join("harness-cargo-targets")
        .join(task_id.as_str());
    let cargo_env: HashMap<String, String> = [(
        "CARGO_TARGET_DIR".to_string(),
        task_target.display().to_string(),
    )]
    .into();
    let _task_target_guard = TaskTargetDir(task_target);

    let (project_config, resolved, resumed_pr_url, resumed_plan, repo_slug) =
        load_task_config(store, task_id, &project, server_config).await?;
    let review_config = &resolved.review;
    let git = Some(&project_config.git);

    let (plan_output, triage_complexity, pipeline_turns) = resolve_plan(
        store,
        task_id,
        agent,
        req,
        &project,
        &cargo_env,
        resumed_pr_url.as_deref(),
        resumed_plan,
    )
    .await?;

    let (triage_default_rounds, skip_agent_review) = match triage_complexity {
        prompts::TriageComplexity::Low => (2u32, false),
        prompts::TriageComplexity::Medium => (8u32, false),
        prompts::TriageComplexity::High => (8u32, false),
    };
    let effective_max_rounds = req.max_rounds.unwrap_or(triage_default_rounds);
    // max_turns: per-request override wins; global config is the fallback.
    // Counts every agent API call (impl + validation retries + review rounds).
    let effective_max_turns: Option<u32> = req.max_turns.or(server_config.concurrency.max_turns);
    // Start from accumulated turns (prior transient-retry attempts + pipeline phases)
    // so the budget is global across the full task lifecycle.
    let mut turns_used: u32 = *turns_used_acc + pipeline_turns;
    *turns_used_acc = turns_used;
    tracing::info!(
        task_id = %task_id,
        ?triage_complexity,
        effective_max_rounds,
        skip_agent_review,
        ?effective_max_turns,
        "triage complexity applied"
    );

    update_status(store, task_id, TaskStatus::Implementing, 1).await?;

    let first_prompt = if let Some(issue) = req.issue {
        let base = match find_existing_pr_for_issue(&project, issue).await {
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
            &repo_slug,
            &review_config.reviewer_name,
            false,
        )
    } else if matches!(
        req.source.as_deref(),
        Some("periodic_review") | Some("sprint_planner")
    ) {
        req.prompt.clone().unwrap_or_default()
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default(), git)
    };

    let first_prompt = if project_config.validation.pre_commit.is_empty()
        && project_config.validation.pre_push.is_empty()
    {
        let lang = harness_core::lang_detect::detect_language(&project);
        let instructions =
            harness_core::lang_detect::validation_prompt_instructions(lang, &project);
        if instructions.is_empty() {
            first_prompt
        } else {
            format!("{first_prompt}\n\n{instructions}")
        }
    } else {
        first_prompt
    };

    let first_prompt = {
        let canonical_project = store
            .get(task_id)
            .and_then(|s| s.project_root)
            .unwrap_or_else(|| project.clone());
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

    let first_prompt =
        prepend_constitution(first_prompt, server_config.server.constitution_enabled);

    let matched_skills = matched_skills_for_prompt(&skills, &first_prompt).await;
    let skill_additions = inject_skills_into_prompt(&skills, &first_prompt).await;
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

    let context_items = collect_context_items(&skills, &project, &first_prompt).await;
    let turn_timeout = crate::task_runner::effective_turn_timeout(req.turn_timeout_secs);

    let (initial_allowed_tools, capability_prompt_note): (Option<Vec<String>>, _) = if matches!(
        req.source.as_deref(),
        Some("periodic_review") | Some("sprint_planner")
    ) {
        (
            Some(restricted_tools(CapabilityProfile::Standard)?),
            CapabilityProfile::Standard.prompt_note(),
        )
    } else {
        (None, None)
    };

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
                return Ok(());
            };
            mutate_and_persist(store, task_id, |s| {
                s.pr_url = Some(pr_str.clone());
                s.rounds.push(RoundResult {
                    turn: 1,
                    action: "implement".into(),
                    result: "resumed_checkpoint".into(),
                    detail: None,
                });
            })
            .await?;
            break 'implement (Some(pr_str), n);
        }

        let impl_phase_start = Instant::now();

        let initial_req = AgentRequest {
            prompt: first_prompt,
            project_root: project.clone(),
            context: context_items.clone(),
            max_budget_usd: req.max_budget_usd,
            execution_phase: Some(ExecutionPhase::Planning),
            allowed_tools: initial_allowed_tools,
            env_vars: cargo_env.clone(),
            ..Default::default()
        };

        let first_req = run_pre_execute(&interceptors, initial_req).await?;

        let max_validation_retries: u32 = interceptors
            .iter()
            .filter_map(|i| i.max_validation_retries())
            .max()
            .unwrap_or(2);
        let mut validation_attempt = 0u32;
        let mut impl_req = first_req.clone();

        let resp = loop {
            if let Some(max) = effective_max_turns {
                if turns_used >= max {
                    return Err(anyhow::anyhow!(
                        "Turn budget exhausted: used {} of {} allowed turns",
                        turns_used,
                        max
                    ));
                }
            }
            let raw = tokio::time::timeout(
                turn_timeout,
                lifecycle::run_agent_streaming(agent, impl_req.clone(), task_id, store, 1),
            )
            .await;
            turns_used += 1;
            *turns_used_acc = turns_used;
            match raw {
                Ok(Ok(r)) => {
                    let impl_tools = impl_req.allowed_tools.as_deref().unwrap_or(&[]);
                    let tool_violations = validate_tool_usage(&r.output, impl_tools);
                    let violation_err: Option<String> = if !tool_violations.is_empty() {
                        let msg = format!(
                            "Tool isolation violation: agent used disallowed tools: [{}]. Only [{}] are permitted.",
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
                        let modified = detect_modified_files(&project).await;
                        if modified.is_empty() {
                            None
                        } else {
                            let hook_event = ToolUseEvent {
                                tool_name: "file_write".to_string(),
                                affected_files: modified,
                                session_id: None,
                            };
                            run_post_tool_use(&interceptors, &hook_event, &project).await
                        }
                    };
                    let post_err = run_post_execute(&interceptors, &impl_req, &r).await;
                    let combined_err = violation_err.or(hook_err).or(post_err);
                    if let Some(err) = combined_err {
                        if validation_attempt < max_validation_retries {
                            validation_attempt += 1;
                            let backoff_ms = lifecycle::compute_backoff_ms(
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
                            let truncated = truncate_validation_error(&err, 2000);
                            impl_req.prompt = format!(
                                "{}\n\nPost-execution validation failed (attempt {}/{}):\n{}",
                                first_req.prompt,
                                validation_attempt,
                                max_validation_retries,
                                truncated
                            );
                            sleep(Duration::from_millis(backoff_ms)).await;
                            continue;
                        } else {
                            tracing::error!(
                                max = max_validation_retries,
                                error = %err,
                                "post-execution validation failed after max retries; aborting task"
                            );
                            run_on_error(&interceptors, &impl_req, &err).await;
                            return Err(anyhow::anyhow!(
                                "Post-execution validation failed after {} attempts: {}",
                                max_validation_retries,
                                err
                            ));
                        }
                    }
                    break r;
                }
                Ok(Err(e)) => {
                    run_on_error(&interceptors, &impl_req, &e.to_string()).await;
                    return Err(e.into());
                }
                Err(_) => {
                    let msg = format!(
                        "Implementation turn timed out after {}s",
                        turn_timeout.as_secs()
                    );
                    run_on_error(&interceptors, &impl_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
            }
        };

        let harness_core::agent::AgentResponse {
            output,
            stderr,
            token_usage: impl_token_usage,
            ..
        } = resp;

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
                s.rounds.push(RoundResult {
                    turn: 1,
                    action: "review".into(),
                    result: "completed".into(),
                    detail: if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    },
                });
            })
            .await?;
            tracing::info!(
                task_id = %task_id,
                status = "done",
                turns = 1,
                pr_url = tracing::field::Empty,
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
            return Ok(());
        }

        let (pr_url, pr_num) = match lifecycle::parse_implementation_outcome(&output) {
            lifecycle::ImplementationOutcome::PlanIssue(plan_issue) => {
                tracing::error!(
                    task_id = %task_id,
                    plan_issue = %plan_issue,
                    "implementation returned PLAN_ISSUE; marking task failed"
                );
                mutate_and_persist(store, task_id, |s| {
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
                    });
                })
                .await?;
                tracing::info!(
                    task_id = %task_id,
                    status = "failed",
                    turns = 2,
                    pr_url = tracing::field::Empty,
                    total_elapsed_secs = task_start.elapsed().as_secs(),
                    "task_completed"
                );
                return Ok(());
            }
            lifecycle::ImplementationOutcome::ParsedPr { pr_url, pr_num } => (pr_url, pr_num),
        };

        mutate_and_persist(store, task_id, |s| {
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
            });
        })
        .await?;

        if let Some(pr_url_str) = pr_url.as_deref() {
            store.log_event(crate::event_replay::TaskEvent::PrDetected {
                task_id: task_id.0.clone(),
                ts: crate::event_replay::now_ts(),
                pr_url: pr_url_str.to_string(),
            });
        }

        if let Some(pr_url_str) = pr_url.as_deref() {
            if let Err(e) = store
                .write_checkpoint(task_id, None, None, Some(pr_url_str), "pr_created")
                .await
            {
                tracing::warn!(task_id = %task_id, "failed to write pr checkpoint: {e}");
            }
        }

        let mut ev = Event::new(
            SessionId::new(),
            "task_implement",
            "task_runner",
            harness_core::types::Decision::Complete,
        );
        ev.detail = pr_num.map(|n| format!("pr={n}"));
        if let Err(e) = events.log(&ev).await {
            tracing::warn!("failed to log task_implement event: {e}");
        }

        let Some(pr_num) = pr_num else {
            tracing::warn!("no PR number found in agent output; skipping review");
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = 2;
            })
            .await?;
            tracing::info!(
                task_id = %task_id,
                status = "done",
                turns = 2,
                pr_url = tracing::field::Empty,
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
            return Ok(());
        };

        (pr_url, pr_num)
    }; // end 'implement

    let mut agent_pushed_commit = false;
    if review_config.enabled && !skip_agent_review {
        if let Some(reviewer) = reviewer {
            tracing::info!(pr_url = %pr_url.as_deref().unwrap_or(""), "starting agent review");
            let (review_ok, pushed) = lifecycle::run_agent_review(
                store,
                task_id,
                agent,
                reviewer,
                review_config,
                &context_items,
                &project,
                &interceptors,
                turn_timeout,
                pr_url.as_deref().unwrap_or(""),
                project_config.review_type.as_str(),
                &events,
                &cargo_env,
                effective_max_turns,
                &mut turns_used,
            )
            .await?;
            if !review_ok {
                return Ok(());
            }
            agent_pushed_commit = pushed;
        } else {
            tracing::warn!("agent review enabled but no reviewer agent configured; skipping");
        }
    }

    if !review_config.review_bot_auto_trigger {
        tracing::info!("review_bot_auto_trigger disabled; skipping external review wait");
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 2;
        })
        .await?;
        tracing::info!(
            task_id = %task_id,
            status = "done",
            turns = 2,
            pr_url = pr_url.as_deref().unwrap_or(""),
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(());
    }

    let wait_secs = resolved.review_wait_secs.unwrap_or(req.wait_secs);
    let max_rounds = resolved.review_max_rounds.unwrap_or(effective_max_rounds);

    review_loop::run_external_review_loop(review_loop::ExternalReviewParams {
        store,
        task_id,
        agent,
        interceptors: &interceptors,
        context_items: &context_items,
        project: &project,
        cargo_env: &cargo_env,
        pr_url: pr_url.as_deref(),
        pr_num,
        agent_pushed_commit,
        max_rounds,
        wait_secs,
        issue: req.issue,
        review_bot_command: &review_config.review_bot_command,
        reviewer_name: &review_config.reviewer_name,
        pre_push_cmds: &project_config.validation.pre_push,
        test_gate_timeout_secs: project_config.validation.test_gate_timeout_secs,
        events: &events,
        turn_timeout,
        task_start,
        jaccard_threshold: server_config.concurrency.loop_jaccard_threshold,
        effective_max_turns,
        turns_used: &mut turns_used,
    })
    .await
}
