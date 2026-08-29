use super::declarative_agent_contract::PinnedAgentContractActivity;
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
/// Metadata layout for definitions without agent contracts. Unchanged from the
/// pre-contract runtime so existing persisted rows keep hydrating.
const METADATA_SCHEMA_VERSION_V1: u64 = 1;
/// Metadata layout that adds the resolved `agent_contracts` map. A reader that
/// does not know this version fails explicitly instead of recomputing a hash
/// without the contracts.
const METADATA_SCHEMA_VERSION_V2: u64 = 2;
/// Metadata layout that also records classifier activity policies, which
/// participate in the pinned definition identity.
const METADATA_SCHEMA_VERSION_V3: u64 = 3;

pub fn declarative_definition_identity(
    policy: &WorkflowDefinitionPolicy,
    activity_contracts: &BTreeMap<String, PinnedAgentContractActivity>,
    classifier_activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<(u32, String)> {
    // Definitions without agent contracts keep the original policy-only hash
    // so every existing pinned instance and persisted definition stays valid.
    let identity_json = if activity_contracts.is_empty() && classifier_activity_policies.is_empty()
    {
        serde_json::to_value(policy)?
    } else if classifier_activity_policies.is_empty() {
        serde_json::json!({
            "schema": "declarative_workflow_identity.v2",
            "policy": policy,
            "agent_contracts": activity_contracts,
        })
    } else {
        serde_json::json!({
            "schema": "declarative_workflow_identity.v3",
            "policy": policy,
            "agent_contracts": activity_contracts,
            "classifier_activity_policies": classifier_activity_policies,
        })
    };
    let definition_hash = stable_remote_fact_hash(&identity_json);
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
    let metadata = if definition.activity_contracts().is_empty()
        && definition.classifier_activity_policies().is_empty()
    {
        serde_json::json!({
            "kind": DECLARATIVE_DEFINITION_METADATA_KIND,
            "schema_version": METADATA_SCHEMA_VERSION_V1,
            "policy": definition.policy(),
        })
    } else if definition.classifier_activity_policies().is_empty() {
        serde_json::json!({
            "kind": DECLARATIVE_DEFINITION_METADATA_KIND,
            "schema_version": METADATA_SCHEMA_VERSION_V2,
            "policy": definition.policy(),
            "agent_contracts": definition.activity_contracts(),
        })
    } else {
        serde_json::json!({
            "kind": DECLARATIVE_DEFINITION_METADATA_KIND,
            "schema_version": METADATA_SCHEMA_VERSION_V3,
            "policy": definition.policy(),
            "agent_contracts": definition.activity_contracts(),
            "classifier_activities": definition.classifier_activities(),
            "classifier_activity_policies": definition.classifier_activity_policies(),
        })
    };
    let mut persisted = DurableWorkflowDefinition::new(
        definition.policy().id.clone(),
        definition.definition_version(),
        definition.policy().id.clone(),
    )
    .with_definition_hash(definition.definition_hash())
    .with_metadata(metadata);
    if let Some(source_path) = source_path {
        persisted = persisted.with_source_path(source_path);
    }
    persisted
}

pub fn hydrate_declarative_definition(
    definition: &DurableWorkflowDefinition,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    let persisted = persisted_declarative_policy(definition)?;
    let hydrated = hydrate_declarative_definition_with_policy(
        definition,
        &persisted.policy,
        activity_policies,
    )?;
    if let Some(persisted) = persisted_classifier_activity_policies(definition)? {
        if persisted != *hydrated.classifier_activity_policies() {
            anyhow::bail!(
                "persisted declarative workflow definition '{}@{}' classifier policies do not match the supplied activity policies",
                definition.id,
                definition.version
            );
        }
    }
    Ok(hydrated)
}

pub fn hydrate_persisted_declarative_definition(
    definition: &DurableWorkflowDefinition,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    let persisted = persisted_declarative_policy(definition)?;
    let mut activity_policies: BTreeMap<String, WorkflowActivityPolicy> = persisted
        .policy
        .states
        .values()
        .filter_map(|state| state.activity.as_ref())
        .map(|activity| {
            let pinned = persisted.agent_contracts.get(activity);
            let activity_policy = WorkflowActivityPolicy {
                prompt: pinned.map(|pinned| pinned.prompt.clone()),
                agent_contract: pinned.map(|pinned| pinned.contract.clone()),
                ..WorkflowActivityPolicy::default()
            };
            (activity.clone(), activity_policy)
        })
        .collect();
    let classifier_activity_policies =
        persisted_classifier_activity_policies(definition)?.unwrap_or_default();
    for (activity, activity_policy) in &classifier_activity_policies {
        activity_policies.insert(activity.clone(), activity_policy.clone());
    }
    let hydrated = build_declarative_definition_with_classifier_policies(
        &persisted.policy,
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

struct PersistedDeclarativePolicy {
    policy: WorkflowDefinitionPolicy,
    agent_contracts: BTreeMap<String, PinnedAgentContractActivity>,
}

fn persisted_declarative_policy(
    definition: &DurableWorkflowDefinition,
) -> anyhow::Result<PersistedDeclarativePolicy> {
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
    let schema_version = metadata
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if schema_version != Some(METADATA_SCHEMA_VERSION_V1)
        && schema_version != Some(METADATA_SCHEMA_VERSION_V2)
        && schema_version != Some(METADATA_SCHEMA_VERSION_V3)
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
    let policy = serde_json::from_value(policy_value.clone())?;
    let agent_contracts = match metadata.get("agent_contracts") {
        Some(contracts_value) => serde_json::from_value(contracts_value.clone())?,
        None => BTreeMap::new(),
    };
    Ok(PersistedDeclarativePolicy {
        policy,
        agent_contracts,
    })
}

fn hydrate_declarative_definition_with_policy(
    definition: &DurableWorkflowDefinition,
    policy: &WorkflowDefinitionPolicy,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<DeclarativeWorkflowDefinition> {
    let hydrated = build_declarative_definition(policy, activity_policies)?;
    validate_hydrated_identity(definition, hydrated)
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
