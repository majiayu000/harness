/// Agent review phase module.
///
/// Runs the internal AI reviewer against the PR diff, cycling through rounds
/// of review + fix until the reviewer approves or the round budget is exhausted.
///
/// Part 3 adds an optional compilation gate (`enable_compilation_gate`):
/// when enabled, `cargo check --workspace` and `cargo test` are run between
/// review rounds and the output is included in the reviewer prompt context.
/// The gate is off by default to preserve existing behavior.
///
/// **Note:** `cargo check`/`cargo test` calls in this module are harness-level
/// orchestration, NOT agent-side. They are not gh/git calls and do not violate
/// the CLAUDE.md ban on `gh`/`git` inside harness crates.
use crate::task_runner::{mutate_and_persist, RoundResult, TaskStatus};
use harness_core::agent::{AgentRequest, AgentResponse};
use harness_core::error::HarnessError;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{ContextItem, Decision, Event, ExecutionPhase, SessionId};
use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command as TokioCommand;
use tokio::time::{timeout, Duration};

use super::helpers::{run_on_error, run_post_execute, run_pre_execute, update_status};
use super::TaskContext;

/// Outcome returned by the agent review phase to the orchestrator.
pub(crate) enum AgentReviewOutcome {
    /// Review determined the task is done or failed (quota exhausted, fatal
    /// impasse). The orchestrator should return `Ok(())` immediately.
    Handled,
    /// Agent review completed (approved or exhausted rounds). The orchestrator
    /// proceeds to the external review bot loop.
    ReviewComplete {
        /// Whether the agent pushed a new commit during any review round.
        pushed_commit: bool,
    },
}

/// Output from a `cargo check` run.
pub(crate) struct CompileOutput {
    pub success: bool,
    pub stderr: String,
}

/// Output from a `cargo test` run.
pub(crate) struct TestOutput {
    pub success: bool,
    pub output: String,
}

/// Run `cargo check --workspace` and capture the result.
///
/// Uses the per-task `CARGO_TARGET_DIR` from `cargo_env` so parallel agents
/// don't contend on the same build directory (issue #488).
pub(crate) async fn run_compile_check(
    workspace: &Path,
    cargo_env: &HashMap<String, String>,
) -> CompileOutput {
    let result = TokioCommand::new("cargo")
        .args(["check", "--workspace"])
        .current_dir(workspace)
        .envs(cargo_env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let child = match result {
        Ok(c) => c,
        Err(e) => {
            return CompileOutput {
                success: false,
                stderr: format!("failed to spawn cargo check: {e}"),
            }
        }
    };

    match timeout(Duration::from_secs(300), child.wait_with_output()).await {
        Ok(Ok(out)) => CompileOutput {
            success: out.status.success(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        },
        Ok(Err(e)) => CompileOutput {
            success: false,
            stderr: format!("cargo check wait error: {e}"),
        },
        Err(_) => CompileOutput {
            success: false,
            stderr: "cargo check timed out after 300s".to_string(),
        },
    }
}

/// Run `cargo test --package <package>` and capture the result.
///
/// If `package` is empty, runs `cargo test --workspace`.
pub(crate) async fn run_test_suite(
    package: &str,
    workspace: &Path,
    cargo_env: &HashMap<String, String>,
) -> TestOutput {
    let args: Vec<&str> = if package.is_empty() {
        vec!["test", "--workspace"]
    } else {
        vec!["test", "--package", package]
    };

    let result = TokioCommand::new("cargo")
        .args(&args)
        .current_dir(workspace)
        .envs(cargo_env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let child = match result {
        Ok(c) => c,
        Err(e) => {
            return TestOutput {
                success: false,
                output: format!("failed to spawn cargo test: {e}"),
            }
        }
    };

    match timeout(Duration::from_secs(600), child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            TestOutput {
                success: out.status.success(),
                output: format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
            }
        }
        Ok(Err(e)) => TestOutput {
            success: false,
            output: format!("cargo test wait error: {e}"),
        },
        Err(_) => TestOutput {
            success: false,
            output: "cargo test timed out after 600s".to_string(),
        },
    }
}

/// Run the agent review loop.
///
/// If `ctx.review_config.enable_compilation_gate` is true, runs
/// `cargo check --workspace` and `cargo test` between rounds and includes the
/// output in the reviewer prompt.
pub(crate) async fn run(
    ctx: &TaskContext<'_>,
    pr_url: &str,
    context_items: &[ContextItem],
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
) -> anyhow::Result<AgentReviewOutcome> {
    let reviewer = match ctx.reviewer {
        Some(r) => r,
        None => {
            tracing::warn!("agent review enabled but no reviewer agent configured; skipping");
            return Ok(AgentReviewOutcome::ReviewComplete {
                pushed_commit: false,
            });
        }
    };

    let max_rounds = ctx.review_config.max_rounds;
    let project_type = ctx.project_config.review_type.as_str();
    let mut impasse_tracker: Option<(Vec<String>, u32)> = None;
    let mut pushed_commit = false;
    // Carry compile/test failure output into the next review round when the
    // compilation gate is enabled and a check failed.
    let mut pending_gate_failure: Option<String> = None;

    for agent_round in 1..=max_rounds {
        update_status(ctx.store, ctx.task_id, TaskStatus::AgentReview, agent_round).await?;

        // Reviewer evaluates the PR diff — read-only except Bash for `gh pr diff`.
        let base_review_prompt = prompts::agent_review_prompt(pr_url, agent_round, project_type);
        let note = prompts::reviewer_capability_note();
        let review_prompt = if let Some(gate_fail) = pending_gate_failure.take() {
            format!(
                "{note}\n\n\
                 IMPORTANT: The previous round's compilation/test gate failed. \
                 Fix the issues before re-reviewing.\n\n\
                 Gate output:\n```\n{gate_fail}\n```\n\n{base_review_prompt}"
            )
        } else {
            format!("{note}\n\n{base_review_prompt}")
        };

        let review_req = AgentRequest {
            prompt: review_prompt,
            project_root: ctx.project.clone(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Validation),
            allowed_tools: Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
                "Bash".to_string(),
            ]),
            env_vars: ctx.cargo_env.clone(),
            ..Default::default()
        };
        let review_req = run_pre_execute(&ctx.interceptors, review_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                return Err(anyhow::anyhow!(
                    "Turn budget exhausted before agent review round {agent_round}: \
                     used {} of {} allowed turns",
                    turns_used,
                    max
                ));
            }
        }
        let resp = timeout(ctx.turn_timeout, reviewer.execute(review_req.clone())).await;
        *turns_used += 1;
        let resp = match resp {
            Ok(Ok(r)) => {
                let tool_violations = validate_tool_usage(
                    &r.output,
                    review_req.allowed_tools.as_deref().unwrap_or(&[]),
                );
                if !tool_violations.is_empty() {
                    let msg = format!(
                        "Tool isolation violation in agent review round {agent_round}: \
                         agent used disallowed tools: [{}]",
                        tool_violations.join(", ")
                    );
                    tracing::warn!(
                        agent_round,
                        ?tool_violations,
                        "agent review: agent used tools outside allowed list"
                    );
                    run_on_error(&ctx.interceptors, &review_req, &msg).await;
                    return Err(anyhow::anyhow!("{msg}"));
                }
                if let Some(val_err) = run_post_execute(&ctx.interceptors, &review_req, &r).await {
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
                    tracing::error!(
                        agent_round,
                        error = %e,
                        "quota exhausted during agent review — aborting"
                    );
                    run_on_error(&ctx.interceptors, &review_req, &e.to_string()).await;
                    mutate_and_persist(ctx.store, ctx.task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(e.to_string());
                    })
                    .await?;
                    return Ok(AgentReviewOutcome::Handled);
                }
                run_on_error(&ctx.interceptors, &review_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review round {agent_round} timed out after {}s",
                    ctx.turn_timeout.as_secs()
                );
                run_on_error(&ctx.interceptors, &review_req, &msg).await;
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

        mutate_and_persist(ctx.store, ctx.task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0,
                action: "agent_review".into(),
                result: if approved {
                    "approved".into()
                } else {
                    format!("{} issues", issues.len())
                },
                detail: Some(review_detail.clone()),
                first_token_latency_ms: None,
            });
        })
        .await?;

        let mut ev = Event::new(
            SessionId::new(),
            "agent_review",
            "task_runner",
            if approved {
                Decision::Complete
            } else {
                Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_url}"));
        ev.reason = Some(if approved {
            format!("round {agent_round}: approved")
        } else {
            format!("round {agent_round}: {} issues", issues.len())
        });
        if let Err(e) = ctx.events.log(&ev).await {
            tracing::warn!("failed to log agent_review event: {e}");
        }

        if approved {
            // Compilation gate (Part 3): verify the code actually compiles and
            // tests pass before accepting the agent's approval.
            if ctx.review_config.enable_compilation_gate {
                let compile = run_compile_check(&ctx.project, &ctx.cargo_env).await;
                if !compile.success {
                    tracing::warn!(
                        agent_round,
                        "compilation gate failed; re-entering review loop"
                    );
                    pending_gate_failure = Some(format!("Compilation failed:\n{}", compile.stderr));
                    continue;
                }
                let pkg = ctx.project_config.review_type.as_str();
                let tests = run_test_suite(pkg, &ctx.project, &ctx.cargo_env).await;
                if !tests.success {
                    tracing::warn!(
                        agent_round,
                        "test gate failed in agent review; re-entering review loop"
                    );
                    pending_gate_failure = Some(format!("Tests failed:\n{}", tests.output));
                    continue;
                }
            }
            tracing::info!("agent review approved at round {agent_round}");
            break;
        }

        // Malformed reviewer output: neither APPROVED nor any ISSUE: lines.
        if issues.is_empty() {
            tracing::warn!(
                agent_round,
                "agent reviewer output contained neither APPROVED nor ISSUE: lines; \
                 treating as protocol failure and skipping fix round"
            );
            break;
        }

        // Detect impasse: track how many consecutive rounds produced identical issues.
        let normalized = super::normalize_issues(&issues);
        let consecutive_count = match &impasse_tracker {
            Some((prev, c)) if *prev == normalized => c + 1,
            _ => 1,
        };
        impasse_tracker = Some((normalized, consecutive_count));

        if consecutive_count >= 5 {
            tracing::warn!(
                agent_round,
                consecutive_count,
                "agent review impasse: same issues repeated 5 times — marking task as failed"
            );
            mutate_and_persist(ctx.store, ctx.task_id, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(format!(
                    "Impasse detected: identical issues repeated {consecutive_count} \
                     consecutive rounds."
                ));
            })
            .await?;
            return Ok(AgentReviewOutcome::Handled);
        }

        let is_impasse = consecutive_count >= 3;
        if is_impasse {
            tracing::warn!(
                agent_round,
                consecutive_count,
                "agent review impasse detected — same issues repeated, using intervention prompt"
            );
        }

        if agent_round == max_rounds && !is_impasse {
            tracing::info!(
                "agent review exhausted {max_rounds} rounds, proceeding to GitHub review"
            );
            break;
        }

        // Implementor fixes the issues.
        let fix_req = AgentRequest {
            prompt: {
                let prompt_fn = if is_impasse {
                    prompts::agent_review_intervention_prompt
                } else {
                    prompts::agent_review_fix_prompt
                };
                prompt_fn(pr_url, &issues, agent_round, project_type)
            },
            project_root: ctx.project.clone(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: ctx.cargo_env.clone(),
            ..Default::default()
        };
        let fix_req = run_pre_execute(&ctx.interceptors, fix_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                return Err(anyhow::anyhow!(
                    "Turn budget exhausted before agent review fix round {agent_round}: \
                     used {} of {} allowed turns",
                    turns_used,
                    max
                ));
            }
        }
        let fix_resp = timeout(ctx.turn_timeout, ctx.agent.execute(fix_req.clone())).await;
        *turns_used += 1;
        match fix_resp {
            Ok(Ok(r)) => {
                if let Some(val_err) = run_post_execute(&ctx.interceptors, &fix_req, &r).await {
                    tracing::warn!(
                        agent_round,
                        error = %val_err,
                        "post-execute validation failed in agent review fix; continuing"
                    );
                }
            }
            Ok(Err(e)) => {
                run_on_error(&ctx.interceptors, &fix_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review fix round {agent_round} timed out after {}s",
                    ctx.turn_timeout.as_secs()
                );
                run_on_error(&ctx.interceptors, &fix_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        }

        mutate_and_persist(ctx.store, ctx.task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0,
                action: "agent_review_fix".into(),
                result: "fixed".into(),
                detail: None,
                first_token_latency_ms: None,
            });
        })
        .await?;
        pushed_commit = true;
    }

    Ok(AgentReviewOutcome::ReviewComplete { pushed_commit })
}
