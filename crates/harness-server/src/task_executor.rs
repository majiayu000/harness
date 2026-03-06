use crate::task_runner::{
    mutate_and_persist, mutate_and_persist_with, update_status, CreateTaskRequest, RoundResult,
    TaskId, TaskStatus, TaskStore,
};
use harness_core::{
    interceptor::TurnInterceptor, prompts, AgentRequest, AgentResponse, CodeAgent, ContextItem,
    Decision, Event, SessionId,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

#[derive(Debug, Deserialize)]
struct GhPrListItem {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
}

/// Run all pre_execute interceptors in order. Returns the (possibly modified) request,
/// or an error if any interceptor returns Block.
async fn run_pre_execute(
    interceptors: &[Arc<dyn TurnInterceptor>],
    mut req: AgentRequest,
) -> anyhow::Result<AgentRequest> {
    for interceptor in interceptors {
        let result = interceptor.pre_execute(&req).await;
        if let Decision::Block = result.decision {
            let reason = result
                .reason
                .unwrap_or_else(|| interceptor.name().to_string());
            return Err(anyhow::anyhow!(
                "Blocked by interceptor '{}': {}",
                interceptor.name(),
                reason
            ));
        }
        if let Some(modified) = result.request {
            req = modified;
        }
    }
    Ok(req)
}

async fn run_post_execute(
    interceptors: &[Arc<dyn TurnInterceptor>],
    req: &AgentRequest,
    resp: &AgentResponse,
) {
    for interceptor in interceptors {
        interceptor.post_execute(req, resp).await;
    }
}

async fn run_on_error(interceptors: &[Arc<dyn TurnInterceptor>], req: &AgentRequest, error: &str) {
    for interceptor in interceptors {
        interceptor.on_error(req, error).await;
    }
}

/// Query GitHub for an existing open PR linked to the given issue.
/// Returns `(pr_number, branch_name)` if found.
async fn find_existing_pr_for_issue(
    project: &Path,
    issue: u64,
) -> anyhow::Result<Option<(u64, String)>> {
    let output = Command::new("gh")
        .current_dir(project)
        .args(["pr", "list", "--search", &format!("#{issue}"), "--state", "open"])
        .args(["--json", "number,headRefName", "--limit", "1"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run `gh pr list` for issue #{issue}: {e}"))?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "`gh pr list` for issue #{issue} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let items: Vec<GhPrListItem> = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow::anyhow!("invalid JSON from `gh pr list`: {e}"))?;

    Ok(items.into_iter().next().map(|item| (item.number, item.head_ref_name)))
}

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    reviewer: Option<&dyn CodeAgent>,
    review_config: &harness_core::AgentReviewConfig,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Arc<Vec<Arc<dyn TurnInterceptor>>>,
    req: &CreateTaskRequest,
    project: PathBuf,
) -> anyhow::Result<()> {
    update_status(store, task_id, TaskStatus::Implementing, 1).await;

    let first_prompt = if let Some(issue) = req.issue {
        match find_existing_pr_for_issue(&project, issue).await {
            Ok(Some((pr_num, branch))) => {
                tracing::info!("reusing existing PR #{pr_num} on branch `{branch}` for issue #{issue}");
                prompts::continue_existing_pr(issue, pr_num, &branch)
            }
            Ok(None) => prompts::implement_from_issue(issue),
            Err(e) => {
                tracing::warn!("failed to check for existing PR for issue #{issue}: {e}");
                prompts::implement_from_issue(issue)
            }
        }
    } else if let Some(pr) = req.pr {
        prompts::check_existing_pr(pr, &review_config.review_bot_command)
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default())
    };

    let mut context_items: Vec<ContextItem> = {
        let guard = skills.read().await;
        guard
            .list()
            .iter()
            .map(|s| ContextItem::Skill {
                id: s.id.to_string(),
                content: s.content.clone(),
            })
            .collect()
    };

    // Load cascading AGENTS.md files and inject as context
    let agents_md = harness_core::agents_md::load_agents_md(&project);
    if !agents_md.is_empty() {
        context_items.push(ContextItem::AgentsMd {
            content: agents_md,
        });
    }

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);

    let initial_req = AgentRequest {
        prompt: first_prompt,
        project_root: project.clone(),
        context: context_items.clone(),
        ..Default::default()
    };

    // Run pre_execute interceptors; Block aborts the task.
    let first_req = run_pre_execute(&interceptors, initial_req).await?;

    let resp = tokio::time::timeout(turn_timeout, agent.execute(first_req.clone())).await;
    let resp = match resp {
        Ok(Ok(r)) => {
            run_post_execute(&interceptors, &first_req, &r).await;
            r
        }
        Ok(Err(e)) => {
            run_on_error(&interceptors, &first_req, &e.to_string()).await;
            return Err(e.into());
        }
        Err(_) => {
            let msg = format!("Turn 1 timed out after {}s", req.turn_timeout_secs);
            run_on_error(&interceptors, &first_req, &msg).await;
            return Err(anyhow::anyhow!("{msg}"));
        }
    };

    let pr_url = prompts::parse_pr_url(&resp.output);
    let pr_number = pr_url
        .as_ref()
        .and_then(|u| prompts::extract_pr_number(u))
        .or(req.pr);

    mutate_and_persist(store, task_id, |s| {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult {
            turn: 1,
            action: "implement".into(),
            result: if pr_url.is_some() || req.pr.is_some() {
                "pr_created".into()
            } else {
                "implemented".into()
            },
        });
    })
    .await;

    let pr_num = match pr_number {
        Some(n) => n,
        None => {
            update_status(store, task_id, TaskStatus::Done, 1).await;
            return Ok(());
        }
    };

    // Agent review phase: independent reviewer evaluates PR diff before GitHub review
    if review_config.enabled {
        if let Some(reviewer) = reviewer {
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
                pr_num,
                &events,
            )
            .await?;
        } else {
            tracing::info!("agent review enabled but no reviewer available, skipping");
        }
    }

    // Review loop: Turn 2..N
    let last_review_round = req.max_rounds.saturating_add(1);
    let mut prev_fixed = false;
    let mut round = 2u32;
    let max_waiting_retries = 3u32;

    while round <= last_review_round {
        update_status(store, task_id, TaskStatus::Waiting, round).await;
        sleep(Duration::from_secs(req.wait_secs)).await;

        update_status(store, task_id, TaskStatus::Reviewing, round).await;

        let review_req = AgentRequest {
            prompt: prompts::review_prompt(req.issue, pr_num, round, prev_fixed, &review_config.review_bot_command),
            project_root: project.clone(),
            context: context_items.clone(),
            ..Default::default()
        };

        let review_req = run_pre_execute(&interceptors, review_req).await?;

        let resp = tokio::time::timeout(turn_timeout, agent.execute(review_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                run_post_execute(&interceptors, &review_req, &r).await;
                r
            }
            Ok(Err(e)) => {
                run_on_error(&interceptors, &review_req, &e.to_string()).await;
                return Err(e.into());
            }
            Err(_) => {
                let msg = format!("Turn {round} timed out after {}s", req.turn_timeout_secs);
                run_on_error(&interceptors, &review_req, &msg).await;
                return Err(anyhow::anyhow!("{msg}"));
            }
        };

        if prompts::is_waiting(&resp.output) {
            let waiting_count = mutate_and_persist_with(store, task_id, |s| {
                s.rounds.push(RoundResult {
                    turn: round,
                    action: "review".into(),
                    result: "waiting".into(),
                });
                s.rounds
                    .iter()
                    .filter(|r| r.result == "waiting" && r.turn == round)
                    .count() as u32
            })
            .await
            .unwrap_or(0);

            if waiting_count >= max_waiting_retries {
                prev_fixed = true;
                round += 1;
            }
            continue;
        }

        let lgtm = prompts::is_lgtm(&resp.output);

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: if lgtm { "lgtm".into() } else { "fixed".into() },
            });
        })
        .await;

        // Log pr_review event for observability and GC signal detection.
        let mut ev = Event::new(
            SessionId::new(),
            "pr_review",
            "task_runner",
            if lgtm {
                Decision::Complete
            } else {
                Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(if lgtm {
            format!("round {round}: lgtm")
        } else {
            format!("round {round}: fixed")
        });
        if let Err(e) = events.log(&ev) {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            update_status(store, task_id, TaskStatus::Done, round).await;
            return Ok(());
        }

        prev_fixed = true;
        round += 1;
    }

    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.turn = req.max_rounds.saturating_add(1);
        s.error = Some(format!(
            "Task did not receive LGTM after {} review rounds.",
            req.max_rounds
        ));
    })
    .await;
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
    interceptors: &[Arc<dyn TurnInterceptor>],
    turn_timeout: Duration,
    pr_num: u64,
    events: &harness_observe::EventStore,
) -> anyhow::Result<()> {
    let max_rounds = review_config.max_rounds;
    for agent_round in 1..=max_rounds {
        update_status(store, task_id, TaskStatus::AgentReview, agent_round).await;

        // Reviewer evaluates the PR diff
        let review_req = AgentRequest {
            prompt: prompts::agent_review_prompt(pr_num, agent_round),
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            ..Default::default()
        };
        let review_req = run_pre_execute(interceptors, review_req).await?;

        let resp = tokio::time::timeout(turn_timeout, reviewer.execute(review_req.clone())).await;
        let resp = match resp {
            Ok(Ok(r)) => {
                run_post_execute(interceptors, &review_req, &r).await;
                r
            }
            Ok(Err(e)) => {
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

        let approved = prompts::is_approved(&resp.output);
        let issues = prompts::extract_review_issues(&resp.output);

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: 0, // agent review rounds use turn 0
                action: "agent_review".into(),
                result: if approved {
                    "approved".into()
                } else {
                    format!("{} issues", issues.len())
                },
            });
        })
        .await;

        // Log agent_review event
        let mut ev = harness_core::Event::new(
            harness_core::SessionId::new(),
            "agent_review",
            "task_runner",
            if approved {
                harness_core::Decision::Complete
            } else {
                harness_core::Decision::Warn
            },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(if approved {
            format!("round {agent_round}: approved")
        } else {
            format!("round {agent_round}: {} issues", issues.len())
        });
        if let Err(e) = events.log(&ev) {
            tracing::warn!("failed to log agent_review event: {e}");
        }

        if approved || issues.is_empty() {
            tracing::info!("agent review approved at round {agent_round}");
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
            prompt: prompts::agent_review_fix_prompt(pr_num, &issues, agent_round),
            project_root: project.to_path_buf(),
            context: context_items.to_vec(),
            ..Default::default()
        };
        let fix_req = run_pre_execute(interceptors, fix_req).await?;

        let fix_resp = tokio::time::timeout(turn_timeout, agent.execute(fix_req.clone())).await;
        match fix_resp {
            Ok(Ok(r)) => {
                run_post_execute(interceptors, &fix_req, &r).await;
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
            });
        })
        .await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_pr_list_item_parses_from_json() {
        let json = r#"[{"number":50,"headRefName":"fix/issue-29"}]"#;
        let items: Vec<GhPrListItem> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].number, 50);
        assert_eq!(items[0].head_ref_name, "fix/issue-29");
    }

    #[test]
    fn gh_pr_list_empty_array_parses() {
        let json = r#"[]"#;
        let items: Vec<GhPrListItem> = serde_json::from_str(json).unwrap();
        assert!(items.is_empty());
    }
}
