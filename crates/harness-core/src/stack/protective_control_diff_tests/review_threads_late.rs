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

#[test]
fn weakened_rename_counts_supplemental_replacements_as_shared() {
    let renamed_before = configured_control(
        Kind::Validation,
        ".github/workflows/renamed-before.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let removed = configured_control(
        Kind::Validation,
        ".github/workflows/removed.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let weakened_rename = configured_control(
        Kind::Validation,
        ".github/workflows/renamed-after.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );
    let shared = configured_control(
        Kind::Validation,
        ".github/workflows/shared.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let check_replacement = configured_control(
        Kind::Validation,
        ".github/workflows/check.yml",
        None,
        &[Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    let facts = protective_control_diff(
        &[renamed_before, removed],
        &[weakened_rename, shared, check_replacement],
    );

    assert!(facts.iter().any(|fact| {
        fact.reason() == Reason::AmbiguousReplacement
            && fact
                .before()
                .is_some_and(|before| before.source_locator() == ".github/workflows/removed.yml")
    }));
}

#[test]
fn unique_replacement_suppresses_enablement_evidence_loss() {
    let before = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let unknown_legacy = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        UNKNOWN_REQUIRED,
    );
    let replacement = configured_control(
        Kind::Validation,
        ".github/workflows/replacement.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    assert!(protective_control_diff(&[before], &[unknown_legacy, replacement]).is_empty());
}

#[test]
fn safe_replacement_wins_over_conflicted_alternative() {
    let before = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let safe = configured_control(
        Kind::Validation,
        ".github/workflows/safe.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let conflicting_required = configured_control(
        Kind::Validation,
        ".github/workflows/conflicting.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let conflicting_advisory = configured_control(
        Kind::Validation,
        ".github/workflows/conflicting.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );

    assert!(protective_control_diff(
        &[before],
        &[safe, conflicting_required, conflicting_advisory],
    )
    .is_empty());
}

#[test]
fn unanimously_disabled_duplicates_stay_out_of_the_diff() {
    let disabled_required = configured_control(
        Kind::Policy,
        "rules/disabled.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        DISABLED_REQUIRED,
    );
    let disabled_advisory = configured_control(
        Kind::Policy,
        "rules/disabled.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        ControlState(Some(false), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );

    assert!(protective_control_diff(&[disabled_required, disabled_advisory], &[]).is_empty());
}

#[test]
fn conflicted_rename_does_not_hide_shared_supplemental_use() {
    let renamed_before = configured_control(
        Kind::Validation,
        ".github/workflows/renamed-before.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let removed = configured_control(
        Kind::Validation,
        ".github/workflows/removed.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let rename_required = configured_control(
        Kind::Validation,
        ".github/workflows/renamed-after.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let rename_advisory = configured_control(
        Kind::Validation,
        ".github/workflows/renamed-after.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );
    let shared = configured_control(
        Kind::Validation,
        ".github/workflows/shared.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let check_replacement = configured_control(
        Kind::Validation,
        ".github/workflows/check.yml",
        None,
        &[Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let original = protective_control_diff(
        &[renamed_before.clone(), removed.clone()],
        &[
            rename_required.clone(),
            rename_advisory.clone(),
            shared.clone(),
            check_replacement.clone(),
        ],
    );
    let reversed = protective_control_diff(
        &[removed, renamed_before],
        &[check_replacement, shared, rename_advisory, rename_required],
    );

    assert_eq!(original, reversed);
    assert!(original.iter().any(|fact| {
        fact.reason() == Reason::AmbiguousReplacement
            && fact
                .before()
                .is_some_and(|before| before.source_locator() == ".github/workflows/removed.yml")
    }));
}

#[test]
fn ambiguous_enablement_loss_emits_one_replacement_fact() {
    let before = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let unknown_legacy = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        UNKNOWN_REQUIRED,
    );
    let replacement_a = configured_control(
        Kind::Validation,
        ".github/workflows/a.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let replacement_b = configured_control(
        Kind::Validation,
        ".github/workflows/b.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    let facts = protective_control_diff(&[before], &[unknown_legacy, replacement_a, replacement_b]);

    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::AmbiguousReplacement]);
}

#[test]
fn confidence_reduction_counts_replacement_as_shared() {
    let retained_before = configured_control(
        Kind::Validation,
        ".github/workflows/retained.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let removed = configured_control(
        Kind::Validation,
        ".github/workflows/removed.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let retained_after = configured_control(
        Kind::Validation,
        ".github/workflows/retained.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::Medium,
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

    let facts =
        protective_control_diff(&[retained_before, removed], &[retained_after, replacement]);

    assert!(facts.iter().any(|fact| {
        fact.reason() == Reason::AmbiguousReplacement
            && fact
                .before()
                .is_some_and(|before| before.source_locator() == ".github/workflows/removed.yml")
    }));
    assert!(!facts
        .iter()
        .any(|fact| fact.reason() == Reason::ConfidenceReduced));
}

#[test]
fn unique_replacement_suppresses_confidence_reduction() {
    let before = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let lower_confidence = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::Medium,
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

    assert!(protective_control_diff(&[before], &[lower_confidence, replacement]).is_empty());
}

#[test]
fn disabled_role_loss_emits_one_ambiguity_for_retained_control() {
    let retained_before = configured_control(
        Kind::Validation,
        ".github/workflows/retained.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let removed = configured_control(
        Kind::Validation,
        ".github/workflows/removed.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let disabled_without_roles = configured_control(
        Kind::Validation,
        ".github/workflows/retained.yml",
        Some(HASH_A),
        &[],
        Confidence::High,
        DISABLED_REQUIRED,
    );
    let replacement = configured_control(
        Kind::Validation,
        ".github/workflows/replacement.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    let facts = protective_control_diff(
        &[retained_before, removed],
        &[disabled_without_roles, replacement],
    );
    let retained_ambiguities = facts
        .iter()
        .filter(|fact| {
            fact.reason() == Reason::AmbiguousReplacement
                && fact.before().is_some_and(|before| {
                    before.source_locator() == ".github/workflows/retained.yml"
                })
        })
        .collect::<Vec<_>>();

    assert_eq!(retained_ambiguities.len(), 1);
    assert_eq!(retained_ambiguities[0].roles(), [Role::Validation]);
}

#[test]
fn alternative_partial_role_replacements_remain_ambiguous() {
    let before = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let retained = configured_control(
        Kind::Validation,
        ".github/workflows/legacy.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let replacement_a = configured_control(
        Kind::Validation,
        ".github/workflows/a.yml",
        Some(HASH_B),
        &[Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let replacement_b = configured_control(
        Kind::Validation,
        ".github/workflows/b.yml",
        None,
        &[Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    let facts = protective_control_diff(&[before], &[retained, replacement_a, replacement_b]);

    assert_eq!(kinds(&facts), [DiffKind::AmbiguousReviewEvidence]);
    assert_eq!(reasons(&facts), [Reason::AmbiguousReplacement]);
    assert_eq!(facts[0].roles(), [Role::Check]);
}
