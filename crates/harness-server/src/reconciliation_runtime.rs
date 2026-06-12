use super::*;

pub(super) async fn collect_runtime_candidates(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<(Vec<RuntimeWorkflowCandidate>, usize)> {
    let rows: Vec<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT data::text, updated_at
         FROM workflow_instances
         WHERE definition_id = $1
         ORDER BY updated_at DESC",
    )
    .bind(GITHUB_ISSUE_PR_DEFINITION_ID)
    .fetch_all(store.pool())
    .await?;

    let mut candidates = Vec::new();
    let mut skipped_terminal = 0usize;
    for (data, row_updated_at) in rows {
        let instance: WorkflowInstance = serde_json::from_str(&data)?;
        if instance.is_terminal() {
            skipped_terminal += 1;
            continue;
        }
        if let Some(candidate) = runtime_candidate_from_instance(&instance, row_updated_at) {
            candidates.push(candidate);
        }
    }
    Ok((candidates, skipped_terminal))
}

pub(super) fn runtime_candidate_from_instance(
    instance: &WorkflowInstance,
    row_updated_at: chrono::DateTime<chrono::Utc>,
) -> Option<RuntimeWorkflowCandidate> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID || instance.is_terminal() {
        return None;
    }
    let pr_number = instance
        .data
        .get("pr_number")
        .and_then(serde_json::Value::as_u64)?;
    Some(RuntimeWorkflowCandidate {
        workflow_id: instance.id.clone(),
        state: instance.state.clone(),
        row_updated_at,
        repo: optional_json_string(&instance.data, "repo"),
        project_root: optional_json_string(&instance.data, "project_id").map(PathBuf::from),
        issue_number: instance
            .data
            .get("issue_number")
            .and_then(serde_json::Value::as_u64),
        pr_number,
        pr_url: optional_json_string(&instance.data, "pr_url"),
    })
}

pub(super) async fn resolve_runtime_github_state(
    candidate: &RuntimeWorkflowCandidate,
    rate: &mut RateLimiter,
    github_token: Option<&str>,
) -> GitHubState {
    if let Some(pr_url) = candidate.pr_url.as_deref() {
        rate.acquire().await;
        return fetch_pr_state_by_url(pr_url, github_token).await;
    }
    if let Some(repo) = candidate.repo.as_deref() {
        rate.acquire().await;
        return fetch_pr_state_by_slug_with_token(repo, candidate.pr_number, github_token).await;
    }
    tracing::debug!(
        workflow_id = %candidate.workflow_id,
        pr = candidate.pr_number,
        "workflow runtime GitHub state check skipped because repository slug is unavailable"
    );
    GitHubState::Unknown
}
