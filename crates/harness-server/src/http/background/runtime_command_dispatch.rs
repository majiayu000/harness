use super::*;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::http) struct RuntimeCommandDispatchTick {
    pub enqueued: usize,
    pub already_dispatched: usize,
    pub deferred: usize,
    pub skipped: usize,
}

impl RuntimeCommandDispatchTick {
    fn from_outcomes(outcomes: &[CommandDispatchOutcome]) -> Self {
        let mut tick = Self::default();
        for outcome in outcomes {
            match outcome {
                CommandDispatchOutcome::Enqueued { .. } => tick.enqueued += 1,
                CommandDispatchOutcome::AlreadyDispatched { .. } => tick.already_dispatched += 1,
                CommandDispatchOutcome::Deferred { .. } => tick.deferred += 1,
                CommandDispatchOutcome::Skipped { .. } => tick.skipped += 1,
            }
        }
        tick
    }

    fn touched_anything(&self) -> bool {
        self.enqueued > 0 || self.already_dispatched > 0 || self.deferred > 0 || self.skipped > 0
    }
}

pub(in crate::http) async fn run_runtime_command_dispatch_tick(
    state: &Arc<AppState>,
    runtime_profiles: impl Into<RuntimeProfileSelector>,
    batch_limit: i64,
) -> anyhow::Result<RuntimeCommandDispatchTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeCommandDispatchTick::default());
    };
    let release =
        crate::workflow_runtime_submission::release_ready_issue_dependencies(store, batch_limit)
            .await?;
    let prompt_release =
        crate::workflow_runtime_submission::release_ready_prompt_dependencies(store, batch_limit)
            .await?;
    if release.released > 0 || release.failed > 0 || release.skipped > 0 {
        tracing::info!(
            released = release.released,
            failed = release.failed,
            waiting = release.waiting,
            skipped = release.skipped,
            "workflow runtime dependency release tick complete"
        );
    }
    if prompt_release.released > 0 || prompt_release.failed > 0 || prompt_release.skipped > 0 {
        tracing::info!(
            released = prompt_release.released,
            failed = prompt_release.failed,
            waiting = prompt_release.waiting,
            skipped = prompt_release.skipped,
            "workflow runtime prompt dependency release tick complete"
        );
    }
    let fallback_profile_selector = runtime_profiles.into();
    let dispatch_owner = format!("server-runtime-dispatcher:{}", uuid::Uuid::new_v4());
    let commands = store
        .claim_pending_commands(
            &dispatch_owner,
            chrono::Utc::now() + chrono::Duration::seconds(30),
            batch_limit,
        )
        .await?;
    let mut outcomes = Vec::with_capacity(commands.len());
    for command in commands {
        outcomes.push(
            dispatch_runtime_command_with_project_policy(
                state,
                store,
                command,
                fallback_profile_selector.clone(),
                &dispatch_owner,
            )
            .await?,
        );
    }
    Ok(RuntimeCommandDispatchTick::from_outcomes(&outcomes))
}

async fn dispatch_runtime_command_with_project_policy(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    command: WorkflowCommandRecord,
    fallback_profile_selector: RuntimeProfileSelector,
    dispatch_owner: &str,
) -> anyhow::Result<CommandDispatchOutcome> {
    if !command.command.requires_runtime_job() {
        return RuntimeCommandDispatcher::with_profile_selector(store, fallback_profile_selector)
            .with_dispatcher_id(dispatch_owner)
            .dispatch_command(command)
            .await;
    }

    let project_root = runtime_command_project_root(store, &command, &state.core.project_root)
        .await
        .context("failed to resolve runtime command project")?;

    let profile_selector = match runtime_dispatch_profile_selector_for_command(
        state,
        store,
        &command,
        &fallback_profile_selector,
    )
    .await
    {
        Ok(Some(profile_selector)) => profile_selector,
        Ok(None) => {
            let reason = "workflow runtime dispatch or worker is disabled for the command project"
                .to_string();
            let workflow_cfg = load_runtime_workflow_config(
                &project_root,
                "workflow runtime command dispatcher backoff",
            )?;
            return defer_server_dispatch_barrier(
                store,
                &command,
                dispatch_owner,
                DispatchBarrierInput::new(
                    DispatchBarrierReasonCode::RuntimePolicyDisabled,
                    reason,
                    project_root.display().to_string(),
                ),
                dispatch_backoff(&workflow_cfg.runtime_dispatch)?,
            )
            .await;
        }
        Err(RuntimeDispatchProfileSelectionError::WorkflowConfig(error)) => {
            let reason = format!("workflow runtime project config failed to load: {error}");
            return defer_server_dispatch_barrier(
                store,
                &command,
                dispatch_owner,
                DispatchBarrierInput::new(
                    DispatchBarrierReasonCode::WorkflowConfigInvalid,
                    reason,
                    project_root.display().to_string(),
                ),
                DispatchBackoffPolicy::default(),
            )
            .await;
        }
        Err(RuntimeDispatchProfileSelectionError::Other(error)) => {
            return Err(error.context("failed to select runtime dispatch profile"));
        }
    };

    let isolation_config = runtime_isolation_config_for_command(state, store, &command)
        .await
        .context("failed to load runtime isolation config")?;
    let repo = command_repo_hint(store, &command).await?;
    let activity = command.command.runtime_activity_key().to_string();
    let dispatch_gate_fact_hash = command_dispatch_gate_fact_hash(&command);
    let outcome = RuntimeCommandDispatcher::with_profile_selector(store, profile_selector)
        .with_isolation_config(isolation_config)
        .with_isolation_availability(state.isolation_availability.clone())
        .with_dispatcher_id(dispatch_owner)
        .with_defer_backoff(dispatch_backoff(
            &load_runtime_workflow_config(
                &project_root,
                "workflow runtime command dispatcher backoff",
            )?
            .runtime_dispatch,
        )?)
        .dispatch_command(command)
        .await?;
    record_runtime_agent_dispatch_counter(
        state,
        repo.as_deref(),
        &activity,
        &outcome,
        dispatch_gate_fact_hash.as_deref(),
    );
    Ok(outcome)
}

async fn runtime_isolation_config_for_command(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
) -> anyhow::Result<harness_core::config::isolation::IsolationConfig> {
    let project_root = runtime_command_project_root(store, command, &state.core.project_root)
        .await
        .context("failed to resolve command project root for isolation config")?;
    let project_config = harness_core::config::project::load_project_config(&project_root)
        .with_context(|| {
            format!(
                "failed to load project config for isolation at {}",
                project_root.display()
            )
        })?;
    Ok(project_config
        .isolation
        .unwrap_or_else(|| state.core.server.config.isolation.clone()))
}

async fn command_repo_hint(
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
) -> anyhow::Result<Option<String>> {
    let instance_repo = store
        .get_instance(&command.workflow_id)
        .await?
        .and_then(|instance| optional_json_string(&instance.data, "repo"));
    Ok(instance_repo.or_else(|| optional_json_string(&command.command.command, "repo")))
}

fn optional_json_string(value: &serde_json::Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn command_dispatch_gate_fact_hash(command: &WorkflowCommandRecord) -> Option<String> {
    command
        .command
        .command
        .get("dispatch_gate")
        .and_then(|value| value.get("fact_hash"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn record_runtime_agent_dispatch_counter(
    state: &AppState,
    repo: Option<&str>,
    activity: &str,
    outcome: &CommandDispatchOutcome,
    dispatch_gate_fact_hash: Option<&str>,
) {
    let Some(repo) = repo else {
        return;
    };
    match outcome {
        CommandDispatchOutcome::Enqueued { .. } => {
            if let Some(metric) = token_dispatch_metric_for_activity(activity) {
                state.intake.record_github_token_dispatch(repo, metric);
            }
        }
        CommandDispatchOutcome::AlreadyDispatched { .. } if dispatch_gate_fact_hash.is_some() => {
            state.intake.record_github_token_dispatch(
                repo,
                crate::http::GitHubTokenDispatchMetric::AgentSkippedSameFactHash,
            );
        }
        CommandDispatchOutcome::AlreadyDispatched { .. }
        | CommandDispatchOutcome::Deferred { .. }
        | CommandDispatchOutcome::Skipped { .. } => {}
    }
}

fn dispatch_backoff(
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
) -> anyhow::Result<DispatchBackoffPolicy> {
    DispatchBackoffPolicy::from_seconds(policy.defer_backoff_secs, policy.defer_backoff_max_secs)
}

async fn defer_server_dispatch_barrier(
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
    dispatch_owner: &str,
    barrier: DispatchBarrierInput,
    backoff: DispatchBackoffPolicy,
) -> anyhow::Result<CommandDispatchOutcome> {
    Ok(
        match store
            .defer_claimed_command_if_owned(
                &command.id,
                dispatch_owner,
                command.dispatch_claim_generation,
                barrier,
                chrono::Utc::now(),
                backoff,
            )
            .await?
        {
            DeferClaimedCommandOutcome::Deferred(barrier)
            | DeferClaimedCommandOutcome::AlreadyDeferred(barrier) => {
                CommandDispatchOutcome::Deferred {
                    command_id: command.id.clone(),
                    barrier,
                }
            }
            DeferClaimedCommandOutcome::StaleClaim => CommandDispatchOutcome::Skipped {
                command_id: command.id.clone(),
                reason: "dispatch claim became stale before deferral".to_string(),
            },
            DeferClaimedCommandOutcome::WorkflowTerminal { status } => {
                CommandDispatchOutcome::Skipped {
                    command_id: command.id.clone(),
                    reason: format!("workflow became terminal; command is `{status}`"),
                }
            }
        },
    )
}

fn token_dispatch_metric_for_activity(
    activity: &str,
) -> Option<crate::http::GitHubTokenDispatchMetric> {
    match activity {
        "implement_issue" => Some(crate::http::GitHubTokenDispatchMetric::AgentImplementIssue),
        "address_pr_feedback" => Some(crate::http::GitHubTokenDispatchMetric::AgentAddressFeedback),
        "merge_pr" => Some(crate::http::GitHubTokenDispatchMetric::AgentMergePr),
        "analyze_dependencies" => {
            Some(crate::http::GitHubTokenDispatchMetric::AgentDependencyAnalysis)
        }
        _ => None,
    }
}

async fn runtime_dispatch_profile_selector_for_command(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
    fallback_profile_selector: &RuntimeProfileSelector,
) -> Result<Option<RuntimeProfileSelector>, RuntimeDispatchProfileSelectionError> {
    let project_root = runtime_command_project_root(store, command, &state.core.project_root)
        .await
        .map_err(RuntimeDispatchProfileSelectionError::Other)?;
    let workflow_cfg =
        load_runtime_workflow_config(&project_root, "workflow runtime command dispatcher")
            .map_err(RuntimeDispatchProfileSelectionError::WorkflowConfig)?;
    if !workflow_cfg.runtime_dispatch.enabled || !workflow_cfg.runtime_worker.enabled {
        tracing::debug!(
            workflow_id = %command.workflow_id,
            command_id = %command.id,
            project_root = %project_root.display(),
            dispatch_enabled = workflow_cfg.runtime_dispatch.enabled,
            worker_enabled = workflow_cfg.runtime_worker.enabled,
            "workflow runtime command skipped by project runtime policy"
        );
        return Ok(None);
    }
    let inherited_profile = runtime_default_profile_for_project(
        state,
        &project_root,
        Some(fallback_profile_selector.select(None, None)),
    )
    .await
    .map_err(RuntimeDispatchProfileSelectionError::Other)?;
    persist_runtime_profile_manifest(
        store,
        &project_root,
        &state.core.server.config,
        &workflow_cfg.runtime_dispatch,
        &inherited_profile,
    )
    .await
    .map_err(RuntimeDispatchProfileSelectionError::Other)?;
    Ok(Some(
        runtime_dispatch_profile_selector(
            &state.core.server.config,
            &workflow_cfg.runtime_dispatch,
            &inherited_profile,
        )
        .map_err(RuntimeDispatchProfileSelectionError::Other)?,
    ))
}

#[derive(Debug)]
enum RuntimeDispatchProfileSelectionError {
    WorkflowConfig(anyhow::Error),
    Other(anyhow::Error),
}

async fn runtime_command_project_root(
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
    fallback_project_root: &Path,
) -> anyhow::Result<PathBuf> {
    let Some(instance) = store.get_instance(&command.workflow_id).await? else {
        tracing::warn!(
            workflow_id = %command.workflow_id,
            command_id = %command.id,
            fallback_project_root = %fallback_project_root.display(),
            "workflow runtime command has no workflow instance; using fallback project root"
        );
        return Ok(fallback_project_root.to_path_buf());
    };
    Ok(workflow_project_root(&instance).unwrap_or_else(|| {
        tracing::warn!(
            workflow_id = %command.workflow_id,
            command_id = %command.id,
            fallback_project_root = %fallback_project_root.display(),
            "workflow runtime command workflow has no project_id; using fallback project root"
        );
        fallback_project_root.to_path_buf()
    }))
}

pub(super) fn workflow_project_root(instance: &WorkflowInstance) -> Option<PathBuf> {
    instance
        .data
        .get("project_id")
        .and_then(serde_json::Value::as_str)
        .filter(|project_id| !project_id.trim().is_empty())
        .map(PathBuf::from)
}

pub(in crate::http) fn load_runtime_workflow_config(
    project_root: &Path,
    subsystem: &str,
) -> anyhow::Result<harness_core::config::workflow::WorkflowConfig> {
    harness_core::config::workflow::load_workflow_config(project_root).map_err(|error| {
        tracing::error!(
            project_root = %project_root.display(),
            "{subsystem}: failed to load WORKFLOW.md; runtime subsystem disabled until the config is fixed: {error}"
        );
        anyhow::anyhow!(
            "{subsystem}: failed to load WORKFLOW.md at {}: {error}",
            project_root.join("WORKFLOW.md").display()
        )
    })
}

pub(in crate::http) async fn github_repo_project_root(
    repo_config: &harness_core::config::intake::GitHubRepoConfig,
    fallback: &Path,
) -> PathBuf {
    let configured = repo_config
        .project_root
        .as_ref()
        .filter(|path| !path.trim().is_empty())
        .map(|path| expand_home_path(path))
        .unwrap_or_else(|| fallback.to_path_buf());
    match task_runner::resolve_canonical_project(Some(configured.clone())).await {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                repo = %repo_config.repo,
                project_root = %configured.display(),
                "github repo project root resolver failed to canonicalize project root: {error}"
            );
            configured
        }
    }
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

pub(in crate::http) fn spawn_runtime_command_dispatcher(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime command dispatcher disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = match load_runtime_workflow_config(
                &state.core.project_root,
                "workflow runtime command dispatcher",
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
            let policy = workflow_cfg.runtime_dispatch;
            let interval = std::time::Duration::from_secs(policy.interval_secs.max(1));
            let inherited_profile = match runtime_default_profile_for_project(
                &state,
                &state.core.project_root,
                None,
            )
            .await
            {
                Ok(profile) => profile,
                Err(error) => {
                    tracing::warn!(
                            "workflow runtime command dispatcher could not resolve default runtime profile: {error}"
                        );
                    tokio::time::sleep(interval).await;
                    continue;
                }
            };
            let profile_selector = match runtime_dispatch_profile_selector(
                &state.core.server.config,
                &policy,
                &inherited_profile,
            ) {
                Ok(selector) => selector,
                Err(error) => {
                    tracing::warn!(
                        "workflow runtime command dispatcher could not build runtime profile selector: {error}"
                    );
                    tokio::time::sleep(interval).await;
                    continue;
                }
            };
            match run_runtime_command_dispatch_tick(
                &state,
                profile_selector,
                i64::from(policy.batch_limit.max(1)),
            )
            .await
            {
                Ok(tick) if tick.touched_anything() => {
                    tracing::info!(
                        enqueued = tick.enqueued,
                        already_dispatched = tick.already_dispatched,
                        skipped = tick.skipped,
                        "workflow runtime command dispatcher tick complete"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("workflow runtime command dispatcher tick failed: {e}");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}
