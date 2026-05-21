use super::helpers::{
    augment_prompt_with_skills, build_task_event, run_agent_streaming, run_on_error,
    run_post_execute, run_pre_execute, telemetry_for_timeout, update_status,
};
use super::review_loop;
use crate::task_runner::{mutate_and_persist, RoundResult, TaskId, TaskStatus, TaskStore};
use chrono::Utc;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::config::agents::SandboxMode;
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{ContextItem, Decision, ExecutionPhase, TurnFailure, TurnFailureKind};
use harness_observe::event_store::EventStore;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Duration;

/// Compute Jaccard word-similarity between two strings.
///
/// Tokenizes each string into a set of non-empty words (split on non-alphanumeric chars),
/// then returns |intersection| / |union|.
/// Both empty → 1.0; one empty → 0.0.
pub(crate) fn jaccard_word_similarity(a: &str, b: &str) -> f64 {
    let tokens_a: HashSet<&str> = a
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let tokens_b: HashSet<&str> = b
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();

    if tokens_a.is_empty() && tokens_b.is_empty() {
        return 1.0;
    }
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }

    let intersection = tokens_a.intersection(&tokens_b).count();
    let union = tokens_a.union(&tokens_b).count();
    intersection as f64 / union as f64
}

/// Normalize a set of review issues into a canonical ordered form.
/// Issues are sorted by reference before collecting so that insertion order does not affect
/// equality comparisons, and strings are not unnecessarily cloned during the sort.
pub(crate) fn normalize_issues(issues: &[String]) -> Vec<String> {
    let mut sorted: Vec<_> = issues.iter().collect();
    sorted.sort();
    sorted.into_iter().cloned().collect()
}

fn reviewer_sandbox_override(reviewer: &dyn CodeAgent) -> Option<SandboxMode> {
    if reviewer.name().eq_ignore_ascii_case("claude") {
        return None;
    }
    Some(SandboxMode::ReadOnlyWithNetwork)
}

pub(crate) enum ReviewHeadProbe<'a> {
    GitHub {
        repo_slug: &'a str,
        pr_num: u64,
        github_token: Option<&'a str>,
    },
    #[cfg(test)]
    Static(Result<&'a str, &'a str>),
}

impl ReviewHeadProbe<'_> {
    async fn capture(&self) -> Result<String, String> {
        match self {
            ReviewHeadProbe::GitHub {
                repo_slug,
                pr_num,
                github_token,
            } => review_loop::fetch_pr_head_sha_for_gate(repo_slug, *pr_num, *github_token).await,
            #[cfg(test)]
            ReviewHeadProbe::Static(result) => result
                .as_ref()
                .map(|value| (*value).to_string())
                .map_err(|error| (*error).to_string()),
        }
    }
}

async fn fail_agent_review(
    store: &TaskStore,
    events: &EventStore,
    task_id: &TaskId,
    turn: u32,
    pr_url: &str,
    error: String,
    detail: Option<String>,
) -> anyhow::Result<()> {
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.error = Some(error.clone());
    })
    .await?;
    let event = build_task_event(
        task_id,
        turn,
        "agent_review",
        "agent_review",
        Decision::Block,
        Some(error),
        Some(format!("pr={pr_url}")),
        None,
        None,
        detail,
    );
    if let Err(error) = events.log(&event).await {
        tracing::warn!("failed to log agent_review failure event: {error}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_review(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: &dyn CodeAgent,
    review_config: &harness_core::config::agents::AgentReviewConfig,
    context_items: &[ContextItem],
    project: &Path,
    interceptors: &[Arc<dyn harness_core::interceptor::TurnInterceptor>],
    turn_timeout: Duration,
    pr_url: &str,
    project_type: &str,
    events: &EventStore,
    skills: &RwLock<harness_skills::store::SkillStore>,
    cargo_env: &HashMap<String, String>,
    effective_max_turns: Option<u32>,
    review_head_probe: Option<ReviewHeadProbe<'_>>,
    turns_used: &mut u32,
) -> anyhow::Result<(bool, bool, Option<Result<String, String>>)> {
    let max_rounds = review_config.max_rounds;
    // (normalized_issues, consecutive_count): tracks how many consecutive rounds produced identical issues.
    let mut impasse_tracker: Option<(Vec<String>, u32)> = None;
    // Whether the last action in this loop pushed a new commit (fix or intervention).
    let mut pushed_commit = false;
    let mut approved_review = false;
    let mut approved_review_head: Option<Result<String, String>> = None;
    let mut last_issues: Vec<String> = Vec::new();
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await?;

        // Reviewer evaluates the PR diff — read-only except Bash for `gh pr diff`.
        let review_prompt_built_at = Utc::now();
        let base = prompts::agent_review_prompt(pr_url, agent_round, project_type);
        let note = prompts::agent_review_capability_note();
        let augmented_base = augment_prompt_with_skills(skills, events, task_id, base).await;
        let review_req = AgentRequest {
            prompt: format!("{note}\n\n{augmented_base}"),
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::SimpleReview),
            sandbox_mode: reviewer_sandbox_override(reviewer),
            allowed_tools: Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
                "Bash".to_string(),
            ]),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let review_req = run_pre_execute(interceptors, review_req).await?;
        let reviewed_head = match review_head_probe.as_ref() {
            Some(probe) => Some(probe.capture().await),
            None => None,
        };

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                return Err(anyhow::anyhow!(
                    "Turn budget exhausted before agent review round {agent_round}: used {} of {} allowed turns",
                    turns_used, max
                ));
            }
        }
        let review_started_at = Utc::now();
        let resp = tokio::time::timeout(
            turn_timeout,
            run_agent_streaming(
                reviewer,
                review_req.clone(),
                task_id,
                store,
                agent_round as u32,
                review_prompt_built_at,
                review_started_at,
            ),
        )
        .await;
        *turns_used += 1;
        let resp = match resp {
            Ok(Ok(success)) => {
                let r = success.response;
                let tool_violations = validate_tool_usage(
                    &r.output,
                    review_req.allowed_tools.as_deref().unwrap_or(&[]),
                );
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
                (r, success.telemetry)
            }
            Ok(Err(failure)) => {
                // Quota/billing failures are not retryable — break immediately.
                // Do NOT activate the global rate-limit circuit breaker: the reviewer
                // agent is configured independently from the implementation agent and a
                // depleted reviewer account must not stall unrelated implementation tasks.
                if matches!(
                    failure.failure.kind,
                    TurnFailureKind::Quota | TurnFailureKind::Billing
                ) {
                    tracing::error!(agent_round, error = %failure.error, "quota/billing failure during agent review — aborting");
                    run_on_error(interceptors, &review_req, &failure.error.to_string()).await;
                    mutate_and_persist(store, task_id, |s| {
                        s.status = TaskStatus::Failed;
                        s.error = Some(failure.error.to_string());
                        s.rounds.push(RoundResult::new(
                            0,
                            "agent_review",
                            match failure.failure.kind {
                                TurnFailureKind::Quota => "quota_exhausted",
                                TurnFailureKind::Billing => "billing_failed",
                                TurnFailureKind::Upstream => "upstream_failure",
                                _ => "failed",
                            },
                            None,
                            Some(failure.telemetry.clone()),
                            Some(failure.failure.clone()),
                        ));
                    })
                    .await?;
                    let event = build_task_event(
                        task_id,
                        0,
                        "agent_review",
                        "agent_review",
                        Decision::Block,
                        Some("agent reviewer failed".to_string()),
                        Some(format!("pr={pr_url}")),
                        Some(failure.telemetry),
                        Some(failure.failure),
                        None,
                    );
                    if let Err(error) = events.log(&event).await {
                        tracing::warn!("failed to log agent_review event: {error}");
                    }
                    return Ok((false, false, approved_review_head));
                }
                run_on_error(interceptors, &review_req, &failure.error.to_string()).await;
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        0,
                        "agent_review",
                        "failed",
                        None,
                        Some(failure.telemetry.clone()),
                        Some(failure.failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    0,
                    "agent_review",
                    "agent_review",
                    Decision::Block,
                    Some("agent reviewer failed".to_string()),
                    Some(format!("pr={pr_url}")),
                    Some(failure.telemetry),
                    Some(failure.failure),
                    None,
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log agent_review event: {error}");
                }
                return Err(failure.error.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review round {agent_round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &review_req, &msg).await;
                let telemetry = telemetry_for_timeout(
                    review_prompt_built_at,
                    review_started_at,
                    Utc::now(),
                    None,
                );
                let failure = TurnFailure {
                    kind: TurnFailureKind::Timeout,
                    provider: Some(reviewer.name().to_string()),
                    upstream_status: None,
                    message: Some(msg.clone()),
                    body_excerpt: None,
                };
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        0,
                        "agent_review",
                        "timeout",
                        None,
                        Some(telemetry.clone()),
                        Some(failure.clone()),
                    ));
                })
                .await?;
                let event = build_task_event(
                    task_id,
                    0,
                    "agent_review",
                    "agent_review",
                    Decision::Block,
                    Some("agent reviewer timed out".to_string()),
                    Some(format!("pr={pr_url}")),
                    Some(telemetry),
                    Some(failure),
                    None,
                );
                if let Err(error) = events.log(&event).await {
                    tracing::warn!("failed to log agent_review event: {error}");
                }
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        let (AgentResponse { output, stderr, .. }, review_telemetry) = resp;

        if !stderr.is_empty() {
            tracing::warn!(agent_round, stderr = %stderr, "agent reviewer stderr");
        }

        let approved = prompts::is_approved(&output);
        let issues = prompts::extract_review_issues(&output);
        let review_detail = output;
        last_issues = issues.clone();

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult::new(
                0,
                "agent_review",
                if approved {
                    "approved".to_string()
                } else {
                    format!("{} issues", issues.len())
                },
                Some(review_detail.clone()),
                Some(review_telemetry.clone()),
                None,
            ));
        })
        .await?;

        let event = build_task_event(
            task_id,
            0,
            "agent_review",
            "agent_review",
            if approved {
                Decision::Complete
            } else {
                Decision::Warn
            },
            Some(if approved {
                format!("round {agent_round}: approved")
            } else {
                format!("round {agent_round}: {} issues", issues.len())
            }),
            Some(format!("pr={pr_url}")),
            Some(review_telemetry.clone()),
            None,
            Some(review_detail.clone()),
        );
        if let Err(e) = events.log(&event).await {
            tracing::warn!("failed to log agent_review event: {e}");
        }

        if approved {
            tracing::info!("agent review approved at round {agent_round}");
            approved_review = true;
            approved_review_head = reviewed_head;
            break;
        }

        // Malformed reviewer output: neither APPROVED nor any ISSUE: lines.
        // Sending an empty fix prompt would produce arbitrary or no-op commits,
        // so treat this as a reviewer protocol failure and abort the review loop.
        if issues.is_empty() {
            let error = format!(
                "Agent review round {agent_round} returned neither APPROVED nor ISSUE lines."
            );
            tracing::warn!(agent_round, error, "agent reviewer protocol failure");
            fail_agent_review(
                store,
                events,
                task_id,
                agent_round,
                pr_url,
                error,
                Some(review_detail),
            )
            .await?;
            return Ok((false, pushed_commit, approved_review_head));
        }

        // Detect impasse: track how many consecutive rounds produced identical issues.
        // Must happen before the max_rounds break so thresholds are reachable at the last round.
        // Compare normalized issue lists directly to avoid false positives from hash collisions.
        let normalized = normalize_issues(&issues);
        let consecutive_count = match &impasse_tracker {
            Some((prev, c)) if *prev == normalized => c + 1,
            _ => 1,
        };
        impasse_tracker = Some((normalized, consecutive_count));

        // Hard-fail after this many consecutive identical-issue rounds.
        const IMPASSE_HARD_FAIL_ROUNDS: u32 = 5;
        // Switch to intervention prompt after this many consecutive identical-issue rounds.
        const IMPASSE_INTERVENTION_ROUNDS: u32 = 3;

        if consecutive_count >= IMPASSE_HARD_FAIL_ROUNDS {
            tracing::warn!(
                agent_round,
                consecutive_count,
                    "agent review impasse: same issues repeated {IMPASSE_HARD_FAIL_ROUNDS} times — marking task as failed"
            );
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(format!(
                    "Impasse detected: identical issues repeated {consecutive_count} consecutive rounds."
                ));
            })
            .await?;
            return Ok((false, false, approved_review_head));
        }

        // 3+ consecutive rounds with identical issues → use the intervention prompt.
        let is_impasse = consecutive_count >= IMPASSE_INTERVENTION_ROUNDS;
        if is_impasse {
            tracing::warn!(
                agent_round,
                consecutive_count,
                "agent review impasse detected — same issues repeated, using intervention prompt"
            );
        }

        // Do not push a new fix after the final allowed review pass. Every local
        // fix must be followed by another reviewer pass before the task can pass.
        if agent_round == max_rounds {
            tracing::info!(
                "agent review exhausted {max_rounds} rounds with unresolved local issues"
            );
            break;
        }

        // Implementor fixes the issues
        let fix_prompt_built_at = Utc::now();
        let fix_req = AgentRequest {
            prompt: {
                let prompt_fn = if is_impasse {
                    prompts::agent_review_intervention_prompt
                } else {
                    prompts::agent_review_fix_prompt
                };
                prompt_fn(pr_url, &issues, agent_round, project_type)
            },
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::Execution),
            env_vars: cargo_env.clone(),
            ..Default::default()
        };
        let fix_req = run_pre_execute(interceptors, fix_req).await?;

        if let Some(max) = effective_max_turns {
            if *turns_used >= max {
                return Err(anyhow::anyhow!(
                    "Turn budget exhausted before agent review fix round {agent_round}: used {} of {} allowed turns",
                    turns_used, max
                ));
            }
        }
        let fix_started_at = Utc::now();
        let fix_resp = tokio::time::timeout(
            turn_timeout,
            run_agent_streaming(
                agent,
                fix_req.clone(),
                task_id,
                store,
                agent_round as u32,
                fix_prompt_built_at,
                fix_started_at,
            ),
        )
        .await;
        *turns_used += 1;
        match fix_resp {
            Ok(Ok(success)) => {
                let r = success.response;
                if let Some(val_err) = run_post_execute(interceptors, &fix_req, &r).await {
                    tracing::warn!(
                        agent_round,
                        error = %val_err,
                        "post-execute validation failed in agent review fix; continuing"
                    );
                }
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        0,
                        "agent_review_fix",
                        "fixed",
                        None,
                        Some(success.telemetry.clone()),
                        None,
                    ));
                })
                .await?;
            }
            Ok(Err(failure)) => {
                run_on_error(interceptors, &fix_req, &failure.error.to_string()).await;
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        0,
                        "agent_review_fix",
                        "failed",
                        None,
                        Some(failure.telemetry.clone()),
                        Some(failure.failure.clone()),
                    ));
                })
                .await?;
                return Err(failure.error.into());
            }
            Err(_) => {
                let msg = format!(
                    "Agent review fix round {agent_round} timed out after {}s",
                    turn_timeout.as_secs()
                );
                run_on_error(interceptors, &fix_req, &msg).await;
                let telemetry =
                    telemetry_for_timeout(fix_prompt_built_at, fix_started_at, Utc::now(), None);
                let failure = TurnFailure {
                    kind: TurnFailureKind::Timeout,
                    provider: Some(agent.name().to_string()),
                    upstream_status: None,
                    message: Some(msg.clone()),
                    body_excerpt: None,
                };
                mutate_and_persist(store, task_id, |s| {
                    s.rounds.push(RoundResult::new(
                        0,
                        "agent_review_fix",
                        "timeout",
                        None,
                        Some(telemetry.clone()),
                        Some(failure.clone()),
                    ));
                })
                .await?;
                return Err(anyhow::anyhow!("{msg}"));
            }
        }
        pushed_commit = true;
    }

    if approved_review {
        return Ok((true, pushed_commit, approved_review_head));
    }

    let detail = if last_issues.is_empty() {
        None
    } else {
        Some(format!(
            "Unresolved local review issues:\n{}",
            last_issues.join("\n")
        ))
    };
    let error = if max_rounds == 0 {
        "Agent review is enabled but max_rounds is 0.".to_string()
    } else {
        format!("Agent review exhausted {max_rounds} rounds without approval.")
    };
    fail_agent_review(store, events, task_id, max_rounds, pr_url, error, detail).await?;
    Ok((false, pushed_commit, approved_review_head))
}
