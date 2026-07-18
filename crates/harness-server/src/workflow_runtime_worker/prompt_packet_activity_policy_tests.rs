use super::*;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use harness_workflow::runtime::{
    build_declarative_definition, DeclarativeDefinitionResolution, RuntimeKind, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

fn compiled_activity_policy_definition() -> harness_workflow::runtime::DeclarativeWorkflowDefinition
{
    let policy = WorkflowDefinitionPolicy {
        id: "prompt_policy_test".to_string(),
        initial: "working".to_string(),
        states: BTreeMap::from([
            (
                "working".to_string(),
                DeclaredState {
                    activity: Some("inspect_repository".to_string()),
                    on_success: Some("done".to_string()),
                    on_failure: Some("failed".to_string()),
                    on_blocked: Some("blocked".to_string()),
                    on_signal: BTreeMap::from([("cancel".to_string(), "cancelled".to_string())]),
                    ..Default::default()
                },
            ),
            (
                "blocked".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::OperatorGate),
                    ..Default::default()
                },
            ),
        ]),
        terminal: BTreeMap::from([
            ("done".to_string(), "succeeded".to_string()),
            ("failed".to_string(), "failed".to_string()),
            ("cancelled".to_string(), "cancelled".to_string()),
        ]),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec!["working".to_string()],
        intake: None,
    };
    build_declarative_definition(
        &policy,
        &BTreeMap::from([(
            "inspect_repository".to_string(),
            WorkflowActivityPolicy::default(),
        )]),
    )
    .expect("compile declarative prompt policy fixture")
}

#[test]
fn declarative_activity_policy_binds_exactly_and_missing_policy_fails_closed() {
    let definition = Arc::new(compiled_activity_policy_definition());
    let workflow = WorkflowInstance::new(
        definition.policy().id.clone(),
        definition.definition_version(),
        "working",
        WorkflowSubject::new("declarative", "task:policy-test"),
    )
    .with_data(json!({ "definition_hash": definition.definition_hash() }));
    let job = RuntimeJob::pending(
        "command-policy",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "inspect_repository" }),
    );
    let mut workflow_document = WorkflowDocument::default();
    workflow_document.config.activities.insert(
        "inspect_repository".to_string(),
        WorkflowActivityPolicy {
            prompt: Some("Inspect only the declared repository surface.".to_string()),
            validation: vec!["cargo check -p harness-server --all-targets".to_string()],
        },
    );
    let mut packet = json!({
        "activity_result_schema": {},
        "required_structured_output": {},
        "runtime_job": { "activity": "inspect_repository" },
    });

    super::activity_policy::apply_activity_policy_with_resolver(
        &mut packet,
        &job,
        Some(&workflow),
        &workflow_document,
        |_| DeclarativeDefinitionResolution::Resolved(definition.clone()),
    )
    .expect("exact declarative activity policy should bind");

    assert_eq!(
        packet["activity_policy"]["prompt"],
        "Inspect only the declared repository surface."
    );
    assert_eq!(
        packet["activity_result_schema"]["validation_contract"]["required_commands"][0],
        "cargo check -p harness-server --all-targets"
    );
    let prompt = build_runtime_job_prompt(&packet, None);
    assert!(prompt.contains("Activity policy instructions:"));
    assert!(prompt.contains("Inspect only the declared repository surface."));
    assert!(prompt.contains("cargo check -p harness-server --all-targets"));

    workflow_document.config.activities.clear();
    let error = super::activity_policy::apply_activity_policy_with_resolver(
        &mut json!({
            "activity_result_schema": {},
            "required_structured_output": {},
        }),
        &job,
        Some(&workflow),
        &workflow_document,
        |_| DeclarativeDefinitionResolution::Resolved(definition.clone()),
    )
    .expect_err("a missing declared activity policy must fail closed");
    assert!(error.to_string().contains("missing from WORKFLOW.md"));
}

#[test]
fn built_in_or_unmatched_activity_does_not_bind_declarative_activity_policy() {
    let job = RuntimeJob::pending(
        "command-built-in",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    );
    let workflow = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "task:built-in"),
    );
    let mut workflow_document = WorkflowDocument::default();
    workflow_document.config.activities.insert(
        "implement_issue".to_string(),
        WorkflowActivityPolicy {
            prompt: Some("Must not bind to built-in behavior.".to_string()),
            validation: vec!["false".to_string()],
        },
    );
    let mut packet = json!({
        "activity_result_schema": {},
        "required_structured_output": {},
    });

    super::activity_policy::apply_activity_policy_with_resolver(
        &mut packet,
        &job,
        Some(&workflow),
        &workflow_document,
        |_| DeclarativeDefinitionResolution::NotDeclarative,
    )
    .expect("built-in workflows should not bind declarative activity policy");

    assert!(packet.get("activity_policy").is_none());
    assert!(packet["activity_result_schema"]
        .get("validation_contract")
        .is_none());
}

#[test]
fn runtime_prompt_packet_omits_duplicated_additional_prompt() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "plan_issue",
            "command": {
                "additional_prompt": "Inspect the existing pull request."
            }
        }),
    );
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "planning",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_data(json!({
        "additional_prompt": "Inspect the existing pull request.",
        "issue_number": 42
    }));
    let runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

    let packet = build_runtime_prompt_packet(
        &job,
        Some(&workflow),
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &runtime_profile,
        &WorkflowDocument::default(),
        &[],
    )
    .expect("prompt packet should build");

    assert!(packet["workflow"]["data"]
        .get("additional_prompt")
        .is_none());
    assert_eq!(
        packet["command_input"]["command"]["additional_prompt"],
        "Inspect the existing pull request."
    );
}
