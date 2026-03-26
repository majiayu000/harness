use super::*;

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: Option<&dyn CodeAgent>,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
    server_config: &harness_core::HarnessConfig,
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

    let project_config = load_project_config(&project);
    let resolved = harness_core::config::resolve_config(server_config, &project_config);
    let review_config = &resolved.review;
    let git = Some(&project_config.git);
    let repo_slug = detect_repo_slug(&project)
        .await
        .unwrap_or_else(|| "{owner}/{repo}".to_string());

    // --- Pipeline: Triage → Plan → Implement ---
    // For issue-based tasks without an existing PR, run triage first.
    // Triage decides whether to skip planning or go through a plan phase.
    let plan_output = if let Some(issue) = req.issue {
        // Only triage fresh issues (no existing PR to continue).
        let has_existing_pr = find_existing_pr_for_issue(&project, issue)
            .await
            .ok()
            .flatten()
            .is_some();
        if !has_existing_pr {
            run_triage_plan_pipeline(agent, store, task_id, issue, &cargo_env, &project, req)
                .await?
        } else {
            None
        }
    } else {
        None
    };

    // Resume normal flow — update status to Implementing for the main turn.
    update_status(store, task_id, TaskStatus::Implementing, 1).await?;
    let impl_phase_start = Instant::now();

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
        prompts::check_existing_pr(pr, &review_config.review_bot_command, &repo_slug)
    } else if matches!(
        req.source.as_deref(),
        Some("periodic_review") | Some("sprint_planner")
    ) {
        // Review/planner tasks use their prompt as-is — no "create PR" wrapper.
        req.prompt.clone().unwrap_or_default()
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default(), git)
    };

    // Inject language-detected validation instructions into the prompt when no
    // explicit validation config is set. This delegates validation to the agent
    // instead of running external commands that may not be installed.
    let first_prompt = if project_config.validation.pre_commit.is_empty()
        && project_config.validation.pre_push.is_empty()
    {
        let lang = lang_detect::detect_language(&project);
        let instructions = lang_detect::validation_prompt_instructions(lang, &project);
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

    // Prepend the Golden Principles constitution when enabled.
    let first_prompt =
        prepend_constitution(first_prompt, server_config.server.constitution_enabled);

    // Inject skill content directly into the prompt text.
    // Since harness uses single-turn `claude -p`, context items are not visible
    // to the agent — we must embed skill content in the prompt string itself.
    // Also records usage for any matched skills via record_use().
    let skill_additions = inject_skills_into_prompt(&skills, &first_prompt).await;
    let first_prompt = if skill_additions.is_empty() {
        first_prompt
    } else {
        first_prompt + &skill_additions
    };

    let context_items = collect_context_items(&skills, &project, &first_prompt).await;

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);

    // Periodic review tasks need Bash to run guard check commands but should
    // not have unrestricted write access — use Standard profile. All other
    // tasks (implementation) keep Full (no restriction, Vec::new()).
    //
    // NOTE: --allowedTools is NOT passed as a CLI flag (see claude.rs).
    // Tool restriction is enforced via prompt_note injection below.
    let (initial_allowed_tools, capability_prompt_note) =
        if req.source.as_deref() == Some("periodic_review") {
            (
                restricted_tools(CapabilityProfile::Standard)?,
                CapabilityProfile::Standard.prompt_note(),
            )
        } else {
            (Vec::new(), None)
        };

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

    // Run pre_execute interceptors; Block aborts the task.
    let first_req = run_pre_execute(&interceptors, initial_req).await?;

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
        let raw = tokio::time::timeout(
            turn_timeout,
            run_agent_streaming(agent, impl_req.clone(), task_id, store, 1),
        )
        .await;
        match raw {
            Ok(Ok(r)) => {
                // Post-execution tool isolation check. Since --allowedTools is not
                // passed to the CLI, we scan the output for disallowed tool calls.
                // Violations feed into the retry loop so the agent gets a chance to
                // self-correct (fail-closed, not fail-open).
                let tool_violations = validate_tool_usage(&r.output, &impl_req.allowed_tools);
                let violation_err: Option<String> = if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation: agent used disallowed tools: [{}]. Only [{}] are permitted.",
                        tool_violations.join(", "),
                        impl_req.allowed_tools.join(", ")
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
                        let truncated = truncate_validation_error(&err, 2000);
                        impl_req.prompt = format!(
                            "{}\n\nPost-execution validation failed (attempt {}/{}):\n{}",
                            first_req.prompt, validation_attempt, max_validation_retries, truncated
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

    let AgentResponse {
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

    let pr_url = prompts::parse_pr_url(&output);
    let pr_num = pr_url.as_deref().and_then(prompts::extract_pr_number);

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

    // Log implementation event
    let mut ev = Event::new(
        SessionId::new(),
        "task_implement",
        "task_runner",
        harness_core::Decision::Complete,
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

    // Agent review loop (if enabled and reviewer available)
    if review_config.enabled {
        if let Some(reviewer) = reviewer {
            tracing::info!(pr_url = %pr_url.as_deref().unwrap_or(""), "starting agent review");
            run_agent_review(
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
            )
            .await?;
        } else {
            tracing::warn!("agent review enabled but no reviewer agent configured; skipping");
        }
    }

    // Skip external review bot wait when auto-trigger is disabled — there is
    // no bot to wait for, so the loop would always exhaust all rounds and fail.
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

    // Wait for external review bot.
    // Use a local counter instead of querying the store to derive waiting_count —
    // task execution is sequential within a single tokio task, so a plain u32 suffices.
    let mut waiting_count: u32 = 0;
    waiting_count += 1;
    update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;

    let wait_secs = req.wait_secs;
    tracing::info!("waiting {wait_secs}s for review bot on PR #{pr_num}");
    sleep(Duration::from_secs(wait_secs)).await;

    let review_phase_start = Instant::now();

    // Review loop — agent checks bot review, fixes issues, pushes, triggers re-review.
    // Uses review_prompt (full write access) instead of check_existing_pr (read-only)
    // so the agent can actually fix issues the bot found.
    let mut prev_fixed = false;
    let repo_slug = prompts::repo_slug_from_pr_url(pr_url.as_deref());
    for round in 1..=req.max_rounds {
        update_status(store, task_id, TaskStatus::Reviewing, round).await?;

        let check_req = AgentRequest {
            prompt: prompts::review_prompt(
                req.issue,
                pr_num,
                round,
                prev_fixed,
                &review_config.review_bot_command,
                &review_config.reviewer_name,
                &repo_slug,
            ),
            project_root: project.clone(),
            context: context_items.clone(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let check_req = run_pre_execute(&interceptors, check_req).await?;

        let resp = tokio::time::timeout(turn_timeout, agent.execute(check_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(&r.output, &check_req.allowed_tools);
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in review check round {round}: agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        round,
                        ?tool_violations,
                        "review check: agent used tools outside allowed list"
                    );
                    run_on_error(&interceptors, &check_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
                if let Some(val_err) = run_post_execute(&interceptors, &check_req, &r).await {
                    tracing::warn!(
                        round,
                        error = %val_err,
                        "post-execute validation failed in review check; continuing"
                    );
                }
                r
            }
            Ok(Err(e)) => {
                // Quota exhausted is not retryable — break immediately instead of
                // burning remaining review rounds on repeated 402 errors.
                if matches!(e, HarnessError::QuotaExhausted(_)) {
                    tracing::error!(round, error = %e, "quota exhausted during review — aborting review loop");
                    run_on_error(&interceptors, &check_req, &e.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(e.to_string());
                    })
                    .await?;
                    return Ok(());
                }
                run_on_error(&interceptors, &check_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Review check round {round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(&interceptors, &check_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let AgentResponse { output, stderr, .. } = resp;

        if !stderr.is_empty() {
            tracing::warn!(round, stderr = %stderr, "agent stderr during review check");
        }

        let lgtm = prompts::is_lgtm(&output);
        let waiting = prompts::is_waiting(&output);
        let fixed = !lgtm && !waiting;

        // WAITING means review bot hasn't posted yet (e.g., quota exhausted).
        // Don't consume a round — just sleep and retry without incrementing.
        if waiting {
            tracing::info!(
                round,
                "PR #{pr_num} review bot has not responded yet; sleeping without consuming round"
            );
            waiting_count += 1;
            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
            continue;
        }

        let result_label = if lgtm { "lgtm" } else { "fixed" };
        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: result_label.into(),
                detail: None,
            });
        })
        .await?;

        // Log pr_review event for observability and GC signal detection.
        let mut ev = Event::new(
            SessionId::new(),
            "pr_review",
            "task_runner",
            if lgtm {
                harness_core::Decision::Complete
            } else {
                harness_core::Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(format!("round {round}: {result_label}"));
        if let Err(e) = events.log(&ev).await {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            tracing::info!("PR #{pr_num} approved at round {round}");
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = round.saturating_add(1);
            })
            .await?;
            tracing::info!(
                task_id = %task_id,
                phase = "reviewing",
                elapsed_secs = review_phase_start.elapsed().as_secs(),
                "phase_completed"
            );
            tracing::info!(
                task_id = %task_id,
                status = "done",
                turns = round.saturating_add(1),
                pr_url = pr_url.as_deref().unwrap_or(""),
                total_elapsed_secs = task_start.elapsed().as_secs(),
                "task_completed"
            );
            return Ok(());
        }

        // Agent output FIXED — it already fixed the bot's comments and pushed.
        // Track prev_fixed so the next round's prompt includes freshness check.
        prev_fixed = fixed;
        tracing::info!("PR #{pr_num} fixed at round {round}; waiting for bot re-review");
        if round < req.max_rounds {
            waiting_count += 1;
            update_status(store, task_id, TaskStatus::Waiting, waiting_count).await?;
            sleep(Duration::from_secs(wait_secs)).await;
        }
    }

    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.turn = req.max_rounds.saturating_add(1);
        s.error = Some(format!(
            "Task did not receive LGTM after {} review rounds.",
            req.max_rounds
        ));
    })
    .await?;
    tracing::info!(
        task_id = %task_id,
        phase = "reviewing",
        elapsed_secs = review_phase_start.elapsed().as_secs(),
        "phase_completed"
    );
    tracing::info!(
        task_id = %task_id,
        status = "failed",
        turns = req.max_rounds.saturating_add(1),
        pr_url = pr_url.as_deref().unwrap_or(""),
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_review(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: &dyn CodeAgent,
    review_config: &harness_core::AgentReviewConfig,
    context_items: &[ContextItem],
    project: &Path,
    interceptors: &[Arc<dyn harness_core::interceptor::TurnInterceptor>],
    turn_timeout: Duration,
    pr_url: &str,
    project_type: &str,
    events: &harness_observe::EventStore,
    cargo_env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let max_rounds = review_config.max_rounds;
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await?;

        // Reviewer evaluates the PR diff — read-only except Bash for `gh pr diff`.
        let review_req = AgentRequest {
            prompt: {
                let base = prompts::agent_review_prompt(pr_url, agent_round, project_type);
                // Inject capability note — primary enforcement now that --allowedTools
                // is not passed to the CLI (issue #483).
                let note = "Tool restriction: you are operating in review mode. \
                     Only Read, Grep, Glob, and Bash are permitted. \
                     Use Bash ONLY for read-only commands like `gh pr diff`. \
                     Do NOT call Write, Edit, or any other tool.";
                format!("{note}\n\n{base}")
            },
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Validation),
            allowed_tools: vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
                "Bash".to_string(),
            ],
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let review_req = run_pre_execute(interceptors, review_req).await?;

        let resp = tokio::time::timeout(turn_timeout, reviewer.execute(review_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(&r.output, &review_req.allowed_tools);
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in agent review round {agent_round}: agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        agent_round,
                        ?tool_violations,
                        "agent review: agent used tools outside allowed list"
                    );
                    run_on_error(interceptors, &review_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
                if let Some(val_err) = run_post_execute(interceptors, &review_req, &r).await {
                    tracing::warn!(
                        agent_round,
                        error = %val_err,
                        "post-execute validation failed in agent review; continuing"
                    );
                }
                r
            }
            Ok(Err(e)) => {
                if matches!(e, HarnessError::QuotaExhausted(_)) {
                    tracing::error!(agent_round, error = %e, "quota exhausted during agent review — aborting");
                    run_on_error(interceptors, &review_req, &e.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(e.to_string());
                    })
                    .await?;
                    return Ok(());
                }
                run_on_error(interceptors, &review_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review round {agent_round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &review_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let AgentResponse { output, stderr, .. } = resp;

        if !stderr.is_empty() {
            tracing::warn!(agent_round, stderr = %stderr, "agent reviewer stderr");
        }

        let approved = prompts::is_approved(&output);
        let issues = prompts::extract_review_issues(&output);
        let review_detail = output;

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0, // agent review rounds use turn 0
                action: "agent_review".into(),
                result: if approved {
                    "approved".into()
                } else {
                    format!("{} issues", issues.len())
                },
                detail: Some(review_detail),
            });
        })
        .await?;

        // Log agent_review event
        let mut ev = Event::new(
            SessionId::new(),
            "agent_review",
            "task_runner",
            if approved {
                harness_core::Decision::Complete
            } else {
                harness_core::Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_url}"));
        ev.reason = Some(if approved {
            format!("round {agent_round}: approved")
        } else {
            format!("round {agent_round}: {} issues", issues.len())
        });
        if let Err(e) = events.log(&ev).await {
            tracing::warn!("failed to log agent_review event: {e}");
        }

        if approved {
            tracing::info!("agent review approved at round {agent_round}");
            break;
        }

        // Malformed reviewer output: neither APPROVED nor any ISSUE: lines.
        // Sending an empty fix prompt would produce arbitrary or no-op commits,
        // so treat this as a reviewer protocol failure and abort the review loop.
        if issues.is_empty() {
            tracing::warn!(
                agent_round,
                "agent reviewer output contained neither APPROVED nor ISSUE: lines; \
                 treating as protocol failure and skipping fix round"
            );
            break;
        }

        if agent_round == max_rounds {
            tracing::info!(
                "agent review exhausted {max_rounds} rounds, proceeding to GitHub review"
            );
            break;
        }

        // Implementor fixes the issues
        let fix_req = AgentRequest {
            prompt: prompts::agent_review_fix_prompt(pr_url, &issues, agent_round, project_type),
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let fix_req = run_pre_execute(interceptors, fix_req).await?;

        let fix_resp = tokio::time::timeout(turn_timeout, agent.execute(fix_req.clone())).await;
        match fix_resp {
            Ok(Ok(r)) => {
                if let Some(val_err) = run_post_execute(interceptors, &fix_req, &r).await {
                    tracing::warn!(
                        agent_round,
                        error = %val_err,
                        "post-execute validation failed in agent review fix; continuing"
                    );
                }
            }
            Ok(Err(e)) => {
                run_on_error(interceptors, &fix_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review fix round {agent_round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &fix_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        }

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0,
                action: "agent_review_fix".into(),
                result: "fixed".into(),
                detail: None,
            });
        })
        .await?;
    }

    Ok(())
}
