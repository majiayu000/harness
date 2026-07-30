use super::*;
use AgentStackProtectionFailureMode as Mode;
use AgentStackProtectionScope as Scope;

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ACTIVE_REQUIRED: ControlState =
    ControlState(Some(true), Some(Scope::Required), Some(Mode::FailClosed));
const ACTIVE_COMPREHENSIVE: ControlState = ControlState(
    Some(true),
    Some(Scope::Comprehensive),
    Some(Mode::FailClosed),
);
const DISABLED_PARTIAL_OPEN: ControlState =
    ControlState(Some(false), Some(Scope::Partial), Some(Mode::FailOpen));
const DISABLED_REQUIRED: ControlState =
    ControlState(Some(false), Some(Scope::Required), Some(Mode::FailClosed));
const UNKNOWN_REQUIRED: ControlState =
    ControlState(None, Some(Scope::Required), Some(Mode::FailClosed));

#[derive(Clone, Copy)]
struct ControlState(Option<bool>, Option<Scope>, Option<Mode>);

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
    configured_control(
        kind,
        locator,
        Some(HASH_A),
        roles,
        confidence,
        ACTIVE_REQUIRED,
    )
}

fn configured_control(
    kind: AgentStackComponentKind,
    locator: &str,
    integrity: Option<&str>,
    roles: &[AgentStackProtectionRole],
    confidence: AgentStackProtectionConfidence,
    state: ControlState,
) -> AgentStackProtectionControl {
    let mut control = match AgentStackProtectionControl::new(
        component(kind, locator, integrity),
        roles.iter().copied(),
        confidence,
    ) {
        Ok(control) => control,
        Err(error) => panic!("valid protection control: {error}"),
    };
    let ControlState(enabled, scope, failure_mode) = state;
    if let Some(enabled) = enabled {
        control = control.with_enabled(enabled);
    }
    if let Some(scope) = scope {
        control = control.with_scope(scope);
    }
    if let Some(failure_mode) = failure_mode {
        control = control.with_failure_mode(failure_mode);
    }
    control
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
    let relaxed = configured_control(
        AgentStackComponentKind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_B),
        &[AgentStackProtectionRole::Validation],
        AgentStackProtectionConfidence::High,
        DISABLED_PARTIAL_OPEN,
    );
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
    let replacement = configured_control(
        AgentStackComponentKind::Hook,
        ".harness/guards/pre-push.sh",
        Some(HASH_B),
        &[AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::High,
        ACTIVE_COMPREHENSIVE,
    );
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
    let renamed = configured_control(
        AgentStackComponentKind::Hook,
        ".harness/guards/pre-push.sh",
        Some(HASH_A),
        &[AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::High,
        DISABLED_PARTIAL_OPEN,
    );
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
    let replacement = configured_control(
        AgentStackComponentKind::Hook,
        ".harness/guards/pre-commit.sh",
        Some(HASH_B),
        &[AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::Low,
        ACTIVE_REQUIRED,
    );
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
fn protective_control_diff_aggregates_split_duplicate_after_roles() {
    let before = [control(
        AgentStackComponentKind::Policy,
        "rules/protect.toml",
        &[
            AgentStackProtectionRole::Policy,
            AgentStackProtectionRole::Validation,
        ],
    )];
    let after = [
        control(
            AgentStackComponentKind::Policy,
            "rules/protect.toml",
            &[AgentStackProtectionRole::Policy],
        ),
        control(
            AgentStackComponentKind::Policy,
            "rules/protect.toml",
            &[AgentStackProtectionRole::Validation],
        ),
    ];

    let facts = protective_control_diff(&before, &after);

    assert!(
        facts.is_empty(),
        "duplicate reports for one component should be combined before diffing"
    );
}

#[test]
fn protective_control_diff_rejects_disabled_unknown_state_replacement() {
    let before = [configured_control(
        AgentStackComponentKind::Hook,
        ".githooks/pre-commit",
        Some(HASH_A),
        &[AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::High,
        UNKNOWN_REQUIRED,
    )];
    let replacement = configured_control(
        AgentStackComponentKind::Hook,
        ".harness/guards/pre-commit.sh",
        Some(HASH_B),
        &[AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::High,
        DISABLED_REQUIRED,
    );
    let after = [replacement];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), AgentStackProtectionDiffKind::Removed);
}

#[test]
fn protective_control_diff_caps_rename_evidence_at_source_confidence() {
    let before = [control_with_confidence(
        AgentStackComponentKind::Policy,
        "rules/protect.toml",
        &[AgentStackProtectionRole::Policy],
        AgentStackProtectionConfidence::Low,
    )];
    let after = [control(
        AgentStackComponentKind::Policy,
        "rules/renamed.toml",
        &[AgentStackProtectionRole::Policy],
    )];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 1);
    assert_eq!(
        facts[0].reason(),
        AgentStackProtectionControlReason::PossibleRename
    );
    assert_eq!(facts[0].confidence(), AgentStackProtectionConfidence::Low);
}

#[test]
fn protective_control_diff_keeps_multi_candidate_renames_ambiguous() {
    let before = [control(
        AgentStackComponentKind::Policy,
        "rules/protect.toml",
        &[AgentStackProtectionRole::Policy],
    )];
    let after = [
        control(
            AgentStackComponentKind::Policy,
            "rules/renamed-a.toml",
            &[AgentStackProtectionRole::Policy],
        ),
        control(
            AgentStackComponentKind::Policy,
            "rules/renamed-b.toml",
            &[AgentStackProtectionRole::Policy],
        )
        .with_enabled(false),
    ];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 1);
    assert_eq!(
        facts[0].reason(),
        AgentStackProtectionControlReason::PossibleRename
    );
    assert!(facts[0].after().is_none());
}

#[test]
fn protective_control_diff_reports_many_to_one_replacements_as_ambiguous() {
    let before = [
        control(
            AgentStackComponentKind::Hook,
            ".githooks/pre-commit",
            &[AgentStackProtectionRole::Hook],
        ),
        control(
            AgentStackComponentKind::Hook,
            ".githooks/pre-push",
            &[AgentStackProtectionRole::Hook],
        ),
    ];
    let replacement = configured_control(
        AgentStackComponentKind::Hook,
        ".harness/guards/git-hooks.sh",
        Some(HASH_B),
        &[AgentStackProtectionRole::Hook],
        AgentStackProtectionConfidence::High,
        ACTIVE_REQUIRED,
    );
    let after = [replacement];

    let facts = protective_control_diff(&before, &after);

    assert_eq!(facts.len(), 2);
    assert!(facts.iter().all(|fact| {
        fact.reason() == AgentStackProtectionControlReason::AmbiguousReplacement
            && fact.kind() == AgentStackProtectionDiffKind::AmbiguousReviewEvidence
    }));
}
