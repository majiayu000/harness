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

fn system_time(value: DateTime<Utc>) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(value.timestamp().try_into().unwrap())
}

fn capability_token(
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    allowed_write_paths: Vec<PathBuf>,
) -> CapabilityToken {
    CapabilityToken {
        token_id: Uuid::nil(),
        subtask_index: 0,
        allowed_write_paths,
        issued_at: system_time(issued_at),
        expires_at: system_time(expires_at),
    }
}

fn assert_grants(
    actual: &[AgentStackCapabilityEvidence],
    expected: &[(AgentStackCapability, AgentStackCapabilityScope)],
) {
    assert_eq!(actual.len(), expected.len(), "{actual:#?}");
    for (capability, scope) in expected {
        assert!(
            actual
                .iter()
                .any(|item| item.capability() == *capability && item.scope() == scope),
            "missing {capability:?} at {scope:?} in {actual:#?}"
        );
    }
    assert!(actual.iter().all(|item| {
        item.evidence_class() == AgentStackCapabilityEvidenceClass::Granted
            && item.observed_at() == Some(&observed_at())
    }));
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
fn evidence_json_rejects_noncanonical_component_ids() {
    let component = component();
    let mut wire = serde_json::to_value(
        AgentStackCapabilityEvidence::granted(
            &component,
            AgentStackCapability::Shell,
            runtime_source(),
            observed_at(),
            AgentStackTrustLevel::RuntimeObserved,
            AgentStackCapabilityScope::Host,
        )
        .unwrap(),
    )
    .unwrap();
    wire["component_id"] = json!("bogus:bogus:anything");

    assert!(matches!(
        AgentStackCapabilityEvidence::from_json(&wire.to_string()),
        Err(AgentStackCapabilityEvidenceParseError::Validation(
            AgentStackCapabilityEvidenceError::InvalidComponentId
        ))
    ));
}

#[test]
fn runtime_evidence_requires_trust_exactly_matching_its_source() {
    let component = component();
    for evidence_class in [
        AgentStackCapabilityEvidenceClass::Granted,
        AgentStackCapabilityEvidenceClass::Observed,
    ] {
        for (source, trust_level) in [
            (runtime_source(), AgentStackTrustLevel::RunnerObserved),
            (runner_source(), AgentStackTrustLevel::RuntimeObserved),
        ] {
            let error = AgentStackCapabilityEvidence::new(
                evidence_class,
                AgentStackCapability::Shell,
                component.component_id().clone(),
                source,
                Some(observed_at()),
                trust_level,
                AgentStackCapabilityScope::Host,
            )
            .unwrap_err();
            assert_eq!(error, AgentStackCapabilityEvidenceError::TrustNotSupported);
        }
    }
}

#[test]
fn evidence_json_rejects_unknown_nested_scope_fields() {
    let component = component();
    let mut wire = serde_json::to_value(
        AgentStackCapabilityEvidence::granted(
            &component,
            AgentStackCapability::Network,
            runtime_source(),
            observed_at(),
            AgentStackTrustLevel::RuntimeObserved,
            AgentStackCapabilityScope::network(None::<String>).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    wire["scope"]["allow_all"] = json!(true);

    assert!(matches!(
        AgentStackCapabilityEvidence::from_json(&wire.to_string()),
        Err(AgentStackCapabilityEvidenceParseError::Syntax(_))
    ));
}

#[test]
fn evidence_json_normalizes_path_scopes_before_storage() {
    let component = component();
    let mut wire = serde_json::to_value(
        AgentStackCapabilityEvidence::granted(
            &component,
            AgentStackCapability::FileWrite,
            runtime_source(),
            observed_at(),
            AgentStackTrustLevel::RuntimeObserved,
            AgentStackCapabilityScope::path(Path::new("/tmp/evidence/canonical")).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    wire["scope"] = json!({
        "kind": "path",
        "path": "/tmp/evidence/a/../canonical"
    });

    let decoded = AgentStackCapabilityEvidence::from_json(&wire.to_string()).unwrap();
    assert_eq!(
        decoded.scope(),
        &AgentStackCapabilityScope::path(Path::new("/tmp/evidence/canonical")).unwrap()
    );
}

#[test]
fn capability_scope_compatibility_rejects_cross_domain_pairs() {
    let component = component();
    let incompatible = [
        (
            AgentStackCapability::FileWrite,
            AgentStackCapabilityScope::network(None::<String>).unwrap(),
        ),
        (
            AgentStackCapability::Network,
            AgentStackCapabilityScope::repository("deploy/production").unwrap(),
        ),
        (
            AgentStackCapability::Network,
            AgentStackCapabilityScope::path(Path::new("/tmp/capability-scope")).unwrap(),
        ),
    ];

    for (capability, scope) in incompatible {
        let error = AgentStackCapabilityEvidence::granted(
            &component,
            capability,
            runtime_source(),
            observed_at(),
            AgentStackTrustLevel::RuntimeObserved,
            scope,
        )
        .unwrap_err();
        assert_eq!(
            error,
            AgentStackCapabilityEvidenceError::IncompatibleCapabilityScope
        );
    }
}

#[test]
fn cross_boundary_risk_capabilities_accept_specific_scopes() {
    let component = component();
    let compatible = [
        (
            AgentStackCapability::ProductionWrite,
            AgentStackCapabilityScope::path(Path::new("/srv/production")).unwrap(),
        ),
        (
            AgentStackCapability::ProductionWrite,
            AgentStackCapabilityScope::repository("deploy/production").unwrap(),
        ),
        (
            AgentStackCapability::Destructive,
            AgentStackCapabilityScope::network(Some("production.example.com")).unwrap(),
        ),
        (
            AgentStackCapability::SecretRead,
            AgentStackCapabilityScope::path(Path::new("/run/secrets")).unwrap(),
        ),
        (
            AgentStackCapability::Shell,
            AgentStackCapabilityScope::repository("scripts/release").unwrap(),
        ),
        (
            AgentStackCapability::Privileged,
            AgentStackCapabilityScope::network(None::<String>).unwrap(),
        ),
    ];

    for (capability, scope) in compatible {
        AgentStackCapabilityEvidence::granted(
            &component,
            capability,
            runtime_source(),
            observed_at(),
            AgentStackTrustLevel::RuntimeObserved,
            scope,
        )
        .unwrap();
    }
}

#[test]
fn capability_token_lifetime_is_inclusive_and_validated() {
    let component = component();
    let path = PathBuf::from("/tmp/harness-token");
    let at = observed_at();

    for token in [
        capability_token(at, at, vec![path.clone()]),
        capability_token(at - chrono::Duration::seconds(60), at, vec![path.clone()]),
        capability_token(at, at + chrono::Duration::seconds(60), vec![path.clone()]),
    ] {
        assert!(AgentStackCapabilityEvidence::granted_by_sandbox_mode(
            &component,
            SandboxMode::WorkspaceWrite,
            Path::new("/tmp/project"),
            Some(&token),
            runtime_source(),
            at,
        )
        .is_ok());
    }

    for token in [
        capability_token(
            at + chrono::Duration::seconds(1),
            at - chrono::Duration::seconds(1),
            vec![path.clone()],
        ),
        capability_token(
            at + chrono::Duration::seconds(1),
            at + chrono::Duration::seconds(60),
            vec![path.clone()],
        ),
        capability_token(
            at - chrono::Duration::seconds(60),
            at - chrono::Duration::seconds(1),
            vec![path.clone()],
        ),
    ] {
        let error = AgentStackCapabilityEvidence::granted_by_sandbox_mode(
            &component,
            SandboxMode::WorkspaceWrite,
            Path::new("/tmp/project"),
            Some(&token),
            runtime_source(),
            at,
        )
        .unwrap_err();
        assert_eq!(
            error,
            AgentStackCapabilityEvidenceError::CapabilityTokenNotEffectiveAtEvidenceTime
        );
    }
}

#[test]
fn sandbox_modes_without_token_report_effective_authority() {
    let component = component();
    let project_scope =
        AgentStackCapabilityScope::path(Path::new("/tmp/harness-workspace")).unwrap();
    let network_scope = AgentStackCapabilityScope::network(None::<String>).unwrap();

    for (sandbox_mode, expected) in [
        (
            SandboxMode::ReadOnly,
            vec![
                (AgentStackCapability::Shell, AgentStackCapabilityScope::Host),
                (
                    AgentStackCapability::SecretRead,
                    AgentStackCapabilityScope::Host,
                ),
            ],
        ),
        (
            SandboxMode::ReadOnlyWithNetwork,
            vec![
                (AgentStackCapability::Shell, AgentStackCapabilityScope::Host),
                (
                    AgentStackCapability::SecretRead,
                    AgentStackCapabilityScope::Host,
                ),
                (AgentStackCapability::Network, network_scope.clone()),
            ],
        ),
        (
            SandboxMode::WorkspaceWrite,
            vec![
                (AgentStackCapability::Shell, AgentStackCapabilityScope::Host),
                (
                    AgentStackCapability::SecretRead,
                    AgentStackCapabilityScope::Host,
                ),
                (AgentStackCapability::Network, network_scope.clone()),
                (AgentStackCapability::FileWrite, project_scope.clone()),
                (AgentStackCapability::Destructive, project_scope.clone()),
            ],
        ),
        (
            SandboxMode::DangerFullAccess,
            [
                AgentStackCapability::Shell,
                AgentStackCapability::SecretRead,
                AgentStackCapability::Network,
                AgentStackCapability::Privileged,
                AgentStackCapability::FileWrite,
                AgentStackCapability::Destructive,
            ]
            .into_iter()
            .map(|capability| (capability, AgentStackCapabilityScope::Host))
            .collect(),
        ),
    ] {
        let actual = AgentStackCapabilityEvidence::granted_by_sandbox_mode(
            &component,
            sandbox_mode,
            Path::new("/tmp/harness-workspace"),
            None,
            runtime_source(),
            observed_at(),
        )
        .unwrap();
        assert_grants(&actual, &expected);
    }
}

#[test]
fn capability_token_narrows_writable_sandboxes_without_upgrading_read_only_modes() {
    let component = component();
    let at = observed_at();
    let token_paths = [
        PathBuf::from("/tmp/harness-token-a"),
        PathBuf::from("/tmp/harness-token-b/child/.."),
    ];
    let token = capability_token(
        at - chrono::Duration::seconds(60),
        at + chrono::Duration::seconds(60),
        token_paths.to_vec(),
    );
    let network_scope = AgentStackCapabilityScope::network(None::<String>).unwrap();
    let baseline = vec![
        (AgentStackCapability::Shell, AgentStackCapabilityScope::Host),
        (
            AgentStackCapability::SecretRead,
            AgentStackCapabilityScope::Host,
        ),
    ];
    let mut networked = baseline.clone();
    networked.push((AgentStackCapability::Network, network_scope));
    let mut token_scoped = networked.clone();
    for path in token_paths {
        let scope = AgentStackCapabilityScope::path(&path).unwrap();
        token_scoped.push((AgentStackCapability::FileWrite, scope.clone()));
        token_scoped.push((AgentStackCapability::Destructive, scope));
    }

    for (sandbox_mode, expected) in [
        (SandboxMode::ReadOnly, baseline),
        (SandboxMode::ReadOnlyWithNetwork, networked.clone()),
        (SandboxMode::WorkspaceWrite, token_scoped.clone()),
        (SandboxMode::DangerFullAccess, token_scoped),
    ] {
        let actual = AgentStackCapabilityEvidence::granted_by_sandbox_mode(
            &component,
            sandbox_mode,
            Path::new("/tmp/harness-workspace"),
            Some(&token),
            runtime_source(),
            observed_at(),
        )
        .unwrap();
        assert_grants(&actual, &expected);
        assert!(!actual.iter().any(|item| {
            item.capability() == AgentStackCapability::Privileged
                || matches!(
                    (item.capability(), item.scope()),
                    (
                        AgentStackCapability::FileWrite | AgentStackCapability::Destructive,
                        AgentStackCapabilityScope::Host
                    )
                )
        }));
    }
}

#[test]
fn sandbox_evidence_preserves_runtime_source_and_trust() {
    let component = component();
    let token = capability_token(
        observed_at(),
        observed_at(),
        vec![PathBuf::from("/tmp/harness-token")],
    );
    let evidence = AgentStackCapabilityEvidence::granted_by_sandbox_mode(
        &component,
        SandboxMode::WorkspaceWrite,
        Path::new("/tmp/harness-workspace"),
        Some(&token),
        runtime_source(),
        observed_at(),
    )
    .unwrap();

    assert!(evidence.iter().all(|item| {
        item.component_id() == component.component_id()
            && item.source() == &runtime_source()
            && item.trust_level() == AgentStackTrustLevel::RuntimeObserved
    }));
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
