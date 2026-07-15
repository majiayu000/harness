use super::*;
use std::collections::BTreeSet;

type ExpectedRule = (Option<&'static str>, &'static str, &'static [&'static str]);

struct ExpectedDefinition {
    id: &'static str,
    states: &'static [(&'static str, Option<WorkflowTerminalState>)],
    rules: &'static [ExpectedRule],
}

#[test]
fn builtins_preserve_literal_states_terminal_mappings_and_transition_rules() {
    let expected = [
        ExpectedDefinition {
            id: "github_issue_pr",
            states: &[
                ("discovered", None),
                ("awaiting_dependencies", None),
                ("scheduled", None),
                ("planning", None),
                ("implementing", None),
                ("replanning", None),
                ("pr_open", None),
                ("local_review_gate", None),
                ("awaiting_feedback", None),
                ("addressing_feedback", None),
                ("quality_gate_pending", None),
                ("ready_to_merge", None),
                ("merging", None),
                ("blocked", None),
                ("done", Some(WorkflowTerminalState::Succeeded)),
                ("failed", Some(WorkflowTerminalState::Failed)),
                ("cancelled", Some(WorkflowTerminalState::Cancelled)),
            ],
            rules: GITHUB_ISSUE_PR_RULES,
        },
        ExpectedDefinition {
            id: "prompt_task",
            states: &[
                ("submitted", None),
                ("awaiting_dependencies", None),
                ("implementing", None),
                ("blocked", None),
                ("done", Some(WorkflowTerminalState::Succeeded)),
                ("failed", Some(WorkflowTerminalState::Failed)),
                ("cancelled", Some(WorkflowTerminalState::Cancelled)),
            ],
            rules: PROMPT_TASK_RULES,
        },
        ExpectedDefinition {
            id: "quality_gate",
            states: &[
                ("pending", None),
                ("checking", None),
                ("blocked", None),
                ("passed", Some(WorkflowTerminalState::Succeeded)),
                ("failed", Some(WorkflowTerminalState::Failed)),
                ("cancelled", Some(WorkflowTerminalState::Cancelled)),
            ],
            rules: QUALITY_GATE_RULES,
        },
        ExpectedDefinition {
            id: "pr_feedback",
            states: &[
                ("pending", None),
                ("inspecting", None),
                ("feedback_found", None),
                ("no_actionable_feedback", None),
                ("ready_to_merge", None),
                ("blocked", None),
                ("done", Some(WorkflowTerminalState::Succeeded)),
                ("failed", Some(WorkflowTerminalState::Failed)),
                ("cancelled", Some(WorkflowTerminalState::Cancelled)),
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
        for (actual_state, (expected_state, expected_terminal)) in
            actual.states.iter().zip(expected_definition.states)
        {
            assert_eq!(
                actual_state.key.definition_id.as_ref(),
                expected_definition.id
            );
            assert_eq!(actual_state.key.state.as_ref(), *expected_state);
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
    (Some("implementing"), "implementing", &[E, W]),
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
