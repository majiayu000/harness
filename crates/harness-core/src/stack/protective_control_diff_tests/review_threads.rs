use super::*;

#[test]
fn counts_partial_role_replacement_claims() {
    let before = [
        configured_control(
            Kind::Validation,
            ".github/workflows/retained.yml",
            Some(HASH_A),
            &[Role::Validation, Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/removed.yml",
            Some(HASH_A),
            &[Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/retained.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/replacement.yml",
            Some(HASH_B),
            &[Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 2);
    assert!(facts.iter().all(|fact| {
        fact.kind() == DiffKind::AmbiguousReviewEvidence
            && fact.reason() == Reason::AmbiguousReplacement
            && fact.roles() == [Role::Check]
    }));
    assert!(facts.iter().any(|fact| {
        fact.before()
            .is_some_and(|evidence| evidence.source_locator() == ".github/workflows/removed.yml")
    }));
}

#[test]
fn keeps_uncovered_roles_with_ambiguous_renames() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/renamed-a.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/renamed-b.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(
        kinds(&facts),
        [DiffKind::AmbiguousReviewEvidence, DiffKind::Removed]
    );
    assert_eq!(facts[0].reason(), Reason::PossibleRename);
    assert_eq!(facts[1].reason(), Reason::RemovedWithoutEquivalent);
    assert_eq!(facts[1].roles(), [Role::Check]);
}

#[test]
fn suppresses_disablement_with_unique_replacement() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/legacy.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            DISABLED_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/replacement.yml",
            Some(HASH_B),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];

    let facts = protective_control_diff(&before, &after);

    assert!(facts.is_empty());
}

#[test]
fn keeps_removal_with_before_conflict() {
    let before = [
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ControlState(Some(true), Some(Scope::Partial), Some(Mode::FailClosed)),
        ),
    ];

    let facts = protective_control_diff(&before, &[]);

    assert_eq!(
        kinds(&facts),
        [DiffKind::AmbiguousReviewEvidence, DiffKind::Removed]
    );
    assert_eq!(facts[0].reason(), Reason::ConflictingDuplicateReport);
    assert_eq!(facts[1].reason(), Reason::RemovedWithoutEquivalent);
    assert_eq!(facts[1].roles(), [Role::Validation]);
}

#[test]
fn reviews_shared_replacement_across_retained_controls() {
    let before = [
        configured_control(
            Kind::Validation,
            ".github/workflows/a.yml",
            Some(HASH_A),
            &[Role::Validation, Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/b.yml",
            Some(HASH_A),
            &[Role::Policy, Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/a.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/b.yml",
            Some(HASH_A),
            &[Role::Policy],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/replacement.yml",
            Some(HASH_B),
            &[Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 2);
    assert!(facts.iter().all(|fact| {
        fact.kind() == DiffKind::AmbiguousReviewEvidence
            && fact.reason() == Reason::AmbiguousReplacement
            && fact.roles() == [Role::Check]
    }));
}

#[test]
fn before_enablement_conflict_does_not_claim_definite_removal() {
    let before = [
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            DISABLED_REQUIRED,
        ),
    ];

    let facts = protective_control_diff(&before, &[]);

    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::ConflictingDuplicateReport]);
}

#[test]
fn classifies_non_unique_disablement_replacements_as_ambiguous() {
    let legacy = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let disabled = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        DISABLED_REQUIRED,
    );
    let replacement_a = configured_control(
        Kind::Validation,
        ".github/workflows/replacement-a.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let replacement_b = configured_control(
        Kind::Validation,
        ".github/workflows/replacement-b.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    let facts = protective_control_diff(
        std::slice::from_ref(&legacy),
        &[disabled.clone(), replacement_a.clone(), replacement_b],
    );
    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::AmbiguousReplacement]);

    let removed = configured_control(
        Kind::Validation,
        ".github/workflows/removed.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let facts = protective_control_diff(&[legacy, removed], &[disabled, replacement_a]);
    assert_eq!(facts.len(), 2);
    assert!(facts.iter().all(|fact| {
        fact.kind() == DiffKind::AmbiguousReviewEvidence
            && fact.reason() == Reason::AmbiguousReplacement
    }));
}

#[test]
fn unique_replacement_suppresses_inactive_legacy_state_changes() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/legacy.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            DISABLED_PARTIAL_OPEN,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/replacement.yml",
            Some(HASH_B),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];

    assert!(protective_control_diff(&before, &after).is_empty());
}

#[test]
fn after_enablement_conflict_does_not_claim_definite_disablement() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let after = [
        before[0].clone(),
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            DISABLED_REQUIRED,
        ),
    ];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::ConflictingDuplicateReport]);
}

#[test]
fn counts_supplemental_roles_used_by_partial_renames() {
    let before = [
        configured_control(
            Kind::Validation,
            ".github/workflows/multi-role.yml",
            Some(HASH_A),
            &[Role::Validation, Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/removed-check.yml",
            Some(HASH_A),
            &[Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/renamed.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/supplement.yml",
            Some(HASH_B),
            &[Role::Check],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 3);
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.reason() == Reason::PossibleRename)
            .count(),
        1
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.reason() == Reason::AmbiguousReplacement)
            .count(),
        2
    );
}

#[test]
fn merges_known_integrity_independently_of_duplicate_order() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let missing = configured_control(
        Kind::Validation,
        ".github/workflows/renamed.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let known = configured_control(
        Kind::Validation,
        ".github/workflows/renamed.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    let missing_first = protective_control_diff(&before, &[missing.clone(), known.clone()]);
    let known_first = protective_control_diff(&before, &[known, missing]);

    assert_eq!(missing_first, known_first);
    assert_eq!(reasons(&missing_first), [Reason::PossibleRename]);
}

#[test]
fn before_scope_conflict_does_not_claim_definite_reduction() {
    let before = [
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
        ),
    ];
    let after = [configured_control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Partial), Some(Mode::FailClosed)),
    )];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::ConflictingDuplicateReport]);
}

#[test]
fn before_failure_mode_conflict_does_not_claim_definite_relaxation() {
    let before = [
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/ci.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ControlState(Some(true), Some(Scope::Required), Some(Mode::FailOpen)),
        ),
    ];
    let after = [configured_control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Required), Some(Mode::FailOpen)),
    )];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::ConflictingDuplicateReport]);
}

#[test]
fn merges_duplicate_capabilities_independently_of_order() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let shell = configured_control_with_capabilities(
        ".github/workflows/ci.yml",
        &[AgentStackCapability::Shell],
    );
    let network = configured_control_with_capabilities(
        ".github/workflows/ci.yml",
        &[AgentStackCapability::Network],
    );

    let shell_first = protective_control_diff(&before, &[shell.clone(), network.clone()]);
    let network_first = protective_control_diff(&before, &[network.clone(), shell.clone()]);

    assert_eq!(shell_first, network_first);
    let Some(after) = shell_first.first().and_then(Diff::after) else {
        panic!("scope reduction after evidence");
    };
    assert_eq!(
        after.capabilities(),
        [AgentStackCapability::Network, AgentStackCapability::Shell]
    );

    let after = [configured_control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    )];
    let shell_first = protective_control_diff(&[shell.clone(), network.clone()], &after);
    let network_first = protective_control_diff(&[network, shell], &after);
    assert_eq!(shell_first, network_first);
    let Some(before) = shell_first.first().and_then(Diff::before) else {
        panic!("scope reduction before evidence");
    };
    assert_eq!(
        before.capabilities(),
        [AgentStackCapability::Network, AgentStackCapability::Shell]
    );
}

#[test]
fn stronger_equivalent_replacement_precedes_weaker_rename_candidate() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/weak-rename.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::Low,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/replacement.yml",
            Some(HASH_B),
            &[Role::Validation],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
    ];

    assert_eq!(
        reasons(&protective_control_diff(&before, &after)),
        [Reason::PossibleRename]
    );
}

fn configured_control_with_capabilities(
    locator: &str,
    capabilities: &[AgentStackCapability],
) -> AgentStackProtectionControl {
    let component = match component(Kind::Validation, locator, Some(HASH_A))
        .with_capabilities(capabilities.iter().copied())
    {
        Ok(component) => component,
        Err(error) => panic!("valid capabilities: {error}"),
    };
    let control =
        match AgentStackProtectionControl::new(component, [Role::Validation], Confidence::High) {
            Ok(control) => control,
            Err(error) => panic!("valid protection control: {error}"),
        };
    control
        .with_enabled(true)
        .with_scope(Scope::Partial)
        .with_failure_mode(Mode::FailClosed)
}
