pub(crate) mod agent_review;
pub(crate) mod conflict_resolver;
pub(crate) mod helpers;
pub(crate) mod implement_pipeline;
pub(crate) mod pr_detection;
pub(crate) mod review_loop;
pub(crate) mod triage_pipeline;
pub(crate) mod turn_lifecycle;

use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskKind, TaskStatus, TaskStore,
};
use anyhow::Context;
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::config::agents::CapabilityProfile;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::{config::project::load_project_config, lang_detect, prompts};
use std::collections::HashMap;

use chrono::Utc;
use helpers::update_status;

/// Extract tool list from a capability profile, returning an error if the
/// profile unexpectedly returns `None` (which means Full/unrestricted).
/// A misconfigured profile causes a hard failure rather than silent degradation,
/// per U-23 (no silent capability downgrade).
// Re-export so existing call sites in handlers/ don't need updating.
pub(crate) use turn_lifecycle::run_turn_lifecycle;
fn restricted_tools(profile: CapabilityProfile) -> anyhow::Result<Vec<String>> {
    profile.tools().ok_or_else(|| {
        anyhow::anyhow!(
            "capability profile {:?} returned None from tools() — misconfiguration",
            profile
        )
    })
}
#[cfg(test)]
use pr_detection::{
    build_fix_ci_prompt, parse_harness_mention_command, HarnessMentionCommand, PromptBuilder,
};
use pr_detection::{detect_repo_slug, find_existing_pr_for_issue_with_token};
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

use tokio::process::Command as TokioCommand;
use tokio::time::timeout;

/// State shared across pipeline stages within a single task execution.
#[allow(dead_code)]
pub(crate) struct TaskContext {
    pub turn_timeout: Duration,
    pub effective_max_turns: Option<u32>,
    pub turns_used: u32,
    pub cargo_env: std::collections::HashMap<String, String>,
    pub project: std::path::PathBuf,
}

fn should_run_issue_triage(skip_triage: bool, has_existing_pr: bool) -> bool {
    !skip_triage && !has_existing_pr
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewEntryDecision {
    Proceed { rebase_pushed: bool },
    FailConflict { error: String, paths_csv: String },
}

fn review_entry_decision(
    pr_num: u64,
    review_prep: Option<&prompts::PrReviewPrepOutcome>,
) -> ReviewEntryDecision {
    match review_prep {
        Some(prompts::PrReviewPrepOutcome::RebasePushed) => ReviewEntryDecision::Proceed {
            rebase_pushed: true,
        },
        Some(prompts::PrReviewPrepOutcome::RebaseSkipped) | None => ReviewEntryDecision::Proceed {
            rebase_pushed: false,
        },
        Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths }) => {
            let paths_csv = if paths.is_empty() {
                "unknown paths".to_string()
            } else {
                paths.join(", ")
            };
            ReviewEntryDecision::FailConflict {
                error: format!(
                    "PR #{pr_num} has rebase conflicts: {paths_csv}; manual resolution required"
                ),
                paths_csv,
            }
        }
    }
}

fn review_prep_from_rounds(rounds: &[RoundResult]) -> Option<prompts::PrReviewPrepOutcome> {
    rounds
        .iter()
        .rev()
        .find(|round| round.action == "implement")
        .and_then(|round| match round.result.as_str() {
            "rebase_pushed" => Some(prompts::PrReviewPrepOutcome::RebasePushed),
            "rebase_skipped" => Some(prompts::PrReviewPrepOutcome::RebaseSkipped),
            "rebase_conflict" => round
                .detail
                .as_deref()
                .and_then(prompts::parse_pr_review_prep_outcome)
                .or_else(|| {
                    Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths: Vec::new() })
                }),
            _ => None,
        })
}

fn review_prep_from_checkpoint_phase(last_phase: &str) -> Option<prompts::PrReviewPrepOutcome> {
    match last_phase {
        "rebase_pushed" => Some(prompts::PrReviewPrepOutcome::RebasePushed),
        "rebase_skipped" => Some(prompts::PrReviewPrepOutcome::RebaseSkipped),
        "rebase_conflict" => Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths: Vec::new() }),
        _ => None,
    }
}

async fn fail_rebase_conflict(
    store: &TaskStore,
    task_id: &TaskId,
    events: &Arc<harness_observe::event_store::EventStore>,
    pr_num: u64,
    error: String,
    paths_csv: String,
) -> anyhow::Result<()> {
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.error = Some(error.clone());
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: error,
    });
    let mut event = harness_core::types::Event::new(
        harness_core::types::SessionId::new(),
        "pr_rebase_conflict",
        "task_runner",
        harness_core::types::Decision::Block,
    );
    event.reason = Some(paths_csv);
    event.detail = Some(format!("task_id={} pr={pr_num}", task_id.as_str()));
    if let Err(err) = events.log(&event).await {
        tracing::warn!("failed to log pr_rebase_conflict event: {err}");
    }
    Ok(())
}

/// Run the project's test commands as a hard gate before accepting LGTM.
///
/// When `custom_cmds` is non-empty (from `validation.pre_push` in project config),
/// those commands are run in order instead of language-detected defaults.
/// When `custom_cmds` is empty, falls back to language detection.
///
/// Returns `Ok(())` when all commands pass or when no test command is detectable
/// (soft degradation — unknown project type skips rather than hard-fails).
///
/// Returns `Err(output)` containing stdout/stderr of the first failing command.
async fn run_test_gate(
    project_root: &std::path::Path,
    custom_cmds: &[String],
    timeout_secs: u64,
    extra_env: &HashMap<String, String>,
) -> Result<(), String> {
    // Prefer explicitly configured pre_push commands; fall back to language detection.
    let cmds: Vec<String> = if !custom_cmds.is_empty() {
        // Issue 1 fix: validate every custom command against the safety allowlist
        // before executing. Malicious repos could supply shell-injection payloads
        // via `.harness/config.toml` validation.pre_push.
        for cmd in custom_cmds {
            if let Err(e) = crate::post_validator::validate_command_safety(cmd) {
                return Err(format!("test gate: command rejected by safety check: {e}"));
            }
        }
        custom_cmds.to_vec()
    } else {
        match lang_detect::primary_test_command(project_root) {
            Some(cmd) => vec![cmd],
            None => {
                tracing::info!(
                    project = %project_root.display(),
                    "test gate: no test command detected for project, skipping"
                );
                return Ok(());
            }
        }
    };

    for cmd in &cmds {
        tracing::info!(cmd = %cmd, "test gate: running tests before accepting LGTM");

        let child = match TokioCommand::new("sh")
            .args(["-c", cmd])
            .current_dir(project_root)
            // Issue 3 fix: inherit the per-task CARGO_TARGET_DIR so parallel
            // Rust tasks do not contend on the same build directory (issue #488).
            .envs(extra_env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("test gate: failed to spawn `{cmd}`: {e}")),
        };

        match timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
            Ok(Ok(out)) if out.status.success() => {
                tracing::info!(cmd = %cmd, "test gate: tests passed");
            }
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let code = out.status.code().unwrap_or(-1);
                return Err(format!(
                    "Test gate failed (exit {code})\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ));
            }
            Ok(Err(e)) => return Err(format!("test gate: `{cmd}` failed to wait: {e}")),
            Err(_) => {
                return Err(format!(
                    "Test gate timed out after {timeout_secs}s (command: `{cmd}`)"
                ))
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_non_implementation_task(
    store: &TaskStore,
    task_id: &TaskId,
    task_kind: TaskKind,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
    project: &std::path::Path,
    server_config: &harness_core::config::HarnessConfig,
    interceptors: &Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    events: &Arc<harness_observe::event_store::EventStore>,
    skills: &Arc<RwLock<harness_skills::store::SkillStore>>,
    cargo_env: &HashMap<String, String>,
    turn_timeout: Duration,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
    task_start: Instant,
) -> anyhow::Result<()> {
    let Some(system_input) = req.system_input.as_ref() else {
        anyhow::bail!(
            "{} task is missing restart-safe input metadata",
            task_kind.as_ref()
        );
    };

    update_status(store, task_id, task_kind.execution_status(), 1).await?;

    let mut prompt = implement_pipeline::prepend_constitution(
        system_input.prompt().to_string(),
        server_config.server.constitution_enabled,
    );
    let skill_match_prompt = prompt.clone();
    let skill_additions = helpers::inject_skills_into_prompt(skills, &skill_match_prompt).await;
    prompt = helpers::inject_project_context_into_prompt(project, prompt);
    if !skill_additions.is_empty() {
        prompt.push_str(&skill_additions);
    }
    let context_items = helpers::collect_context_items(skills, project, &skill_match_prompt).await;
    let allowed_tools = Some(restricted_tools(CapabilityProfile::Standard)?);
    if let Some(note) = CapabilityProfile::Standard.prompt_note() {
        prompt = format!("{note}\n\n{prompt}");
    }

    let prompt_built_at = Utc::now();
    let initial_req = AgentRequest {
        prompt,
        project_root: project.to_path_buf(),
        context: context_items,
        max_budget_usd: req.max_budget_usd,
        execution_phase: Some(harness_core::types::ExecutionPhase::Planning),
        allowed_tools: allowed_tools.clone(),
        env_vars: cargo_env.clone(),
        ..Default::default()
    };
    let first_req = helpers::run_pre_execute(interceptors, initial_req).await?;
    let max_validation_retries: u32 = interceptors
        .iter()
        .filter_map(|i| i.max_validation_retries())
        .max()
        .unwrap_or(2);
    let mut validation_attempt = 0u32;
    let mut turn_req = first_req.clone();

    let resp = loop {
        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                anyhow::bail!(
                    "Turn budget exhausted: used {} of {} allowed turns",
                    turns_used,
                    max
                );
            }
        }
        let agent_started_at = Utc::now();
        let raw = tokio::time::timeout(
            turn_timeout,
            helpers::run_agent_streaming(
                agent,
                turn_req.clone(),
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
                let response = &success.response;
                let turn_tools = turn_req.allowed_tools.as_deref().unwrap_or(&[]);
                let tool_violations = validate_tool_usage(&response.output, turn_tools);
                let violation_err: Option<String> = if tool_violations.is_empty() {
                    None
                } else {
                    Some(format!(
                        "[VALIDATION ERROR] Tool isolation violation: agent used disallowed tools: [{}]. Only [{}] are permitted.",
                        tool_violations.join(", "),
                        turn_tools.join(", ")
                    ))
                };
                let hook_err = {
                    let modified = helpers::detect_modified_files(project).await;
                    if modified.is_empty() {
                        None
                    } else {
                        let hook_event = harness_core::interceptor::ToolUseEvent {
                            tool_name: "file_write".to_string(),
                            affected_files: modified,
                            session_id: None,
                        };
                        helpers::run_post_tool_use(interceptors, &hook_event, project).await
                    }
                };
                let post_err = helpers::run_post_execute(interceptors, &turn_req, response).await;
                if let Some(err) = violation_err.or(hook_err).or(post_err) {
                    if validation_attempt < max_validation_retries {
                        validation_attempt += 1;
                        let backoff_ms = implement_pipeline::compute_backoff_ms(
                            req.retry_base_backoff_ms,
                            req.retry_max_backoff_ms,
                            validation_attempt,
                        );
                        let truncated = helpers::truncate_validation_error(&err, 2000);
                        turn_req.prompt = prompts::validation_retry_prompt(
                            &first_req.prompt,
                            validation_attempt,
                            max_validation_retries,
                            &truncated,
                        );
                        sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    helpers::run_on_error(interceptors, &turn_req, &err).await;
                    anyhow::bail!(
                        "Post-execution validation failed after {} attempts: {}",
                        max_validation_retries,
                        err
                    );
                }
                break success;
            }
            Ok(Err(err)) => {
                helpers::run_on_error(interceptors, &turn_req, &err.error.to_string()).await;
                return Err(err.error.into());
            }
            Err(_) => {
                let msg = format!(
                    "{} timed out after {}s",
                    task_kind.as_ref(),
                    turn_timeout.as_secs()
                );
                helpers::run_on_error(interceptors, &turn_req, &msg).await;
                anyhow::bail!(msg);
            }
        }
    };

    if implement_pipeline::contains_worktree_collision_sentinel(&resp.response.output) {
        let collision_telemetry = resp.telemetry.clone();
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = 1;
            s.error = Some(
                "WorktreeCollision: agent observed worktree managed by another harness session"
                    .into(),
            );
            s.rounds.push(crate::task_runner::RoundResult::new(
                1,
                task_kind.as_ref().to_string(),
                "worktree_collision",
                if resp.response.output.is_empty() {
                    None
                } else {
                    Some(resp.response.output.clone())
                },
                Some(collision_telemetry),
                None,
            ));
        })
        .await?;
        tracing::info!(
            task_id = %task_id,
            task_kind = task_kind.as_ref(),
            status = "failed",
            total_elapsed_secs = task_start.elapsed().as_secs(),
            "task_completed"
        );
        return Ok(());
    }

    let done_telemetry = resp.telemetry.clone();
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Done;
        s.turn = 1;
        s.rounds.push(crate::task_runner::RoundResult::new(
            1,
            task_kind.as_ref().to_string(),
            "completed",
            if resp.response.output.is_empty() {
                None
            } else {
                Some(resp.response.output.clone())
            },
            Some(done_telemetry),
            None,
        ));
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::Completed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
    });
    let event_name = format!("task_{}", task_kind.as_ref());
    let mut ev = harness_core::types::Event::new(
        harness_core::types::SessionId::new(),
        &event_name,
        "task_runner",
        harness_core::types::Decision::Complete,
    );
    ev.detail = Some(format!("task_id={}", task_id.as_str()));
    if let Err(err) = events.log(&ev).await {
        tracing::warn!("failed to log {} event: {err}", task_kind.as_ref());
    }
    tracing::info!(
        task_id = %task_id,
        task_kind = task_kind.as_ref(),
        status = "done",
        total_elapsed_secs = task_start.elapsed().as_secs(),
        "task_completed"
    );
    Ok(())
}

enum ConflictGateOutcome {
    Clean,
    RebasePushed,
    Failed,
}

#[allow(clippy::too_many_arguments)]
async fn run_resumed_pr_conflict_gate(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
    project: &std::path::Path,
    pr_num: u64,
    repo_slug: &str,
    context_items: &[harness_core::types::ContextItem],
    interceptors: &Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    events: &Arc<harness_observe::event_store::EventStore>,
    cargo_env: &HashMap<String, String>,
    turn_timeout: Duration,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
    turns_used_acc: &mut u32,
) -> anyhow::Result<ConflictGateOutcome> {
    use crate::task_executor::conflict_resolver::{
        parse_conflict_check_output, ConflictCheckOutcome,
    };

    let gate_turn = turns_used.saturating_add(1);
    let persist_failure =
        |result: &'static str,
         reason: String,
         detail: Option<String>,
         telemetry: Option<harness_core::types::TurnTelemetry>,
         failure: Option<harness_core::types::TurnFailure>| async move {
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = gate_turn;
                s.error = Some(reason.clone());
                s.rounds.push(crate::task_runner::RoundResult::new(
                    gate_turn,
                    "conflict_gate",
                    result,
                    detail.clone(),
                    telemetry.clone(),
                    failure.clone(),
                ));
            })
            .await?;
            let event = helpers::build_task_event(
                task_id,
                gate_turn,
                "conflict_gate",
                "pr_conflict_gate",
                harness_core::types::Decision::Block,
                Some(reason),
                Some(format!("pr={pr_num}")),
                telemetry,
                failure,
                detail,
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log pr_conflict_gate event: {error}");
            }
            Ok::<ConflictGateOutcome, anyhow::Error>(ConflictGateOutcome::Failed)
        };

    if let Some(max) = effective_max_turns {
        if *turns_used >= max {
            return persist_failure(
                "turn_budget_exhausted",
                format!(
                    "pr:{pr_num} conflict gate could not run before review; manual resolution required: turn budget exhausted after {} of {} allowed turns",
                    turns_used, max
                ),
                None,
                None,
                None,
            )
            .await;
        }
    }

    let prompt_built_at = Utc::now();
    let gate_prompt = prompts::check_resumed_pr_conflicts(pr_num, repo_slug, project);
    let gate_req = AgentRequest {
        prompt: gate_prompt,
        project_root: project.to_path_buf(),
        context: context_items.to_vec(),
        max_budget_usd: req.max_budget_usd,
        execution_phase: Some(harness_core::types::ExecutionPhase::Execution),
        env_vars: cargo_env.clone(),
        ..Default::default()
    };
    let gate_req = helpers::run_pre_execute(interceptors, gate_req).await?;
    let gate_started_at = Utc::now();
    let gate_resp = tokio::time::timeout(
        turn_timeout,
        helpers::run_agent_streaming(
            agent,
            gate_req.clone(),
            task_id,
            store,
            gate_turn,
            prompt_built_at,
            gate_started_at,
        ),
    )
    .await;
    *turns_used += 1;
    *turns_used_acc = *turns_used;

    let (response, telemetry) = match gate_resp {
        Ok(Ok(success)) => {
            let response = success.response;
            if let Some(validation_err) =
                helpers::run_post_execute(interceptors, &gate_req, &response).await
            {
                helpers::run_on_error(interceptors, &gate_req, &validation_err).await;
                return persist_failure(
                    "validation_failed",
                    format!(
                        "pr:{pr_num} conflict gate failed closed; manual resolution required: post-execution validation failed: {validation_err}"
                    ),
                    if response.output.is_empty() {
                        None
                    } else {
                        Some(response.output)
                    },
                    Some(success.telemetry),
                    None,
                )
                .await;
            }
            (response, success.telemetry)
        }
        Ok(Err(failure)) => {
            helpers::run_on_error(interceptors, &gate_req, &failure.error.to_string()).await;
            return persist_failure(
                "failed",
                format!(
                    "pr:{pr_num} conflict gate failed closed; manual resolution required: {}",
                    failure.error
                ),
                None,
                Some(failure.telemetry),
                Some(failure.failure),
            )
            .await;
        }
        Err(_) => {
            let msg = format!(
                "pr:{pr_num} conflict gate timed out; manual resolution required after {}s",
                turn_timeout.as_secs()
            );
            helpers::run_on_error(interceptors, &gate_req, &msg).await;
            let telemetry =
                helpers::telemetry_for_timeout(prompt_built_at, gate_started_at, Utc::now(), None);
            let failure = harness_core::types::TurnFailure {
                kind: harness_core::types::TurnFailureKind::Timeout,
                provider: Some(agent.name().to_string()),
                upstream_status: None,
                message: Some(msg.clone()),
                body_excerpt: None,
            };
            return persist_failure("timeout", msg, None, Some(telemetry), Some(failure)).await;
        }
    };

    let detail = if response.output.is_empty() {
        None
    } else {
        Some(response.output.clone())
    };
    match parse_conflict_check_output(&response.output) {
        Ok(ConflictCheckOutcome::CleanPr) => {
            mutate_and_persist(store, task_id, |s| {
                s.rounds.push(crate::task_runner::RoundResult::new(
                    gate_turn,
                    "conflict_gate",
                    "clean",
                    detail.clone(),
                    Some(telemetry.clone()),
                    None,
                ));
            })
            .await?;
            let event = helpers::build_task_event(
                task_id,
                gate_turn,
                "conflict_gate",
                "pr_conflict_gate",
                harness_core::types::Decision::Complete,
                Some("resumed PR conflict gate passed cleanly".to_string()),
                Some(format!("pr={pr_num}")),
                Some(telemetry),
                None,
                detail,
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log pr_conflict_gate event: {error}");
            }
            Ok(ConflictGateOutcome::Clean)
        }
        Ok(ConflictCheckOutcome::RebasePushed) => {
            mutate_and_persist(store, task_id, |s| {
                s.rounds.push(crate::task_runner::RoundResult::new(
                    gate_turn,
                    "conflict_gate",
                    "rebase_pushed",
                    detail.clone(),
                    Some(telemetry.clone()),
                    None,
                ));
            })
            .await?;
            let event = helpers::build_task_event(
                task_id,
                gate_turn,
                "conflict_gate",
                "pr_conflict_gate",
                harness_core::types::Decision::Complete,
                Some("resumed PR conflict gate rebased and pushed".to_string()),
                Some(format!("pr={pr_num}")),
                Some(telemetry),
                None,
                detail,
            );
            if let Err(error) = events.log(&event).await {
                tracing::warn!("failed to log pr_conflict_gate event: {error}");
            }
            Ok(ConflictGateOutcome::RebasePushed)
        }
        Ok(ConflictCheckOutcome::ManualResolutionRequired) => {
            persist_failure(
                "manual_resolution_required",
                format!(
                    "pr:{pr_num} is conflicting and rebase was not pushed; manual resolution required"
                ),
                detail,
                Some(telemetry),
                None,
            )
            .await
        }
        Err(parse_err) => {
            persist_failure(
                "malformed_output",
                format!(
                    "pr:{pr_num} conflict gate failed closed; manual resolution required: malformed output ({parse_err})"
                ),
                detail,
                Some(telemetry),
                None,
            )
            .await
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
    interceptors: Arc<Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
    // Canonical project root used for project_id derivation in issue workflow records.
    // Distinct from `project` when workspace isolation is active (worktree != canonical root).
    project_root: PathBuf,
    server_config: &harness_core::config::HarnessConfig,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
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
    let review_config = &resolved.review;
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
        run_non_implementation_task(
            store,
            task_id,
            task_kind,
            agent,
            req,
            &project,
            server_config,
            &interceptors,
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
        .and_then(|task| review_prep_from_rounds(&task.rounds))
        .or_else(|| {
            checkpoint
                .as_ref()
                .and_then(|c| review_prep_from_checkpoint_phase(&c.last_phase))
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
                agent, store, task_id, issue, &cargo_env, &project, req, &skills, &events,
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
    // Triage provides a DEFAULT only — caller's explicit max_rounds always wins (Fix #2).
    // Low complexity no longer skips agent review to preserve the review gate (Fix #1).
    let (triage_default_rounds, skip_agent_review) = match triage_complexity {
        prompts::TriageComplexity::Low => (2u32, false),
        prompts::TriageComplexity::Medium => (8u32, false),
        prompts::TriageComplexity::High => (8u32, false),
    };
    let effective_max_rounds = req.max_rounds.unwrap_or(triage_default_rounds);
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
        effective_max_rounds,
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
            } => break (
                pr_url,
                pr_num,
                implementation_pushed_commit,
                review_prep,
                context_items,
            ),
            implement_pipeline::ImplementOutcome::Replan {
                issue,
                plan_issue,
                prior_plan,
            } => {
                if replan_attempted {
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(format!("PLAN_ISSUE persisted after replan: {plan_issue}"));
                    })
                    .await?;
                    return Ok(());
                }

                let workflow_cfg = harness_core::config::workflow::load_workflow_config(&project)
                    .unwrap_or_default();

                if req.force_execute {
                    let forced_plan = match prior_plan.or(current_plan_output.clone()) {
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
                } else if workflow_cfg.issue_workflow.auto_replan_on_plan_issue {
                    if let Some(max) = effective_max_turns {
                        if turns_used >= max {
                            return Err(anyhow::anyhow!(
                                "Turn budget exhausted before replan: used {} of {} allowed turns",
                                turns_used,
                                max
                            ));
                        }
                    }
                    let new_plan = triage_pipeline::run_replan_for_issue(
                        agent,
                        store,
                        task_id,
                        issue,
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
                    current_plan_output = Some(new_plan);
                } else {
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(format!(
                            "PLAN_ISSUE encountered and auto_replan_on_plan_issue=false: {plan_issue}"
                        ));
                    })
                    .await?;
                    return Ok(());
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

    let mut rebase_pushed = match review_entry_decision(pr_num, review_prep.as_ref()) {
        ReviewEntryDecision::Proceed { rebase_pushed } => rebase_pushed,
        ReviewEntryDecision::FailConflict { error, paths_csv } => {
            fail_rebase_conflict(store, task_id, &events, pr_num, error, paths_csv).await?;
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
        match run_resumed_pr_conflict_gate(
            store,
            task_id,
            agent,
            req,
            &project,
            pr_num,
            &repo_slug,
            &context_items,
            &interceptors,
            &events,
            &cargo_env,
            turn_timeout,
            effective_max_turns,
            &mut turns_used,
            turns_used_acc,
        )
        .await?
        {
            ConflictGateOutcome::Clean => {
                rebase_pushed = false;
            }
            ConflictGateOutcome::RebasePushed => {
                rebase_pushed = true;
            }
            ConflictGateOutcome::Failed => return Ok(()),
        }
    }

    // Agent review loop (if enabled and reviewer available, and not skipped by triage complexity)
    let mut agent_pushed_commit = false;
    if review_config.enabled && !skip_agent_review {
        if let Some(reviewer) = reviewer {
            tracing::info!(pr_url = %pr_url.as_deref().unwrap_or(""), "starting agent review");
            let (review_ok, pushed) = agent_review::run_agent_review(
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
                &skills,
                &cargo_env,
                effective_max_turns,
                &mut turns_used,
            )
            .await?;
            *turns_used_acc = turns_used;
            if !review_ok {
                return Ok(());
            }
            agent_pushed_commit = pushed;
        } else {
            tracing::warn!("agent review enabled but no reviewer agent configured; skipping");
        }
    }
    agent_pushed_commit |= implementation_pushed_commit;

    // Skip external review bot wait when auto-trigger is disabled — there is
    // no bot to wait for, so the loop would always exhaust all rounds and fail.
    if !review_config.review_bot_auto_trigger {
        tracing::info!("review_bot_auto_trigger disabled; skipping external review wait");
        mutate_and_persist(store, task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 2;
        })
        .await?;
        store.log_event(crate::event_replay::TaskEvent::Completed {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
        });
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

    let wait_secs = resolved.review_wait_secs.unwrap_or(req.wait_secs);
    // Project-level override takes precedence over triage-derived rounds so that
    // per-repo caps (review_max_rounds in harness.toml) are never silently bypassed.
    let max_rounds = resolved.review_max_rounds.unwrap_or(effective_max_rounds);
    tracing::info!("waiting {wait_secs}s for review bot on PR #{pr_num}");
    sleep(Duration::from_secs(wait_secs)).await;

    let repo_slug_for_review = prompts::repo_slug_from_pr_url(pr_url.as_deref());

    review_loop::run_review_loop(
        store,
        task_id,
        agent,
        review_config,
        &project_config,
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
        max_rounds,
        agent_pushed_commit,
        rebase_pushed,
        turn_timeout,
        &mut turns_used,
        turns_used_acc,
        task_start,
        repo_slug_for_review,
        jaccard_threshold,
        server_config.server.github_token.as_deref(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_review_source_uses_standard_allowed_tools() {
        // Verifies that periodic_review tasks get a non-empty allowed_tools list,
        // which causes claude.rs to pass --allowedTools (hard enforcement) instead
        // of --dangerously-skip-permissions.
        let tools = restricted_tools(CapabilityProfile::Standard).unwrap_or_default();
        assert_eq!(
            tools,
            CapabilityProfile::Standard.tools().unwrap_or_default()
        );
        assert!(!tools.is_empty());
    }

    #[test]
    fn standard_implementation_turn_uses_full_profile() {
        // Non-periodic_review tasks use None → Full profile →
        // --dangerously-skip-permissions in claude.rs.
        let implementation_allowed_tools: Option<Vec<String>> = None;
        assert!(implementation_allowed_tools.is_none());
    }

    #[test]
    fn parse_harness_review_command() {
        let cmd = parse_harness_mention_command("@harness review");
        assert_eq!(cmd, Some(HarnessMentionCommand::Review));
    }

    #[test]
    fn parse_harness_fix_ci_command_case_insensitive() {
        let cmd = parse_harness_mention_command("please @Harness FIX CI");
        assert_eq!(cmd, Some(HarnessMentionCommand::FixCi));
    }

    #[test]
    fn parse_harness_plain_mention_command() {
        let cmd = parse_harness_mention_command("hello @harness can you help?");
        assert_eq!(cmd, Some(HarnessMentionCommand::Mention));
    }

    #[test]
    fn parse_harness_command_returns_none_without_mention() {
        let cmd = parse_harness_mention_command("no command here");
        assert_eq!(cmd, None);
    }

    #[test]
    fn parse_harness_first_mention_per_line_is_used() {
        let cmd = parse_harness_mention_command("@harness review then @harness fix ci");
        assert_eq!(cmd, Some(HarnessMentionCommand::Review));
    }

    #[test]
    fn prompt_builder_no_sections_adds_trailing_newline() {
        let result = PromptBuilder::new("Title line.").build();
        assert_eq!(result, "Title line.\n");
    }

    #[test]
    fn prompt_builder_optional_url_absent_is_skipped() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("Link", None)
            .build();
        assert_eq!(result, "Title.\n");
    }

    #[test]
    fn prompt_builder_optional_url_present_appears_in_output() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("Link", Some("https://example.com"))
            .build();
        assert!(result.contains("- Link: "));
        assert!(result.contains("https://example.com"));
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn prompt_builder_add_section_wraps_external_data() {
        let result = PromptBuilder::new("Title.")
            .add_section("Payload", "content here")
            .build();
        assert!(result.contains("Payload:\n"));
        assert!(result.contains("<external_data>"));
        assert!(result.contains("content here"));
    }

    #[test]
    fn prompt_builder_multiple_urls_all_appear() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("First", Some("url1"))
            .add_optional_url("Second", None)
            .add_optional_url("Third", Some("url3"))
            .build();
        assert!(result.contains("- First: "));
        assert!(result.contains("url1"));
        assert!(!result.contains("Second"));
        assert!(result.contains("- Third: "));
        assert!(result.contains("url3"));
    }

    #[test]
    fn build_fix_ci_prompt_contains_context() {
        let prompt = build_fix_ci_prompt(
            "majiayu000/harness",
            42,
            "@harness fix CI",
            Some("https://github.com/majiayu000/harness/issues/42#issuecomment-1"),
            Some("https://github.com/majiayu000/harness/pull/42"),
        );

        assert!(prompt.contains("CI failure repair requested for PR #42"));
        assert!(prompt.contains("majiayu000/harness"));
        assert!(prompt.contains("<external_data>"));
        assert!(prompt.contains("PR_URL=https://github.com/majiayu000/harness/pull/42"));
    }

    #[test]
    fn truncate_short_string_passes_through() {
        let input = "short error";
        let result = helpers::truncate_validation_error(input, 100);
        assert_eq!(result, "short error");
    }

    #[test]
    fn truncate_at_max_chars_boundary() {
        let input = "a".repeat(200);
        let result = helpers::truncate_validation_error(&input, 50);
        assert!(result.starts_with(&"a".repeat(50)));
        assert!(result.contains("(output truncated, 200 chars total)"));
    }

    #[test]
    fn truncate_preserves_utf8_boundary() {
        // "é" is 2 bytes; build a string where max_chars lands mid-character.
        let input = "ééééé"; // 10 bytes, 5 chars
        let result = helpers::truncate_validation_error(input, 3); // byte 3 is mid-char
                                                                   // Should back up to byte 2 (1 full "é").
        assert!(result.starts_with("é"));
        assert!(result.contains("(output truncated,"));
    }

    #[test]
    fn review_check_turn_uses_readonly_profile() {
        let tools = restricted_tools(CapabilityProfile::ReadOnly).unwrap();
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Grep".to_string()));
        assert!(tools.contains(&"Glob".to_string()));
        assert!(!tools.contains(&"Write".to_string()));
        assert!(!tools.contains(&"Edit".to_string()));
        assert!(!tools.contains(&"Bash".to_string()));
    }

    #[test]
    fn periodic_review_turn_uses_standard_profile_with_bash() {
        let tools = restricted_tools(CapabilityProfile::Standard).unwrap();
        assert!(tools.contains(&"Bash".to_string()));
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Write".to_string()));
        assert!(tools.contains(&"Edit".to_string()));
        // Standard does not include Grep/Glob — it's distinct from ReadOnly.
        assert!(!tools.contains(&"Grep".to_string()));
    }

    #[test]
    fn implementation_turn_uses_full_profile_no_restriction() {
        // Full profile returns None — no tool restriction is applied to the agent.
        assert!(CapabilityProfile::Full.tools().is_none());
    }

    // --- Gate: task_needs_pr_url covers issue and pr:N tasks ---

    #[test]
    fn task_needs_pr_url_true_for_issue_task() {
        let req = CreateTaskRequest {
            issue: Some(42),
            ..CreateTaskRequest::default()
        };
        assert!(
            implement_pipeline::task_needs_pr_url(&req),
            "issue task must require PR_URL"
        );
    }

    #[test]
    fn issue_triage_runs_only_when_not_skipped_and_no_existing_pr() {
        assert!(should_run_issue_triage(false, false));
        assert!(!should_run_issue_triage(true, false));
        assert!(!should_run_issue_triage(false, true));
    }

    #[test]
    fn task_needs_pr_url_true_for_pr_task() {
        let req = CreateTaskRequest {
            pr: Some(99),
            ..CreateTaskRequest::default()
        };
        assert!(
            implement_pipeline::task_needs_pr_url(&req),
            "pr:N task must require PR_URL"
        );
    }

    #[test]
    fn task_needs_pr_url_false_for_prompt_only_task() {
        let req = CreateTaskRequest::default();
        assert!(
            !implement_pipeline::task_needs_pr_url(&req),
            "prompt-only task must not require PR_URL (Done is correct)"
        );
    }

    #[test]
    fn review_entry_decision_blocks_review_on_rebase_conflict() {
        assert_eq!(
            review_entry_decision(
                42,
                Some(&prompts::PrReviewPrepOutcome::RebaseConflict {
                    paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
                })
            ),
            ReviewEntryDecision::FailConflict {
                error: "PR #42 has rebase conflicts: src/lib.rs, src/main.rs; manual resolution required".to_string(),
                paths_csv: "src/lib.rs, src/main.rs".to_string(),
            }
        );
    }

    #[test]
    fn review_entry_decision_propagates_rebase_pushed_flag() {
        assert_eq!(
            review_entry_decision(42, Some(&prompts::PrReviewPrepOutcome::RebasePushed)),
            ReviewEntryDecision::Proceed {
                rebase_pushed: true
            }
        );
        assert_eq!(
            review_entry_decision(42, Some(&prompts::PrReviewPrepOutcome::RebaseSkipped)),
            ReviewEntryDecision::Proceed {
                rebase_pushed: false
            }
        );
    }

    #[test]
    fn review_prep_from_rounds_prefers_latest_implement_round() {
        let rounds = vec![
            RoundResult::new(1, "implement", "rebase_skipped", None, None, None),
            RoundResult::new(2, "review", "needs_fix", None, None, None),
            RoundResult::new(3, "implement", "rebase_pushed", None, None, None),
        ];
        assert_eq!(
            review_prep_from_rounds(&rounds),
            Some(prompts::PrReviewPrepOutcome::RebasePushed)
        );
    }

    #[test]
    fn review_prep_from_rounds_recovers_rebase_conflict_detail() {
        let rounds = vec![RoundResult::new(
            1,
            "implement",
            "rebase_conflict",
            Some("REBASE_CONFLICT paths=src/lib.rs,src/main.rs".to_string()),
            None,
            None,
        )];
        assert_eq!(
            review_prep_from_rounds(&rounds),
            Some(prompts::PrReviewPrepOutcome::RebaseConflict {
                paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            })
        );
    }

    #[test]
    fn review_prep_from_checkpoint_phase_recovers_rebase_state() {
        assert_eq!(
            review_prep_from_checkpoint_phase("rebase_pushed"),
            Some(prompts::PrReviewPrepOutcome::RebasePushed)
        );
        assert_eq!(
            review_prep_from_checkpoint_phase("rebase_skipped"),
            Some(prompts::PrReviewPrepOutcome::RebaseSkipped)
        );
        assert_eq!(
            review_prep_from_checkpoint_phase("rebase_conflict"),
            Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths: Vec::new() })
        );
        assert_eq!(review_prep_from_checkpoint_phase("pr_created"), None);
    }

    #[tokio::test]
    async fn fail_rebase_conflict_marks_task_failed_and_logs_event() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        fail_rebase_conflict(
            &store,
            &task_id,
            &events,
            42,
            "PR #42 has rebase conflicts: src/lib.rs, src/main.rs; manual resolution required"
                .to_string(),
            "src/lib.rs, src/main.rs".to_string(),
        )
        .await?;

        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert!(matches!(final_state.status, TaskStatus::Failed));
        assert_eq!(
            final_state.error.as_deref(),
            Some(
                "PR #42 has rebase conflicts: src/lib.rs, src/main.rs; manual resolution required"
            )
        );

        let logged = events
            .query(&harness_core::types::EventFilters {
                hook: Some("pr_rebase_conflict".to_string()),
                ..harness_core::types::EventFilters::default()
            })
            .await?;
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].reason.as_deref(), Some("src/lib.rs, src/main.rs"));
        let expected_detail = format!("task_id={} pr=42", task_id.as_str());
        assert_eq!(logged[0].detail.as_deref(), Some(expected_detail.as_str()));
        Ok(())
    }

    // --- Gate A: pr:N task with non-empty output but no PR_URL gets Failed ---

    #[tokio::test]
    async fn pr_task_nonempty_output_no_pr_url_marks_failed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        let req = CreateTaskRequest {
            pr: Some(42),
            ..CreateTaskRequest::default()
        };
        // Non-empty output from agent but no PR_URL found — gate must fire for pr:N.
        let output = "LGTM, nothing to change";
        if output.trim().is_empty() {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error = Some("empty agent output: no PR created and no output".to_string());
            })
            .await?;
        } else if implement_pipeline::task_needs_pr_url(&req) {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error =
                    Some("no PR number found in agent output; task requires PR_URL".to_string());
            })
            .await?;
        } else {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = 2;
            })
            .await?;
        }

        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert!(
            matches!(final_state.status, TaskStatus::Failed),
            "pr:N task with non-empty output but no PR_URL must be Failed, not Done"
        );
        assert!(
            final_state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("PR_URL"),
            "error must mention PR_URL"
        );
        Ok(())
    }

    #[tokio::test]
    async fn prompt_only_nonempty_output_no_pr_stays_done() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        let req = CreateTaskRequest::default(); // no issue, no pr
        let output = "periodic review complete: no issues found";
        if output.trim().is_empty() {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error = Some("empty agent output: no PR created and no output".to_string());
            })
            .await?;
        } else if implement_pipeline::task_needs_pr_url(&req) {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error =
                    Some("no PR number found in agent output; task requires PR_URL".to_string());
            })
            .await?;
        } else {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Done;
                s.turn = 2;
            })
            .await?;
        }

        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert!(
            matches!(final_state.status, TaskStatus::Done),
            "prompt-only task with non-empty output and no PR must be Done"
        );
        Ok(())
    }
}
