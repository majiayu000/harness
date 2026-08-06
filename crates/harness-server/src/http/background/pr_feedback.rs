use super::*;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::http) struct RuntimePrFeedbackSweepTick {
    pub requested: usize,
    pub auto_merge_requested: usize,
    pub active_command_exists: usize,
    pub remote_request_attempts: usize,
    pub skipped: usize,
    pub rejected: usize,
}

impl RuntimePrFeedbackSweepTick {
    fn touched_anything(&self) -> bool {
        self.requested > 0
            || self.auto_merge_requested > 0
            || self.active_command_exists > 0
            || self.skipped > 0
            || self.rejected > 0
    }
}

#[cfg(test)]
pub(in crate::http) async fn run_runtime_pr_feedback_sweep_tick(
    state: &Arc<AppState>,
    limit: usize,
) -> anyhow::Result<RuntimePrFeedbackSweepTick> {
    let mut cursor = 0;
    run_runtime_pr_feedback_sweep_tick_with_cursor(state, limit, &mut cursor).await
}

pub(in crate::http) async fn run_runtime_pr_feedback_sweep_tick_with_cursor(
    state: &Arc<AppState>,
    limit: usize,
    cursor: &mut usize,
) -> anyhow::Result<RuntimePrFeedbackSweepTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimePrFeedbackSweepTick::default());
    };
    let mut workflows = store
        .list_instances_by_definition(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            None,
            None,
        )
        .await?;
    if workflows.is_empty() {
        *cursor = 0;
        return Ok(RuntimePrFeedbackSweepTick::default());
    }
    let workflow_count = workflows.len();
    let start = *cursor % workflow_count;
    workflows.rotate_left(start);
    let mut tick = RuntimePrFeedbackSweepTick::default();
    let work_limit = limit.max(1);
    let remote_request_limit = limit.max(1);
    for (offset, mut workflow) in workflows.into_iter().enumerate() {
        *cursor = (start + offset + 1) % workflow_count;
        if !matches!(
            workflow.state.as_str(),
            "pr_open" | "awaiting_feedback" | "ready_to_merge"
        ) {
            continue;
        }
        if workflow
            .data
            .get("pr_number")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            match recover_runtime_pr_binding_from_bind_pr_command(store, workflow).await? {
                Some(recovered) => workflow = recovered,
                None => {
                    tick.skipped += 1;
                    continue;
                }
            }
        }
        let Some(project_root) = workflow_project_root(&workflow) else {
            tick.skipped += 1;
            continue;
        };
        if !project_root.exists() {
            tracing::warn!(
                workflow_id = %workflow.id,
                project_root = %project_root.display(),
                "workflow runtime PR feedback sweep skipped unresolvable project path"
            );
            tick.skipped += 1;
            continue;
        }
        let workflow_cfg = match load_runtime_workflow_config(
            &project_root,
            "workflow runtime PR feedback sweeper",
        ) {
            Ok(config) => config,
            Err(_) => {
                tick.skipped += 1;
                continue;
            }
        };
        if !workflow_cfg.pr_feedback.enabled
            || !workflow_cfg.runtime_dispatch.enabled
            || !workflow_cfg.runtime_worker.enabled
        {
            tick.skipped += 1;
            continue;
        }
        if tick.requested + tick.auto_merge_requested >= work_limit {
            *cursor = (start + offset) % workflow_count;
            break;
        }

        if workflow.state == "ready_to_merge" {
            if tick.remote_request_attempts >= remote_request_limit {
                *cursor = (start + offset) % workflow_count;
                break;
            }
            tick.remote_request_attempts += 1;
            match request_auto_merge_if_enabled(state, store, &workflow).await? {
                AutoMergeRequestOutcome::Requested => tick.auto_merge_requested += 1,
                AutoMergeRequestOutcome::Disabled => tick.skipped += 1,
                AutoMergeRequestOutcome::NotReady => tick.skipped += 1,
                AutoMergeRequestOutcome::Rejected => tick.rejected += 1,
            }
            continue;
        }

        let request_outcome = if workflow.state == "pr_open" {
            crate::workflow_runtime_pr_feedback::request_local_review(store, &workflow.id).await?
        } else {
            if crate::workflow_runtime_pr_feedback::pr_feedback_driver_command_is_active(
                store,
                &workflow.id,
            )
            .await?
            {
                tick.active_command_exists += 1;
                continue;
            }
            if tick.remote_request_attempts >= remote_request_limit {
                *cursor = (start + offset) % workflow_count;
                break;
            }
            tick.remote_request_attempts += 1;
            let refreshed = match refresh_pr_feedback_sweep_remote_fact(state, store, &workflow)
                .await
            {
                Ok(refreshed) => refreshed,
                Err(error) => {
                    tracing::warn!(
                        workflow_id = %workflow.id,
                        error = %error,
                        "workflow runtime PR feedback sweep skipped workflow after PR fact refresh failure"
                    );
                    tick.rejected += 1;
                    continue;
                }
            };
            if !refreshed {
                tick.skipped += 1;
                continue;
            }
            crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep(store, &workflow.id)
                .await?
        };
        match request_outcome {
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Requested {
                ..
            } => tick.requested += 1,
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::ActiveCommandExists {
                ..
            } => tick.active_command_exists += 1,
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::NotCandidate {
                ..
            } => tick.skipped += 1,
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Rejected {
                ..
            } => tick.rejected += 1,
        }
    }
    Ok(tick)
}

async fn refresh_pr_feedback_sweep_remote_fact(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    workflow: &WorkflowInstance,
) -> anyhow::Result<bool> {
    let Some(target) = pr_feedback_snapshot_target_from_workflow(workflow)? else {
        tracing::warn!(
            workflow_id = %workflow.id,
            "workflow runtime PR feedback sweep skipped because the PR snapshot target is incomplete"
        );
        return Ok(false);
    };
    let snapshot = fetch_github_pr_snapshot(
        &target,
        state.core.server.config.server.github_token.as_deref(),
    )
    .await?;
    let remote_fact = snapshot.remote_fact_snapshot()?;
    store.upsert_remote_fact_snapshot(&remote_fact).await?;
    Ok(true)
}

fn pr_feedback_snapshot_target_from_workflow(
    workflow: &WorkflowInstance,
) -> anyhow::Result<Option<GitHubPrSnapshotTarget>> {
    let Some(pr_number) = workflow
        .data
        .get("pr_number")
        .and_then(serde_json::Value::as_u64)
    else {
        return Ok(None);
    };
    let repo = workflow
        .data
        .get("repo")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            workflow
                .data
                .get("pr_url")
                .and_then(serde_json::Value::as_str)
                .and_then(harness_core::prompts::parse_github_pr_url)
                .map(|(owner, repo, _)| format!("{owner}/{repo}"))
        });
    let Some(repo) = repo else {
        return Ok(None);
    };
    let mut target = GitHubPrSnapshotTarget::new(repo, pr_number)?;
    if let Some(expected_base_ref) = expected_base_ref_from_workflow_data(&workflow.data) {
        target = target.with_expected_base_ref(expected_base_ref);
    }
    Ok(Some(target))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoMergeRequestOutcome {
    Requested,
    Disabled,
    NotReady,
    Rejected,
}

async fn request_auto_merge_if_enabled(
    state: &Arc<AppState>,
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    workflow: &harness_workflow::runtime::WorkflowInstance,
) -> anyhow::Result<AutoMergeRequestOutcome> {
    let repo = workflow
        .data
        .get("repo")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let Some(repo) = repo else {
        return Ok(AutoMergeRequestOutcome::Disabled);
    };
    let Some(github_cfg) = state.core.server.config.intake.github.as_ref() else {
        return Ok(AutoMergeRequestOutcome::Disabled);
    };
    let policy = github_cfg.auto_merge_policy_for_repo(repo);
    if !policy.enabled {
        return Ok(AutoMergeRequestOutcome::Disabled);
    }

    let pr_number = workflow
        .data
        .get("pr_number")
        .and_then(serde_json::Value::as_u64);
    let Some(pr_number) = pr_number else {
        return Ok(AutoMergeRequestOutcome::NotReady);
    };
    let mut target = GitHubPrSnapshotTarget::new(repo, pr_number)?;
    if let Some(expected_base_ref) = expected_base_ref_from_workflow_data(&workflow.data) {
        target = target.with_expected_base_ref(expected_base_ref);
    }
    let snapshot = fetch_github_pr_snapshot(
        &target,
        state.core.server.config.server.github_token.as_deref(),
    )
    .await?;
    let workflow = match prepare_auto_merge_workflow_from_snapshot(workflow, &snapshot, &policy)? {
        AutoMergeSnapshotGate::Ready(workflow) => workflow,
        AutoMergeSnapshotGate::NotReady => return Ok(AutoMergeRequestOutcome::NotReady),
    };
    match crate::workflow_runtime_pr_feedback::approve_runtime_merge_with_instance(store, *workflow)
        .await?
    {
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::Approved { .. } => {
            Ok(AutoMergeRequestOutcome::Requested)
        }
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::NotReady { .. } => {
            Ok(AutoMergeRequestOutcome::NotReady)
        }
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::Rejected { .. } => {
            Ok(AutoMergeRequestOutcome::Rejected)
        }
        crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::NotFound
        | crate::workflow_runtime_pr_feedback::RuntimeMergeApprovalOutcome::NotCandidate {
            ..
        } => Ok(AutoMergeRequestOutcome::Disabled),
    }
}

async fn recover_runtime_pr_binding_from_bind_pr_command(
    store: &WorkflowRuntimeStore,
    workflow: WorkflowInstance,
) -> anyhow::Result<Option<WorkflowInstance>> {
    match store
        .repair_pr_binding_from_latest_command(
            &workflow.id,
            workflow.version,
            "workflow_runtime_pr_feedback",
        )
        .await?
    {
        WorkflowPrBindingRepairOutcome::Repaired {
            instance,
            command_id,
            pr_number,
            pr_url,
        } => {
            tracing::info!(
                workflow_id = %instance.id,
                pr_number,
                pr_url,
                command_id,
                "workflow runtime PR feedback sweeper recovered missing PR binding"
            );
            Ok(Some(*instance))
        }
        WorkflowPrBindingRepairOutcome::NoBindPrCommand => Ok(None),
        WorkflowPrBindingRepairOutcome::StaleInstance => {
            tracing::debug!(
                workflow_id = %workflow.id,
                expected_version = workflow.version,
                "workflow runtime PR binding repair deferred after concurrent instance change"
            );
            Ok(None)
        }
    }
}

pub(in crate::http) fn spawn_runtime_pr_feedback_sweeper(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime PR feedback sweeper disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut remote_refresh_cursor = 0;
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = match load_runtime_workflow_config(
                &state.core.project_root,
                "workflow runtime PR feedback sweeper",
            ) {
                Ok(config) => config,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(
                        RUNTIME_WORKFLOW_CONFIG_RETRY_SECS,
                    ))
                    .await;
                    continue;
                }
            };
            let interval =
                std::time::Duration::from_secs(workflow_cfg.pr_feedback.sweep_interval_secs.max(1));
            match run_runtime_pr_feedback_sweep_tick_with_cursor(
                &state,
                128,
                &mut remote_refresh_cursor,
            )
            .await
            {
                Ok(tick) if tick.touched_anything() => {
                    tracing::info!(
                        requested = tick.requested,
                        active_command_exists = tick.active_command_exists,
                        remote_request_attempts = tick.remote_request_attempts,
                        skipped = tick.skipped,
                        rejected = tick.rejected,
                        "workflow runtime PR feedback sweeper tick complete"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("workflow runtime PR feedback sweeper tick failed: {error}");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}
