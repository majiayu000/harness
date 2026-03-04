use crate::task_runner::{
    mutate_and_persist, update_status, CreateTaskRequest, RoundResult, TaskId, TaskStatus,
    TaskStore,
};
use harness_core::{
    prompts, AgentRequest, CodeAgent, ContextItem, Decision, Event, SessionId,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

pub(crate) async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    req: &CreateTaskRequest,
    project: PathBuf,
) -> anyhow::Result<()> {
    update_status(store, task_id, TaskStatus::Implementing, 1).await;

    let first_prompt = if let Some(issue) = req.issue {
        prompts::implement_from_issue(issue)
    } else if let Some(pr) = req.pr {
        prompts::check_existing_pr(pr)
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default())
    };

    let skill_items: Vec<ContextItem> = {
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

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);
    let resp = tokio::time::timeout(
        turn_timeout,
        agent.execute(AgentRequest {
            prompt: first_prompt,
            project_root: project.clone(),
            context: skill_items.clone(),
            ..Default::default()
        }),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Turn 1 timed out after {}s", req.turn_timeout_secs))?
    ?;

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

    // Review loop: Turn 2..N
    let last_review_round = req.max_rounds.saturating_add(1);
    let mut prev_fixed = false;
    let mut round = 2u32;
    let max_waiting_retries = 3u32;

    while round <= last_review_round {
        update_status(store, task_id, TaskStatus::Waiting, round).await;
        sleep(Duration::from_secs(req.wait_secs)).await;

        update_status(store, task_id, TaskStatus::Reviewing, round).await;

        let resp = tokio::time::timeout(
            turn_timeout,
            agent.execute(AgentRequest {
                prompt: prompts::review_prompt(req.issue, pr_num, round, prev_fixed),
                project_root: project.clone(),
                context: skill_items.clone(),
                ..Default::default()
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("Turn {round} timed out after {}s", req.turn_timeout_secs)
        })?
        ?;

        if prompts::is_waiting(&resp.output) {
            mutate_and_persist(store, task_id, |s| {
                s.rounds.push(RoundResult {
                    turn: round,
                    action: "review".into(),
                    result: "waiting".into(),
                });
            })
            .await;

            let waiting_count = store
                .get(task_id)
                .map(|s| {
                    s.rounds
                        .iter()
                        .filter(|r| r.result == "waiting")
                        .count() as u32
                })
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
            if lgtm { Decision::Complete } else { Decision::Warn },
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
