use super::*;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowClassifierPolicy,
    WorkflowDefinitionPolicy,
};
use harness_workflow::runtime::{
    build_declarative_definition, DataProvenance, DeclarativeDefinitionResolution, RuntimeKind,
    WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
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
    .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
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
            classifier: None,
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
            classifier: None,
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
fn classifier_policy_uses_independent_prompt_and_structured_contract() {
    let classifier = WorkflowClassifierPolicy {
        verdicts: vec!["allow".to_string(), "needs_human".to_string()],
        environment: vec!["Judge only supplied facts.".to_string()],
        hard_deny: vec!["Escalate ambiguous scope.".to_string()],
        ..WorkflowClassifierPolicy::default()
    };
    let activity_policy = WorkflowActivityPolicy {
        classifier: Some(classifier),
        ..WorkflowActivityPolicy::default()
    };
    let policy = WorkflowDefinitionPolicy {
        id: "classifier_prompt_test".to_string(),
        initial: "classifying".to_string(),
        states: BTreeMap::from([
            (
                "classifying".to_string(),
                DeclaredState {
                    activity: Some("classify_scope".to_string()),
                    on_failure: Some("blocked".to_string()),
                    on_signal: BTreeMap::from([
                        ("allow".to_string(), "done".to_string()),
                        ("needs_human".to_string(), "failed".to_string()),
                    ]),
                    ..DeclaredState::default()
                },
            ),
            (
                "blocked".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::OperatorGate),
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
        recovery_targets: vec!["classifying".to_string()],
        intake: None,
    };
    let definition = Arc::new(
        build_declarative_definition(
            &policy,
            &BTreeMap::from([("classify_scope".to_string(), activity_policy.clone())]),
        )
        .expect("classifier fixture should compile"),
    );
    let workflow = WorkflowInstance::new(
        policy.id,
        definition.definition_version(),
        "classifying",
        WorkflowSubject::new("declarative", "task:classifier"),
    )
    .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
    let job = RuntimeJob::pending(
        "command-classifier",
        RuntimeKind::CodexJsonrpc,
        "classifier-default",
        json!({ "activity": "classify_scope" }),
    );
    let mut mutable_activity_policy = activity_policy;
    mutable_activity_policy
        .classifier
        .as_mut()
        .expect("classifier fixture")
        .environment = vec!["MUTATED_CHECKOUT_RULE_MUST_NOT_APPEAR".to_string()];
    let mut document = WorkflowDocument::default();
    document
        .config
        .activities
        .insert("classify_scope".to_string(), mutable_activity_policy);
    let mut packet = json!({
        "activity_result_schema": {},
        "required_structured_output": {},
        "runtime_job": {
            "id": job.id,
            "activity": "classify_scope",
            "runtime_profile": "classifier-default"
        },
        "workflow_file": { "prompt_template": "MUST_NOT_APPEAR" },
        "project": { "root": "/repo" }
    });

    super::activity_policy::apply_activity_policy_with_resolver(
        &mut packet,
        &job,
        Some(&workflow),
        &document,
        |_| DeclarativeDefinitionResolution::Resolved(definition),
    )
    .expect("classifier policy should bind");
    assert_eq!(
        packet["activity_policy"]["classifier"]["environment"][0],
        "Judge only supplied facts."
    );
    assert!(!packet.to_string().contains("MUTATED_CHECKOUT_RULE"));
    assert!(packet["workflow_file"].get("config").is_none());
    assert!(packet["workflow_file"].get("prompt_template").is_none());
    let prompt = build_runtime_job_prompt(&packet, Some("MUST_NOT_APPEAR_EITHER"));

    assert_eq!(
        packet["activity_policy"]["classifier"]["verdicts"][0],
        "allow"
    );
    assert_eq!(
        packet["activity_result_schema"]["classifier_contract"]["exactly_one"],
        true
    );
    assert!(prompt.contains("independent Harness policy classifier"));
    assert!(prompt.contains("Do not use tools"));
    assert!(!prompt.contains("MUST_NOT_APPEAR"));
}

#[test]
fn built_in_scope_review_requires_workflow_classifier_policy() {
    let registry = WorkflowDefinitionRegistry::with_builtins();
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "plan_scope_review",
        WorkflowSubject::new("issue", "issue:42"),
    );
    let job = RuntimeJob::pending(
        "command-scope-review",
        RuntimeKind::CodexJsonrpc,
        "classifier-default",
        json!({ "activity": harness_workflow::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY }),
    );
    let mut packet = json!({
        "activity_result_schema": {},
        "required_structured_output": {},
    });
    let mut document = WorkflowDocument::default();

    let error = super::activity_policy::apply_activity_policy(
        &registry,
        &mut packet,
        &job,
        Some(&workflow),
        &document,
    )
    .expect_err("missing built-in classifier policy must fail closed");
    assert!(error.to_string().contains("no pinned dispatch policy"));

    let pinned_policy = WorkflowActivityPolicy {
        classifier: Some(WorkflowClassifierPolicy {
            verdicts: vec![
                "allow".to_string(),
                "revise_plan".to_string(),
                "split_required".to_string(),
                "needs_human".to_string(),
            ],
            environment: vec!["Judge the supplied scope facts.".to_string()],
            ..WorkflowClassifierPolicy::default()
        }),
        ..WorkflowActivityPolicy::default()
    };
    let workflow = workflow.with_server_data(json!({
        "pinned_change_scope_classifier_policy": pinned_policy
    }));
    document.config.activities.clear();
    super::activity_policy::apply_activity_policy(
        &registry,
        &mut packet,
        &job,
        Some(&workflow),
        &document,
    )
    .expect("complete built-in classifier policy should bind");
    assert_eq!(
        packet["activity_policy"]["classifier"]["verdicts"][0],
        "allow"
    );
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
    .with_data_field_provenance(
        json!({
            "additional_prompt": "Inspect the existing pull request.",
            "issue_number": 42
        }),
        |field| match field {
            "additional_prompt" => DataProvenance::External,
            _ => DataProvenance::Server,
        },
    );
    let mut runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    runtime_profile.timeout_secs = Some(3600);
    let resolved_settings =
        crate::workflow_runtime_worker::runtime_profile::resolve_runtime_settings(
            &runtime_profile,
            runtime_profile.kind,
            None,
            &harness_core::config::agents::AgentsConfig::default(),
            &harness_core::config::concurrency::ConcurrencyConfig::default(),
        )
        .unwrap_or_else(|error| panic!("test runtime settings should resolve: {error}"));

    let packet = build_runtime_prompt_packet(
        &harness_workflow::runtime::WorkflowDefinitionRegistry::with_builtins(),
        &job,
        Some(&workflow),
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &runtime_profile,
        &resolved_settings,
        &WorkflowDocument::default(),
        &[],
        None,
    )
    .expect("prompt packet should build");

    assert!(packet["workflow"]["data"]
        .get("additional_prompt")
        .is_none());
    assert!(packet["command_input"]["command"]
        .get("additional_prompt")
        .is_none());
    assert!(packet
        .pointer("/untrusted_command_input/external_fields/command/additional_prompt")
        .and_then(Value::as_str)
        .is_some_and(|value| value.contains("Inspect the existing pull request.")));
}
