use super::*;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use std::collections::BTreeMap;
use std::sync::Once;

const PROJECT_DEFINITION_ID: &str = "project_binding_test_declarative";
static REGISTER_PROJECT_DEFINITION: Once = Once::new();

fn project_binding_policy() -> WorkflowDefinitionPolicy {
    WorkflowDefinitionPolicy {
        id: PROJECT_DEFINITION_ID.to_string(),
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
        recovery_targets: vec!["working".to_string()],
        intake: None,
    }
}

fn register_project_definition() {
    REGISTER_PROJECT_DEFINITION.call_once(|| {
        let definition = harness_workflow::runtime::build_declarative_definition(
            &project_binding_policy(),
            &BTreeMap::from([(
                "perform_work".to_string(),
                WorkflowActivityPolicy::default(),
            )]),
        )
        .unwrap();
        harness_workflow::runtime::register_declarative_workflow_definitions([definition]).unwrap();
    });
}

fn write_workflow(project_root: &std::path::Path, extra_route: &str) -> anyhow::Result<()> {
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        format!(
            r#"---
definition:
  id: {PROJECT_DEFINITION_ID}
  initial: working
  states:
    working:
      activity: perform_work
      on_success: done
{extra_route}
    blocked:
      progress: operator_gate
  terminal:
    done: succeeded
    failed: failed
    cancelled: cancelled
  recovery_targets: [working]
runtime_retry_policy:
  max_failed_activity_retries: 2
  retry_delay_secs: 5
activities:
  perform_work:
    prompt: Perform the work.
---
Project workflow.
"#,
        ),
    )?;
    Ok(())
}

#[test]
fn declarative_submission_instance_persists_runtime_retry_policy() -> anyhow::Result<()> {
    register_project_definition();
    let project = tempfile::tempdir()?;
    write_workflow(project.path(), "")?;
    let definition =
        resolve_declarative_definition_for_project(project.path(), PROJECT_DEFINITION_ID)?;
    let task_id = TaskId::from_str("project-retry-policy");
    let ctx = DeclarativeSubmissionRuntimeContext {
        project_root: project.path(),
        definition_id: PROJECT_DEFINITION_ID,
        task_id: &task_id,
        prompt: "perform the declared work",
        depends_on: &[],
        serialization_depends_on: &[],
        source: Some("test"),
        external_id: None,
    };

    let instance = super::declarative::submission_instance(
        &ctx,
        &project.path().to_string_lossy(),
        "project-retry-workflow",
        "project-retry-prompt",
        &definition,
    );

    assert_eq!(
        instance.data["runtime_retry_policy"]["max_failed_activity_retries"],
        2
    );
    assert_eq!(instance.data["runtime_retry_policy"]["retry_delay_secs"], 5);
    Ok(())
}

#[test]
fn project_definition_resolution_requires_matching_effective_workflow() -> anyhow::Result<()> {
    register_project_definition();

    let valid = tempfile::tempdir()?;
    write_workflow(valid.path(), "")?;
    let resolved = resolve_declarative_definition_for_project(valid.path(), PROJECT_DEFINITION_ID)?;
    assert_eq!(resolved.policy().id, PROJECT_DEFINITION_ID);

    let missing = tempfile::tempdir()?;
    let error = resolve_declarative_definition_for_project(missing.path(), PROJECT_DEFINITION_ID)
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("does not declare workflow definition"));

    let stale = tempfile::tempdir()?;
    write_workflow(stale.path(), "      on_failure: failed")?;
    let error = resolve_declarative_definition_for_project(stale.path(), PROJECT_DEFINITION_ID)
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("does not match the registered startup version"));
    Ok(())
}
