use super::{
    stable_remote_fact_hash, DeclarativeDefinitionResolution, WorkflowDefinitionRegistry,
    WorkflowInstance,
};
use serde::Deserialize;
use serde_json::{json, Value};

pub const CLASSIFIER_INPUT_SCHEMA: &str = "harness.runtime.classifier_input.v1";
pub const CLASSIFIER_JOB_SCHEMA: &str = "harness.runtime.classifier_job.v1";
pub const CLASSIFIER_OUTPUT_SCHEMA: &str = "harness.runtime.classifier_output.v1";
pub const CLASSIFIER_ASSESSMENT_SCHEMA: &str = "harness.runtime.classifier_assessment.v1";
pub const CLASSIFIER_OUTPUT_ARTIFACT: &str = "classifier_output";
pub const CLASSIFIER_ASSESSMENT_ARTIFACT: &str = "classifier_assessment";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifierInput {
    schema: String,
    subject: ClassifierSubject,
    facts: Value,
    provenance: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifierSubject {
    kind: String,
    identity: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClassifierAssessment {
    schema: String,
    activity: String,
    subject: Value,
    verdict: String,
    rationale: String,
    evidence_refs: Vec<String>,
    policy_sha256: String,
    prompt_packet_sha256: String,
    runtime_job_id: String,
    runtime_profile: String,
    requested_model: String,
    executed_model: String,
    model_identity_source: String,
    tool_use_detected: bool,
    workspace_isolation: String,
}

pub fn validate_classifier_input(input: &Value) -> anyhow::Result<()> {
    let parsed: ClassifierInput = serde_json::from_value(input.clone())?;
    if parsed.schema != CLASSIFIER_INPUT_SCHEMA {
        anyhow::bail!(
            "classifier input schema must be '{CLASSIFIER_INPUT_SCHEMA}', got '{}'",
            parsed.schema
        );
    }
    if parsed.subject.kind.trim().is_empty() || parsed.subject.identity.trim().is_empty() {
        anyhow::bail!("classifier input subject kind and identity must not be empty");
    }
    if parsed.facts.is_null() || parsed.provenance.is_null() {
        anyhow::bail!("classifier input facts and provenance must not be null");
    }
    Ok(())
}

pub fn classifier_job_snapshot(
    registry: &WorkflowDefinitionRegistry,
    instance: Option<&WorkflowInstance>,
    activity: &str,
) -> anyhow::Result<Option<Value>> {
    let Some(instance) = instance else {
        return Ok(None);
    };
    let definition = match registry.resolve_declarative_definition(instance) {
        DeclarativeDefinitionResolution::Resolved(definition) => definition,
        DeclarativeDefinitionResolution::NotDeclarative => return Ok(None),
        DeclarativeDefinitionResolution::PinError(error) => anyhow::bail!(
            "workflow '{}' has an invalid declarative definition pin while snapshotting classifier policy: {error:?}",
            instance.id
        ),
    };
    let Some(activity_policy) = definition.classifier_activity_policy(activity) else {
        return Ok(None);
    };
    let policy = activity_policy.classifier.as_ref().ok_or_else(|| {
        anyhow::anyhow!("classifier activity '{activity}' has no classifier policy")
    })?;
    let input = instance.data.get("classifier_input").ok_or_else(|| {
        anyhow::anyhow!("classifier activity '{activity}' requires classifier_input")
    })?;
    validate_classifier_input(input)?;
    let policy_value = serde_json::to_value(policy)?;
    Ok(Some(json!({
        "schema": CLASSIFIER_JOB_SCHEMA,
        "activity": activity,
        "policy": policy_value,
        "policy_sha256": stable_remote_fact_hash(&policy_value),
        "input": input,
    })))
}

pub fn validated_classifier_verdict(
    definition: &super::DeclarativeWorkflowDefinition,
    state: &str,
    result: &super::ActivityResult,
) -> anyhow::Result<Option<String>> {
    let Some(activity_policy) = definition
        .policy()
        .states
        .get(state)
        .and_then(|state| state.activity.as_deref())
        .and_then(|activity| definition.classifier_activity_policy(activity))
    else {
        return Ok(None);
    };
    let policy = activity_policy.classifier.as_ref().ok_or_else(|| {
        anyhow::anyhow!("classifier state '{state}' has no pinned classifier policy")
    })?;
    let assessments = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == CLASSIFIER_ASSESSMENT_ARTIFACT)
        .collect::<Vec<_>>();
    let [artifact] = assessments.as_slice() else {
        anyhow::bail!(
            "classifier activity '{}' must contain exactly one server assessment",
            result.activity
        );
    };
    let assessment: ClassifierAssessment = serde_json::from_value(artifact.artifact.clone())?;
    let expected_policy_hash = stable_remote_fact_hash(&serde_json::to_value(policy)?);
    if assessment.schema != CLASSIFIER_ASSESSMENT_SCHEMA
        || assessment.activity != result.activity
        || assessment.policy_sha256 != expected_policy_hash
        || assessment.rationale.trim().is_empty()
        || assessment.runtime_job_id.trim().is_empty()
        || assessment.runtime_profile.trim().is_empty()
        || assessment.prompt_packet_sha256.trim().is_empty()
        || assessment.requested_model.trim().is_empty()
        || assessment.executed_model.trim().is_empty()
        || assessment.requested_model != assessment.executed_model
        || !matches!(
            assessment.model_identity_source.as_str(),
            "provider_reported" | "codex_cli_launch_argument"
        )
        || assessment.tool_use_detected
        || assessment.workspace_isolation != "ephemeral_empty_read_only"
        || assessment.subject.is_null()
        || assessment
            .evidence_refs
            .iter()
            .any(|reference| !reference.starts_with("/classifier_input/"))
        || !policy
            .verdicts
            .iter()
            .any(|verdict| verdict == &assessment.verdict)
    {
        anyhow::bail!("classifier assessment failed reducer validation");
    }
    Ok(Some(assessment.verdict))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowClassifierPolicy,
        WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;

    #[test]
    fn validates_opaque_input_envelope() {
        validate_classifier_input(&json!({
            "schema": CLASSIFIER_INPUT_SCHEMA,
            "subject": {"kind": "caller_defined", "identity": "example:1"},
            "facts": {"anything": [1, 2, 3]},
            "provenance": {"source": "test"},
        }))
        .expect("input should be valid");
    }

    #[test]
    fn rejects_unknown_outer_fields() {
        assert!(validate_classifier_input(&json!({
            "schema": CLASSIFIER_INPUT_SCHEMA,
            "subject": {"kind": "caller_defined", "identity": "example:1"},
            "facts": {},
            "provenance": {},
            "unexpected": true,
        }))
        .is_err());
    }

    #[test]
    fn dispatch_snapshot_uses_pinned_policy_and_caller_input() -> anyhow::Result<()> {
        let policy = WorkflowDefinitionPolicy {
            id: "classifier_snapshot".to_string(),
            initial: "classifying".to_string(),
            states: BTreeMap::from([
                (
                    "classifying".to_string(),
                    DeclaredState {
                        activity: Some("classify".to_string()),
                        on_failure: Some("failed".to_string()),
                        on_signal: BTreeMap::from([("allow".to_string(), "done".to_string())]),
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
        let classifier = WorkflowClassifierPolicy {
            verdicts: vec!["allow".to_string()],
            instructions: vec!["Judge supplied facts.".to_string()],
        };
        let definition = super::super::build_declarative_definition(
            &policy,
            &BTreeMap::from([(
                "classify".to_string(),
                WorkflowActivityPolicy {
                    classifier: Some(classifier.clone()),
                    ..WorkflowActivityPolicy::default()
                },
            )]),
        )?;
        let input = json!({
            "schema": CLASSIFIER_INPUT_SCHEMA,
            "subject": {"kind": "test", "identity": "example:1"},
            "facts": {"scope": "bounded"},
            "provenance": {"source": "caller"}
        });
        let instance = super::super::WorkflowInstance::new(
            "classifier_snapshot",
            definition.definition_version(),
            "classifying",
            super::super::WorkflowSubject::new("test", "example:1"),
        )
        .with_server_data(json!({
            "definition_hash": definition.definition_hash(),
            "classifier_input": input,
        }));
        let mut registry = WorkflowDefinitionRegistry::new();
        registry.register_declarative_current(definition)?;

        let snapshot = classifier_job_snapshot(&registry, Some(&instance), "classify")?
            .ok_or_else(|| anyhow::anyhow!("classifier snapshot should exist"))?;

        assert_eq!(snapshot["policy"], serde_json::to_value(classifier)?);
        assert_eq!(snapshot["input"]["facts"]["scope"], "bounded");
        assert_eq!(snapshot["schema"], CLASSIFIER_JOB_SCHEMA);
        Ok(())
    }
}
