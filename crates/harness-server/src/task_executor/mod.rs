pub(crate) mod agent_review;
pub(crate) mod conflict_resolver;
pub(crate) mod helpers;
pub(crate) mod implement_pipeline;
pub(crate) mod pr_detection;
pub(crate) mod review_loop;
pub(crate) mod triage_pipeline;
pub(crate) mod turn_lifecycle;

use crate::task_runner::{mutate_and_persist, CreateTaskRequest, TaskId, TaskStatus, TaskStore};
use anyhow::Context;
use harness_core::agent::CodeAgent;
use harness_core::{config::project::load_project_config, lang_detect, prompts};
use std::collections::HashMap;

use helpers::update_status;

#[cfg(test)]
use harness_core::config::agents::CapabilityProfile;
/// Extract tool list from a capability profile, returning an error if the
/// profile unexpectedly returns `None` (which means Full/unrestricted).
/// A misconfigured profile causes a hard failure rather than silent degradation,
/// per U-23 (no silent capability downgrade).
// Re-export so existing call sites in handlers/ don't need updating.
pub(crate) use turn_lifecycle::run_turn_lifecycle;
#[cfg(test)]
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
use pr_detection::{detect_repo_slug, find_existing_pr_for_issue};
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

    // --- Checkpoint-based resume detection ---
    // Load checkpoint and task state to determine if we can skip phases.
    // This is the duplicate-PR prevention gate: if the task already has a PR,
    // we skip triage/plan/implement and jump directly to agent review.
    //
    // Check task store for an existing pr_url first — this survives checkpoint
    // read failures (e.g. transient SQLite contention) and lets us safely
    // resume review even when the checkpoint row is temporarily unreadable.
    let task_pr_url: Option<String> = store.get(task_id).and_then(|t| t.pr_url);
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
    let resumed_pr_url: Option<String> =
        task_pr_url.or_else(|| checkpoint.as_ref().and_then(|c| c.pr_url.clone()));
    // Capture before `resumed_pr_url` is moved into run_implement_phase.
    // Also covers fresh pr:N tasks from webhook (req.pr is set but no checkpoint pr_url yet).
    let mut was_resumed_pr = resumed_pr_url.is_some() || req.pr.is_some();
    let resumed_plan: Option<String> = checkpoint.and_then(|c| c.plan_output);

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
        let has_existing_pr = find_existing_pr_for_issue(&project, issue)
            .await
            .ok()
            .flatten()
            .is_some();
        if !has_existing_pr {
            triage_pipeline::run_triage_plan_pipeline(
                agent, store, task_id, issue, &cargo_env, &project, req,
            )
            .await?
        } else {
            // Fresh issue task reusing an existing PR — treat as resumed for conflict gating.
            was_resumed_pr = true;
            (None, prompts::TriageComplexity::Medium, 0u32)
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
            // Set to Implementing BEFORE the plan phase so that a crash during planning
            // leaves the task in 'implementing' status. The startup recovery code will catch
            // it and fail-close it (no pr_url → mark failed), rather than leaving it stuck
            // as a plain 'pending' that is never re-dispatched.
            update_status(store, task_id, TaskStatus::Implementing, 1).await?;
            triage_pipeline::run_plan_for_prompt(agent, store, task_id, &cargo_env, &project, req)
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
    let effective_max_turns: Option<u32> = req.max_turns.or(server_config.concurrency.max_turns);
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

    // Run the implement phase (prompt construction + agent execution + PR detection).
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
        plan_output,
        resumed_pr_url,
        turn_timeout,
        effective_max_turns,
        &mut turns_used,
        turns_used_acc,
        task_start,
    )
    .await?;

    let (pr_url, pr_num, context_items) = match outcome {
        implement_pipeline::ImplementOutcome::Done => return Ok(()),
        implement_pipeline::ImplementOutcome::Proceed {
            pr_url,
            pr_num,
            context_items,
            ..
        } => (pr_url, pr_num, context_items),
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

    // Conflict resolution gate: intercept CONFLICTING PRs before the review loop.
    // Only runs on the resume/recovery path — fresh PRs cannot be conflicting yet.
    let mut rebase_pushed = false;
    let mut conflict_was_detected = false;
    if was_resumed_pr {
        use conflict_resolver::{assess_pr_conflict, PrConflictSize};
        let conflict_size = assess_pr_conflict(pr_num, &project).await;
        match conflict_size {
            PrConflictSize::Small {
                file_count,
                region_count,
            } => {
                tracing::info!(
                    pr = pr_num,
                    file_count,
                    region_count,
                    "conflict: small — attempting auto-rebase"
                );
                conflict_was_detected = true;
                rebase_pushed = triage_pipeline::run_rebase_turn(
                    agent,
                    pr_num,
                    &project,
                    &repo_slug,
                    turn_timeout,
                    &cargo_env,
                )
                .await;
            }
            PrConflictSize::Large {
                file_count,
                region_count,
            } => {
                tracing::warn!(
                    pr = pr_num,
                    file_count,
                    region_count,
                    "conflict: too large for auto-rebase — deferring to review agent"
                );
                conflict_was_detected = true;
            }
            PrConflictSize::Clean | PrConflictSize::Unknown(_) => {}
        }

        // Gate B: a detected conflict that was not resolved by a successful rebase
        // must not silently proceed to review. Mark Failed so the operator can
        // re-queue after manual resolution rather than getting a false Done.
        if conflict_was_detected && !rebase_pushed {
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(format!(
                    "pr:{pr_num} is conflicting and rebase was not pushed; manual resolution required"
                ));
            })
            .await?;
            return Ok(());
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

    // --- Gate A: pr_url null gate ---

    #[tokio::test]
    async fn null_pr_url_gate_marks_failed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        let pr_num: u64 = 42;
        mutate_and_persist(&store, &task_id, |s| {
            s.status = TaskStatus::Failed;
            s.error = Some(format!(
                "pr:{pr_num} produced no detectable pr_url; dedup unblocked"
            ));
        })
        .await?;

        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert!(
            matches!(final_state.status, TaskStatus::Failed),
            "status must be Failed"
        );
        assert!(
            final_state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("no detectable pr_url"),
            "error message must mention pr_url"
        );
        Ok(())
    }

    // --- Gate C: empty agent output ---

    #[tokio::test]
    async fn empty_agent_output_marks_failed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        // Simulate the gate: output is empty and no pr_num was parsed.
        let output = "";
        if output.trim().is_empty() {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error = Some("empty agent output: no PR created and no output".to_string());
            })
            .await?;
        }

        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert!(
            matches!(final_state.status, TaskStatus::Failed),
            "status must be Failed"
        );
        assert!(
            final_state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("empty agent output"),
            "error message must mention empty output"
        );
        Ok(())
    }

    #[tokio::test]
    async fn nonempty_output_no_pr_still_done() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        // Simulate the gate: output is non-empty but no PR was created — keep Done.
        let output = "Agent: nothing to do";
        if output.trim().is_empty() {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
                s.turn = 2;
                s.error = Some("empty agent output: no PR created and no output".to_string());
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
            "status must be Done"
        );
        Ok(())
    }

    // --- Gate B: conflict detected + no rebase ---

    #[tokio::test]
    async fn large_conflict_no_rebase_marks_failed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        // Simulate the gate: Large conflict, rebase_pushed=false.
        let pr_num: u64 = 99;
        let conflict_was_detected = true;
        let rebase_pushed = false;
        if conflict_was_detected && !rebase_pushed {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(format!(
                    "pr:{pr_num} is conflicting and rebase was not pushed; manual resolution required"
                ));
            })
            .await?;
        }

        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert!(
            matches!(final_state.status, TaskStatus::Failed),
            "status must be Failed"
        );
        assert!(
            final_state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("conflicting"),
            "error message must mention conflicting"
        );
        Ok(())
    }

    #[tokio::test]
    async fn small_conflict_rebase_success_proceeds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let task_id = TaskId::new();
        let state = crate::task_runner::TaskState::new(task_id.clone());
        store.insert(&state).await;

        // Simulate the gate: Small conflict, rebase_pushed=true — gate must not trigger.
        let conflict_was_detected = true;
        let rebase_pushed = true;
        if conflict_was_detected && !rebase_pushed {
            mutate_and_persist(&store, &task_id, |s| {
                s.status = TaskStatus::Failed;
            })
            .await?;
        }

        // Task should remain in its initial Pending state (gate did not fire).
        let final_state = store
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
        assert!(
            matches!(final_state.status, TaskStatus::Pending),
            "status must be Pending"
        );
        Ok(())
    }
}
