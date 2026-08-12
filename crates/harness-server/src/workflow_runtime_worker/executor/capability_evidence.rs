use super::super::runtime_profile::ToolAllowlistEnforcement;
use harness_core::config::agents::{AgentPermissionMode, SandboxMode};
use harness_core::stack::capability_evidence::{
    AgentStackCapabilityEvidence, AgentStackCapabilityScope,
};
use harness_core::stack::{
    AgentStackCapability, AgentStackComponent, AgentStackComponentKind, AgentStackFreshness,
    AgentStackObservationClass, AgentStackSelectionState, AgentStackSource, AgentStackSourceScope,
    AgentStackTrustLevel,
};
use harness_core::types::Item;
use harness_workflow::runtime::{ActivityArtifact, RuntimeKind};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use thiserror::Error;

const ARTIFACT_TYPE: &str = "agent_capability_evidence";
const SCHEMA_VERSION: &str = "harness.runtime.agent_capability_evidence/v0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapabilityEvidencePolicy {
    BestEffort,
    RequireObservedSensitiveCapability,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum CapabilityEvidenceError {
    #[error("runtime capability evidence could not be built: {0}")]
    BuildFailed(String),
    #[error("runtime capability evidence observation surface is unavailable: {0}")]
    ObservationSurfaceUnavailable(String),
    #[error("runtime capability evidence has no observed sensitive capability")]
    SensitiveCapabilityAbsent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ObservationStatus {
    Observed,
    Absent,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MissingEvidenceStatus {
    Absent,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PolicyMode {
    BestEffort,
    RequireObservedSensitiveCapability,
}

impl From<CapabilityEvidencePolicy> for PolicyMode {
    fn from(policy: CapabilityEvidencePolicy) -> Self {
        match policy {
            CapabilityEvidencePolicy::BestEffort => Self::BestEffort,
            CapabilityEvidencePolicy::RequireObservedSensitiveCapability => {
                Self::RequireObservedSensitiveCapability
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct MissingEvidence {
    surface: &'static str,
    status: MissingEvidenceStatus,
    reason: String,
}

#[derive(Debug, Clone)]
struct ObservedCapability {
    capability: AgentStackCapability,
    scope: AgentStackCapabilityScope,
    surface: &'static str,
    tool_name: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct AggregatedObservation {
    capability: AgentStackCapability,
    scope: AgentStackCapabilityScope,
    event_count: u32,
    surfaces: Vec<&'static str>,
    tool_names: Vec<&'static str>,
    evidence_index: usize,
    linked_grant_indices: Vec<usize>,
    redaction: ObservationRedaction,
}

#[derive(Debug, Clone, Serialize)]
struct ObservationRedaction {
    command_arguments_omitted: bool,
    tool_inputs_omitted: bool,
    tool_outputs_omitted: bool,
}

impl ObservationRedaction {
    const fn transcript_metadata_only() -> Self {
        Self {
            command_arguments_omitted: true,
            tool_inputs_omitted: true,
            tool_outputs_omitted: true,
        }
    }
}

pub(super) struct RuntimeCapabilityEvidenceInput<'a> {
    pub(super) runtime_job_id: &'a str,
    pub(super) activity: &'a str,
    pub(super) attempt: u32,
    pub(super) runtime_kind: RuntimeKind,
    pub(super) sandbox_mode: SandboxMode,
    pub(super) permission_mode: AgentPermissionMode,
    pub(super) allowed_tools: Option<&'a [String]>,
    pub(super) tool_allowlist_enforcement: ToolAllowlistEnforcement,
    pub(super) correction_only: bool,
    pub(super) project_root: &'a Path,
    pub(super) items: &'a [Item],
    pub(super) observed_at: chrono::DateTime<chrono::Utc>,
    pub(super) policy: CapabilityEvidencePolicy,
}

pub(super) fn capability_evidence_artifact(
    input: RuntimeCapabilityEvidenceInput<'_>,
) -> Result<ActivityArtifact, CapabilityEvidenceError> {
    let unsupported_surfaces = unsupported_observation_surfaces(input.runtime_kind);
    if input.policy == CapabilityEvidencePolicy::RequireObservedSensitiveCapability
        && !unsupported_surfaces.is_empty()
    {
        return Err(CapabilityEvidenceError::ObservationSurfaceUnavailable(
            unsupported_surfaces.join(", "),
        ));
    }

    let component = runtime_component(input.runtime_kind)
        .map_err(|error| CapabilityEvidenceError::BuildFailed(error.to_string()))?;
    let grant_source = runtime_source()
        .map_err(|error| CapabilityEvidenceError::BuildFailed(error.to_string()))?;
    let runner_source =
        runner_source().map_err(|error| CapabilityEvidenceError::BuildFailed(error.to_string()))?;
    let grants = AgentStackCapabilityEvidence::granted_by_sandbox_mode(
        &component,
        input.sandbox_mode,
        input.project_root,
        None,
        grant_source,
        input.observed_at,
    )
    .map_err(|error| CapabilityEvidenceError::BuildFailed(error.to_string()))?;
    let observed_events = observed_capabilities(input.items, input.project_root);
    if input.policy == CapabilityEvidencePolicy::RequireObservedSensitiveCapability
        && observed_events.is_empty()
    {
        return Err(CapabilityEvidenceError::SensitiveCapabilityAbsent);
    }

    let mut observations = Vec::new();
    let mut aggregated = Vec::new();
    for event in observed_events {
        let Some(position) = aggregation_position(&aggregated, event.capability, &event.scope)
        else {
            let evidence_index = observations.len();
            let observation = AgentStackCapabilityEvidence::observed(
                &component,
                event.capability,
                runner_source.clone(),
                input.observed_at,
                AgentStackTrustLevel::RunnerObserved,
                event.scope.clone(),
            )
            .map_err(|error| CapabilityEvidenceError::BuildFailed(error.to_string()))?;
            observations.push(observation);
            let linked_grant_indices =
                linked_grant_indices(&grants, event.capability, &event.scope);
            aggregated.push(AggregatedObservation {
                capability: event.capability,
                scope: event.scope,
                event_count: 1,
                surfaces: vec![event.surface],
                tool_names: vec![event.tool_name],
                evidence_index,
                linked_grant_indices,
                redaction: ObservationRedaction::transcript_metadata_only(),
            });
            continue;
        };
        let observation = &mut aggregated[position];
        observation.event_count += 1;
        push_unique(&mut observation.surfaces, event.surface);
        push_unique(&mut observation.tool_names, event.tool_name);
    }

    let mut missing_evidence = Vec::new();
    for surface in unsupported_surfaces {
        missing_evidence.push(MissingEvidence {
            surface,
            status: MissingEvidenceStatus::Unsupported,
            reason: format!(
                "{} runtime does not expose this observation surface to the server worker",
                input.runtime_kind.as_str()
            ),
        });
    }
    if aggregated.is_empty() {
        missing_evidence.push(MissingEvidence {
            surface: "sensitive_tool_invocation",
            status: MissingEvidenceStatus::Absent,
            reason: "no sensitive tool class was observed in transcript-safe item metadata"
                .to_string(),
        });
    }

    let observation_status = if !unsupported_observation_surfaces(input.runtime_kind).is_empty() {
        ObservationStatus::Unsupported
    } else if aggregated.is_empty() {
        ObservationStatus::Absent
    } else {
        ObservationStatus::Observed
    };

    Ok(ActivityArtifact::new(
        ARTIFACT_TYPE,
        json!({
            "schema_version": SCHEMA_VERSION,
            "runtime_job_id": input.runtime_job_id,
            "activity": input.activity,
            "attempt": input.attempt,
            "runtime_kind": input.runtime_kind,
            "observed_at": input.observed_at,
            "observation_status": observation_status,
            "policy": {
                "mode": PolicyMode::from(input.policy),
                "outcome": "recorded",
            },
            "launch_permissions": {
                "sandbox_mode": input.sandbox_mode,
                "permission_mode": input.permission_mode,
                "allowed_tools": input.allowed_tools,
                "tool_allowlist_enforcement": input.tool_allowlist_enforcement,
                "correction_only": input.correction_only,
            },
            "grant_linkage": {
                "source": "sandbox_mode",
                "grants_available": true,
            },
            "grants": grants,
            "observations": observations,
            "observed_capabilities": aggregated,
            "missing_evidence": missing_evidence,
            "redaction": {
                "raw_commands": "omitted",
                "tool_inputs": "omitted",
                "tool_outputs": "omitted",
            },
        }),
    ))
}

fn runtime_component(runtime_kind: RuntimeKind) -> anyhow::Result<AgentStackComponent> {
    let source = AgentStackSource::logical(
        AgentStackSourceScope::System,
        "harness",
        &format!("agent_runtime/{}", runtime_kind.as_str()),
    )?;
    Ok(AgentStackComponent::new(
        AgentStackComponentKind::AgentRuntime,
        source,
        AgentStackObservationClass::RuntimeObserved,
        AgentStackSelectionState::Loaded,
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackFreshness::Fresh,
    )?)
}

fn runtime_source() -> anyhow::Result<AgentStackSource> {
    Ok(AgentStackSource::logical(
        AgentStackSourceScope::Runtime,
        "workflow_runtime",
        "job/capability_evidence",
    )?)
}

fn runner_source() -> anyhow::Result<AgentStackSource> {
    Ok(AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "workflow_runtime_worker",
        "transcript/capability_evidence",
    )?)
}

fn unsupported_observation_surfaces(runtime_kind: RuntimeKind) -> Vec<&'static str> {
    match runtime_kind {
        RuntimeKind::AnthropicApi => vec!["agent_tool_invocation_stream"],
        RuntimeKind::RemoteHost => vec!["server_local_transcript"],
        RuntimeKind::CodexExec
        | RuntimeKind::CodexJsonrpc
        | RuntimeKind::ClaudeCode
        | RuntimeKind::OpenCode => Vec::new(),
    }
}

fn observed_capabilities(items: &[Item], project_root: &Path) -> Vec<ObservedCapability> {
    items
        .iter()
        .filter_map(|item| observed_capability(item, project_root))
        .collect()
}

fn observed_capability(item: &Item, project_root: &Path) -> Option<ObservedCapability> {
    match item {
        Item::ShellCommand { .. } => Some(ObservedCapability {
            capability: AgentStackCapability::Shell,
            scope: AgentStackCapabilityScope::Host,
            surface: "shell_command",
            tool_name: "shell",
        }),
        Item::FileEdit { path, .. } => Some(ObservedCapability {
            capability: AgentStackCapability::FileWrite,
            scope: path_scope(project_root, path).unwrap_or(AgentStackCapabilityScope::Host),
            surface: "file_edit",
            tool_name: "file_edit",
        }),
        Item::FileRead { path, .. } => Some(ObservedCapability {
            capability: AgentStackCapability::SecretRead,
            scope: path_scope(project_root, path).unwrap_or(AgentStackCapabilityScope::Host),
            surface: "file_read",
            tool_name: "file_read",
        }),
        Item::ToolCall { name, .. } => tool_call_capability(name),
        Item::UserMessage { .. }
        | Item::AgentReasoning { .. }
        | Item::ApprovalRequest { .. }
        | Item::Error { .. } => None,
    }
}

fn tool_call_capability(name: &str) -> Option<ObservedCapability> {
    let normalized = name.to_ascii_lowercase();
    let (capability, scope) = if matches!(
        normalized.as_str(),
        "bash" | "shell" | "exec" | "command" | "run_command"
    ) || normalized.contains("bash")
        || normalized.contains("shell")
        || normalized.contains("exec_command")
    {
        (AgentStackCapability::Shell, AgentStackCapabilityScope::Host)
    } else if matches!(
        normalized.as_str(),
        "edit" | "write" | "multiedit" | "apply_patch" | "file_write"
    ) || normalized.contains("apply_patch")
        || normalized.contains("file_write")
        || normalized.contains("write")
        || normalized.contains("edit")
    {
        (
            AgentStackCapability::FileWrite,
            AgentStackCapabilityScope::Host,
        )
    } else if matches!(
        normalized.as_str(),
        "read" | "grep" | "glob" | "file_read" | "view_image"
    ) || normalized.contains("file_read")
        || normalized.contains("read")
        || normalized.contains("grep")
        || normalized.contains("glob")
    {
        (
            AgentStackCapability::SecretRead,
            AgentStackCapabilityScope::Host,
        )
    } else if matches!(
        normalized.as_str(),
        "webfetch" | "websearch" | "fetch" | "search" | "web.run"
    ) || normalized.contains("web")
        || normalized.contains("fetch")
        || normalized.contains("search")
        || normalized.contains("http")
    {
        (
            AgentStackCapability::Network,
            AgentStackCapabilityScope::network(None::<String>)
                .expect("empty network observation scope is valid"),
        )
    } else {
        return None;
    };

    Some(ObservedCapability {
        capability,
        scope,
        surface: "tool_call",
        tool_name: stable_tool_name(name),
    })
}

fn stable_tool_name(name: &str) -> &'static str {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("bash")
        || normalized.contains("shell")
        || normalized.contains("exec")
        || normalized == "command"
    {
        "shell"
    } else if normalized.contains("apply_patch")
        || normalized.contains("write")
        || normalized.contains("edit")
    {
        "file_write"
    } else if normalized.contains("read")
        || normalized.contains("grep")
        || normalized.contains("glob")
    {
        "file_read"
    } else if normalized.contains("web")
        || normalized.contains("fetch")
        || normalized.contains("search")
        || normalized.contains("http")
    {
        "network"
    } else {
        "tool"
    }
}

fn path_scope(project_root: &Path, path: &Path) -> Option<AgentStackCapabilityScope> {
    let absolute = if path.is_absolute() {
        PathBuf::from(path)
    } else {
        project_root.join(path)
    };
    AgentStackCapabilityScope::path(&absolute).ok()
}

fn aggregation_position(
    observations: &[AggregatedObservation],
    capability: AgentStackCapability,
    scope: &AgentStackCapabilityScope,
) -> Option<usize> {
    observations
        .iter()
        .position(|observation| observation.capability == capability && &observation.scope == scope)
}

fn push_unique<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn linked_grant_indices(
    grants: &[AgentStackCapabilityEvidence],
    capability: AgentStackCapability,
    scope: &AgentStackCapabilityScope,
) -> Vec<usize> {
    grants
        .iter()
        .enumerate()
        .filter_map(|(index, grant)| {
            (grant.capability() == capability
                && grant_scope_covers_observation(grant.scope(), scope))
            .then_some(index)
        })
        .collect()
}

fn grant_scope_covers_observation(
    grant_scope: &AgentStackCapabilityScope,
    observed_scope: &AgentStackCapabilityScope,
) -> bool {
    match (grant_scope, observed_scope) {
        (AgentStackCapabilityScope::Host, _) => true,
        (left, right) if left == right => true,
        (
            AgentStackCapabilityScope::Path { path: grant_path },
            AgentStackCapabilityScope::Path {
                path: observed_path,
            },
        ) => Path::new(observed_path).starts_with(Path::new(grant_path)),
        (
            AgentStackCapabilityScope::Network { endpoint: None },
            AgentStackCapabilityScope::Network { .. },
        ) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use serde_json::Value;

    fn observed_at() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 8, 12, 20, 0, 0).unwrap()
    }

    fn input<'a>(
        runtime_kind: RuntimeKind,
        sandbox_mode: SandboxMode,
        project_root: &'a Path,
        items: &'a [Item],
    ) -> RuntimeCapabilityEvidenceInput<'a> {
        RuntimeCapabilityEvidenceInput {
            runtime_job_id: "job-1",
            activity: "implement_issue",
            attempt: 1,
            runtime_kind,
            sandbox_mode,
            permission_mode: AgentPermissionMode::Scoped,
            allowed_tools: Some(&[]),
            tool_allowlist_enforcement: ToolAllowlistEnforcement::NotEnforcedByHarness,
            correction_only: false,
            project_root,
            items,
            observed_at: observed_at(),
            policy: CapabilityEvidencePolicy::BestEffort,
        }
    }

    fn artifact_json(input: RuntimeCapabilityEvidenceInput<'_>) -> Value {
        capability_evidence_artifact(input).unwrap().artifact
    }

    #[test]
    fn observed_shell_and_file_edits_record_capability_classes_and_grant_links() {
        let project_root = Path::new("/tmp/harness-project");
        let items = [
            Item::ShellCommand {
                command: "cargo test -- --secret-token=abc".to_string(),
                exit_code: Some(0),
                stdout: "ok".to_string(),
                stderr: String::new(),
            },
            Item::FileEdit {
                path: PathBuf::from("src/lib.rs"),
                before: "old".to_string(),
                after: "new".to_string(),
            },
        ];

        let artifact = artifact_json(input(
            RuntimeKind::CodexJsonrpc,
            SandboxMode::WorkspaceWrite,
            project_root,
            &items,
        ));

        assert_eq!(artifact["observation_status"], "observed");
        assert_eq!(
            artifact["observed_capabilities"].as_array().unwrap().len(),
            2
        );
        assert!(artifact["observed_capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["capability"] == "shell"
                && !item["linked_grant_indices"].as_array().unwrap().is_empty()));
        assert!(artifact["observed_capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["capability"] == "file_write"
                && !item["linked_grant_indices"].as_array().unwrap().is_empty()));
    }

    #[test]
    fn absent_sensitive_tools_are_recorded_as_missing_evidence_not_successful_observation() {
        let items = [Item::AgentReasoning {
            content: "done".to_string(),
        }];

        let artifact = artifact_json(input(
            RuntimeKind::CodexJsonrpc,
            SandboxMode::ReadOnly,
            Path::new("/tmp/harness-project"),
            &items,
        ));

        assert_eq!(artifact["observation_status"], "absent");
        assert_eq!(artifact["observed_capabilities"], json!([]));
        assert_eq!(
            artifact["missing_evidence"][0]["surface"],
            "sensitive_tool_invocation"
        );
        assert_eq!(artifact["missing_evidence"][0]["status"], "absent");
    }

    #[test]
    fn unsupported_runtime_surfaces_are_explicit_missing_evidence() {
        let artifact = artifact_json(input(
            RuntimeKind::AnthropicApi,
            SandboxMode::ReadOnly,
            Path::new("/tmp/harness-project"),
            &[],
        ));

        assert_eq!(artifact["observation_status"], "unsupported");
        assert_eq!(
            artifact["missing_evidence"][0]["surface"],
            "agent_tool_invocation_stream"
        );
        assert_eq!(artifact["missing_evidence"][0]["status"], "unsupported");
    }

    #[test]
    fn strict_policy_fails_closed_when_required_surface_is_unsupported() {
        let mut strict = input(
            RuntimeKind::AnthropicApi,
            SandboxMode::ReadOnly,
            Path::new("/tmp/harness-project"),
            &[],
        );
        strict.policy = CapabilityEvidencePolicy::RequireObservedSensitiveCapability;

        assert_eq!(
            capability_evidence_artifact(strict).unwrap_err(),
            CapabilityEvidenceError::ObservationSurfaceUnavailable(
                "agent_tool_invocation_stream".to_string()
            )
        );
    }

    #[test]
    fn strict_policy_fails_closed_when_sensitive_activity_is_absent() {
        let items = [Item::AgentReasoning {
            content: "no tools".to_string(),
        }];
        let mut strict = input(
            RuntimeKind::CodexJsonrpc,
            SandboxMode::ReadOnly,
            Path::new("/tmp/harness-project"),
            &items,
        );
        strict.policy = CapabilityEvidencePolicy::RequireObservedSensitiveCapability;

        assert_eq!(
            capability_evidence_artifact(strict).unwrap_err(),
            CapabilityEvidenceError::SensitiveCapabilityAbsent
        );
    }

    #[test]
    fn raw_commands_inputs_and_outputs_are_redacted_from_evidence_artifact() {
        let items = [
            Item::ShellCommand {
                command: "deploy --token super-secret-token".to_string(),
                exit_code: Some(0),
                stdout: "secret stdout".to_string(),
                stderr: "secret stderr".to_string(),
            },
            Item::ToolCall {
                name: "Bash".to_string(),
                input: json!({ "cmd": "cat /run/secrets/token" }),
                output: Some(json!({ "token": "super-secret-token" })),
            },
        ];

        let artifact = artifact_json(input(
            RuntimeKind::ClaudeCode,
            SandboxMode::DangerFullAccess,
            Path::new("/tmp/harness-project"),
            &items,
        ));
        let encoded = artifact.to_string();

        assert!(!encoded.contains("super-secret-token"));
        assert!(!encoded.contains("secret stdout"));
        assert!(!encoded.contains("secret stderr"));
        assert!(!encoded.contains("/run/secrets/token"));
        assert_eq!(artifact["redaction"]["raw_commands"], "omitted");
        assert_eq!(artifact["redaction"]["tool_inputs"], "omitted");
        assert_eq!(artifact["redaction"]["tool_outputs"], "omitted");
    }
}
