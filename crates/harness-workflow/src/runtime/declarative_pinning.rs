use super::{
    declarative::{build_declarative_definition, DeclarativeWorkflowDefinition},
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
    declarative_definition_identity_with_classifier_policies(policy, &BTreeMap::new())
}

pub(super) fn declarative_definition_identity_with_classifier_policies(
    policy: &WorkflowDefinitionPolicy,
    classifier_activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<(u32, String)> {
    let identity = if classifier_activity_policies.is_empty() {
        serde_json::to_value(policy)?
    } else {
        serde_json::json!({
            "policy": policy,
            "classifier_activity_policies": classifier_activity_policies,
        })
    };
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
    hydrate_declarative_definition_with_policy(definition, &policy, activity_policies)
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
    if let Some(policies) = definition
        .metadata
        .get("classifier_activity_policies")
        .map(|value| {
            serde_json::from_value::<BTreeMap<String, WorkflowActivityPolicy>>(value.clone())
        })
        .transpose()?
    {
        activity_policies.extend(policies);
    }
    hydrate_declarative_definition_with_policy(definition, &policy, &activity_policies)
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

fn hydrate_declarative_definition_with_policy(
    definition: &DurableWorkflowDefinition,
    policy: &WorkflowDefinitionPolicy,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    let hydrated = build_declarative_definition(policy, activity_policies)?;
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
