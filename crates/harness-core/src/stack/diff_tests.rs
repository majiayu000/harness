use super::*;
use serde_json::json;

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn repository_source(locator: &str) -> AgentStackSource {
    AgentStackSource::new(AgentStackSourceScope::Repository, locator)
        .expect("valid repository source")
}

fn component(kind: AgentStackComponentKind, locator: &str) -> AgentStackComponent {
    AgentStackComponent::new(
        kind,
        repository_source(locator),
        AgentStackObservationClass::RepositoryObserved,
        AgentStackSelectionState::Selected,
        AgentStackTrustLevel::RepositoryObserved,
        AgentStackFreshness::Fresh,
    )
    .expect("valid component")
}

fn runtime_component(locator: &str) -> AgentStackComponent {
    AgentStackComponent::new(
        AgentStackComponentKind::AgentRuntime,
        AgentStackSource::logical(AgentStackSourceScope::Runtime, "runtime", locator)
            .expect("valid runtime source"),
        AgentStackObservationClass::RuntimeObserved,
        AgentStackSelectionState::Loaded,
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackFreshness::Fresh,
    )
    .expect("valid runtime component")
}

fn snapshot(components: &[AgentStackComponent]) -> AgentStackDiffSnapshot<'_> {
    AgentStackDiffSnapshot::from_components(components)
}

fn fact(
    facts: &[AgentStackDiffFact],
    field: AgentStackDiffField,
    fact_kind: AgentStackDiffFactKind,
) -> &AgentStackDiffFact {
    facts
        .iter()
        .find(|fact| fact.field() == field && fact.fact_kind() == fact_kind)
        .expect("expected diff fact")
}

#[test]
fn stack_diff_reports_no_change_and_reorder_as_empty() {
    let alpha = component(AgentStackComponentKind::Instructions, "AGENTS.md");
    let beta = runtime_component("codex-default");
    let before = vec![alpha.clone(), beta.clone()];
    let after = vec![beta, alpha];

    let facts = stack_diff(snapshot(&before), snapshot(&after)).expect("diff succeeds");

    assert!(facts.is_empty());
}

#[test]
fn stack_diff_reports_added_and_removed_components_in_stable_order() {
    let removed = component(AgentStackComponentKind::Skill, "skills/old/SKILL.md");
    let added = component(
        AgentStackComponentKind::Validation,
        ".github/workflows/ci.yml",
    );

    let facts = stack_diff(snapshot(&[removed]), snapshot(&[added])).expect("diff succeeds");

    assert_eq!(facts.len(), 2);
    assert_eq!(facts[0].change_kind(), AgentStackDiffChangeKind::Removed);
    assert_eq!(
        facts[0].component_id(),
        "repository:skill:skills/old/SKILL.md"
    );
    assert_eq!(facts[0].fact_kind(), AgentStackDiffFactKind::Context);
    assert_eq!(facts[1].change_kind(), AgentStackDiffChangeKind::Added);
    assert_eq!(
        facts[1].component_id(),
        "repository:validation:.github/workflows/ci.yml"
    );
    assert_eq!(facts[1].fact_kind(), AgentStackDiffFactKind::Validation);
}

#[test]
fn stack_diff_reports_typed_modification_facts() {
    let before_skill = AgentStackComponent::new(
        AgentStackComponentKind::Skill,
        repository_source("skills/example/SKILL.md"),
        AgentStackObservationClass::RepositoryObserved,
        AgentStackSelectionState::Selected,
        AgentStackTrustLevel::SelfDeclared,
        AgentStackFreshness::Fresh,
    )
    .unwrap()
    .with_integrity(Some(Sha256Digest::parse(HASH_A).unwrap()))
    .with_capabilities([AgentStackCapability::Network, AgentStackCapability::Shell])
    .unwrap();
    let after_skill = AgentStackComponent::new(
        AgentStackComponentKind::Skill,
        repository_source("skills/example/SKILL.md"),
        AgentStackObservationClass::RuntimeObserved,
        AgentStackSelectionState::Loaded,
        AgentStackTrustLevel::RuntimeObserved,
        AgentStackFreshness::Stale,
    )
    .unwrap()
    .with_integrity(Some(Sha256Digest::parse(HASH_B).unwrap()))
    .with_capabilities([
        AgentStackCapability::FileWrite,
        AgentStackCapability::Network,
    ])
    .unwrap();
    let before_validation = component(AgentStackComponentKind::Validation, "checks/lint.toml")
        .with_integrity(Some(Sha256Digest::parse(HASH_A).unwrap()));
    let after_validation = component(AgentStackComponentKind::Validation, "checks/lint.toml")
        .with_integrity(Some(Sha256Digest::parse(HASH_B).unwrap()));

    let before = vec![before_skill, before_validation];
    let after = vec![after_skill, after_validation];
    let facts = stack_diff(snapshot(&before), snapshot(&after)).expect("diff succeeds");

    assert_eq!(facts.len(), 8);
    assert_eq!(
        fact(
            &facts,
            AgentStackDiffField::ObservationClass,
            AgentStackDiffFactKind::Runtime
        )
        .change_kind(),
        AgentStackDiffChangeKind::Modified
    );
    assert_eq!(
        fact(
            &facts,
            AgentStackDiffField::SelectionState,
            AgentStackDiffFactKind::Runtime
        )
        .change_kind(),
        AgentStackDiffChangeKind::Modified
    );
    assert_eq!(
        fact(
            &facts,
            AgentStackDiffField::Integrity,
            AgentStackDiffFactKind::Context
        )
        .change_kind(),
        AgentStackDiffChangeKind::Modified
    );
    assert_eq!(
        fact(
            &facts,
            AgentStackDiffField::Integrity,
            AgentStackDiffFactKind::Validation
        )
        .component_kind(),
        AgentStackComponentKind::Validation
    );
    assert_eq!(
        fact(
            &facts,
            AgentStackDiffField::TrustLevel,
            AgentStackDiffFactKind::Trust
        )
        .change_kind(),
        AgentStackDiffChangeKind::Modified
    );
    assert_eq!(
        fact(
            &facts,
            AgentStackDiffField::Freshness,
            AgentStackDiffFactKind::Freshness
        )
        .change_kind(),
        AgentStackDiffChangeKind::Modified
    );
    assert!(facts.iter().any(|fact| {
        fact.change_kind() == AgentStackDiffChangeKind::Added
            && fact.fact_kind() == AgentStackDiffFactKind::Capability
            && fact.capability() == Some(AgentStackCapability::FileWrite)
    }));
    assert!(facts.iter().any(|fact| {
        fact.change_kind() == AgentStackDiffChangeKind::Removed
            && fact.fact_kind() == AgentStackDiffFactKind::Capability
            && fact.capability() == Some(AgentStackCapability::Shell)
    }));
}

#[test]
fn stack_diff_rejects_incompatible_versions() {
    let components = vec![component(
        AgentStackComponentKind::Skill,
        "skills/example/SKILL.md",
    )];
    let error = stack_diff(
        AgentStackDiffSnapshot::new(AGENT_STACK_COMPONENT_SCHEMA_VERSION, &components),
        AgentStackDiffSnapshot::new("agent-stack-component/v0.2", &components),
    )
    .expect_err("incompatible versions must fail");

    assert_eq!(
        error,
        AgentStackDiffError::IncompatibleSchemaVersions {
            before_schema_version: AGENT_STACK_COMPONENT_SCHEMA_VERSION.to_owned(),
            after_schema_version: "agent-stack-component/v0.2".to_owned(),
        }
    );
}

#[test]
fn stack_diff_rejects_duplicate_component_ids() {
    let duplicate = component(AgentStackComponentKind::Skill, "skills/example/SKILL.md");
    let components = vec![duplicate.clone(), duplicate];

    let error = stack_diff(snapshot(&components), snapshot(&[])).expect_err("duplicates fail");

    assert_eq!(
        error,
        AgentStackDiffError::DuplicateComponentId {
            side: AgentStackDiffSide::Before,
            component_id: "repository:skill:skills/example/SKILL.md".to_owned(),
        }
    );
}

#[test]
fn stack_diff_serializes_facts_without_free_form_change_text() {
    let before = vec![
        component(AgentStackComponentKind::Skill, "skills/example/SKILL.md")
            .with_integrity(Some(Sha256Digest::parse(HASH_A).unwrap())),
    ];
    let after = vec![
        component(AgentStackComponentKind::Skill, "skills/example/SKILL.md")
            .with_integrity(Some(Sha256Digest::parse(HASH_B).unwrap())),
    ];

    let facts = stack_diff(snapshot(&before), snapshot(&after)).expect("diff succeeds");
    let encoded = serde_json::to_value(&facts).expect("facts serialize");

    assert_eq!(
        encoded,
        json!([
            {
                "change_kind": "modified",
                "fact_kind": "context",
                "component_id": "repository:skill:skills/example/SKILL.md",
                "component_kind": "skill",
                "source_scope": "repository",
                "source_locator": "skills/example/SKILL.md",
                "field": "integrity",
                "before": {
                    "value_type": "integrity",
                    "value": HASH_A
                },
                "after": {
                    "value_type": "integrity",
                    "value": HASH_B
                }
            }
        ])
    );
}
