mod declarative_validation {
    use super::super::*;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn activity_state(on_success: impl Into<String>) -> DeclaredState {
        DeclaredState {
            activity: Some("implement".to_string()),
            on_success: Some(on_success.into()),
            on_failure: Some("failed".to_string()),
            on_signal: BTreeMap::from([("cancel".to_string(), "cancelled".to_string())]),
            ..DeclaredState::default()
        }
    }

    fn valid_policy() -> WorkflowDefinitionPolicy {
        WorkflowDefinitionPolicy {
            id: "docs_review".to_string(),
            initial: "reviewing".to_string(),
            states: BTreeMap::from([
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
                ("reviewing".to_string(), activity_state("done")),
            ]),
            terminal: BTreeMap::from([
                ("cancelled".to_string(), "cancelled".to_string()),
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
            ]),
            evidence_required: BTreeMap::from([(
                "done".to_string(),
                vec!["review_report".to_string()],
            )]),
            recovery_targets: vec!["reviewing".to_string()],
        }
    }

    fn activity_policies() -> BTreeMap<String, WorkflowActivityPolicy> {
        BTreeMap::from([("implement".to_string(), WorkflowActivityPolicy::default())])
    }

    fn error_for(policy: &WorkflowDefinitionPolicy) -> String {
        build_declarative_definition(policy, &activity_policies())
            .expect_err("declaration should be invalid")
            .to_string()
    }

    #[test]
    fn compiles_valid_definition_without_registration_side_effects() {
        let policy = valid_policy();
        let compiled = build_declarative_definition(&policy, &activity_policies())
            .expect("valid declaration should compile");

        assert_eq!(compiled.registered().id, "docs_review");
        assert_eq!(compiled.policy(), &policy);
        assert_eq!(compiled.registered().states.len(), 5);
        assert_eq!(
            compiled
                .registered()
                .allowlist
                .rule_for("reviewing", "done")
                .expect("declared transition should be compiled")
                .allowed_commands,
            BTreeSet::from([WorkflowCommandType::MarkDone])
        );
        assert_eq!(
            compiled
                .registered()
                .allowlist
                .rule_for("reviewing", "blocked")
                .expect("blocked fallback should be compiled")
                .allowed_commands,
            BTreeSet::from([
                WorkflowCommandType::MarkBlocked,
                WorkflowCommandType::Wait,
                WorkflowCommandType::RequestOperatorAttention,
            ])
        );
        assert!(workflow_definition("docs_review").is_none());
    }

    #[test]
    fn rejects_builtin_id_collision() {
        let mut policy = valid_policy();
        policy.id = PROMPT_TASK_DEFINITION_ID.to_string();
        assert!(error_for(&policy).contains("collides with a built-in definition"));
    }

    #[test]
    fn rejects_empty_declarations_and_invalid_initial_state() {
        let mut empty_active = valid_policy();
        empty_active.states.clear();
        assert!(error_for(&empty_active).contains("at least one active state"));

        let mut empty_terminal = valid_policy();
        empty_terminal.terminal.clear();
        assert!(error_for(&empty_terminal).contains("must declare terminal states"));

        let mut missing_initial = valid_policy();
        missing_initial.initial = "missing".to_string();
        assert!(error_for(&missing_initial).contains("is not declared as an active state"));

        let mut terminal_initial = valid_policy();
        terminal_initial.initial = "done".to_string();
        assert!(error_for(&terminal_initial).contains("must be active, not terminal"));
    }

    #[test]
    fn rejects_namespace_overlap_and_invalid_blocked_state() {
        let mut overlap = valid_policy();
        overlap
            .terminal
            .insert("blocked".to_string(), "succeeded".to_string());
        overlap
            .terminal
            .insert("done".to_string(), "failed".to_string());
        overlap
            .terminal
            .insert("failed".to_string(), "cancelled".to_string());
        overlap.terminal.remove("cancelled");
        assert!(error_for(&overlap).contains("both active and terminal namespaces"));

        let mut missing_blocked = valid_policy();
        missing_blocked.states.remove("blocked");
        assert!(error_for(&missing_blocked).contains("must declare active state 'blocked'"));

        let mut wrong_blocked = valid_policy();
        wrong_blocked.states.insert(
            "blocked".to_string(),
            DeclaredState {
                progress: Some(DeclaredProgressMode::ExternalWait),
                ..DeclaredState::default()
            },
        );
        assert!(error_for(&wrong_blocked).contains("progress 'operator_gate'"));
    }

    #[test]
    fn rejects_incomplete_or_duplicate_terminal_mapping() {
        let mut missing = valid_policy();
        missing.terminal.remove("failed");
        assert!(error_for(&missing).contains("missing terminal class 'failed'"));

        let mut duplicate = valid_policy();
        duplicate
            .terminal
            .insert("also_done".to_string(), "succeeded".to_string());
        assert!(error_for(&duplicate).contains("terminal class 'succeeded' is assigned to both"));

        let mut unknown = valid_policy();
        unknown
            .terminal
            .insert("failed".to_string(), "aborted".to_string());
        assert!(error_for(&unknown).contains("unknown class 'aborted'"));
    }

    #[test]
    fn rejects_missing_or_dual_progress_mode_and_unknown_activity() {
        let mut missing = valid_policy();
        missing
            .states
            .insert("reviewing".to_string(), DeclaredState::default());
        assert!(error_for(&missing).contains("exactly one of activity or progress"));

        let mut dual = valid_policy();
        dual.states.get_mut("reviewing").unwrap().progress =
            Some(DeclaredProgressMode::ExternalWait);
        assert!(error_for(&dual).contains("exactly one of activity or progress"));

        let mut unknown_activity = valid_policy();
        unknown_activity
            .states
            .get_mut("reviewing")
            .unwrap()
            .activity = Some("missing_policy".to_string());
        assert!(
            error_for(&unknown_activity).contains("absent from the workflow activities policy map")
        );
    }

    #[test]
    fn rejects_undeclared_transition_and_evidence_targets() {
        let mut transition = valid_policy();
        transition.states.get_mut("reviewing").unwrap().on_failure = Some("missing".to_string());
        let error = error_for(&transition);
        assert!(
            error.contains("active state 'reviewing' on_failure target 'missing' is undeclared")
        );

        let mut evidence = valid_policy();
        evidence
            .evidence_required
            .insert("missing".to_string(), vec!["report".to_string()]);
        assert!(
            error_for(&evidence).contains("evidence requirement target 'missing' is undeclared")
        );
    }

    #[test]
    fn rejects_invalid_recovery_targets() {
        let mut terminal = valid_policy();
        terminal.recovery_targets = vec!["done".to_string()];
        assert!(error_for(&terminal).contains("must name an active state, but it is terminal"));

        let mut missing = valid_policy();
        missing.recovery_targets = vec!["missing".to_string()];
        assert!(error_for(&missing).contains("must name an active state, but it is undeclared"));

        let mut duplicate = valid_policy();
        duplicate.recovery_targets = vec!["reviewing".to_string(), "reviewing".to_string()];
        assert!(error_for(&duplicate).contains("is declared more than once"));
    }

    #[test]
    fn rejects_unreachable_active_and_terminal_states() {
        let mut active = valid_policy();
        active.states.insert(
            "orphan".to_string(),
            DeclaredState {
                progress: Some(DeclaredProgressMode::ExternalWait),
                ..DeclaredState::default()
            },
        );
        assert!(error_for(&active)
            .contains("states unreachable from initial state 'reviewing': orphan"));

        let mut terminal = valid_policy();
        terminal
            .terminal
            .insert("unused".to_string(), "succeeded".to_string());
        terminal.terminal.remove("done");
        terminal.states.get_mut("reviewing").unwrap().on_success = Some("failed".to_string());
        terminal.evidence_required =
            BTreeMap::from([("unused".to_string(), vec!["review_report".to_string()])]);
        let error = error_for(&terminal);
        assert!(
            error.contains("states unreachable from initial state 'reviewing': unused"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn compiles_deterministic_signal_edges_and_target_driver_commands() {
        let mut policy = valid_policy();
        policy.states.insert(
            "waiting".to_string(),
            DeclaredState {
                progress: Some(DeclaredProgressMode::ExternalWait),
                on_success: Some("failed".to_string()),
                ..DeclaredState::default()
            },
        );
        let reviewing = policy.states.get_mut("reviewing").unwrap();
        reviewing.on_success = Some("waiting".to_string());
        reviewing.on_blocked = Some("blocked".to_string());
        reviewing
            .on_signal
            .insert("approved".to_string(), "done".to_string());

        let compiled = build_declarative_definition(&policy, &activity_policies()).unwrap();
        let allowlist = &compiled.registered().allowlist;
        assert_eq!(
            allowlist
                .rule_for("reviewing", "waiting")
                .unwrap()
                .allowed_commands,
            BTreeSet::from([WorkflowCommandType::Wait])
        );
        assert_eq!(
            allowlist
                .rule_for("reviewing", "done")
                .unwrap()
                .allowed_commands,
            BTreeSet::from([WorkflowCommandType::MarkDone])
        );
        assert_eq!(
            allowlist
                .rule_for("reviewing", "blocked")
                .unwrap()
                .allowed_commands,
            BTreeSet::from([
                WorkflowCommandType::MarkBlocked,
                WorkflowCommandType::RequestOperatorAttention,
                WorkflowCommandType::Wait,
            ])
        );
    }
}
