use super::*;

pub(super) async fn latest_observed_pr_fact_for_workflow(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<Option<ObservedPrFact>> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        return Ok(None);
    };
    latest_observed_pr_fact_for_instance(store, &instance).await
}

async fn latest_observed_pr_fact_for_instance(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
) -> anyhow::Result<Option<ObservedPrFact>> {
    let Some(repo) = pr_repo_for_fact_lookup(&instance.data) else {
        return Ok(None);
    };
    let Some(pr_number) = instance
        .data
        .get("pr_number")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| i64::try_from(value).ok())
    else {
        return Ok(None);
    };
    Ok(store
        .get_remote_fact_snapshot("github", &repo, "pull_request", pr_number)
        .await?
        .map(|snapshot| ObservedPrFact {
            fact_hash: snapshot.fact_hash.clone(),
            activity_at: observed_pr_fact_activity_at(&snapshot),
        }))
}

fn pr_repo_for_fact_lookup(data: &serde_json::Value) -> Option<String> {
    if !data.is_object() {
        return None;
    }
    optional_string_field(data, "repo")
        .map(|repo| repo.to_ascii_lowercase())
        .or_else(|| {
            optional_string_field(data, "pr_url").and_then(|pr_url| {
                harness_agents::output_parsing::parse_github_pr_url(pr_url.trim())
                    .map(|(owner, repo, _)| format!("{owner}/{repo}").to_ascii_lowercase())
            })
        })
}

#[cfg(test)]
mod repository_lookup_tests {
    use super::pr_repo_for_fact_lookup;
    use serde_json::json;

    #[test]
    fn pr_fact_repository_lookup_requires_an_object_and_normalizes_case() {
        assert_eq!(pr_repo_for_fact_lookup(&json!(["Owner/Repo"])), None);
        assert_eq!(
            pr_repo_for_fact_lookup(&json!({"repo": "Owner/Repo"})).as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            pr_repo_for_fact_lookup(&json!({
                "pr_url": "https://github.com/Owner/Repo/pull/7"
            }))
            .as_deref(),
            Some("owner/repo")
        );
    }
}

fn observed_pr_fact_activity_at(
    snapshot: &RemoteFactSnapshot,
) -> Option<chrono::DateTime<chrono::Utc>> {
    ["updated_at", "updatedAt"].into_iter().find_map(|field| {
        snapshot
            .facts
            .get(field)
            .and_then(serde_json::Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc))
    })
}

pub(super) async fn request_pr_feedback_sweep_with_failed_child_suppression_secs_and_activity<
    F,
    Fut,
>(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failed_child_suppression_secs: u64,
    latest_pr_fact: Option<ObservedPrFact>,
    admission: F,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let Some(instance) = store.get_instance(workflow_id).await? else {
        anyhow::bail!("workflow runtime instance `{workflow_id}` was not found");
    };
    if instance.definition_id != "github_issue_pr" || instance.state != "awaiting_feedback" {
        return Ok(PrFeedbackSweepRequestOutcome::NotCandidate {
            workflow_id: instance.id,
            state: instance.state,
        });
    }
    if has_active_pr_feedback_command_with_activity(
        store,
        &instance.id,
        failed_child_suppression_secs,
        latest_pr_fact.as_ref().map(|fact| fact.fact_hash.as_str()),
        latest_pr_fact.as_ref().and_then(|fact| fact.activity_at),
    )
    .await?
    {
        let task_id = runtime_task_id_from_instance(&instance);
        return Ok(PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: instance.id,
            task_id,
        });
    }
    persist_pr_feedback_sweep_request(store, instance, latest_pr_fact.as_ref(), admission).await
}

pub(super) async fn persist_local_review_request<F, Fut>(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    new_instance: bool,
    admission: F,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let workflow_id = instance.id.clone();
    let task_id = runtime_task_id_from_instance(&instance);
    let pr_number = required_u64_field(&instance.data, "pr_number")?;
    let pr_url = optional_string_field(&instance.data, "pr_url");
    let issue_number = instance
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let repo = optional_string_field(&instance.data, "repo");
    let accepted_data = instance.data.clone();
    let review_nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let output = build_local_review_request_decision(
        &instance,
        LocalReviewDecisionInput {
            dedupe_key: &format!("local-review:{}:{review_nonce}", instance.id),
            pr_number,
            pr_url: pr_url.as_deref(),
            issue_number,
            repo: repo.as_deref(),
            summary: "Runtime workflow requested local agent review before remote feedback.",
        },
    );
    let event_payload = json!({
        "issue_number": issue_number,
        "repo": repo.as_deref(),
        "pr_number": pr_number,
        "pr_url": pr_url.as_deref(),
    });
    admission().await?;
    match commit_runtime_decision(
        store,
        instance,
        new_instance,
        output.decision,
        "LocalReviewRequested",
        "workflow_runtime_pr_feedback",
        event_payload,
        accepted_data,
    )
    .await?
    {
        RuntimeDecisionCommitOutcome::Accepted => Ok(PrFeedbackSweepRequestOutcome::Requested {
            workflow_id,
            task_id,
        }),
        RuntimeDecisionCommitOutcome::Rejected { reason } => {
            Ok(PrFeedbackSweepRequestOutcome::Rejected {
                workflow_id,
                reason,
            })
        }
        RuntimeDecisionCommitOutcome::Stale => {
            let state = store
                .get_instance(&workflow_id)
                .await?
                .map(|instance| instance.state)
                .unwrap_or_else(|| "missing".to_string());
            Ok(PrFeedbackSweepRequestOutcome::NotCandidate { workflow_id, state })
        }
    }
}
