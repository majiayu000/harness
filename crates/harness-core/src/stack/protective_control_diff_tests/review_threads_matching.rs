use super::*;

#[test]
fn forced_global_assignment_preserves_both_removed_controls_in_any_order() {
    let flexible = configured_control(
        Kind::Validation,
        ".github/workflows/flexible.yml",
        Some(HASH_A),
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let constrained = configured_control(
        Kind::Validation,
        ".github/workflows/constrained.yml",
        Some(HASH_A),
        &[Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let shared = configured_control(
        Kind::Validation,
        ".github/workflows/shared.yml",
        Some(HASH_B),
        &[Role::Validation, Role::Check],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let flexible_only = configured_control(
        Kind::Validation,
        ".github/workflows/flexible-only.yml",
        None,
        &[Role::Validation],
        Confidence::High,
        ACTIVE_REQUIRED,
    );

    let original = protective_control_diff(
        &[flexible.clone(), constrained.clone()],
        &[shared.clone(), flexible_only.clone()],
    );
    let reversed = protective_control_diff(&[constrained, flexible], &[flexible_only, shared]);

    assert_eq!(original, reversed);
    assert!(original.is_empty());
}

#[test]
fn unanimously_disabled_conflicted_rename_keeps_definite_role_loss_in_any_order() {
    let before = configured_control(
        Kind::Policy,
        "rules/legacy.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let disabled_required = configured_control(
        Kind::Policy,
        "rules/replacement.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        DISABLED_REQUIRED,
    );
    let disabled_advisory = configured_control(
        Kind::Policy,
        "rules/replacement.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        ControlState(Some(false), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );

    let original = protective_control_diff(
        std::slice::from_ref(&before),
        &[disabled_required.clone(), disabled_advisory.clone()],
    );
    let reversed = protective_control_diff(&[before], &[disabled_advisory, disabled_required]);

    assert_eq!(original, reversed);
    assert_conflict_and_definite_policy_removal(&original);
}

#[test]
fn cross_report_strengths_do_not_synthesize_an_equivalent_rename() {
    let before = configured_control(
        Kind::Policy,
        "rules/legacy.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let strong_scope = configured_control(
        Kind::Policy,
        "rules/replacement.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Comprehensive), Some(Mode::FailOpen)),
    );
    let strong_failure_mode = configured_control(
        Kind::Policy,
        "rules/replacement.toml",
        Some(HASH_A),
        &[Role::Policy],
        Confidence::High,
        ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailClosed)),
    );

    let original = protective_control_diff(
        std::slice::from_ref(&before),
        &[strong_scope.clone(), strong_failure_mode.clone()],
    );
    let reversed = protective_control_diff(&[before], &[strong_failure_mode, strong_scope]);

    assert_eq!(original, reversed);
    assert_conflict_and_definite_policy_removal(&original);
}

fn assert_conflict_and_definite_policy_removal(facts: &[Diff]) {
    assert!(facts
        .iter()
        .any(|fact| fact.reason() == Reason::ConflictingDuplicateReport));
    assert!(facts.iter().any(|fact| {
        fact.kind() == DiffKind::Removed
            && fact.reason() == Reason::RemovedWithoutEquivalent
            && fact.roles() == [Role::Policy]
    }));
}
