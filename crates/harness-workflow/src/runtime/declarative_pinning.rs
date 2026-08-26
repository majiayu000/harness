use super::{
    declarative::{
        build_declarative_definition, build_declarative_definition_with_classifier_policies,
        DeclarativeWorkflowDefinition,
    },
    model::WorkflowDefinition as DurableWorkflowDefinition,
    remote_facts::stable_remote_fact_hash,
};
use harness_core::config::workflow::{WorkflowActivityPolicy, WorkflowDefinitionPolicy};
use std::collections::BTreeMap;

pub(super) const DECLARATIVE_DEFINITION_METADATA_KIND: &str = "declarative_workflow";
const METADATA_SCHEMA_VERSION: u64 = 1;

pub fn declarative_definition_identity(
    policy: &WorkflowDefinitionPolicy,
) -> anyhow::Result<(u32, String)> {
    let policy_json = serde_json::to_value(policy)?;
    let definition_hash = stable_remote_fact_hash(&policy_json);
    let version_hex = definition_hash
        .get(definition_hash.len().saturating_sub(8)..)
        .ok_or_else(|| anyhow::anyhow!("declarative definition hash is too short"))?;
    let definition_version = u32::from_str_radix(version_hex, 16).map_err(|error| {
        anyhow::anyhow!(
            "declarative definition hash '{}' has an invalid version suffix: {error}",
            definition_hash
        )
    })?;
    Ok((definition_version, definition_hash))
}

pub(super) fn declarative_definition_identity_with_classifier_policies(
    policy: &WorkflowDefinitionPolicy,
    classifier_activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<(u32, String)> {
    if classifier_activity_policies.is_empty() {
        return declarative_definition_identity(policy);
    }
    let identity = serde_json::json!({
        "policy": policy,
        "classifier_activity_policies": classifier_activity_policies,
    });
    let definition_hash = stable_remote_fact_hash(&identity);
    let version_hex = definition_hash
        .get(definition_hash.len().saturating_sub(8)..)
        .ok_or_else(|| anyhow::anyhow!("declarative definition hash is too short"))?;
    let definition_version = u32::from_str_radix(version_hex, 16).map_err(|error| {
        anyhow::anyhow!(
            "declarative definition hash '{}' has an invalid version suffix: {error}",
            definition_hash
        )
    })?;
    Ok((definition_version, definition_hash))
}

pub fn persisted_declarative_definition(
    definition: &DeclarativeWorkflowDefinition,
    source_path: Option<&str>,
) -> DurableWorkflowDefinition {
    let mut persisted = DurableWorkflowDefinition::new(
        definition.policy().id.clone(),
        definition.definition_version(),
        definition.policy().id.clone(),
    )
    .with_definition_hash(definition.definition_hash())
    .with_metadata(serde_json::json!({
        "kind": DECLARATIVE_DEFINITION_METADATA_KIND,
        "schema_version": METADATA_SCHEMA_VERSION,
        "policy": definition.policy(),
        "classifier_activities": definition.classifier_activities(),
        "classifier_activity_policies": definition.classifier_activity_policies(),
    }));
    if let Some(source_path) = source_path {
        persisted = persisted.with_source_path(source_path);
    }
    persisted
}

pub fn hydrate_declarative_definition(
    definition: &DurableWorkflowDefinition,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    let policy = persisted_declarative_policy(definition)?;
    let hydrated = build_declarative_definition(&policy, activity_policies)?;
    if let Some(persisted) = persisted_classifier_activity_policies(definition)? {
        if persisted != *hydrated.classifier_activity_policies() {
            anyhow::bail!(
                "persisted declarative workflow definition '{}@{}' classifier policies do not match the supplied activity policies",
                definition.id,
                definition.version
            );
        }
    }
    validate_hydrated_identity(definition, hydrated)
}

pub fn hydrate_persisted_declarative_definition(
    definition: &DurableWorkflowDefinition,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    let policy = persisted_declarative_policy(definition)?;
    let mut activity_policies: BTreeMap<String, WorkflowActivityPolicy> = policy
        .states
        .values()
        .filter_map(|state| state.activity.as_ref())
        .map(|activity| (activity.clone(), WorkflowActivityPolicy::default()))
        .collect();
    let classifier_activity_policies =
        persisted_classifier_activity_policies(definition)?.unwrap_or_default();
    for (activity, activity_policy) in &classifier_activity_policies {
        activity_policies.insert(activity.clone(), activity_policy.clone());
    }
    let hydrated = build_declarative_definition_with_classifier_policies(
        &policy,
        &activity_policies,
        classifier_activity_policies,
    )?;
    validate_hydrated_identity(definition, hydrated)
}

fn persisted_classifier_activity_policies(
    definition: &DurableWorkflowDefinition,
) -> anyhow::Result<Option<BTreeMap<String, WorkflowActivityPolicy>>> {
    definition
        .metadata
        .get("classifier_activity_policies")
        .map(|value| serde_json::from_value(value.clone()).map_err(Into::into))
        .transpose()
}

fn persisted_declarative_policy(
    definition: &DurableWorkflowDefinition,
) -> anyhow::Result<WorkflowDefinitionPolicy> {
    let metadata = definition.metadata.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "persisted workflow definition '{}@{}' metadata must be an object",
            definition.id,
            definition.version
        )
    })?;
    if metadata.get("kind").and_then(serde_json::Value::as_str)
        != Some(DECLARATIVE_DEFINITION_METADATA_KIND)
    {
        anyhow::bail!(
            "persisted workflow definition '{}@{}' is not a declarative workflow definition",
            definition.id,
            definition.version
        );
    }
    if metadata
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(METADATA_SCHEMA_VERSION)
    {
        anyhow::bail!(
            "persisted declarative workflow definition '{}@{}' has an unsupported metadata schema",
            definition.id,
            definition.version
        );
    }
    let policy_value = metadata.get("policy").ok_or_else(|| {
        anyhow::anyhow!(
            "persisted declarative workflow definition '{}@{}' is missing policy metadata",
            definition.id,
            definition.version
        )
    })?;
    serde_json::from_value(policy_value.clone()).map_err(Into::into)
}

fn validate_hydrated_identity(
    definition: &DurableWorkflowDefinition,
    hydrated: DeclarativeWorkflowDefinition,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    if hydrated.policy().id != definition.id {
        anyhow::bail!(
            "persisted declarative workflow definition id '{}' does not match policy id '{}'",
            definition.id,
            hydrated.policy().id
        );
    }
    if hydrated.definition_version() != definition.version {
        anyhow::bail!(
            "persisted declarative workflow definition '{}@{}' version does not match canonical policy version {}",
            definition.id,
            definition.version,
            hydrated.definition_version()
        );
    }
    if hydrated.definition_hash() != definition.definition_hash {
        anyhow::bail!(
            "persisted declarative workflow definition '{}@{}' hash '{}' does not match canonical policy hash '{}'",
            definition.id,
            definition.version,
            definition.definition_hash,
            hydrated.definition_hash()
        );
    }
    Ok(hydrated)
}
