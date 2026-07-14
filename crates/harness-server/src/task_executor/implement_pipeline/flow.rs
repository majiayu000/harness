use super::outcome::{self, ProcessedImplementation};
use super::turn;
use super::{prepend_constitution, ImplementOutcome};
use crate::task_executor::helpers::{
    collect_context_items, inject_project_context_into_prompt, inject_skills_into_prompt,
    matched_skills_for_prompt, update_status,
};
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskStatus, TaskStore,
};
use anyhow::Context;
use harness_core::agent::CodeAgent;
use harness_core::compress::ObservationCompressor;
use harness_core::types::{Decision, Event, SessionId};
use harness_core::{lang_detect, prompts};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::time::{Duration, Instant};

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
    _review_config: &harness_core::config::agents::AgentReviewConfig,
    interceptors: &[Arc<dyn harness_core::interceptor::TurnInterceptor>],
    events: &Arc<harness_observe::event_store::EventStore>,
    skills: &Arc<tokio::sync::RwLock<harness_skills::store::SkillStore>>,
    cargo_env: &HashMap<String, String>,
    git: Option<&harness_core::config::project::GitConfig>,
    repo_slug: &str,
    project: &Path,
    project_root: &Path,
    observation_compressor: Option<&dyn ObservationCompressor>,
    plan_output: Option<String>,
    resumed_pr_url: Option<String>,
    resumed_review_prep: Option<prompts::PrReviewPrepOutcome>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    turn_timeout: Duration,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
    task_start: Instant,
) -> anyhow::Result<ImplementOutcome> {
    // Resume normal flow — update status to Implementing for the main turn.
    update_status(store, task_id, TaskStatus::Implementing, 1).await?;

    let first_prompt = if let Some(issue) = req.issue {
        let base = match super::super::pr_detection::find_existing_pr_for_issue_with_token(
            project,
            issue,
            server_config.server.github_token.as_deref(),
        )
        .await
        {
            Ok(Some((pr_num, branch, pr_url))) => {
                tracing::info!(
                    "reusing existing PR #{pr_num} on branch `{branch}` for issue #{issue}"
                );
                let slug = prompts::repo_slug_from_pr_url(Some(&pr_url));
                prompts::continue_existing_pr(issue, pr_num, &branch, &slug)
            }
            Ok(None) => {
                let plan_for_prompt = match plan_output.as_deref() {
                    Some(raw) => Some(
                        super::super::compression::compress_observation_for_prompt(
                            observation_compressor,
                            raw,
                            &format!("implementation phase for issue #{issue} in {repo_slug}"),
                        )
                        .await,
                    ),
                    None => None,
                };
                prompts::implement_from_issue(issue, git, plan_for_prompt.as_deref())
                    .to_prompt_string()
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to check for an existing PR for issue #{issue}; refusing to create a duplicate PR while lookup is unavailable"
                    )
                });
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
        prompts::prepare_pr_for_review(pr, repo_slug)
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
    //
    // Match against the task prompt before appending project instruction files.
    // Otherwise AGENTS.md / CLAUDE.md trigger phrases can cause unrelated skills
    // to be injected and logged as used.
    let skill_match_prompt = first_prompt.clone();
    let matched_skills = matched_skills_for_prompt(skills, &skill_match_prompt).await;
    let skill_additions = inject_skills_into_prompt(skills, &skill_match_prompt).await;

    // Inject project instructions directly into the prompt text.
    // AgentRequest.context is retained for observability, but CLI agents do not
    // receive it automatically in single-turn mode.
    let first_prompt = inject_project_context_into_prompt(project, first_prompt);
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

    let context_items = collect_context_items(skills, project, &skill_match_prompt).await;

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

    if let Some(pr_str) = resumed_pr_url {
        tracing::info!(
            task_id = %task_id,
            pr_url = %pr_str,
            "checkpoint resume: PR exists, skipping implement phase"
        );
        let Some(pr_num) = prompts::extract_pr_number(&pr_str) else {
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
        return Ok(ImplementOutcome::Proceed {
            pr_url: Some(pr_str),
            pr_num,
            implementation_pushed_commit: false,
            review_prep: resumed_review_prep,
            context_items,
            turn_timeout,
            initial_allowed_tools,
        });
    }

    let executed = turn::execute_implementation_turn(
        agent,
        first_prompt,
        &context_items,
        initial_allowed_tools.as_deref(),
        cargo_env,
        req,
        store,
        task_id,
        interceptors,
        events,
        project,
        turn_timeout,
        effective_max_turns,
        turns_used,
        turns_used_acc,
    )
    .await?;

    match outcome::process_implementation_output(
        store,
        task_id,
        req,
        events,
        project,
        project_root,
        plan_output,
        issue_workflow_store.as_deref(),
        workflow_runtime_store.as_deref(),
        executed,
        task_start,
    )
    .await?
    {
        ProcessedImplementation::Done => Ok(ImplementOutcome::Done),
        ProcessedImplementation::Replan {
            issue,
            plan_issue,
            prior_plan,
        } => Ok(ImplementOutcome::Replan {
            issue,
            plan_issue,
            prior_plan,
        }),
        ProcessedImplementation::Proceed {
            pr_url,
            pr_num,
            pushed_commit,
            review_prep,
        } => Ok(ImplementOutcome::Proceed {
            pr_url,
            pr_num,
            implementation_pushed_commit: pushed_commit,
            review_prep,
            context_items,
            turn_timeout,
            initial_allowed_tools,
        }),
    }
}
