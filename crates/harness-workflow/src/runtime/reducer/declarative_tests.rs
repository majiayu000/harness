use super::*;
use crate::runtime::model::{
    ActivityErrorKind, ActivityResult, WorkflowCommandType, WorkflowEvent, WorkflowInstance,
    WorkflowSubject,
};
use crate::runtime::validator::ValidationContext;
use chrono::Utc;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn declarative_failure_without_route_uses_configured_generic_retry() -> anyhow::Result<()> {
    const DEFINITION_ID: &str = "declarative_generic_retry_test";
    let policy = WorkflowDefinitionPolicy {
        id: DEFINITION_ID.to_string(),
        initial: "working".to_string(),
        states: BTreeMap::from([
            (
                "working".to_string(),
                DeclaredState {
                    activity: Some("perform_work".to_string()),
                    on_success: Some("done".to_string()),
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
        recovery_targets: Vec::new(),
    };
    let definition = super::super::declarative::build_declarative_definition(
        &policy,
        &BTreeMap::from([(
            "perform_work".to_string(),
            WorkflowActivityPolicy::default(),
        )]),
    )?;
    super::super::state_registry::register_declarative_workflow_definitions([definition])?;
    let definition =
        super::super::state_registry::current_declarative_workflow_definition(DEFINITION_ID)
            .ok_or_else(|| anyhow::anyhow!("generic-retry definition should resolve"))?;
    let instance = WorkflowInstance::new(
        DEFINITION_ID,
        definition.definition_version(),
        "working",
        WorkflowSubject::new("test", "generic-retry"),
    )
    .with_data(json!({
        "definition_hash": definition.definition_hash(),
        "runtime_retry_policy": { "max_failed_activity_retries": 1 }
    }));
    let result = ActivityResult::failed("perform_work", "failed", "transient failure")
        .with_error_kind(ActivityErrorKind::Timeout);
    let event = WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-test")
        .with_payload(json!({ "activity_result": result }));

    let decision = reduce_runtime_job_completed(&instance, &event)?
        .ok_or_else(|| anyhow::anyhow!("generic retry should produce a decision"))?;
    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.next_state, "working");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    super::super::state_registry::decision_validator_for_instance(&instance)
        .ok_or_else(|| anyhow::anyhow!("generic retry validator should resolve"))?
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-test", Utc::now()),
        )?;
    Ok(())
}
