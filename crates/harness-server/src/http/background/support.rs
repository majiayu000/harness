use super::*;

pub(super) const RUNTIME_WORKFLOW_CONFIG_RETRY_SECS: u64 = 30;

pub(super) fn parse_issue_pr(task: &task_runner::TaskState) -> (Option<u64>, Option<u64>) {
    let (issue_from_external_id, pr_from_external_id) = task
        .external_id
        .as_deref()
        .map(|eid| {
            if let Some(n) = eid.strip_prefix("issue:") {
                (n.parse::<u64>().ok(), None)
            } else if let Some(n) = eid.strip_prefix("pr:") {
                (None, n.parse::<u64>().ok())
            } else {
                (None, None)
            }
        })
        .unwrap_or((None, None));

    let (issue_from_settings, pr_from_settings) = task
        .request_settings
        .as_ref()
        .map(|settings| (settings.issue, settings.pr))
        .unwrap_or((None, None));

    let issue = issue_from_external_id
        .or(task.issue)
        .or(issue_from_settings)
        .or_else(|| {
            if matches!(task.task_kind, task_runner::TaskKind::Issue) {
                parse_description_number(task.description.as_deref(), "issue #")
            } else {
                None
            }
        });
    let pr = pr_from_external_id.or(pr_from_settings).or_else(|| {
        if matches!(task.task_kind, task_runner::TaskKind::Pr) {
            parse_description_number(task.description.as_deref(), "PR #")
        } else {
            None
        }
    });

    (issue, pr)
}

fn parse_description_number(description: Option<&str>, prefix: &str) -> Option<u64> {
    description
        .and_then(|text| text.strip_prefix(prefix))
        .and_then(|tail| tail.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
}

pub(super) fn build_recovered_request(
    task: &task_runner::TaskState,
    canonical: std::path::PathBuf,
    issue: Option<u64>,
    pr: Option<u64>,
) -> task_runner::CreateTaskRequest {
    let mut req = task_runner::CreateTaskRequest {
        issue,
        pr,
        project: Some(canonical),
        repo: task.repo.clone(),
        source: task.source.clone(),
        external_id: task.external_id.clone(),
        parent_task_id: task.parent_id.clone(),
        priority: task.priority,
        ..Default::default()
    };
    if let Some(ref settings) = task.request_settings {
        settings.apply_to_req(&mut req);
    }
    if req.prompt.is_none() {
        if let Some(system_input) = req.system_input.as_ref() {
            req.prompt = Some(system_input.prompt().to_string());
        }
    }
    req
}

pub(super) fn resolve_reviewer_for_request(
    state: &AppState,
    req: &task_runner::CreateTaskRequest,
    implementor_name: &str,
) -> anyhow::Result<Option<Arc<dyn harness_core::agent::CodeAgent>>> {
    let review_config = review_config_for_request(&state.core.server.config, req)?;
    let (reviewer, _) = crate::http::resolve_reviewer(
        &state.core.server.agent_registry,
        &review_config,
        implementor_name,
    );
    Ok(reviewer)
}

pub(super) fn review_config_for_request(
    server_config: &harness_core::config::HarnessConfig,
    req: &task_runner::CreateTaskRequest,
) -> anyhow::Result<harness_core::config::agents::AgentReviewConfig> {
    let project_root = req
        .project
        .as_deref()
        .context("task request is missing a resolved project root")?;
    let project_config = harness_core::config::project::load_project_config(project_root)
        .with_context(|| {
            format!(
                "failed to load project config for {}",
                project_root.display()
            )
        })?;
    Ok(harness_core::config::resolve::resolve_config(server_config, &project_config).review)
}

pub(super) async fn fail_background_dispatch_task(
    state: &Arc<AppState>,
    task_id: &task_runner::TaskId,
    reason: String,
) {
    if let Err(pe) = task_runner::mutate_and_persist(&state.core.tasks, task_id, move |s| {
        s.status = task_runner::TaskStatus::Failed;
        s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
        s.error = Some(reason);
    })
    .await
    {
        tracing::error!(
            task_id = ?task_id,
            "failed to persist failed background dispatch status: {pe}"
        );
        return;
    }

    if let Some(cb) = &state.intake.completion_callback {
        if let Some(final_state) = state.core.tasks.get(task_id) {
            cb(final_state).await;
        }
    }
}

fn task_has_restart_safe_prompt(task: &task_runner::TaskState) -> bool {
    let mut req = task_runner::CreateTaskRequest::default();
    if let Some(ref settings) = task.request_settings {
        settings.apply_to_req(&mut req);
    }
    if req.prompt.is_none() {
        if let Some(system_input) = req.system_input.as_ref() {
            req.prompt = Some(system_input.prompt().to_string());
        }
    }
    req.prompt.is_some()
}

pub(super) fn task_allows_prompt_orphan_recovery(task: &task_runner::TaskState) -> bool {
    matches!(
        task.task_kind,
        task_runner::TaskKind::Prompt
            | task_runner::TaskKind::Review
            | task_runner::TaskKind::Planner
    ) && task_has_restart_safe_prompt(task)
}

pub(super) fn orphan_recovery_failure_reason(task: &task_runner::TaskState) -> &'static str {
    task.task_kind.orphan_recovery_failure_reason()
}

pub(in crate::http) fn recovery_queue_domain(
    task_kind: task_runner::TaskKind,
) -> task_routes::QueueDomain {
    match task_kind {
        task_runner::TaskKind::Review => task_routes::QueueDomain::Review,
        task_runner::TaskKind::Issue
        | task_runner::TaskKind::Pr
        | task_runner::TaskKind::Prompt
        | task_runner::TaskKind::Planner => task_routes::QueueDomain::Primary,
    }
}

pub(super) async fn await_startup_recovery_ready_task(
    state: &Arc<AppState>,
    task_id: &crate::task_runner::TaskId,
    recovery_kind: &'static str,
) -> Option<crate::task_runner::TaskState> {
    loop {
        let task = state.core.tasks.get(task_id)?;
        if !matches!(task.status, task_runner::TaskStatus::Pending) {
            return None;
        }

        let now = chrono::Utc::now();
        if task.scheduler.has_live_runtime_host_lease(now) {
            let expires_at = task.scheduler.lease_expiry()?;
            let wait_for = expires_at
                .signed_duration_since(now)
                .to_std()
                .unwrap_or_default()
                .saturating_add(std::time::Duration::from_millis(50));
            tracing::info!(
                task_id = ?task.id,
                recovery_kind,
                owner = ?task.scheduler.runtime_host_id(),
                lease_expires_at = %expires_at,
                "startup recovery: waiting for runtime-host lease expiry before redispatch"
            );
            drop(task);
            tokio::time::sleep(wait_for).await;
            continue;
        }

        let task_id = task.id.clone();
        let Some(host_id) = task.scheduler.runtime_host_id().map(str::to_string) else {
            return Some(task);
        };
        drop(task);

        match state
            .core
            .tasks
            .release_runtime_host_claim(&task_id, &host_id)
            .await
        {
            Ok(true) => {
                tracing::info!(
                    task_id = ?task_id,
                    host_id,
                    recovery_kind,
                    "startup recovery: cleared stale runtime-host lease before redispatch"
                );
            }
            Ok(false) => {}
            Err(e) => {
                tracing::error!(
                    task_id = ?task_id,
                    host_id,
                    recovery_kind,
                    error = %e,
                    "startup recovery: failed to clear stale runtime-host lease"
                );
                return None;
            }
        }
    }
}
