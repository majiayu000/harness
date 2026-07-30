use super::*;

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn source(locator: &str) -> AgentStackSource {
    match AgentStackSource::new(AgentStackSourceScope::Repository, locator) {
        Ok(source) => source,
        Err(error) => panic!("valid source: {error}"),
    }
}

fn component(
    kind: AgentStackComponentKind,
    locator: &str,
    integrity: Option<&str>,
) -> AgentStackComponent {
    let component = match AgentStackComponent::new(
        kind,
        source(locator),
        AgentStackObservationClass::RepositoryObserved,
        AgentStackSelectionState::Discovered,
        AgentStackTrustLevel::RepositoryObserved,
        AgentStackFreshness::Fresh,
    ) {
        Ok(component) => component,
        Err(error) => panic!("valid component: {error}"),
    };
    let integrity = integrity.map(|digest| match Sha256Digest::parse(digest) {
        Ok(parsed) => parsed,
        Err(error) => panic!("valid digest: {error}"),
    });
    component.with_integrity(integrity)
}

fn control(
    kind: AgentStackComponentKind,
    locator: &str,
    roles: &[AgentStackProtectionRole],
) -> AgentStackProtectionControl {
    control_with_confidence(kind, locator, roles, AgentStackProtectionConfidence::High)
}

fn control_with_confidence(
    kind: AgentStackComponentKind,
    locator: &str,
    roles: &[AgentStackProtectionRole],
    confidence: AgentStackProtectionConfidence,
) -> AgentStackProtectionControl {
    match AgentStackProtectionControl::new(
        component(kind, locator, Some(HASH_A)),
        roles.iter().copied(),
        confidence,
    ) {
        Ok(control) => control
            .with_enabled(true)
            .with_scope(AgentStackProtectionScope::Required)
            .with_failure_mode(AgentStackProtectionFailureMode::FailClosed),
        Err(error) => panic!("valid protection control: {error}"),
    }
}

#[test]
fn protective_control_diff_defines_all_protection_roles() {
    assert_eq!(
        AgentStackProtectionRole::ALL
            .iter()
            .map(AgentStackProtectionRole::as_str)
            .collect::<Vec<_>>(),
        ["policy", "hook", "validation", "sandboxing", "check"]
    );
}

#[test]
fn protective_control_diff_ignores_benign_hook_removal_without_role_evidence() {
    let before = [control(
        AgentStackComponentKind::Hook,
        ".githooks/pre-commit",
        &[],
    )];
    let facts = protective_control_diff(&before, &[]);
    assert!(
        facts.is_empty(),
        "hook kind alone must not imply protection"
    );
}

#[test]
fn protective_control_diff_reports_protective_policy_removal() {
    let before = [control(
        AgentStackComponentKind::Policy,
        "rules/protect.toml",
        &[AgentStackProtectionRole::Policy],
    )];
    let facts = protective_control_diff(&before, &[]);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), AgentStackProtectionDiffKind::Removed);
    assert_eq!(
        facts[0].reason(),
        AgentStackProtectionControlReason::RemovedWithoutEquivalent
    );
    assert_eq!(facts[0].confidence(), AgentStackProtectionConfidence::High);
    let Some(before) = facts[0].before() else {
        panic!("before evidence");
    };
    assert_eq!(before.source_locator(), "rules/protect.toml");
    assert!(facts[0].after().is_none());
}

#[test]
fn protective_control_diff_reports_relaxed_existing_controls() {
    let before = [control(
        AgentStackComponentKind::Validation,
        ".github/workflows/ci.yml",
        &[
            AgentStackProtectionRole::Validation,
            AgentStackProtectionRole::Sandboxing,
            AgentStackProtectionRole::Check,
        ],
    )];
    let relaxed = match AgentStackProtectionControl::new(
        component(
            AgentStackComponentKind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_B),
        ),
        [AgentStackProtectionRole::Validation],
        AgentStackProtectionConfidence::High,
    ) {
        Ok(control) => control
            .with_enabled(false)
            .with_scope(AgentStackProtectionScope::Partial)
            .with_failure_mode(AgentStackProtectionFailureMode::FailOpen),
        Err(error) => panic!("valid control: {error}"),
    };
    let after = [relaxed];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(
        facts
            .iter()
            .map(AgentStackProtectionControlDiff::kind)
            .collect::<Vec<_>>(),
        [
            AgentStackProtectionDiffKind::Disabled,
            AgentStackProtectionDiffKind::FailOpen,
            AgentStackProtectionDiffKind::ScopeReduced,
            AgentStackProtectionDiffKind::ScopeReduced,
        ]
    );
    assert!(facts
        .iter()
        .all(|fact| fact.before().is_some() && fact.after().is_some()));
    assert!(facts
        .iter()
        .any(|fact| fact.reason() == AgentStackProtectionControlReason::RoleSetReduced));
    assert!(facts
        .iter()
        .any(|fact| fact.reason() == AgentStackProtectionControlReason::ScopeLevelReduced));
}

#[test]
fn protective_control_diff_treats_rename_as_review_evidence() {
    let before = [control(
        AgentStackComponentKind::Policy,
        "rules/protect.toml",
        &[AgentStackProtectionRole::Policy],
    )];
    let after = [control(
        AgentStackComponentKind::Policy,
        "rules/renamed.toml",
        &[AgentStackProtectionRole::Policy],
    )];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 1);
    assert_eq!(
        facts[0].kind(),
        AgentStackProtectionDiffKind::AmbiguousReviewEvidence
    );
    assert_eq!(
        facts[0].reason(),
        AgentStackProtectionControlReason::PossibleRename
    );
    assert_eq!(
        facts[0].confidence(),
        AgentStackProtectionConfidence::Medium
    );
    let Some(after) = facts[0].after() else {
        panic!("after evidence");
    };
    assert_eq!(after.source_locator(), "rules/renamed.toml");
}

#[test]
fn protective_control_diff_suppresses_equivalent_replacement() {
    let before = [control(
        AgentStackComponentKind::Hook,
        ".githooks/pre-push",
        &[AgentStackProtectionRole::Hook],
    )];
    let replacement = match AgentStackProtectionControl::new(
        component(
            AgentStackComponentKind::Hook,
            ".harness/guards/pre-push.sh",
            Some(HASH_B),
        ),
        [AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::High,
    ) {
        Ok(control) => control
            .with_enabled(true)
            .with_scope(AgentStackProtectionScope::Comprehensive)
            .with_failure_mode(AgentStackProtectionFailureMode::FailClosed),
        Err(error) => panic!("valid replacement: {error}"),
    };
    let after = [replacement];
    let facts = protective_control_diff(&before, &after);
    assert!(
        facts.is_empty(),
        "same role with equal-or-stronger state is not a weakening fact"
    );
}

#[test]
fn protective_control_diff_reports_weakened_rename_state() {
    let before = [control(
        AgentStackComponentKind::Hook,
        ".githooks/pre-push",
        &[AgentStackProtectionRole::Hook],
    )];
    let renamed = match AgentStackProtectionControl::new(
        component(
            AgentStackComponentKind::Hook,
            ".harness/guards/pre-push.sh",
            Some(HASH_A),
        ),
        [AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::High,
    ) {
        Ok(control) => control
            .with_enabled(false)
            .with_scope(AgentStackProtectionScope::Partial)
            .with_failure_mode(AgentStackProtectionFailureMode::FailOpen),
        Err(error) => panic!("valid renamed control: {error}"),
    };
    let after = [renamed];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(
        facts
            .iter()
            .map(AgentStackProtectionControlDiff::reason)
            .collect::<Vec<_>>(),
        [
            AgentStackProtectionControlReason::PossibleRename,
            AgentStackProtectionControlReason::ExplicitlyDisabled,
            AgentStackProtectionControlReason::FailureModeRelaxed,
            AgentStackProtectionControlReason::ScopeLevelReduced,
        ]
    );
}

#[test]
fn protective_control_diff_excludes_unchanged_controls_from_rename_candidates() {
    let removed = control(
        AgentStackComponentKind::Policy,
        "rules/deleted.toml",
        &[AgentStackProtectionRole::Policy],
    );
    let retained_before = control(
        AgentStackComponentKind::Policy,
        "rules/retained.toml",
        &[AgentStackProtectionRole::Policy],
    );
    let retained_after = control(
        AgentStackComponentKind::Policy,
        "rules/retained.toml",
        &[AgentStackProtectionRole::Policy],
    );
    let before = [removed, retained_before];
    let after = [retained_after];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), AgentStackProtectionDiffKind::Removed);
    assert_eq!(
        facts[0].reason(),
        AgentStackProtectionControlReason::RemovedWithoutEquivalent
    );
}

#[test]
fn protective_control_diff_ignores_removed_controls_already_disabled() {
    let before = [control(
        AgentStackComponentKind::Policy,
        "rules/disabled.toml",
        &[AgentStackProtectionRole::Policy],
    )
    .with_enabled(false)];

    let facts = protective_control_diff(&before, &[]);

    assert!(
        facts.is_empty(),
        "removing an explicitly disabled control is not a weakening"
    );
}

#[test]
fn protective_control_diff_keeps_low_confidence_replacements_ambiguous() {
    let before = [control(
        AgentStackComponentKind::Hook,
        ".githooks/pre-commit",
        &[AgentStackProtectionRole::Hook],
    )];
    let replacement = match AgentStackProtectionControl::new(
        component(
            AgentStackComponentKind::Hook,
            ".harness/guards/pre-commit.sh",
            Some(HASH_B),
        ),
        [AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::Low,
    ) {
        Ok(control) => control
            .with_enabled(true)
            .with_scope(AgentStackProtectionScope::Required)
            .with_failure_mode(AgentStackProtectionFailureMode::FailClosed),
        Err(error) => panic!("valid low-confidence replacement: {error}"),
    };
    let after = [replacement];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 1);
    assert_eq!(
        facts[0].kind(),
        AgentStackProtectionDiffKind::AmbiguousReviewEvidence
    );
    assert_eq!(
        facts[0].reason(),
        AgentStackProtectionControlReason::AmbiguousReplacement
    );
    assert_eq!(facts[0].confidence(), AgentStackProtectionConfidence::Low);
}

#[test]
fn protective_control_diff_compares_all_duplicate_after_component_ids() {
    let before = [control(
        AgentStackComponentKind::Validation,
        ".github/workflows/ci.yml",
        &[AgentStackProtectionRole::Validation],
    )];
    let enabled = control(
        AgentStackComponentKind::Validation,
        ".github/workflows/ci.yml",
        &[AgentStackProtectionRole::Validation],
    );
    let disabled = control(
        AgentStackComponentKind::Validation,
        ".github/workflows/ci.yml",
        &[AgentStackProtectionRole::Validation],
    )
    .with_enabled(false);
    let first_order = [enabled.clone(), disabled.clone()];
    let second_order = [disabled, enabled];

    for after in [&first_order, &second_order] {
        let facts = protective_control_diff(&before, after);
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].reason(),
            AgentStackProtectionControlReason::ExplicitlyDisabled
        );
    }
}
