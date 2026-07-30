use super::*;
use AgentStackComponentKind as Kind;
use AgentStackProtectionConfidence as Confidence;
use AgentStackProtectionControlDiff as Diff;
use AgentStackProtectionControlReason as Reason;
use AgentStackProtectionDiffKind as DiffKind;
use AgentStackProtectionFailureMode as Mode;
use AgentStackProtectionRole as Role;
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
fn component(kind: Kind, locator: &str, integrity: Option<&str>) -> AgentStackComponent {
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
fn control(kind: Kind, locator: &str, roles: &[Role]) -> AgentStackProtectionControl {
    control_with_confidence(kind, locator, roles, Confidence::High)
}
fn control_with_confidence(
    kind: Kind,
    locator: &str,
    roles: &[Role],
    confidence: Confidence,
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
    kind: Kind,
    locator: &str,
    integrity: Option<&str>,
    roles: &[Role],
    confidence: Confidence,
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
fn configured_hook(
    locator: &str,
    integrity: Option<&str>,
    confidence: Confidence,
    state: ControlState,
) -> AgentStackProtectionControl {
    configured_control(
        Kind::Hook,
        locator,
        integrity,
        &[Role::Hook],
        confidence,
        state,
    )
}
fn kinds(facts: &[Diff]) -> Vec<DiffKind> {
    facts.iter().map(Diff::kind).collect()
}
fn reasons(facts: &[Diff]) -> Vec<Reason> {
    facts.iter().map(Diff::reason).collect()
}
#[test]
fn protective_control_diff_ignores_benign_hook_removal_without_role_evidence() {
    let before = [control(Kind::Hook, ".githooks/pre-commit", &[])];
    let facts = protective_control_diff(&before, &[]);
    assert!(
        facts.is_empty(),
        "hook kind alone must not imply protection"
    );
}
#[test]
fn protective_control_diff_reports_protective_policy_removal() {
    let before = [control(Kind::Policy, "rules/protect.toml", &[Role::Policy])];
    let facts = protective_control_diff(&before, &[]);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), DiffKind::Removed);
    assert_eq!(facts[0].reason(), Reason::RemovedWithoutEquivalent);
    assert_eq!(facts[0].confidence(), Confidence::High);
    let Some(before) = facts[0].before() else {
        panic!("before evidence");
    };
    assert_eq!(before.source_locator(), "rules/protect.toml");
    assert!(facts[0].after().is_none());
}
#[test]
fn protective_control_diff_reports_relaxed_existing_controls() {
    let before = [control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        &[Role::Validation, Role::Sandboxing, Role::Check],
    )];
    let relaxed = configured_control(
        Kind::Validation,
        ".github/workflows/ci.yml",
        Some(HASH_B),
        &[Role::Validation],
        Confidence::High,
        DISABLED_PARTIAL_OPEN,
    );
    let after = [relaxed];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(
        kinds(&facts),
        [
            DiffKind::Disabled,
            DiffKind::FailOpen,
            DiffKind::ScopeReduced,
            DiffKind::ScopeReduced,
        ]
    );
    assert!(facts
        .iter()
        .all(|fact| fact.before().is_some() && fact.after().is_some()));
    assert!(facts
        .iter()
        .any(|fact| fact.reason() == Reason::RoleSetReduced));
    assert!(facts
        .iter()
        .any(|fact| fact.reason() == Reason::ScopeLevelReduced));
}
#[test]
fn protective_control_diff_treats_rename_as_review_evidence() {
    let before = [control(Kind::Policy, "rules/protect.toml", &[Role::Policy])];
    let after = [control(Kind::Policy, "rules/renamed.toml", &[Role::Policy])];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), DiffKind::AmbiguousReviewEvidence);
    assert_eq!(facts[0].reason(), Reason::PossibleRename);
    assert_eq!(facts[0].confidence(), Confidence::Medium);
    let Some(after) = facts[0].after() else {
        panic!("after evidence");
    };
    assert_eq!(after.source_locator(), "rules/renamed.toml");
}
#[test]
fn protective_control_diff_suppresses_equivalent_replacement() {
    let before = [control(Kind::Hook, ".githooks/pre-push", &[Role::Hook])];
    let replacement = configured_hook(
        ".harness/guards/pre-push.sh",
        Some(HASH_B),
        Confidence::High,
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
    let before = [control(Kind::Hook, ".githooks/pre-push", &[Role::Hook])];
    let renamed = configured_hook(
        ".harness/guards/pre-push.sh",
        Some(HASH_A),
        Confidence::High,
        DISABLED_PARTIAL_OPEN,
    );
    let after = [renamed];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(
        reasons(&facts),
        [
            Reason::PossibleRename,
            Reason::ExplicitlyDisabled,
            Reason::FailureModeRelaxed,
            Reason::ScopeLevelReduced,
        ]
    );
}
#[test]
fn protective_control_diff_excludes_unchanged_controls_from_rename_candidates() {
    let removed = control(Kind::Policy, "rules/deleted.toml", &[Role::Policy]);
    let retained_before = control(Kind::Policy, "rules/retained.toml", &[Role::Policy]);
    let retained_after = control(Kind::Policy, "rules/retained.toml", &[Role::Policy]);
    let before = [removed, retained_before];
    let after = [retained_after];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), DiffKind::Removed);
    assert_eq!(facts[0].reason(), Reason::RemovedWithoutEquivalent);
}
#[test]
fn protective_control_diff_ignores_removed_controls_already_disabled() {
    let before =
        [control(Kind::Policy, "rules/disabled.toml", &[Role::Policy]).with_enabled(false)];
    let facts = protective_control_diff(&before, &[]);
    assert!(
        facts.is_empty(),
        "removing an explicitly disabled control is not a weakening"
    );
}
#[test]
fn protective_control_diff_keeps_low_confidence_replacements_ambiguous() {
    let before = [control(Kind::Hook, ".githooks/pre-commit", &[Role::Hook])];
    let replacement = configured_control(
        Kind::Hook,
        ".harness/guards/pre-commit.sh",
        Some(HASH_B),
        &[Role::Hook],
        Confidence::Low,
        ACTIVE_REQUIRED,
    );
    let after = [replacement];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), DiffKind::AmbiguousReviewEvidence);
    assert_eq!(facts[0].reason(), Reason::AmbiguousReplacement);
    assert_eq!(facts[0].confidence(), Confidence::Low);
}
#[test]
fn protective_control_diff_aggregates_split_duplicate_after_roles() {
    let before = [control(
        Kind::Policy,
        "rules/protect.toml",
        &[Role::Policy, Role::Validation],
    )];
    let after = [
        control(Kind::Policy, "rules/protect.toml", &[Role::Policy]),
        control(Kind::Policy, "rules/protect.toml", &[Role::Validation]),
    ];
    let facts = protective_control_diff(&before, &after);
    assert!(
        facts.is_empty(),
        "duplicate reports for one component should be combined before diffing"
    );
}
#[test]
fn protective_control_diff_rejects_disabled_unknown_state_replacement() {
    let before = [configured_hook(
        ".githooks/pre-commit",
        Some(HASH_A),
        Confidence::High,
        UNKNOWN_REQUIRED,
    )];
    let replacement = configured_hook(
        ".harness/guards/pre-commit.sh",
        Some(HASH_B),
        Confidence::High,
        DISABLED_REQUIRED,
    );
    let after = [replacement];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind(), DiffKind::Removed);
}
#[test]
fn protective_control_diff_caps_rename_evidence_at_source_confidence() {
    let before = [control_with_confidence(
        Kind::Policy,
        "rules/protect.toml",
        &[Role::Policy],
        Confidence::Low,
    )];
    let after = [control(Kind::Policy, "rules/renamed.toml", &[Role::Policy])];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].reason(), Reason::PossibleRename);
    assert_eq!(facts[0].confidence(), Confidence::Low);
}
#[test]
fn protective_control_diff_keeps_multi_candidate_renames_ambiguous() {
    let before = [control(Kind::Policy, "rules/protect.toml", &[Role::Policy])];
    let after = [
        control(Kind::Policy, "rules/renamed-a.toml", &[Role::Policy]),
        control(Kind::Policy, "rules/renamed-b.toml", &[Role::Policy]).with_enabled(false),
    ];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].reason(), Reason::PossibleRename);
    assert!(facts[0].after().is_none());
}
#[test]
fn protective_control_diff_reports_many_to_one_replacements_as_ambiguous() {
    let before = [
        control(Kind::Hook, ".githooks/pre-commit", &[Role::Hook]),
        control(Kind::Hook, ".githooks/pre-push", &[Role::Hook]),
    ];
    let replacement = configured_control(
        Kind::Hook,
        ".harness/guards/git-hooks.sh",
        Some(HASH_B),
        &[Role::Hook],
        Confidence::High,
        ACTIVE_REQUIRED,
    );
    let after = [replacement];
    let facts = protective_control_diff(&before, &after);
    assert_eq!(facts.len(), 2);
    assert!(facts.iter().all(|fact| {
        fact.reason() == Reason::AmbiguousReplacement
            && fact.kind() == DiffKind::AmbiguousReviewEvidence
    }));
}
#[test]
#[rustfmt::skip]
fn protective_control_diff_handles_review_edge_cases() {
    let before = [configured_hook(".githooks/pre-commit", Some(HASH_A), Confidence::High, UNKNOWN_REQUIRED)];
    let after = [before[0].clone().with_enabled(false)];
    assert_eq!(
        reasons(&protective_control_diff(&before, &after)),
        [Reason::ExplicitlyDisabled]
    );
    let before = [
        control(Kind::Hook, ".githooks/pre-commit", &[Role::Hook]),
        configured_hook(".githooks/pre-push", Some(HASH_B), Confidence::Medium, ACTIVE_REQUIRED),
    ];
    let after = [configured_hook(".harness/guards/git-hooks.sh", Some(HASH_A), Confidence::Medium, ACTIVE_REQUIRED)];
    assert_eq!(
        reasons(&protective_control_diff(&before, &after)),
        [Reason::PossibleRename, Reason::AmbiguousReplacement]
    );
    let before = [control(Kind::Hook, ".githooks/pre-merge", &[Role::Hook])];
    let after = [
        configured_hook(".harness/guards/pre-merge.sh", Some(HASH_A), Confidence::High, ACTIVE_REQUIRED),
        configured_hook(".harness/guards/pre-merge.sh", Some(HASH_B), Confidence::High, ACTIVE_REQUIRED),
    ];
    assert_eq!(
        reasons(&protective_control_diff(&before, &after)),
        [Reason::ConflictingDuplicateReport]
    );
    let removed = control(Kind::Hook, ".githooks/pre-receive", &[Role::Hook]);
    let retained_before = configured_hook(".harness/guards/git-hooks.sh", Some(HASH_B), Confidence::High, ControlState(Some(true), Some(Scope::Advisory), Some(Mode::FailOpen)));
    let retained_after = configured_hook(".harness/guards/git-hooks.sh", Some(HASH_B), Confidence::High, ACTIVE_REQUIRED);
    assert!(protective_control_diff(&[removed, retained_before], &[retained_after]).is_empty());
    let before = [
        control(Kind::Hook, ".githooks/pre-rebase", &[Role::Hook]),
        configured_hook(".harness/guards/rebase.sh", Some(HASH_B), Confidence::High, ACTIVE_REQUIRED),
        configured_hook(".harness/guards/rebase.sh", Some(HASH_B), Confidence::High, DISABLED_REQUIRED),
    ];
    let after = [configured_hook(".harness/guards/rebase.sh", Some(HASH_B), Confidence::High, ACTIVE_REQUIRED)];
    let facts = protective_control_diff(&before, &after);
    assert!(facts.iter().any(|fact| {
        fact.reason() == Reason::ConflictingDuplicateReport
            && fact
                .before()
                .is_some_and(|before| before.source_locator() == ".githooks/pre-rebase")
    }));
}
