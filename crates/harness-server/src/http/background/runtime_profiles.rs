use super::*;

fn runtime_kind_from_config(value: &str) -> Option<RuntimeKind> {
    match value {
        "codex_exec" => Some(RuntimeKind::CodexExec),
        "codex_jsonrpc" => Some(RuntimeKind::CodexJsonrpc),
        "claude_code" => Some(RuntimeKind::ClaudeCode),
        "anthropic_api" => Some(RuntimeKind::AnthropicApi),
        "remote_host" => Some(RuntimeKind::RemoteHost),
        _ => None,
    }
}

fn runtime_profile_from_kind(
    config: &harness_core::config::HarnessConfig,
    kind: RuntimeKind,
) -> RuntimeProfile {
    match kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => {
            let mut profile = RuntimeProfile::new("codex-default", kind);
            profile.model = Some(config.agents.codex.default_model.clone());
            profile.reasoning_effort = Some(config.agents.codex.reasoning_effort.clone());
            profile
        }
        RuntimeKind::ClaudeCode => {
            let mut profile = RuntimeProfile::new("claude-default", kind);
            profile.model = Some(config.agents.claude.default_model.clone());
            profile
        }
        RuntimeKind::AnthropicApi => {
            let mut profile = RuntimeProfile::new("anthropic-api-default", kind);
            profile.model = Some(config.agents.anthropic_api.default_model.clone());
            profile
        }
        RuntimeKind::RemoteHost => RuntimeProfile::new("remote-host-default", kind),
    }
}

pub(super) fn runtime_profile_from_agent(
    config: &harness_core::config::HarnessConfig,
    agent_name: &str,
) -> Option<RuntimeProfile> {
    match agent_name {
        "codex" => Some(runtime_profile_from_kind(config, RuntimeKind::CodexJsonrpc)),
        "claude" => Some(runtime_profile_from_kind(config, RuntimeKind::ClaudeCode)),
        "anthropic-api" => Some(runtime_profile_from_kind(config, RuntimeKind::AnthropicApi)),
        _ => None,
    }
}

pub(super) fn runtime_profile_with_prompt_execution_policy(
    config: &harness_core::config::HarnessConfig,
    base_profile: &RuntimeProfile,
    policy: &crate::workflow_runtime_submission::runtime_models::PromptExecutionPolicy,
) -> anyhow::Result<RuntimeProfile> {
    let mut profile = match policy.agent.as_deref() {
        None => base_profile.clone(),
        Some(agent_name) => {
            let requested = runtime_profile_from_agent(config, agent_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "workflow runtime does not support requested agent `{agent_name}` for prompt execution"
                )
            })?;
            if runtime_kinds_share_agent(requested.kind, base_profile.kind) {
                base_profile.clone()
            } else {
                RuntimeProfile {
                    sandbox: base_profile.sandbox.clone(),
                    approval_policy: None,
                    max_turns: base_profile.max_turns,
                    timeout_secs: base_profile.timeout_secs,
                    ..requested
                }
            }
        }
    };
    if let Some(timeout_secs) = policy.turn_timeout_secs {
        profile.timeout_secs = Some(timeout_secs);
    }
    Ok(profile)
}

fn runtime_kinds_share_agent(left: RuntimeKind, right: RuntimeKind) -> bool {
    left == right
        || matches!(
            (left, right),
            (
                RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc,
                RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc
            )
        )
}

async fn runtime_default_agent_name_for_project(
    state: &Arc<AppState>,
    project_root: &Path,
) -> anyhow::Result<Option<String>> {
    let project_root_for_config = project_root.to_path_buf();
    let project_cfg = tokio::task::spawn_blocking(move || {
        harness_core::config::project::load_project_config(&project_root_for_config)
    })
    .await
    .context("failed to join project config load task")?
    .with_context(|| {
        format!(
            "failed to load project config at {}",
            project_root.display()
        )
    })?;
    if let Some(agent_name) = project_cfg
        .agent
        .as_ref()
        .and_then(|agent| agent.default.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("auto"))
    {
        return Ok(Some(agent_name.to_string()));
    }

    if let Some(registry) = state.core.project_registry.as_deref() {
        let canonical_root = tokio::fs::canonicalize(project_root)
            .await
            .unwrap_or_else(|_| project_root.to_path_buf());
        if let Some(project) = registry.get_by_root(&canonical_root).await? {
            if let Some(agent_name) = project
                .default_agent
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("auto"))
            {
                return Ok(Some(agent_name.to_string()));
            }
        }
    }

    Ok(state
        .core
        .server
        .agent_registry
        .resolved_default_agent_name()
        .map(str::to_string))
}

pub(super) async fn runtime_default_profile_for_project(
    state: &Arc<AppState>,
    project_root: &Path,
    fallback_profile: Option<&RuntimeProfile>,
) -> anyhow::Result<RuntimeProfile> {
    let agent_name = runtime_default_agent_name_for_project(state, project_root).await?;
    if let Some(agent_name) = agent_name.as_deref() {
        if let Some(profile) = runtime_profile_from_agent(&state.core.server.config, agent_name) {
            return Ok(profile);
        }
        tracing::warn!(
            agent = agent_name,
            project_root = %project_root.display(),
            "workflow runtime cannot derive a runtime profile from the project default agent; falling back to dispatcher profile"
        );
    }

    fallback_profile.cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "workflow runtime cannot derive a runtime profile for {}; set runtime_dispatch.runtime_kind or configure a supported default agent",
            project_root.display()
        )
    })
}

pub(super) fn runtime_dispatch_profile(
    config: &harness_core::config::HarnessConfig,
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
    inherited_profile: &RuntimeProfile,
) -> anyhow::Result<RuntimeProfile> {
    let base_profile = match policy.runtime_kind.as_deref() {
        Some(value) => match runtime_kind_from_config(value) {
            Some(kind) => runtime_profile_from_kind(config, kind),
            None => {
                tracing::warn!(
                    runtime_kind = value,
                    "workflow runtime command dispatcher: unknown runtime_kind, inheriting the configured default runtime profile"
                );
                inherited_profile.clone()
            }
        },
        None => inherited_profile.clone(),
    };
    let kind = base_profile.kind;
    let profile_name = policy
        .runtime_profile
        .clone()
        .unwrap_or_else(|| base_profile.name.clone());
    let mut profile = RuntimeProfile::new(profile_name, kind);
    profile.model = policy.model.clone().or_else(|| base_profile.model.clone());
    profile.reasoning_effort = policy
        .reasoning_effort
        .clone()
        .or_else(|| base_profile.reasoning_effort.clone());
    profile.sandbox = policy
        .sandbox
        .clone()
        .or_else(|| base_profile.sandbox.clone());
    profile.approval_policy = runtime_kind_supports_approval_policy(kind)
        .then(|| {
            policy
                .approval_policy
                .clone()
                .or_else(|| base_profile.approval_policy.clone())
        })
        .flatten();
    profile.max_turns = policy.max_turns.or(base_profile.max_turns);
    profile.timeout_secs = policy.timeout_secs.or(base_profile.timeout_secs);
    Ok(profile)
}

#[derive(Clone)]
struct ResolvedRuntimeDispatchProfiles {
    default_profile: RuntimeProfile,
    workflow_profiles: std::collections::BTreeMap<String, RuntimeProfile>,
    activity_profiles: std::collections::BTreeMap<String, RuntimeProfile>,
    workflow_activity_profiles:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, RuntimeProfile>>,
}

fn resolve_runtime_dispatch_profiles(
    config: &harness_core::config::HarnessConfig,
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
    inherited_profile: &RuntimeProfile,
) -> anyhow::Result<ResolvedRuntimeDispatchProfiles> {
    let default_profile = runtime_dispatch_profile(config, policy, inherited_profile)?;
    let workflow_profiles = policy
        .workflow_profiles
        .iter()
        .map(|(definition_id, override_policy)| {
            (
                definition_id.clone(),
                apply_runtime_dispatch_profile_override(config, &default_profile, override_policy),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let activity_profiles = policy
        .activity_profiles
        .iter()
        .map(|(activity, override_policy)| {
            (
                activity.clone(),
                apply_runtime_dispatch_profile_override(config, &default_profile, override_policy),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut workflow_activity_profiles = std::collections::BTreeMap::new();
    for (definition_id, activity_overrides) in &policy.workflow_activity_profiles {
        for (activity, override_policy) in activity_overrides {
            let base_profile = activity_profiles
                .get(activity)
                .or_else(|| workflow_profiles.get(definition_id))
                .unwrap_or(&default_profile);
            let profile =
                apply_runtime_dispatch_profile_override(config, base_profile, override_policy);
            workflow_activity_profiles
                .entry(definition_id.clone())
                .or_insert_with(std::collections::BTreeMap::new)
                .insert(activity.clone(), profile);
        }
    }
    Ok(ResolvedRuntimeDispatchProfiles {
        default_profile,
        workflow_profiles,
        activity_profiles,
        workflow_activity_profiles,
    })
}

pub(super) fn runtime_dispatch_profile_selector(
    config: &harness_core::config::HarnessConfig,
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
    inherited_profile: &RuntimeProfile,
) -> anyhow::Result<RuntimeProfileSelector> {
    let resolved = resolve_runtime_dispatch_profiles(config, policy, inherited_profile)?;
    let default_profile = resolved.default_profile;
    let mut selector = RuntimeProfileSelector::new(default_profile);
    for (definition_id, profile) in &resolved.workflow_profiles {
        selector = selector.with_workflow_profile(definition_id.clone(), profile.clone());
    }
    for (activity, profile) in &resolved.activity_profiles {
        selector = selector.with_activity_profile(activity.clone(), profile.clone());
    }
    for (definition_id, activity_profiles) in &resolved.workflow_activity_profiles {
        for (activity, profile) in activity_profiles {
            selector = selector.with_workflow_activity_profile(
                definition_id.clone(),
                activity.clone(),
                profile.clone(),
            );
        }
    }
    Ok(selector)
}

pub(super) async fn persist_runtime_profile_manifest(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    config: &harness_core::config::HarnessConfig,
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
    inherited_profile: &RuntimeProfile,
) -> anyhow::Result<()> {
    let definition =
        runtime_profile_manifest_definition(project_root, config, policy, inherited_profile)?;
    store.upsert_definition(&definition).await
}

pub(in crate::http) fn runtime_profile_manifest_definition(
    project_root: &Path,
    config: &harness_core::config::HarnessConfig,
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
    inherited_profile: &RuntimeProfile,
) -> anyhow::Result<WorkflowDefinition> {
    let resolved = resolve_runtime_dispatch_profiles(config, policy, inherited_profile)?;
    let metadata = serde_json::json!({
        "artifact_type": "runtime_profile_manifest",
        "project_root": project_root.to_string_lossy(),
        "selection_precedence": [
            "workflow_activity_profiles",
            "activity_profiles",
            "workflow_profiles",
            "default_profile"
        ],
        "default_profile": resolved.default_profile,
        "workflow_profiles": resolved.workflow_profiles,
        "activity_profiles": resolved.activity_profiles,
        "workflow_activity_profiles": resolved.workflow_activity_profiles,
    });
    let metadata_json = serde_json::to_string(&metadata)?;
    let definition_digest = Sha256::digest(metadata_json.as_bytes());
    let project_digest = Sha256::digest(project_root.to_string_lossy().as_bytes());
    Ok(WorkflowDefinition::new(
        format!("runtime_profiles:{project_digest:x}"),
        1,
        "Runtime Profile Manifest",
    )
    .with_source_path(
        project_root
            .join("WORKFLOW.md")
            .to_string_lossy()
            .into_owned(),
    )
    .with_definition_hash(format!("{definition_digest:x}"))
    .with_metadata(metadata))
}

fn apply_runtime_dispatch_profile_override(
    config: &harness_core::config::HarnessConfig,
    default_profile: &RuntimeProfile,
    override_policy: &harness_core::config::workflow::RuntimeDispatchProfileOverride,
) -> RuntimeProfile {
    let runtime_kind_profile = override_policy
        .runtime_kind
        .as_deref()
        .and_then(runtime_kind_from_config)
        .map(|kind| {
            if kind == default_profile.kind {
                default_profile.clone()
            } else {
                runtime_profile_from_kind(config, kind)
            }
        })
        .unwrap_or_else(|| default_profile.clone());
    let kind = runtime_kind_profile.kind;
    let profile_name = override_policy
        .runtime_profile
        .clone()
        .unwrap_or_else(|| runtime_kind_profile.name.clone());
    let mut profile = RuntimeProfile::new(profile_name, kind);
    profile.model = override_policy
        .model
        .clone()
        .or_else(|| runtime_kind_profile.model.clone());
    profile.reasoning_effort = override_policy
        .reasoning_effort
        .clone()
        .or_else(|| runtime_kind_profile.reasoning_effort.clone());
    profile.sandbox = override_policy
        .sandbox
        .clone()
        .or_else(|| default_profile.sandbox.clone());
    profile.approval_policy =
        runtime_dispatch_approval_policy(&runtime_kind_profile, override_policy, kind);
    profile.max_turns = override_policy.max_turns.or(default_profile.max_turns);
    profile.timeout_secs = override_policy
        .timeout_secs
        .or(default_profile.timeout_secs);
    profile
}

fn runtime_dispatch_approval_policy(
    default_profile: &RuntimeProfile,
    override_policy: &harness_core::config::workflow::RuntimeDispatchProfileOverride,
    kind: RuntimeKind,
) -> Option<String> {
    runtime_kind_supports_approval_policy(kind)
        .then(|| {
            override_policy
                .approval_policy
                .clone()
                .or_else(|| default_profile.approval_policy.clone())
        })
        .flatten()
}

fn runtime_kind_supports_approval_policy(kind: RuntimeKind) -> bool {
    matches!(kind, RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_agent_policy_preserves_configured_codex_surface() -> anyhow::Result<()> {
        let base = RuntimeProfile::new("codex-exec-review", RuntimeKind::CodexExec);
        let policy = crate::workflow_runtime_submission::runtime_models::PromptExecutionPolicy {
            agent: Some("codex".to_string()),
            turn_timeout_secs: Some(45),
            ..Default::default()
        };

        let profile = runtime_profile_with_prompt_execution_policy(
            &harness_core::config::HarnessConfig::default(),
            &base,
            &policy,
        )?;
        assert_eq!(profile.kind, RuntimeKind::CodexExec);
        assert_eq!(profile.name, "codex-exec-review");
        assert_eq!(profile.timeout_secs, Some(45));
        Ok(())
    }
}
