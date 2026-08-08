use anyhow::Context;
use harness_core::config::agents::{
    AgentPermissionMode, AgentsConfig, CapabilityProfile, SandboxMode,
};
use harness_core::config::concurrency::ConcurrencyConfig;
use harness_core::config::stall_timeout::normalize_stall_timeout_secs;
use harness_core::types::ExecutionPhase;
use harness_workflow::runtime::{RuntimeJob, RuntimeKind, RuntimeProfile};
use serde::Serialize;

pub(super) fn agent_name_for_runtime_kind(kind: RuntimeKind) -> anyhow::Result<&'static str> {
    match kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => Ok("codex"),
        RuntimeKind::ClaudeCode => Ok("claude"),
        RuntimeKind::AnthropicApi => Ok("anthropic-api"),
        RuntimeKind::OpenCode => Ok("opencode"),
        RuntimeKind::RemoteHost => {
            anyhow::bail!("remote_host runtime jobs must be claimed by an external runtime host")
        }
    }
}

pub(super) fn runtime_profile_for_job(job: &RuntimeJob) -> anyhow::Result<RuntimeProfile> {
    let Some(value) = job.input.get("runtime_profile") else {
        return Ok(RuntimeProfile::new(
            job.runtime_profile.clone(),
            job.runtime_kind,
        ));
    };
    serde_json::from_value(value.clone())
        .with_context(|| format!("runtime job {} has invalid runtime_profile input", job.id))
}

/// Typed rejection for a runtime profile whose timeout cannot drive a turn.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(super) enum RuntimeSettingsResolutionError {
    #[error("runtime profile `{profile}` timeout_secs must be a positive number of seconds")]
    ZeroTimeout { profile: String },
    #[error("runtime profile `{profile}` has no resolved timeout")]
    MissingTimeout { profile: String },
}

/// Final approval policy shared by provenance and agent launch (B-016).
///
/// When a Codex profile omits `approval_policy`, the Codex CLI resolves the
/// effective policy from configuration Harness does not observe, so the
/// resolved settings record an explicit unobserved marker instead of a
/// fabricated final value. Claude Code and Anthropic API have no
/// approval-policy setting at all, so an omitted policy is recorded as
/// not applicable — a distinct audit claim from unobserved, because no
/// approval-policy value participates in launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub(super) enum ResolvedApprovalPolicy {
    Explicit { value: String },
    UnobservedAgentDefault,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolAllowlistEnforcement {
    ClaudeCli,
    NotEnforcedByHarness,
}

impl ToolAllowlistEnforcement {
    fn for_runtime_kind(runtime_kind: RuntimeKind) -> Self {
        match runtime_kind {
            RuntimeKind::ClaudeCode => Self::ClaudeCli,
            RuntimeKind::CodexExec
            | RuntimeKind::CodexJsonrpc
            | RuntimeKind::AnthropicApi
            | RuntimeKind::RemoteHost
            | RuntimeKind::OpenCode => Self::NotEnforcedByHarness,
        }
    }
}

impl ResolvedApprovalPolicy {
    pub(super) fn explicit_value(&self) -> Option<&str> {
        match self {
            Self::Explicit { value } => Some(value.as_str()),
            Self::UnobservedAgentDefault | Self::NotApplicable => None,
        }
    }
}

/// Launch settings resolved exactly once before prompt packet construction.
///
/// The same value feeds context provenance and `TurnLifecycleOptions`, so the
/// recorded evidence cannot diverge from the executed launch configuration.
/// Field order is the canonical serialization order used for digests.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct ResolvedRuntimeSettings {
    pub(super) profile_name: String,
    pub(super) runtime_kind: RuntimeKind,
    pub(super) execution_phase: Option<ExecutionPhase>,
    pub(super) model: String,
    pub(super) reasoning_effort: Option<String>,
    pub(super) sandbox_mode: SandboxMode,
    pub(super) approval_policy: ResolvedApprovalPolicy,
    pub(super) capability_profile: CapabilityProfile,
    pub(super) permission_mode: AgentPermissionMode,
    pub(super) allowed_tools: Option<Vec<String>>,
    pub(super) tool_allowlist_enforcement: ToolAllowlistEnforcement,
    pub(super) max_turns: Option<u32>,
    pub(super) timeout_secs: u64,
    pub(super) stall_timeout_secs: u64,
}

/// Resolve the final launch settings for an already timeout-adjusted profile.
///
/// `max_turns` is copied from the effective profile already enforced by the
/// workflow runtime; it is recorded evidence, not a second enforcement point.
pub(super) fn resolve_runtime_settings(
    profile: &RuntimeProfile,
    runtime_kind: RuntimeKind,
    execution_phase: Option<ExecutionPhase>,
    agents: &AgentsConfig,
    concurrency: &ConcurrencyConfig,
) -> anyhow::Result<ResolvedRuntimeSettings> {
    let timeout_secs = match profile.timeout_secs {
        Some(0) => {
            return Err(RuntimeSettingsResolutionError::ZeroTimeout {
                profile: profile.name.clone(),
            }
            .into())
        }
        Some(timeout_secs) => timeout_secs,
        None => {
            return Err(RuntimeSettingsResolutionError::MissingTimeout {
                profile: profile.name.clone(),
            }
            .into())
        }
    };
    let sandbox_mode = runtime_profile_sandbox_mode(profile)?.unwrap_or(agents.sandbox_mode);
    let approval_policy = resolve_approval_policy(profile, runtime_kind)?;
    Ok(ResolvedRuntimeSettings {
        profile_name: profile.name.clone(),
        runtime_kind,
        execution_phase,
        model: resolve_model(profile, runtime_kind, execution_phase, agents)?,
        reasoning_effort: resolve_reasoning_effort(profile, runtime_kind, execution_phase, agents),
        sandbox_mode,
        approval_policy,
        capability_profile: agents.capability_profile,
        permission_mode: agents.resolve_permission_mode(),
        allowed_tools: resolve_activity_allowed_tools(agents, execution_phase),
        tool_allowlist_enforcement: ToolAllowlistEnforcement::for_runtime_kind(runtime_kind),
        max_turns: profile.max_turns,
        timeout_secs,
        stall_timeout_secs: normalize_stall_timeout_secs(
            concurrency.stall_timeout_secs,
            Some(timeout_secs),
        )
        .effective_secs,
    })
}

fn resolve_activity_allowed_tools(
    agents: &AgentsConfig,
    execution_phase: Option<ExecutionPhase>,
) -> Option<Vec<String>> {
    if agents.allowed_tools.is_some()
        || agents.resolve_permission_mode() == AgentPermissionMode::Full
        || agents.capability_profile == CapabilityProfile::ReadOnly
    {
        return agents.resolve_allowed_tools();
    }

    match execution_phase {
        Some(ExecutionPhase::Execution | ExecutionPhase::Rebase) => {
            CapabilityProfile::Standard.tools()
        }
        Some(
            ExecutionPhase::Planning
            | ExecutionPhase::Validation
            | ExecutionPhase::SimpleReview
            | ExecutionPhase::Triage,
        )
        | None => CapabilityProfile::ReadOnly.tools(),
    }
}

fn resolve_model(
    profile: &RuntimeProfile,
    runtime_kind: RuntimeKind,
    execution_phase: Option<ExecutionPhase>,
    agents: &AgentsConfig,
) -> anyhow::Result<String> {
    if let Some(model) = &profile.model {
        return Ok(model.clone());
    }
    match runtime_kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => {
            Ok(agents.codex.default_model.clone())
        }
        RuntimeKind::ClaudeCode => Ok(match (&agents.claude.reasoning_budget, execution_phase) {
            (Some(budget), Some(phase)) => budget.model_for_phase(phase).to_string(),
            _ => agents.claude.default_model.clone(),
        }),
        RuntimeKind::AnthropicApi => Ok(agents.anthropic_api.default_model.clone()),
        RuntimeKind::OpenCode => {
            let model = agents.opencode.default_model.clone();
            if model.is_empty() {
                // OpenCode resolves its own default model when none is
                // configured; record the agent name as the audit placeholder.
                Ok("opencode".to_string())
            } else {
                Ok(model)
            }
        }
        RuntimeKind::RemoteHost => {
            anyhow::bail!("remote_host runtime jobs are not resolved by this server")
        }
    }
}

fn resolve_reasoning_effort(
    profile: &RuntimeProfile,
    runtime_kind: RuntimeKind,
    execution_phase: Option<ExecutionPhase>,
    agents: &AgentsConfig,
) -> Option<String> {
    match runtime_kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => profile
            .reasoning_effort
            .clone()
            .or_else(|| Some(agents.codex.reasoning_effort.clone())),
        RuntimeKind::ClaudeCode => profile
            .reasoning_effort
            .clone()
            .or_else(|| execution_phase.map(|phase| phase.effort_level().to_string())),
        // The Anthropic API runtime has no reasoning-effort contract, so no
        // effort value is recorded for it. OpenCode's ACP v1 has no
        // reasoning-effort option either.
        RuntimeKind::AnthropicApi | RuntimeKind::OpenCode | RuntimeKind::RemoteHost => None,
    }
}

fn runtime_profile_sandbox_mode(profile: &RuntimeProfile) -> anyhow::Result<Option<SandboxMode>> {
    let Some(raw) = profile.sandbox.as_deref() else {
        return Ok(None);
    };
    let mode = match raw {
        "read-only" => SandboxMode::ReadOnly,
        "read-only-with-network" => SandboxMode::ReadOnlyWithNetwork,
        "workspace-write" => SandboxMode::WorkspaceWrite,
        "danger-full-access" => SandboxMode::DangerFullAccess,
        other => anyhow::bail!("runtime profile sandbox `{other}` is not supported"),
    };
    Ok(Some(mode))
}

/// Closed runtime-kind approval resolution (B-016).
///
/// An explicit policy is accepted only for Codex runtime kinds and rejected
/// with a typed error for every other kind — it is never silently discarded.
/// An omitted policy records `UnobservedAgentDefault` for Codex runtimes
/// (the effective value is resolved outside Harness) and `NotApplicable` for
/// runtimes without an approval-policy contract. `RemoteHost` with an omitted
/// policy resolves to `NotApplicable` here but never produces resolved
/// settings: model resolution rejects remote-host jobs locally.
fn resolve_approval_policy(
    profile: &RuntimeProfile,
    runtime_kind: RuntimeKind,
) -> anyhow::Result<ResolvedApprovalPolicy> {
    match runtime_profile_approval_policy(profile, runtime_kind)? {
        Some(value) => Ok(ResolvedApprovalPolicy::Explicit { value }),
        None => Ok(match runtime_kind {
            RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => {
                ResolvedApprovalPolicy::UnobservedAgentDefault
            }
            RuntimeKind::ClaudeCode
            | RuntimeKind::AnthropicApi
            | RuntimeKind::OpenCode
            | RuntimeKind::RemoteHost => ResolvedApprovalPolicy::NotApplicable,
        }),
    }
}

fn runtime_profile_approval_policy(
    profile: &RuntimeProfile,
    runtime_kind: RuntimeKind,
) -> anyhow::Result<Option<String>> {
    let Some(raw) = profile.approval_policy.as_deref() else {
        return Ok(None);
    };
    match raw {
        "untrusted" | "on-failure" | "on-request" | "never" => {}
        other => anyhow::bail!("runtime profile approval_policy `{other}` is not supported"),
    }
    match runtime_kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => Ok(Some(raw.to_string())),
        other => anyhow::bail!(
            "runtime profile approval_policy `{raw}` is only supported for Codex runtime kinds, not {}",
            other.as_str()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with_timeout(name: &str, kind: RuntimeKind) -> RuntimeProfile {
        let mut profile = RuntimeProfile::new(name, kind);
        profile.timeout_secs = Some(3600);
        profile
    }

    #[test]
    fn runtime_profile_approval_policy_accepts_codex_values() {
        for value in ["untrusted", "on-failure", "on-request", "never"] {
            let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexExec);
            profile.approval_policy = Some(value.to_string());

            assert_eq!(
                runtime_profile_approval_policy(&profile, RuntimeKind::CodexExec)
                    .expect("codex approval policy should be accepted"),
                Some(value.to_string())
            );
        }
    }

    #[test]
    fn resolved_settings_codex_defaults_preserve_explicit_overrides() {
        let agents = AgentsConfig {
            codex: harness_core::config::agents::CodexAgentConfig {
                default_model: "configured-model".to_string(),
                reasoning_effort: "configured-effort".to_string(),
                ..harness_core::config::agents::CodexAgentConfig::default()
            },
            ..AgentsConfig::default()
        };
        let concurrency = ConcurrencyConfig::default();
        let profile = profile_with_timeout("codex-default", RuntimeKind::CodexJsonrpc);

        let resolved = resolve_runtime_settings(
            &profile,
            RuntimeKind::CodexJsonrpc,
            None,
            &agents,
            &concurrency,
        )
        .expect("codex defaults should resolve");
        assert_eq!(resolved.model, "configured-model");
        assert_eq!(
            resolved.reasoning_effort.as_deref(),
            Some("configured-effort")
        );
        assert_eq!(resolved.capability_profile, CapabilityProfile::Standard);
        assert_eq!(resolved.permission_mode, AgentPermissionMode::Scoped);
        assert_eq!(
            resolved.tool_allowlist_enforcement,
            ToolAllowlistEnforcement::NotEnforcedByHarness
        );
        assert_eq!(resolved.allowed_tools, CapabilityProfile::ReadOnly.tools());

        let mut profile = profile;
        profile.model = Some("profile-model".to_string());
        profile.reasoning_effort = Some("profile-effort".to_string());
        let resolved = resolve_runtime_settings(
            &profile,
            RuntimeKind::CodexJsonrpc,
            None,
            &agents,
            &concurrency,
        )
        .expect("explicit overrides should resolve");
        assert_eq!(resolved.model, "profile-model");
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("profile-effort"));
    }

    #[test]
    fn scoped_defaults_derive_tools_from_the_activity_phase() {
        let agents = AgentsConfig::default();
        let profile = profile_with_timeout("claude-default", RuntimeKind::ClaudeCode);
        let concurrency = ConcurrencyConfig::default();

        for phase in [
            None,
            Some(ExecutionPhase::Planning),
            Some(ExecutionPhase::Validation),
            Some(ExecutionPhase::SimpleReview),
            Some(ExecutionPhase::Triage),
        ] {
            let resolved = resolve_runtime_settings(
                &profile,
                RuntimeKind::ClaudeCode,
                phase,
                &agents,
                &concurrency,
            )
            .expect("read-class activity settings should resolve");
            assert_eq!(resolved.allowed_tools, CapabilityProfile::ReadOnly.tools());
        }

        for phase in [ExecutionPhase::Execution, ExecutionPhase::Rebase] {
            let resolved = resolve_runtime_settings(
                &profile,
                RuntimeKind::ClaudeCode,
                Some(phase),
                &agents,
                &concurrency,
            )
            .expect("implementation-class activity settings should resolve");
            assert_eq!(resolved.allowed_tools, CapabilityProfile::Standard.tools());
        }
    }

    #[test]
    fn explicit_tool_and_full_profiles_override_activity_defaults() {
        let profile = profile_with_timeout("claude-default", RuntimeKind::ClaudeCode);
        let concurrency = ConcurrencyConfig::default();
        let explicit_tools = AgentsConfig {
            allowed_tools: Some(vec!["Read".to_string(), "Bash".to_string()]),
            ..AgentsConfig::default()
        };
        let resolved = resolve_runtime_settings(
            &profile,
            RuntimeKind::ClaudeCode,
            Some(ExecutionPhase::Planning),
            &explicit_tools,
            &concurrency,
        )
        .expect("explicit tools should resolve");
        assert_eq!(resolved.allowed_tools, explicit_tools.allowed_tools);

        let full = AgentsConfig {
            capability_profile: CapabilityProfile::Full,
            ..AgentsConfig::default()
        };
        let resolved = resolve_runtime_settings(
            &profile,
            RuntimeKind::ClaudeCode,
            Some(ExecutionPhase::Planning),
            &full,
            &concurrency,
        )
        .expect("explicit Full profile should resolve");
        assert_eq!(resolved.permission_mode, AgentPermissionMode::Full);
        assert!(resolved.allowed_tools.is_none());
    }

    #[test]
    fn resolved_settings_require_explicit_full_capability_profile() {
        let agents = AgentsConfig {
            capability_profile: CapabilityProfile::Full,
            ..AgentsConfig::default()
        };
        let profile = profile_with_timeout("claude-default", RuntimeKind::ClaudeCode);

        let resolved = resolve_runtime_settings(
            &profile,
            RuntimeKind::ClaudeCode,
            None,
            &agents,
            &ConcurrencyConfig::default(),
        )
        .expect("explicit Full profile should resolve");

        assert_eq!(resolved.capability_profile, CapabilityProfile::Full);
        assert_eq!(resolved.permission_mode, AgentPermissionMode::Full);
        assert!(resolved.allowed_tools.is_none());
        assert_eq!(
            resolved.tool_allowlist_enforcement,
            ToolAllowlistEnforcement::ClaudeCli
        );
    }

    #[test]
    fn runtime_profile_approval_policy_rejects_unknown_values() {
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexExec);
        profile.approval_policy = Some("always".to_string());

        let error = runtime_profile_approval_policy(&profile, RuntimeKind::CodexExec)
            .expect_err("unknown approval policy should fail");

        assert!(error
            .to_string()
            .contains("runtime profile approval_policy `always` is not supported"));
    }

    #[test]
    fn runtime_profile_approval_policy_rejects_non_codex_runtimes() {
        for kind in [
            RuntimeKind::ClaudeCode,
            RuntimeKind::AnthropicApi,
            RuntimeKind::RemoteHost,
        ] {
            let mut profile = RuntimeProfile::new("non-codex", kind);
            profile.approval_policy = Some("on-request".to_string());

            let error = runtime_profile_approval_policy(&profile, kind)
                .expect_err("explicit approval policy must be rejected for non-Codex runtimes");

            assert!(error
                .to_string()
                .contains("only supported for Codex runtime kinds"));
            assert!(
                error.to_string().contains(kind.as_str()),
                "the rejection must name the unsupported runtime kind: {error}"
            );
        }
    }

    #[test]
    fn approval_policy_resolution_matches_runtime_capability_matrix() {
        let agents = AgentsConfig::default();
        let concurrency = ConcurrencyConfig::default();

        // Omitted policy for Codex runtimes: an effective value may still be
        // selected outside Harness, so it is recorded as unobserved.
        for kind in [RuntimeKind::CodexExec, RuntimeKind::CodexJsonrpc] {
            let profile = profile_with_timeout("codex-default", kind);
            let resolved = resolve_runtime_settings(&profile, kind, None, &agents, &concurrency)
                .unwrap_or_else(|error| panic!("{kind:?} omitted policy should resolve: {error}"));
            assert_eq!(
                resolved.approval_policy,
                ResolvedApprovalPolicy::UnobservedAgentDefault
            );
            assert_eq!(resolved.approval_policy.explicit_value(), None);
            assert_eq!(
                serde_json::to_value(&resolved.approval_policy)
                    .expect("approval policy serializes"),
                serde_json::json!({ "resolution": "unobserved_agent_default" })
            );
        }

        // Omitted policy for runtimes without an approval-policy contract:
        // not applicable, a distinct audit claim from unobserved.
        for kind in [RuntimeKind::ClaudeCode, RuntimeKind::AnthropicApi] {
            let profile = profile_with_timeout("non-codex", kind);
            let resolved = resolve_runtime_settings(&profile, kind, None, &agents, &concurrency)
                .unwrap_or_else(|error| panic!("{kind:?} omitted policy should resolve: {error}"));
            assert_eq!(
                resolved.approval_policy,
                ResolvedApprovalPolicy::NotApplicable
            );
            assert_eq!(resolved.approval_policy.explicit_value(), None);
            assert_eq!(
                serde_json::to_value(&resolved.approval_policy)
                    .expect("approval policy serializes"),
                serde_json::json!({ "resolution": "not_applicable" })
            );
        }

        // Remote Host remains rejected by local runtime-settings resolution;
        // it never produces resolved settings.
        let remote = profile_with_timeout("remote-host-default", RuntimeKind::RemoteHost);
        let error = resolve_runtime_settings(
            &remote,
            RuntimeKind::RemoteHost,
            None,
            &agents,
            &concurrency,
        )
        .expect_err("remote_host must be rejected by local settings resolution");
        assert!(error.to_string().contains("remote_host"));
    }
}
