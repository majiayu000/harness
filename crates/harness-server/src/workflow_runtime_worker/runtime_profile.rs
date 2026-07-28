use anyhow::Context;
use harness_core::config::agents::{AgentsConfig, SandboxMode};
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

/// Final approval policy shared by provenance and agent launch.
///
/// When a Codex profile omits `approval_policy`, the Codex CLI resolves the
/// effective policy from configuration Harness does not observe, so the
/// resolved settings record an explicit unobserved marker instead of a
/// fabricated final value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub(super) enum ResolvedApprovalPolicy {
    Explicit { value: String },
    UnobservedAgentDefault,
}

impl ResolvedApprovalPolicy {
    pub(super) fn explicit_value(&self) -> Option<&str> {
        match self {
            Self::Explicit { value } => Some(value.as_str()),
            Self::UnobservedAgentDefault => None,
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
    let approval_policy = match runtime_profile_approval_policy(profile, runtime_kind)? {
        Some(value) => ResolvedApprovalPolicy::Explicit { value },
        None => ResolvedApprovalPolicy::UnobservedAgentDefault,
    };
    Ok(ResolvedRuntimeSettings {
        profile_name: profile.name.clone(),
        runtime_kind,
        execution_phase,
        model: resolve_model(profile, runtime_kind, execution_phase, agents)?,
        reasoning_effort: resolve_reasoning_effort(profile, runtime_kind, execution_phase, agents),
        sandbox_mode,
        approval_policy,
        max_turns: profile.max_turns,
        timeout_secs,
        stall_timeout_secs: normalize_stall_timeout_secs(
            concurrency.stall_timeout_secs,
            Some(timeout_secs),
        )
        .effective_secs,
    })
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
        // effort value is recorded for it.
        RuntimeKind::AnthropicApi | RuntimeKind::RemoteHost => None,
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
        let mut profile = RuntimeProfile::new("claude-default", RuntimeKind::ClaudeCode);
        profile.approval_policy = Some("on-request".to_string());

        let error = runtime_profile_approval_policy(&profile, RuntimeKind::ClaudeCode)
            .expect_err("Claude approval policy should fail until it has a contract");

        assert!(error
            .to_string()
            .contains("only supported for Codex runtime kinds"));
    }
}
