use super::*;
use crate::runtime::declarative::build_declarative_definition;
use crate::runtime::model::{WorkflowInstance, WorkflowSubject};
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use serde_json::json;
use std::collections::BTreeMap;

fn definition() -> DeclarativeWorkflowDefinition {
    let policy = WorkflowDefinitionPolicy {
        id: "pin_test".to_string(),
        initial: "running".to_string(),
        states: BTreeMap::from([
            (
                "blocked".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::OperatorGate),
                    ..DeclaredState::default()
                },
            ),
            (
                "running".to_string(),
                DeclaredState {
                    activity: Some("run".to_string()),
                    on_success: Some("done".to_string()),
                    on_failure: Some("failed".to_string()),
                    on_signal: BTreeMap::from([("cancel".to_string(), "cancelled".to_string())]),
                    ..DeclaredState::default()
                },
            ),
        ]),
        terminal: BTreeMap::from([
            ("done".to_string(), "succeeded".to_string()),
            ("failed".to_string(), "failed".to_string()),
            ("cancelled".to_string(), "cancelled".to_string()),
        ]),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec!["running".to_string()],
    };
    build_declarative_definition(
        &policy,
        &BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]),
    )
    .expect("fixture definition should compile")
}

fn instance(definition: &DeclarativeWorkflowDefinition) -> WorkflowInstance {
    WorkflowInstance::new(
        "pin_test",
        definition.definition_version(),
        "running",
        WorkflowSubject::new("test", "one"),
    )
}

#[test]
fn strict_resolution_distinguishes_all_pin_errors() {
    let definition = definition();
    let mut registry = WorkflowDefinitionRegistry::new_for_tests();
    registry
        .register_declarative_current(definition.clone())
        .expect("fixture should register");

    assert!(matches!(
        registry.resolve_declarative_definition(&instance(&definition)),
        DeclarativeDefinitionResolution::PinError(DeclarativeDefinitionPinError::MissingHash)
    ));
    let invalid = instance(&definition).with_data(json!({ "definition_hash": "SHA256:bad" }));
    assert!(matches!(
        registry.resolve_declarative_definition(&invalid),
        DeclarativeDefinitionResolution::PinError(DeclarativeDefinitionPinError::InvalidHash)
    ));
    let mut other_hash = definition.definition_hash().to_string();
    other_hash.replace_range(other_hash.len() - 1.., "0");
    if other_hash == definition.definition_hash() {
        other_hash.replace_range(other_hash.len() - 1.., "1");
    }
    let mismatch = instance(&definition).with_data(json!({ "definition_hash": other_hash }));
    assert!(matches!(
        registry.resolve_declarative_definition(&mismatch),
        DeclarativeDefinitionResolution::PinError(DeclarativeDefinitionPinError::HashMismatch)
    ));
    let missing_version = WorkflowInstance::new(
        "pin_test",
        definition.definition_version() ^ 1,
        "running",
        WorkflowSubject::new("test", "missing"),
    )
    .with_data(json!({ "definition_hash": definition.definition_hash() }));
    assert!(matches!(
        registry.resolve_declarative_definition(&missing_version),
        DeclarativeDefinitionResolution::PinError(DeclarativeDefinitionPinError::MissingVersion)
    ));
}

#[test]
fn strict_resolution_and_validator_use_exact_pinned_definition() {
    let definition = definition();
    let mut registry = WorkflowDefinitionRegistry::new_for_tests();
    registry
        .register_declarative_current(definition.clone())
        .expect("fixture should register");
    let pinned =
        instance(&definition).with_data(json!({ "definition_hash": definition.definition_hash() }));
    assert!(matches!(
        registry.resolve_declarative_definition(&pinned),
        DeclarativeDefinitionResolution::Resolved(resolved)
            if resolved.definition_hash() == definition.definition_hash()
    ));
    assert!(registry
        .decision_validator_for_instance(&pinned)
        .expect("pin should resolve")
        .is_some());
}

#[test]
fn builtins_ignore_forged_declarative_pin_markers() {
    let registry = WorkflowDefinitionRegistry::new_for_tests();
    let builtin = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "discovered",
        WorkflowSubject::new("issue", "one"),
    )
    .with_data(json!({
        "definition_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    }));
    assert!(matches!(
        registry.resolve_declarative_definition(&builtin),
        DeclarativeDefinitionResolution::NotDeclarative
    ));
}
