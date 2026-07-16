use harness_core::config::workflow::WorkflowDocument;
use harness_workflow::runtime::{
    resolve_declarative_definition, DeclarativeDefinitionResolution, RuntimeJob, WorkflowInstance,
};
use serde_json::{json, Value};

use crate::workflow_runtime_worker::data_helpers::activity_name;

pub(super) fn apply_activity_policy(
    packet: &mut Value,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workflow_document: &WorkflowDocument,
) -> anyhow::Result<()> {
    apply_activity_policy_with_resolver(packet, job, workflow, workflow_document, |workflow| {
        resolve_declarative_definition(workflow)
    })
}

fn apply_activity_policy_with_resolver(
    packet: &mut Value,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workflow_document: &WorkflowDocument,
    resolve: impl FnOnce(&WorkflowInstance) -> DeclarativeDefinitionResolution,
) -> anyhow::Result<()> {
    let Some(workflow) = workflow else {
        return Ok(());
    };
    let definition = match resolve(workflow) {
        DeclarativeDefinitionResolution::NotDeclarative => return Ok(()),
        DeclarativeDefinitionResolution::Resolved(definition) => definition,
        DeclarativeDefinitionResolution::PinError(error) => anyhow::bail!(
            "declarative workflow '{}' has an invalid definition pin while binding activity policy: {error:?}",
            workflow.id
        ),
    };
    let state = definition
        .policy()
        .states
        .get(&workflow.state)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' cannot dispatch a runtime job from non-active state '{}'",
                workflow.id,
                workflow.state
            )
        })?;
    let expected_activity = state.activity.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "declarative workflow '{}' state '{}' has no activity for runtime dispatch",
            workflow.id,
            workflow.state
        )
    })?;
    let activity = activity_name(job);
    if activity != expected_activity {
        anyhow::bail!(
            "declarative workflow '{}' state '{}' expects activity '{}', got '{}'",
            workflow.id,
            workflow.state,
            expected_activity,
            activity
        );
    }
    let policy = workflow_document
        .config
        .activities
        .get(expected_activity)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' activity '{}' is missing from WORKFLOW.md at dispatch",
                workflow.id,
                expected_activity
            )
        })?;
    if let Some(prompt) = policy
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty())
    {
        packet["workflow_file"]["activity_prompt"] = json!(prompt);
    }
    packet["workflow_file"]["activity_validation"] = json!(&policy.validation);
    Ok(())
}

pub(super) fn append_activity_policy_prompt(prompt: &mut String, prompt_packet: &Value) {
    if let Some(activity_prompt) = prompt_packet
        .get("workflow_file")
        .and_then(|workflow_file| workflow_file.get("activity_prompt"))
        .and_then(Value::as_str)
        .filter(|activity_prompt| !activity_prompt.trim().is_empty())
    {
        prompt.push_str("\nRepository workflow activity prompt:\n");
        prompt.push_str(activity_prompt);
        prompt.push('\n');
    }
    if let Some(commands) = prompt_packet
        .get("workflow_file")
        .and_then(|workflow_file| workflow_file.get("activity_validation"))
        .and_then(Value::as_array)
        .filter(|commands| !commands.is_empty())
    {
        prompt.push_str("\nRepository workflow activity validation commands:\n");
        for command in commands.iter().filter_map(Value::as_str) {
            prompt.push_str("- ");
            prompt.push_str(command);
            prompt.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use harness_workflow::runtime::{build_declarative_definition, RuntimeKind, WorkflowSubject};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn definition() -> harness_workflow::runtime::DeclarativeWorkflowDefinition {
        build_declarative_definition(
            &WorkflowDefinitionPolicy {
                id: "activity_policy_binding_v1".to_string(),
                initial: "reviewing".to_string(),
                states: BTreeMap::from([
                    (
                        "blocked".to_string(),
                        DeclaredState {
                            progress: Some(DeclaredProgressMode::OperatorGate),
                            ..DeclaredState::default()
                        },
                    ),
                    (
                        "reviewing".to_string(),
                        DeclaredState {
                            activity: Some("review_docs".to_string()),
                            on_success: Some("done".to_string()),
                            on_failure: Some("failed".to_string()),
                            on_signal: BTreeMap::from([(
                                "cancel".to_string(),
                                "cancelled".to_string(),
                            )]),
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
                recovery_targets: vec!["reviewing".to_string()],
            },
            &BTreeMap::from([("review_docs".to_string(), WorkflowActivityPolicy::default())]),
        )
        .expect("activity policy fixture should compile")
    }

    #[test]
    fn exact_declarative_activity_binds_policy_and_missing_policy_fails_closed() {
        let definition = Arc::new(definition());
        let workflow = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "reviewing",
            WorkflowSubject::new("declarative", "docs-1"),
        )
        .with_data(json!({ "definition_hash": definition.definition_hash() }));
        let job = RuntimeJob::pending(
            "command-activity-policy",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "review_docs" }),
        );
        let mut document = WorkflowDocument::default();
        document.config.activities.insert(
            "review_docs".to_string(),
            WorkflowActivityPolicy {
                prompt: Some("Review the documentation.".to_string()),
                validation: vec!["cargo test -p docs".to_string()],
            },
        );
        let mut packet = json!({ "workflow_file": {} });

        apply_activity_policy_with_resolver(&mut packet, &job, Some(&workflow), &document, |_| {
            DeclarativeDefinitionResolution::Resolved(definition.clone())
        })
        .expect("exact declared activity policy should bind");
        assert_eq!(
            packet["workflow_file"]["activity_prompt"],
            "Review the documentation."
        );
        assert_eq!(
            packet["workflow_file"]["activity_validation"],
            json!(["cargo test -p docs"])
        );

        document.config.activities.clear();
        let error = apply_activity_policy_with_resolver(
            &mut json!({ "workflow_file": {} }),
            &job,
            Some(&workflow),
            &document,
            |_| DeclarativeDefinitionResolution::Resolved(definition.clone()),
        )
        .expect_err("missing declared activity policy must fail closed");
        assert!(error.to_string().contains("missing from WORKFLOW.md"));
    }
}
