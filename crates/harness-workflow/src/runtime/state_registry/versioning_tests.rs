use super::*;
use crate::runtime::declarative::{
    build_builtin_declarative_definition, build_declarative_definition,
};
use crate::runtime::model::{
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowInstance, WorkflowSubject,
};
use crate::runtime::validator::{TransitionAllowlist, ValidationContext};
use crate::runtime::{
    github_issue_pr_definition_hash, WorkflowProgressMode, GITHUB_ISSUE_PR_DEFINITION_VERSION,
};
use chrono::Utc;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

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
        intake: None,
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
    let invalid =
        instance(&definition).with_server_data(json!({ "definition_hash": "SHA256:bad" }));
    assert!(matches!(
        registry.resolve_declarative_definition(&invalid),
        DeclarativeDefinitionResolution::PinError(DeclarativeDefinitionPinError::InvalidHash)
    ));
    let mut other_hash = definition.definition_hash().to_string();
    other_hash.replace_range(other_hash.len() - 1.., "0");
    if other_hash == definition.definition_hash() {
        other_hash.replace_range(other_hash.len() - 1.., "1");
    }
    let mismatch = instance(&definition).with_server_data(json!({ "definition_hash": other_hash }));
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
    .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
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
    let pinned = instance(&definition)
        .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
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
fn historical_only_declarative_definition_is_enumerated() {
    let definition = definition();
    let mut registry = WorkflowDefinitionRegistry::new_for_tests();
    registry
        .register_declarative_historical(definition)
        .expect("historical fixture should register");

    assert_eq!(registry.known_definition_ids(), vec!["pin_test"]);
}

#[test]
fn legacy_and_current_github_builtins_resolve_their_exact_versions() {
    let registry = WorkflowDefinitionRegistry::with_builtins();
    let legacy = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "discovered",
        WorkflowSubject::new("issue", "one"),
    );
    let DeclarativeDefinitionResolution::Resolved(legacy_definition) =
        registry.resolve_declarative_definition(&legacy)
    else {
        panic!("legacy built-in definition should resolve");
    };
    assert!(!legacy_definition
        .classifier_activities()
        .contains(crate::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY));
    assert!(!legacy_definition
        .policy()
        .states
        .contains_key("plan_scope_review"));
    let current = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "discovered",
        WorkflowSubject::new("issue", "two"),
    )
    .with_server_data(json!({
        "definition_hash": github_issue_pr_definition_hash()
    }));
    let DeclarativeDefinitionResolution::Resolved(current_definition) =
        registry.resolve_declarative_definition(&current)
    else {
        panic!("current built-in definition should resolve");
    };
    assert!(current_definition
        .classifier_activities()
        .contains(crate::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY));
    assert!(current_definition
        .policy()
        .states
        .contains_key("plan_scope_review"));

    let legacy_transition = WorkflowDecision::new(
        &legacy.id,
        "discovered",
        "submit_issue",
        "planning",
        "legacy submission starts planning",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "plan_issue",
        "legacy:plan",
    ));
    registry
        .decision_validator_for_instance(&legacy)
        .expect("legacy pin should resolve")
        .expect("legacy definition should expose a validator")
        .validate(
            &legacy,
            &legacy_transition,
            &ValidationContext::new("versioning-test", Utc::now()),
        )
        .expect("v1 transition table must remain independently usable");
}

#[test]
fn github_builtin_selectors_keep_v1_unpinned_and_v2_exactly_pinned() {
    let registry = WorkflowDefinitionRegistry::with_builtins();
    let terminal = registry.terminal_state_selectors(GITHUB_ISSUE_PR_DEFINITION_ID);
    assert!(terminal.iter().any(|selector| {
        selector.state == "done"
            && selector.definition_version == Some(1)
            && selector.definition_hash.is_none()
    }));
    assert!(terminal.iter().any(|selector| {
        selector.state == "done"
            && selector.definition_version == Some(GITHUB_ISSUE_PR_DEFINITION_VERSION)
            && selector.definition_hash.as_deref()
                == Some(github_issue_pr_definition_hash().as_str())
    }));

    let progress = registry.progress_state_selectors(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        WorkflowProgressMode::ExternalWait,
    );
    assert!(progress.iter().any(|selector| {
        selector.state == "awaiting_feedback"
            && selector.definition_version == Some(1)
            && selector.definition_hash.is_none()
    }));
    assert!(progress.iter().any(|selector| {
        selector.state == "awaiting_feedback"
            && selector.definition_version == Some(GITHUB_ISSUE_PR_DEFINITION_VERSION)
            && selector.definition_hash.as_deref()
                == Some(github_issue_pr_definition_hash().as_str())
    }));
}

#[test]
fn builtin_content_hash_covers_the_transition_contract() {
    let fixture = definition();
    let policy = fixture.policy().clone();
    let activities = BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]);
    let first = build_builtin_declarative_definition(
        &policy,
        &activities,
        TransitionAllowlist::default().allow("running", "done", [WorkflowCommandType::MarkDone]),
        BTreeSet::new(),
        7,
    )
    .expect("first transition contract should compile");
    let second = build_builtin_declarative_definition(
        &policy,
        &activities,
        TransitionAllowlist::default().allow("running", "done", [WorkflowCommandType::Wait]),
        BTreeSet::new(),
        7,
    )
    .expect("second transition contract should compile");

    assert_ne!(first.definition_hash(), second.definition_hash());
}
