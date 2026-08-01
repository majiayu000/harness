use super::*;

#[test]
fn enablement_conflict_does_not_claim_role_loss_from_disabled_evidence() {
    let before = [
        configured_control(
            Kind::Policy,
            "rules/protect.toml",
            Some(HASH_A),
            &[Role::Policy],
            Confidence::High,
            ACTIVE_REQUIRED,
        ),
        configured_control(
            Kind::Policy,
            "rules/protect.toml",
            Some(HASH_A),
            &[Role::Hook],
            Confidence::High,
            DISABLED_REQUIRED,
        ),
    ];
    let after = [configured_control(
        Kind::Policy,
        "rules/protect.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::ConflictingDuplicateReport]);
}

#[test]
fn unique_replacement_suppresses_standalone_state_reductions() {
    let legacy = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let replacement = configured_control(
        Kind::Validation,
        ".github/workflows/replacement.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let advisory_legacy = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );
    let fail_open_legacy = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Required), Some(Mode::FailOpen)),
    );

    assert!(protective_control_diff(
        std::slice::from_ref(&legacy),
        &[advisory_legacy, replacement.clone()],
    )
    .is_empty());
    assert!(protective_control_diff(&[legacy], &[fail_open_legacy, replacement]).is_empty());
}

#[test]
fn conflicted_control_counts_toward_shared_replacement_use() {
    let normal_before = configured_control(
        Kind::Validation,
        ".github/workflows/normal.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let conflict_active = configured_control(
        Kind::Validation,
        ".github/workflows/conflict.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let conflict_disabled = configured_control(
        Kind::Validation,
        ".github/workflows/conflict.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        DISABLED_REQUIRED,
    );
    let after = [
        configured_control(
            Kind::Validation,
            ".github/workflows/normal.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
        ),
        configured_control(
            Kind::Validation,
            ".github/workflows/conflict.yml",
            Some(HASH_A),
            &[Role::Validation],
            Confidence::High,
            ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
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
    let active_first = protective_control_diff(
        &[
            normal_before.clone(),
            conflict_active.clone(),
            conflict_disabled.clone(),
        ],
        &after,
    );
    let disabled_first =
        protective_control_diff(&[normal_before, conflict_disabled, conflict_active], &after);

    assert_eq!(active_first, disabled_first);
    assert!(active_first.iter().any(|fact| {
        fact.reason() == Reason::AmbiguousReplacement
            && fact
                .before()
                .is_some_and(|before| before.source_locator() == ".github/workflows/normal.yml")
    }));
}

#[test]
fn full_replacement_covers_combined_role_and_scope_reduction() {
    let before = [configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    )];
    let retained = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );
    let complete = configured_control(
        Kind::Validation,
        ".github/workflows/complete.yml",
        Some(HASH_B),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    assert!(protective_control_diff(&before, &[retained.clone(), complete]).is_empty());

    let partial = configured_control(
        Kind::Validation,
        ".github/workflows/partial.yml",
        Some(HASH_B),
        &[Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let facts = protective_control_diff(&before, &[retained, partial]);
    assert!(facts.iter().any(|fact| {
        fact.kind() == DiffKind::ScopeReduced && fact.reason() == Reason::ScopeLevelReduced
    }));
}

#[test]
fn shared_replacement_preserves_all_roles_for_mixed_state_reduction() {
    let before_a = configured_control(
        Kind::Validation,
        ".github/workflows/a.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let before_b = configured_control(
        Kind::Validation,
        ".github/workflows/b.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let retained_a = configured_control(
        Kind::Validation,
        ".github/workflows/a.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );
    let replacement = configured_control(
        Kind::Validation,
        ".github/workflows/replacement.yml",
        Some(HASH_B),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let original = protective_control_diff(
        &[before_a.clone(), before_b.clone()],
        &[retained_a.clone(), replacement.clone()],
    );
    let reversed = protective_control_diff(&[before_b, before_a], &[replacement, retained_a]);

    assert_eq!(original, reversed);
    let a_facts = original
        .iter()
        .filter(|fact| {
            fact.reason() == Reason::AmbiguousReplacement
                && fact
                    .before()
                    .is_some_and(|before| before.source_locator() == ".github/workflows/a.yml")
        })
        .collect::<Vec<_>>();
    assert_eq!(a_facts.len(), 1);
    assert_eq!(a_facts[0].roles(), [Role::Check, Role::Validation]);
}
