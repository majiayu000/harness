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
