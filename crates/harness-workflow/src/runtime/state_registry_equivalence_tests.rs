use super::*;
use crate::runtime::{WorkflowCommand, WorkflowCommandType};
use serde_json::json;
use std::collections::BTreeSet;

type ExpectedRule = (Option<&'static str>, &'static str, &'static [&'static str]);

struct ExpectedDefinition<'a> {
    id: &'static str,
    states: &'a [(
        &'static str,
        Option<WorkflowProgressMode>,
        Option<WorkflowTerminalState>,
    )],
    rules: &'static [ExpectedRule],
}

const fn command_driven(
    state: &'static str,
) -> (
    &'static str,
    Option<WorkflowProgressMode>,
    Option<WorkflowTerminalState>,
) {
    (state, Some(WorkflowProgressMode::CommandDriven), None)
}

const fn external_wait(
    state: &'static str,
) -> (
    &'static str,
    Option<WorkflowProgressMode>,
    Option<WorkflowTerminalState>,
) {
    (state, Some(WorkflowProgressMode::ExternalWait), None)
}

const fn operator_gate(
    state: &'static str,
) -> (
    &'static str,
    Option<WorkflowProgressMode>,
    Option<WorkflowTerminalState>,
) {
    (state, Some(WorkflowProgressMode::OperatorGate), None)
}

const fn parent_handoff(
    state: &'static str,
) -> (
    &'static str,
    Option<WorkflowProgressMode>,
    Option<WorkflowTerminalState>,
) {
    (state, Some(WorkflowProgressMode::ParentHandoff), None)
}

const fn terminal_state(
    state: &'static str,
    terminal_state: WorkflowTerminalState,
) -> (
    &'static str,
    Option<WorkflowProgressMode>,
    Option<WorkflowTerminalState>,
) {
    (state, None, Some(terminal_state))
}

#[test]
fn builtins_preserve_literal_states_terminal_mappings_and_transition_rules() {
    let expected = [
        ExpectedDefinition {
            id: "github_issue_pr",
            states: &[
                command_driven("discovered"),
                external_wait("awaiting_dependencies"),
                command_driven("scheduled"),
                command_driven("planning"),
                command_driven("implementing"),
                command_driven("replanning"),
                external_wait("pr_open"),
                command_driven("local_review_gate"),
                external_wait("awaiting_feedback"),
                command_driven("addressing_feedback"),
                parent_handoff("quality_gate_pending"),
                operator_gate("ready_to_merge"),
                command_driven("merging"),
                operator_gate("blocked"),
                terminal_state("done", WorkflowTerminalState::Succeeded),
                terminal_state("failed", WorkflowTerminalState::Failed),
                terminal_state("cancelled", WorkflowTerminalState::Cancelled),
            ],
            rules: GITHUB_ISSUE_PR_RULES,
        },
        ExpectedDefinition {
            id: "prompt_task",
            states: &[
                command_driven("submitted"),
                external_wait("awaiting_dependencies"),
                command_driven("implementing"),
                operator_gate("blocked"),
                terminal_state("done", WorkflowTerminalState::Succeeded),
                terminal_state("failed", WorkflowTerminalState::Failed),
                terminal_state("cancelled", WorkflowTerminalState::Cancelled),
            ],
            rules: PROMPT_TASK_RULES,
        },
        ExpectedDefinition {
            id: "quality_gate",
            states: &[
                command_driven("pending"),
                command_driven("checking"),
                operator_gate("blocked"),
                terminal_state("passed", WorkflowTerminalState::Succeeded),
                terminal_state("failed", WorkflowTerminalState::Failed),
                terminal_state("cancelled", WorkflowTerminalState::Cancelled),
            ],
            rules: QUALITY_GATE_RULES,
        },
        ExpectedDefinition {
            id: "pr_feedback",
            states: &[
                command_driven("pending"),
                command_driven("inspecting"),
                parent_handoff("feedback_found"),
                parent_handoff("no_actionable_feedback"),
                parent_handoff("ready_to_merge"),
                operator_gate("blocked"),
                terminal_state("done", WorkflowTerminalState::Succeeded),
                terminal_state("failed", WorkflowTerminalState::Failed),
                terminal_state("cancelled", WorkflowTerminalState::Cancelled),
            ],
            rules: PR_FEEDBACK_RULES,
        },
    ];

    assert_eq!(
        known_workflow_definition_ids(),
        expected
            .iter()
            .map(|definition| definition.id.to_string())
            .collect::<Vec<_>>()
    );

    for expected_definition in expected {
        let actual = workflow_definition(expected_definition.id)
            .expect("literal built-in definition should be registered");
        assert_eq!(actual.id, expected_definition.id);
        assert_eq!(actual.states.len(), expected_definition.states.len());
        for (actual_state, (expected_state, expected_progress, expected_terminal)) in
            actual.states.iter().zip(expected_definition.states)
        {
            assert_eq!(
                actual_state.key.definition_id.as_ref(),
                expected_definition.id
            );
            assert_eq!(actual_state.key.state.as_ref(), *expected_state);
            assert_eq!(actual_state.progress_mode, *expected_progress);
            assert_eq!(actual_state.terminal_state, *expected_terminal);
        }

        let actual_rules = actual.allowlist.rules().collect::<Vec<_>>();
        assert_eq!(actual_rules.len(), expected_definition.rules.len());
        for (actual_rule, (expected_from, expected_to, expected_commands)) in
            actual_rules.iter().zip(expected_definition.rules)
        {
            assert_eq!(actual_rule.from_state.as_deref(), *expected_from);
            assert_eq!(actual_rule.to_state, *expected_to);
            assert_eq!(
                actual_rule
                    .allowed_commands
                    .iter()
                    .map(|command| command.as_str())
                    .collect::<BTreeSet<_>>(),
                expected_commands.iter().copied().collect::<BTreeSet<_>>()
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ProgressOwner {
    RuntimeJobDriver(WorkflowCommandType),
    ExternalObserver(&'static str),
    OperatorAction(&'static str),
    ParentPropagation(&'static str),
}

#[test]
fn progress_mode_semantics_match_authoritative_ownership_matrix() {
    use WorkflowCommandType::{EnqueueActivity, StartChildWorkflow};
    use WorkflowProgressMode::{CommandDriven, ExternalWait, OperatorGate, ParentHandoff};

    let expected = [
        (
            "github_issue_pr",
            "discovered",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "github_issue_pr",
            "awaiting_dependencies",
            ExternalWait,
            ProgressOwner::ExternalObserver("runtime_dependency_release_observer"),
        ),
        (
            "github_issue_pr",
            "scheduled",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "github_issue_pr",
            "planning",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "github_issue_pr",
            "implementing",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "github_issue_pr",
            "replanning",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "github_issue_pr",
            "pr_open",
            ExternalWait,
            ProgressOwner::ExternalObserver("pr_feedback_sweeper_local_review_selector"),
        ),
        (
            "github_issue_pr",
            "local_review_gate",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "github_issue_pr",
            "awaiting_feedback",
            ExternalWait,
            ProgressOwner::ExternalObserver("pr_feedback_sweeper_feedback_selector"),
        ),
        (
            "github_issue_pr",
            "addressing_feedback",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(StartChildWorkflow),
        ),
        (
            "github_issue_pr",
            "quality_gate_pending",
            ParentHandoff,
            ProgressOwner::ParentPropagation("quality_gate_child_completion"),
        ),
        (
            "github_issue_pr",
            "ready_to_merge",
            OperatorGate,
            ProgressOwner::OperatorAction("merge_approval"),
        ),
        (
            "github_issue_pr",
            "merging",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "github_issue_pr",
            "blocked",
            OperatorGate,
            ProgressOwner::OperatorAction("authorized_recovery"),
        ),
        (
            "prompt_task",
            "submitted",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "prompt_task",
            "awaiting_dependencies",
            ExternalWait,
            ProgressOwner::ExternalObserver("runtime_dependency_release_observer"),
        ),
        (
            "prompt_task",
            "implementing",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "prompt_task",
            "blocked",
            OperatorGate,
            ProgressOwner::OperatorAction("authorized_recovery"),
        ),
        (
            "quality_gate",
            "pending",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "quality_gate",
            "checking",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "quality_gate",
            "blocked",
            OperatorGate,
            ProgressOwner::OperatorAction("authorized_recovery"),
        ),
        (
            "pr_feedback",
            "pending",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "pr_feedback",
            "inspecting",
            CommandDriven,
            ProgressOwner::RuntimeJobDriver(EnqueueActivity),
        ),
        (
            "pr_feedback",
            "feedback_found",
            ParentHandoff,
            ProgressOwner::ParentPropagation("pr_feedback_child_completion"),
        ),
        (
            "pr_feedback",
            "no_actionable_feedback",
            ParentHandoff,
            ProgressOwner::ParentPropagation("pr_feedback_child_completion"),
        ),
        (
            "pr_feedback",
            "ready_to_merge",
            ParentHandoff,
            ProgressOwner::ParentPropagation("pr_feedback_child_completion"),
        ),
        (
            "pr_feedback",
            "blocked",
            OperatorGate,
            ProgressOwner::OperatorAction("authorized_recovery"),
        ),
    ];

    let actual = known_workflow_definition_ids()
        .into_iter()
        .flat_map(|definition_id| {
            workflow_states_for_definition(&definition_id)
                .into_iter()
                .filter_map(move |state| {
                    state
                        .progress_mode
                        .map(|mode| (definition_id.clone(), state.key.state.to_string(), mode))
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(actual.len(), expected.len());

    for ((definition_id, state, mode, owner), (actual_definition, actual_state, actual_mode)) in
        expected.iter().zip(&actual)
    {
        assert_eq!(actual_definition, definition_id);
        assert_eq!(actual_state, state);
        assert_eq!(actual_mode, mode);
        match (mode, owner) {
            (CommandDriven, ProgressOwner::RuntimeJobDriver(command_type)) => {
                let command = WorkflowCommand::new(*command_type, "semantic-fixture", json!({}));
                assert!(
                    command.requires_runtime_job(),
                    "{definition_id}.{state} must name a runtime-job-producing driver"
                );
            }
            (ExternalWait, ProgressOwner::ExternalObserver(observer)) => assert!(matches!(
                *observer,
                "runtime_dependency_release_observer"
                    | "pr_feedback_sweeper_local_review_selector"
                    | "pr_feedback_sweeper_feedback_selector"
            )),
            (OperatorGate, ProgressOwner::OperatorAction(action)) => {
                assert!(matches!(*action, "merge_approval" | "authorized_recovery"))
            }
            (ParentHandoff, ProgressOwner::ParentPropagation(hook)) => assert!(matches!(
                *hook,
                "quality_gate_child_completion" | "pr_feedback_child_completion"
            )),
            _ => panic!("{definition_id}.{state} has a mode/owner category mismatch"),
        }
    }
}

#[test]
fn registry_scopes_success_states_to_workflow_definitions() {
    assert_eq!(
        workflow_state_terminal_state(QUALITY_GATE_DEFINITION_ID, "passed"),
        Some(WorkflowTerminalState::Succeeded)
    );
    assert_eq!(
        workflow_state_terminal_state(GITHUB_ISSUE_PR_DEFINITION_ID, "passed"),
        None
    );
    assert_eq!(
        workflow_state_terminal_state(QUALITY_GATE_DEFINITION_ID, "done"),
        None
    );
}

#[test]
fn registry_lists_only_known_definition_states() {
    assert_eq!(
        known_workflow_definition_ids(),
        vec![
            GITHUB_ISSUE_PR_DEFINITION_ID,
            PROMPT_TASK_DEFINITION_ID,
            QUALITY_GATE_DEFINITION_ID,
            PR_FEEDBACK_DEFINITION_ID,
        ]
    );
    assert!(workflow_states_for_definition("unknown_workflow").is_empty());
    assert!(workflow_state_exists(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "awaiting_feedback"
    ));
    assert!(!workflow_state_exists(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "inspecting"
    ));
}

#[test]
fn registry_lists_terminal_state_names_by_definition() {
    assert_eq!(
        workflow_terminal_state_names_for_definition(GITHUB_ISSUE_PR_DEFINITION_ID),
        vec!["done", "failed", "cancelled"]
    );
    assert_eq!(
        workflow_terminal_state_names_for_definition(QUALITY_GATE_DEFINITION_ID),
        vec!["passed", "failed", "cancelled"]
    );
    assert!(workflow_terminal_state_names_for_definition("unknown_workflow").is_empty());
}

#[test]
fn registry_covers_validator_transition_states() {
    let allowlists = [
        (
            GITHUB_ISSUE_PR_DEFINITION_ID,
            TransitionAllowlist::github_issue_pr_defaults(),
        ),
        (
            PROMPT_TASK_DEFINITION_ID,
            TransitionAllowlist::prompt_task_defaults(),
        ),
        (
            QUALITY_GATE_DEFINITION_ID,
            TransitionAllowlist::quality_gate_defaults(),
        ),
        (
            PR_FEEDBACK_DEFINITION_ID,
            TransitionAllowlist::pr_feedback_defaults(),
        ),
    ];

    for (definition_id, allowlist) in allowlists {
        let definition = workflow_definition(definition_id)
            .expect("built-in workflow definition should be registered");
        for state in &definition.states {
            assert!(
                state.has_complete_progress_contract(),
                "{definition_id}.{} must declare exactly one progress or terminal contract",
                state.key.state
            );
        }
        for rule in allowlist.rules() {
            if let Some(from_state) = rule.from_state.as_deref() {
                assert!(
                    workflow_state_exists(definition_id, from_state),
                    "{definition_id} missing from_state {from_state}"
                );
            }
            assert!(
                workflow_state_definition(definition_id, &rule.to_state)
                    .is_some_and(|state| state.has_complete_progress_contract()),
                "{definition_id} to_state {} is missing a complete contract",
                rule.to_state,
            );
        }
    }
}

#[test]
fn progress_mode_lookup_fails_closed_for_terminal_and_unknown_states() {
    assert_eq!(
        workflow_state_progress_mode(GITHUB_ISSUE_PR_DEFINITION_ID, "implementing"),
        Some(WorkflowProgressMode::CommandDriven)
    );
    assert_eq!(
        workflow_state_progress_mode(GITHUB_ISSUE_PR_DEFINITION_ID, "done"),
        None
    );
    assert_eq!(
        workflow_state_progress_mode(GITHUB_ISSUE_PR_DEFINITION_ID, "unknown"),
        None
    );
    assert_eq!(
        workflow_state_progress_mode("unknown_definition", "implementing"),
        None
    );
}

#[test]
fn state_key_clones_reuse_owned_string_allocations() {
    let state = workflow_state_definition(PROMPT_TASK_DEFINITION_ID, "implementing")
        .expect("prompt task implementing state should exist");
    let cloned = state.key.clone();

    assert!(Arc::ptr_eq(&state.key.definition_id, &cloned.definition_id));
    assert!(Arc::ptr_eq(&state.key.state, &cloned.state));
}

#[test]
fn duplicate_registration_fails_without_replacing_the_first_definition() {
    let mut registry = WorkflowDefinitionRegistry::new_for_tests();
    let first = definition(
        "fixture",
        vec![active(
            "fixture",
            "pending",
            WorkflowProgressMode::ExternalWait,
        )],
        TransitionAllowlist::default(),
    );
    let duplicate = definition(
        "fixture",
        vec![active(
            "fixture",
            "other",
            WorkflowProgressMode::ExternalWait,
        )],
        TransitionAllowlist::default(),
    );

    registry
        .register(first)
        .expect("first registration should pass");
    let error = registry
        .register(duplicate)
        .expect_err("duplicate registration should fail");

    assert!(error.to_string().contains("already registered"));
    assert!(registry
        .definition("fixture")
        .is_some_and(|definition| definition.states[0].key.state.as_ref() == "pending"));
}

#[test]
fn freeze_is_idempotent_and_rejects_late_registration() {
    let mut registry = WorkflowDefinitionRegistry::new_for_tests();
    registry.freeze();
    registry.freeze();

    let error = registry
        .register(definition(
            "late",
            vec![active(
                "late",
                "pending",
                WorkflowProgressMode::ExternalWait,
            )],
            TransitionAllowlist::default(),
        ))
        .expect_err("post-freeze registration should fail");

    assert!(registry.is_frozen());
    assert!(error.to_string().contains("is frozen"));
    assert!(registry.definition("late").is_none());
}

#[test]
fn registration_rejects_incomplete_or_conflicting_progress_contracts() {
    let invalid_states = [
        WorkflowStateDefinition {
            key: WorkflowStateKey {
                definition_id: Arc::from("fixture"),
                state: Arc::from("missing"),
            },
            progress_mode: None,
            terminal_state: None,
        },
        WorkflowStateDefinition {
            key: WorkflowStateKey {
                definition_id: Arc::from("fixture"),
                state: Arc::from("conflicting"),
            },
            progress_mode: Some(WorkflowProgressMode::OperatorGate),
            terminal_state: Some(WorkflowTerminalState::Failed),
        },
    ];

    for state in invalid_states {
        let mut registry = WorkflowDefinitionRegistry::new_for_tests();
        let error = registry
            .register(RegisteredWorkflowDefinition::new(
                "fixture",
                vec![state],
                TransitionAllowlist::default(),
            ))
            .expect_err("invalid state contract should fail registration");

        assert!(error.to_string().contains("exactly one"));
        assert!(registry.definition("fixture").is_none());
    }
}

const E: &str = "enqueue_activity";
const S: &str = "start_child_workflow";
const B: &str = "bind_pr";
const P: &str = "record_plan_concern";
const W: &str = "wait";
const MB: &str = "mark_blocked";
const MD: &str = "mark_done";
const MF: &str = "mark_failed";
const MC: &str = "mark_cancelled";
const O: &str = "request_operator_attention";

const GITHUB_ISSUE_PR_RULES: &[ExpectedRule] = &[
    (Some("discovered"), "awaiting_dependencies", &[W]),
    (Some("failed"), "awaiting_dependencies", &[W]),
    (Some("cancelled"), "awaiting_dependencies", &[W]),
    (Some("awaiting_dependencies"), "awaiting_dependencies", &[W]),
    (Some("awaiting_dependencies"), "scheduled", &[E, W]),
    (Some("awaiting_dependencies"), "planning", &[E, W]),
    (Some("awaiting_dependencies"), "implementing", &[E, W]),
    (Some("discovered"), "scheduled", &[E, W]),
    (Some("discovered"), "planning", &[E, W]),
    (Some("discovered"), "implementing", &[E, W]),
    (Some("scheduled"), "scheduled", &[E, W]),
    (Some("failed"), "scheduled", &[E, W]),
    (Some("failed"), "planning", &[E, W]),
    (Some("failed"), "implementing", &[E, W]),
    (Some("failed"), "replanning", &[E, W]),
    (Some("failed"), "local_review_gate", &[E, W]),
    (Some("failed"), "awaiting_feedback", &[E, S, W]),
    (Some("failed"), "addressing_feedback", &[E, S, W]),
    (Some("failed"), "merging", &[E]),
    (Some("blocked"), "implementing", &[E, W]),
    (Some("blocked"), "replanning", &[E, W]),
    (Some("blocked"), "local_review_gate", &[E, W]),
    (Some("blocked"), "awaiting_feedback", &[E, S, W]),
    (Some("blocked"), "addressing_feedback", &[E, S, W]),
    (Some("blocked"), "merging", &[E]),
    (Some("cancelled"), "scheduled", &[E, W]),
    (Some("cancelled"), "planning", &[E, W]),
    (Some("cancelled"), "implementing", &[E, W]),
    (Some("scheduled"), "planning", &[E, W]),
    (Some("scheduled"), "implementing", &[E, P, W]),
    (Some("scheduled"), "replanning", &[E, P, MB, W]),
    (Some("planning"), "implementing", &[E, MB]),
    (Some("planning"), "planning", &[E, W]),
    (Some("implementing"), "implementing", &[E, P, W]),
    (Some("implementing"), "replanning", &[E, P, MB, W]),
    (Some("replanning"), "implementing", &[E, P, MB, W]),
    (Some("implementing"), "pr_open", &[B, E, S, W]),
    (Some("implementing"), "done", &[MD]),
    (Some("scheduled"), "pr_open", &[B, E, S, W]),
    (Some("pr_open"), "pr_open", &[B, W]),
    (Some("pr_open"), "local_review_gate", &[E, W]),
    (Some("pr_open"), "awaiting_feedback", &[W]),
    (Some("local_review_gate"), "local_review_gate", &[E, W]),
    (Some("local_review_gate"), "awaiting_feedback", &[W]),
    (
        Some("local_review_gate"),
        "addressing_feedback",
        &[E, MB, W],
    ),
    (Some("pr_open"), "done", &[MD]),
    (Some("awaiting_feedback"), "awaiting_feedback", &[E, S, W]),
    (
        Some("awaiting_feedback"),
        "addressing_feedback",
        &[E, S, MB, W],
    ),
    (
        Some("addressing_feedback"),
        "addressing_feedback",
        &[E, S, MB, W],
    ),
    (Some("addressing_feedback"), "local_review_gate", &[E, S, W]),
    (Some("awaiting_feedback"), "quality_gate_pending", &[S, W]),
    (Some("quality_gate_pending"), "ready_to_merge", &[]),
    (Some("awaiting_feedback"), "done", &[MD]),
    (Some("addressing_feedback"), "done", &[MD]),
    (Some("quality_gate_pending"), "done", &[MD]),
    (Some("quality_gate_pending"), "quality_gate_pending", &[W]),
    (Some("ready_to_merge"), "ready_to_merge", &[W]),
    (Some("ready_to_merge"), "merging", &[E]),
    (Some("merging"), "done", &[MD]),
    (Some("ready_to_merge"), "done", &[MD]),
    (None, "blocked", &[MB, O, W]),
    (None, "failed", &[MF]),
    (None, "cancelled", &[MC]),
];

const PROMPT_TASK_RULES: &[ExpectedRule] = &[
    (Some("submitted"), "awaiting_dependencies", &[W]),
    (Some("failed"), "awaiting_dependencies", &[W]),
    (Some("cancelled"), "awaiting_dependencies", &[W]),
    (Some("awaiting_dependencies"), "awaiting_dependencies", &[W]),
    (Some("awaiting_dependencies"), "implementing", &[E, W]),
    (Some("submitted"), "implementing", &[E, W]),
    (Some("failed"), "implementing", &[E, W]),
    (Some("cancelled"), "implementing", &[E, W]),
    (Some("implementing"), "implementing", &[E]),
    (Some("blocked"), "awaiting_dependencies", &[W]),
    (Some("blocked"), "implementing", &[E, W]),
    (Some("implementing"), "done", &[MD]),
    (None, "blocked", &[MB, O, W]),
    (None, "failed", &[MF]),
    (None, "cancelled", &[MC]),
];

const QUALITY_GATE_RULES: &[ExpectedRule] = &[
    (Some("pending"), "checking", &[E, W]),
    (Some("checking"), "checking", &[E, W]),
    (Some("checking"), "passed", &[]),
    (None, "blocked", &[MB, O, W]),
    (None, "failed", &[MF]),
    (None, "cancelled", &[MC]),
];

const PR_FEEDBACK_RULES: &[ExpectedRule] = &[
    (Some("pending"), "inspecting", &[E, W]),
    (Some("inspecting"), "inspecting", &[E, W]),
    (Some("inspecting"), "feedback_found", &[]),
    (Some("inspecting"), "no_actionable_feedback", &[]),
    (Some("inspecting"), "ready_to_merge", &[]),
    (Some("feedback_found"), "done", &[W]),
    (Some("no_actionable_feedback"), "done", &[W]),
    (Some("ready_to_merge"), "done", &[W]),
    (None, "blocked", &[MB, O, W]),
    (None, "failed", &[MF]),
    (None, "cancelled", &[MC]),
];
