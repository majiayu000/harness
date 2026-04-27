use super::helpers::{
    build_task_event, run_agent_streaming, run_on_error, run_post_execute, run_pre_execute,
    telemetry_for_timeout, update_status,
};
use crate::task_runner::{mutate_and_persist, RoundResult, TaskId, TaskStatus, TaskStore};
use chrono::Utc;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent};
use harness_core::prompts;
use harness_core::tool_isolation::validate_tool_usage;
use harness_core::types::{ContextItem, Decision, ExecutionPhase, TurnFailure, TurnFailureKind};
use harness_observe::event_store::EventStore;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
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
    cargo_env: &HashMap<String, String>,
    effective_max_turns: Option<u32>,
    turns_used: &mut u32,
) -> anyhow::Result<(bool, bool)> {
    let max_rounds = review_config.max_rounds;
    // (normalized_issues, consecutive_count): tracks how many consecutive rounds produced identical issues.
    let mut impasse_tracker: Option<(Vec<String>, u32)> = None;
    // Whether the last action in this loop pushed a new commit (fix or intervention).
    let mut pushed_commit = false;
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await?;

        // Reviewer evaluates the PR diff — read-only except Bash for `gh pr diff`.
        let review_prompt_built_at = Utc::now();
        let review_req = AgentRequest {
            prompt: {
                let base = prompts::agent_review_prompt(pr_url, agent_round, project_type);
                // Inject capability note — primary enforcement now that --allowedTools
                // is not passed to the CLI (issue #483).
                let note = prompts::agent_review_capability_note();
                format!("{note}\n\n{base}")
            },
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            execution_phase: Some(ExecutionPhase::SimpleReview),
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
                    return Ok((false, false));
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
            return Ok((false, false));
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

        // Skip the fix round only when rounds are exhausted and there is no impasse.
        // When impasse is detected at the last round, we still apply the intervention prompt
        // to give the agent one final attempt to break the cycle before GitHub review.
        if agent_round == max_rounds && !is_impasse {
            tracing::info!(
                "agent review exhausted {max_rounds} rounds, proceeding to GitHub review"
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

    Ok((true, pushed_commit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_tracker(
        tracker: &mut Option<(Vec<String>, u32)>,
        issues: &[String],
    ) -> (u32, bool, bool) {
        let normalized = normalize_issues(issues);
        let count = match tracker.as_ref() {
            Some((prev, c)) if *prev == normalized => c + 1,
            _ => 1,
        };
        *tracker = Some((normalized, count));
        let intervention = count >= 3;
        let fatal = count >= 5;
        (count, intervention, fatal)
    }

    #[test]
    fn normalize_issues_is_order_invariant() {
        let ordered = vec!["issue A".to_string(), "issue B".to_string()];
        let reversed = vec!["issue B".to_string(), "issue A".to_string()];
        assert_eq!(normalize_issues(&ordered), normalize_issues(&reversed));
    }

    #[test]
    fn impasse_no_intervention_for_first_two_rounds() {
        let issues = vec!["null pointer".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        let (c1, i1, f1) = step_tracker(&mut tracker, &issues);
        let (c2, i2, f2) = step_tracker(&mut tracker, &issues);
        assert_eq!(c1, 1);
        assert!(!i1 && !f1, "no action on first occurrence");
        assert_eq!(c2, 2);
        assert!(!i2 && !f2, "no action on second occurrence");
    }

    #[test]
    fn impasse_intervention_at_third_consecutive_round() {
        let issues = vec!["null pointer".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        step_tracker(&mut tracker, &issues); // round 1
        step_tracker(&mut tracker, &issues); // round 2
        let (c3, i3, f3) = step_tracker(&mut tracker, &issues); // round 3
        assert_eq!(c3, 3);
        assert!(i3, "intervention at 3rd consecutive round");
        assert!(!f3, "not yet fatal at round 3");
    }

    #[test]
    fn impasse_fatal_at_fifth_consecutive_round() {
        let issues = vec!["null pointer".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        for _ in 0..4 {
            step_tracker(&mut tracker, &issues);
        }
        let (c5, i5, f5) = step_tracker(&mut tracker, &issues);
        assert_eq!(c5, 5);
        assert!(i5, "intervention still active at round 5");
        assert!(f5, "fatal at 5th consecutive round");
    }

    #[test]
    fn impasse_counter_resets_when_issues_change() {
        let issues = vec!["null pointer".to_string()];
        let other = vec!["different bug".to_string()];
        let mut tracker: Option<(Vec<String>, u32)> = None;
        step_tracker(&mut tracker, &issues); // count 1
        step_tracker(&mut tracker, &issues); // count 2
        step_tracker(&mut tracker, &issues); // count 3 — would trigger intervention
        let (c_reset, i_reset, _) = step_tracker(&mut tracker, &other); // different issues
        assert_eq!(c_reset, 1, "counter resets on different issues");
        assert!(!i_reset, "no intervention after reset");
    }

    // --- jaccard_word_similarity unit tests ---

    #[test]
    fn jaccard_identical_strings() {
        assert_eq!(jaccard_word_similarity("hello world", "hello world"), 1.0);
    }

    #[test]
    fn jaccard_disjoint_strings() {
        assert_eq!(jaccard_word_similarity("foo bar", "baz qux"), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // {"a", "b"} ∩ {"b", "c"} = {"b"}, union = {"a","b","c"} → 1/3
        let score = jaccard_word_similarity("a b", "b c");
        let expected = 1.0_f64 / 3.0_f64;
        assert!(
            (score - expected).abs() < 1e-10,
            "expected ~{expected}, got {score}"
        );
    }

    #[test]
    fn jaccard_one_empty() {
        assert_eq!(jaccard_word_similarity("", "hello world"), 0.0);
        assert_eq!(jaccard_word_similarity("hello world", ""), 0.0);
    }

    #[test]
    fn jaccard_both_empty() {
        assert_eq!(jaccard_word_similarity("", ""), 1.0);
    }
}
