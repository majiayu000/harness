use super::*;
use crate::stack::{
    AgentStackComponentKind, AgentStackFreshness, AgentStackObservationClass,
    AgentStackSelectionState,
};
use chrono::TimeZone;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

fn component() -> AgentStackComponent {
    let source = AgentStackSource::logical(
        AgentStackSourceScope::System,
        "harness",
        "agent_runtime/codex",
    )
    .unwrap();
    AgentStackComponent::new(
        AgentStackComponentKind::AgentRuntime,
        source,
        AgentStackObservationClass::RuntimeObserved,
        AgentStackSelectionState::Loaded,
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackFreshness::Fresh,
    )
    .unwrap()
}

fn repository_source() -> AgentStackSource {
    AgentStackSource::new(AgentStackSourceScope::Repository, "AGENTS.md").unwrap()
}

fn runtime_source() -> AgentStackSource {
    AgentStackSource::logical(
        AgentStackSourceScope::Runtime,
        "workflow_runtime",
        "job/capability_evidence",
    )
    .unwrap()
}

fn runner_source() -> AgentStackSource {
    AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "codex",
        "sandbox/capability_evidence",
    )
    .unwrap()
}

fn observed_at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap()
}

#[test]
fn capability_evidence_defines_complete_initial_vocabulary() {
    let defined = AGENT_STACK_CAPABILITY_DEFINITIONS
        .iter()
        .map(|definition| definition.capability)
        .collect::<Vec<_>>();
    assert_eq!(defined, AgentStackCapability::ALL);
    for capability in AgentStackCapability::ALL {
        assert!(!capability.definition().is_empty());
    }
}

#[test]
fn capability_evidence_classes_round_trip_in_closed_wire_order() {
    let actual = AgentStackCapabilityEvidenceClass::ALL
        .iter()
        .map(|class| {
            serde_json::to_value(class)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, ["declared", "granted", "observed"]);
    assert!(serde_json::from_str::<AgentStackCapabilityEvidenceClass>("\"runtime\"").is_err());
}

#[test]
fn declared_granted_and_observed_evidence_remain_distinct() {
    let component = component();
    let declared = AgentStackCapabilityEvidence::declared(
        &component,
        AgentStackCapability::Shell,
        repository_source(),
        None,
        AgentStackTrustLevel::RepositoryObserved,
        AgentStackCapabilityScope::Component,
    )
    .unwrap();
    let granted = AgentStackCapabilityEvidence::granted(
        &component,
        AgentStackCapability::Network,
        runtime_source(),
        observed_at(),
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackCapabilityScope::network(None::<String>).unwrap(),
    )
    .unwrap();
    let observed = AgentStackCapabilityEvidence::observed(
        &component,
        AgentStackCapability::FileWrite,
        runner_source(),
        observed_at(),
        AgentStackTrustLevel::RunnerObserved,
        AgentStackCapabilityScope::path(Path::new("/tmp/harness-output")).unwrap(),
    )
    .unwrap();

    assert_eq!(
        declared.evidence_class(),
        AgentStackCapabilityEvidenceClass::Declared
    );
    assert_eq!(
        granted.evidence_class(),
        AgentStackCapabilityEvidenceClass::Granted
    );
    assert_eq!(
        observed.evidence_class(),
        AgentStackCapabilityEvidenceClass::Observed
    );
    assert_eq!(declared.observed_at(), None);
    assert!(granted.observed_at().is_some());
    assert!(observed.observed_at().is_some());
}

#[test]
fn capability_evidence_json_round_trips_through_validation() {
    let component = component();
    let evidence = AgentStackCapabilityEvidence::observed(
        &component,
        AgentStackCapability::ProductionWrite,
        runner_source(),
        observed_at(),
        AgentStackTrustLevel::RunnerObserved,
        AgentStackCapabilityScope::Host,
    )
    .unwrap();

    let encoded = serde_json::to_string(&evidence).unwrap();
    let decoded = AgentStackCapabilityEvidence::from_json(&encoded).unwrap();

    assert_eq!(decoded, evidence);
    assert_eq!(
        decoded.schema_version(),
        AGENT_STACK_CAPABILITY_EVIDENCE_SCHEMA_VERSION
    );
}

#[test]
fn granted_evidence_requires_runtime_source_time_and_trust() {
    let component = component();
    let missing_time = AgentStackCapabilityEvidence::new(
        AgentStackCapabilityEvidenceClass::Granted,
        AgentStackCapability::Network,
        component.component_id().clone(),
        runtime_source(),
        None,
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackCapabilityScope::Host,
    )
    .unwrap_err();
    assert_eq!(
        missing_time,
        AgentStackCapabilityEvidenceError::MissingEvidenceTime
    );

    let repository_source = AgentStackCapabilityEvidence::granted(
        &component,
        AgentStackCapability::Network,
        repository_source(),
        observed_at(),
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackCapabilityScope::Host,
    )
    .unwrap_err();
    assert_eq!(
        repository_source,
        AgentStackCapabilityEvidenceError::InvalidEvidenceSource
    );

    let weak_trust = AgentStackCapabilityEvidence::granted(
        &component,
        AgentStackCapability::Network,
        runtime_source(),
        observed_at(),
        AgentStackTrustLevel::SelfDeclared,
        AgentStackCapabilityScope::Host,
    )
    .unwrap_err();
    assert_eq!(
        weak_trust,
        AgentStackCapabilityEvidenceError::TrustNotSupported
    );
}

#[test]
fn declared_evidence_rejects_runtime_observation_source() {
    let component = component();
    let error = AgentStackCapabilityEvidence::declared(
        &component,
        AgentStackCapability::Shell,
        runtime_source(),
        None,
        AgentStackTrustLevel::SelfDeclared,
        AgentStackCapabilityScope::Component,
    )
    .unwrap_err();
    assert_eq!(
        error,
        AgentStackCapabilityEvidenceError::InvalidEvidenceSource
    );
}

#[test]
fn observed_evidence_rejects_repository_source_and_weak_trust() {
    let component = component();
    let repository_source = AgentStackCapabilityEvidence::observed(
        &component,
        AgentStackCapability::Shell,
        repository_source(),
        observed_at(),
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackCapabilityScope::Component,
    )
    .unwrap_err();
    assert_eq!(
        repository_source,
        AgentStackCapabilityEvidenceError::InvalidEvidenceSource
    );

    let weak_trust = AgentStackCapabilityEvidence::observed(
        &component,
        AgentStackCapability::Shell,
        runtime_source(),
        observed_at(),
        AgentStackTrustLevel::RepositoryObserved,
        AgentStackCapabilityScope::Component,
    )
    .unwrap_err();
    assert_eq!(
        weak_trust,
        AgentStackCapabilityEvidenceError::TrustNotSupported
    );
}

#[test]
fn capability_token_grants_file_write_path_evidence_only() {
    let component = component();
    let token = CapabilityToken {
        token_id: Uuid::nil(),
        subtask_index: 0,
        allowed_write_paths: vec![
            PathBuf::from("/tmp/harness-worktree-0"),
            PathBuf::from("/tmp/harness-worktree-1/src"),
        ],
        issued_at: SystemTime::UNIX_EPOCH,
        expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(60),
    };

    let evidence = AgentStackCapabilityEvidence::granted_by_capability_token(
        &component,
        &token,
        runtime_source(),
        observed_at(),
    )
    .unwrap();

    assert_eq!(evidence.len(), 2);
    assert!(evidence.iter().all(|item| {
        item.evidence_class() == AgentStackCapabilityEvidenceClass::Granted
            && item.capability() == AgentStackCapability::FileWrite
            && matches!(item.scope(), AgentStackCapabilityScope::Path { .. })
    }));
}

#[test]
fn sandbox_mode_grants_evidence_without_claiming_runtime_use() {
    let component = component();
    let evidence = AgentStackCapabilityEvidence::granted_by_sandbox_mode(
        &component,
        SandboxMode::WorkspaceWrite,
        Path::new("/tmp/harness-workspace"),
        runtime_source(),
        observed_at(),
    )
    .unwrap();

    let capabilities = evidence
        .iter()
        .map(AgentStackCapabilityEvidence::capability)
        .collect::<Vec<_>>();
    assert_eq!(
        capabilities,
        [
            AgentStackCapability::Network,
            AgentStackCapability::FileWrite
        ]
    );
    assert!(evidence
        .iter()
        .all(|item| item.evidence_class() == AgentStackCapabilityEvidenceClass::Granted));
}

#[test]
fn danger_full_access_sandbox_grants_host_boundary_evidence() {
    let component = component();
    let evidence = AgentStackCapabilityEvidence::granted_by_sandbox_mode(
        &component,
        SandboxMode::DangerFullAccess,
        Path::new("/tmp/harness-workspace"),
        runtime_source(),
        observed_at(),
    )
    .unwrap();

    let capabilities = evidence
        .iter()
        .map(AgentStackCapabilityEvidence::capability)
        .collect::<Vec<_>>();
    assert_eq!(
        capabilities,
        [
            AgentStackCapability::Destructive,
            AgentStackCapability::SecretRead,
            AgentStackCapability::Network,
            AgentStackCapability::Privileged,
            AgentStackCapability::FileWrite,
        ]
    );
    assert!(evidence
        .iter()
        .all(|item| matches!(item.scope(), AgentStackCapabilityScope::Host)));
}

#[test]
fn invalid_scope_and_schema_are_rejected() {
    let component = component();
    let invalid_path = AgentStackCapabilityEvidence::observed(
        &component,
        AgentStackCapability::FileWrite,
        runner_source(),
        observed_at(),
        AgentStackTrustLevel::RunnerObserved,
        AgentStackCapabilityScope::Path {
            path: "relative/path".to_string(),
        },
    )
    .unwrap_err();
    assert_eq!(
        invalid_path,
        AgentStackCapabilityEvidenceError::InvalidScope
    );

    let mut wire = serde_json::to_value(
        AgentStackCapabilityEvidence::declared(
            &component,
            AgentStackCapability::SecretRead,
            repository_source(),
            None,
            AgentStackTrustLevel::SelfDeclared,
            AgentStackCapabilityScope::Component,
        )
        .unwrap(),
    )
    .unwrap();
    wire["schema_version"] = json!("agent-stack-capability-evidence/v9");
    assert!(matches!(
        AgentStackCapabilityEvidence::from_json(&wire.to_string()),
        Err(AgentStackCapabilityEvidenceParseError::Validation(
            AgentStackCapabilityEvidenceError::UnsupportedSchemaVersion
        ))
    ));
}
