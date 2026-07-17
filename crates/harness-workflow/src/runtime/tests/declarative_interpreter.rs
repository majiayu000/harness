mod declarative_interpreter {
    use super::super::*;
    use super::issue_instance;
    use crate::runtime::reducer::declarative_completion::reduce_declarative_completion;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    const DEFINITION_ID: &str = "declarative_interpreter_fixture_v1";

    fn policy() -> WorkflowDefinitionPolicy {
        WorkflowDefinitionPolicy {
            id: DEFINITION_ID.to_string(),
            initial: "reviewing".to_string(),
            states: BTreeMap::from([
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "needs_operator".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "publishing".to_string(),
                    DeclaredState {
                        activity: Some("publish".to_string()),
                        on_success: Some("completed".to_string()),
                        on_failure: Some("aborted".to_string()),
                        on_blocked: Some("blocked".to_string()),
                        on_signal: BTreeMap::from([(
                            "cancel".to_string(),
                            "withdrawn".to_string(),
                        )]),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "reviewing".to_string(),
                    DeclaredState {
                        activity: Some("review".to_string()),
                        on_success: Some("publishing".to_string()),
                        on_failure: Some("aborted".to_string()),
                        on_blocked: Some("needs_operator".to_string()),
                        on_signal: BTreeMap::from([
                            ("alpha_approved".to_string(), "completed".to_string()),
                            ("cancel".to_string(), "withdrawn".to_string()),
                            ("wait".to_string(), "waiting".to_string()),
                            ("zeta_rejected".to_string(), "aborted".to_string()),
                        ]),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "waiting".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::ExternalWait),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("aborted".to_string(), "failed".to_string()),
                ("completed".to_string(), "succeeded".to_string()),
                ("withdrawn".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::from([(
                "completed".to_string(),
                vec!["review_report".to_string()],
            )]),
            recovery_targets: vec!["reviewing".to_string()],
            intake: None,
        }
    }

    fn definition_for(
        policy: &WorkflowDefinitionPolicy,
    ) -> crate::runtime::DeclarativeWorkflowDefinition {
        build_declarative_definition(
            policy,
            &BTreeMap::from([
                ("publish".to_string(), WorkflowActivityPolicy::default()),
                ("review".to_string(), WorkflowActivityPolicy::default()),
            ]),
        )
        .expect("interpreter fixture should compile")
    }

    fn instance_for(
        definition: &crate::runtime::DeclarativeWorkflowDefinition,
        state: &str,
    ) -> WorkflowInstance {
        WorkflowInstance::new(
            DEFINITION_ID,
            definition.definition_version(),
            state,
            WorkflowSubject::new("document", "doc-1"),
        )
        .with_id("declarative-run-1")
        .with_data(json!({ "definition_hash": definition.definition_hash() }))
    }

    fn completion_event(
        instance: &WorkflowInstance,
        activity: &str,
        result: &ActivityResult,
    ) -> WorkflowEvent {
        WorkflowEvent::new(
            &instance.id,
            1,
            RUNTIME_JOB_COMPLETED_EVENT,
            "runtime-test",
        )
        .with_payload(json!({
            "command_id": "command-1",
            "runtime_job_id": "job-1",
            "command": WorkflowCommand::enqueue_activity(activity, "source-command"),
            "activity_result": result,
        }))
    }

    fn blocked_result(activity: &str) -> ActivityResult {
        let mut result = ActivityResult::failed(activity, "blocked", "operator input required");
        result.status = ActivityStatus::Blocked;
        result
    }

    #[test]
    fn success_signal_precedence_is_deterministic_and_maps_only_artifact_evidence() {
        let definition = definition_for(&policy());
        let instance = instance_for(&definition, "reviewing");
        let result = ActivityResult::succeeded("review", "reviewed")
            .with_signal(ActivitySignal::new("zeta_rejected", json!({})))
            .with_signal(ActivitySignal::new("alpha_approved", json!({})))
            .with_artifact(ActivityArtifact::new(
                "workflow_decision",
                json!({ "next_state": "forged" }),
            ))
            .with_artifact(ActivityArtifact::new(
                "review_report",
                json!({ "approved": true }),
            ));
        let event = completion_event(&instance, "review", &result);

        let decision = reduce_declarative_completion(&definition, &instance, &event, &result);

        assert_eq!(decision.next_state, "completed");
        assert_eq!(decision.commands.len(), 1);
        assert_eq!(
            decision.commands[0].command_type,
            WorkflowCommandType::MarkDone
        );
        assert!(decision.reason.contains("alpha_approved"));
        assert!(decision.reason.contains("zeta_rejected"));
        assert_eq!(
            decision
                .evidence
                .iter()
                .map(|evidence| evidence.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["runtime_completion", "review_report"]
        );

        let replay = reduce_declarative_completion(&definition, &instance, &event, &result);
        assert_eq!(
            decision.commands[0].dedupe_key,
            replay.commands[0].dedupe_key
        );
        let different_event = completion_event(&instance, "review", &result);
        let different =
            reduce_declarative_completion(&definition, &instance, &different_event, &result);
        assert_ne!(
            decision.commands[0].dedupe_key,
            different.commands[0].dedupe_key
        );
    }

    #[test]
    fn every_target_shape_emits_its_driver_in_the_transition_decision() {
        let definition = definition_for(&policy());
        let instance = instance_for(&definition, "reviewing");
        let cases = [
            (
                ActivityResult::succeeded("review", "continue"),
                "publishing",
                WorkflowCommandType::EnqueueActivity,
            ),
            (
                ActivityResult::succeeded("review", "wait")
                    .with_signal(ActivitySignal::new("wait", json!({}))),
                "waiting",
                WorkflowCommandType::Wait,
            ),
            (
                blocked_result("review"),
                "needs_operator",
                WorkflowCommandType::RequestOperatorAttention,
            ),
            (
                ActivityResult::failed("review", "failed", "review failed"),
                "aborted",
                WorkflowCommandType::MarkFailed,
            ),
            (
                ActivityResult::cancelled("review", "cancelled"),
                "withdrawn",
                WorkflowCommandType::MarkCancelled,
            ),
        ];

        for (result, expected_state, expected_command) in cases {
            let event = completion_event(&instance, "review", &result);
            let decision =
                reduce_declarative_completion(&definition, &instance, &event, &result);
            assert_eq!(decision.next_state, expected_state);
            assert_eq!(decision.commands.len(), 1);
            assert_eq!(decision.commands[0].command_type, expected_command);
        }
    }

    #[test]
    fn generic_blocked_failed_and_retry_fallbacks_remain_declared_shape_aware() {
        let mut fallback_policy = policy();
        let reviewing = fallback_policy.states.get_mut("reviewing").unwrap();
        reviewing.on_blocked = None;
        reviewing.on_failure = None;
        fallback_policy.states.remove("needs_operator");
        let definition = definition_for(&fallback_policy);
        let instance = instance_for(&definition, "reviewing");

        let blocked = blocked_result("review");
        let blocked_event = completion_event(&instance, "review", &blocked);
        let blocked_decision =
            reduce_declarative_completion(&definition, &instance, &blocked_event, &blocked);
        assert_eq!(blocked_decision.next_state, "blocked");
        assert!(blocked_decision
            .commands
            .iter()
            .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
        assert!(blocked_decision.commands.iter().any(|command| {
            command.command_type == WorkflowCommandType::RequestOperatorAttention
        }));

        let failed = ActivityResult::failed("review", "failed", "review failed");
        let failed_event = completion_event(&instance, "review", &failed);
        let failed_decision =
            reduce_declarative_completion(&definition, &instance, &failed_event, &failed);
        assert_eq!(failed_decision.next_state, "aborted");
        assert_eq!(
            failed_decision.commands[0].command_type,
            WorkflowCommandType::MarkFailed
        );

        let retrying = instance.clone().with_data(json!({
            "definition_hash": definition.definition_hash(),
            "runtime_retry_policy": {
                "activity_retries": {
                    "review": { "max_failed_activity_retries": 2 }
                }
            }
        }));
        let retry_event = completion_event(&retrying, "review", &failed);
        let retry_decision =
            reduce_declarative_completion(&definition, &retrying, &retry_event, &failed);
        assert_eq!(retry_decision.next_state, "reviewing");
        assert_eq!(
            retry_decision.commands[0].command_type,
            WorkflowCommandType::EnqueueActivity
        );
    }

    #[test]
    fn unexpected_state_activity_or_command_fails_closed() {
        let definition = definition_for(&policy());
        let valid_instance = instance_for(&definition, "reviewing");
        let valid_result = ActivityResult::succeeded("review", "reviewed");

        let terminal_instance = instance_for(&definition, "completed");
        let terminal_event = completion_event(&terminal_instance, "review", &valid_result);
        let wrong_state = reduce_declarative_completion(
            &definition,
            &terminal_instance,
            &terminal_event,
            &valid_result,
        );

        let wrong_result = ActivityResult::succeeded("publish", "published");
        let wrong_activity_event = completion_event(&valid_instance, "review", &wrong_result);
        let wrong_activity = reduce_declarative_completion(
            &definition,
            &valid_instance,
            &wrong_activity_event,
            &wrong_result,
        );

        let missing_command = WorkflowEvent::new(
            &valid_instance.id,
            1,
            RUNTIME_JOB_COMPLETED_EVENT,
            "runtime-test",
        )
        .with_payload(json!({ "activity_result": valid_result }));
        let wrong_command = reduce_declarative_completion(
            &definition,
            &valid_instance,
            &missing_command,
            &valid_result,
        );

        for decision in [wrong_state, wrong_activity, wrong_command] {
            assert_eq!(decision.next_state, "blocked");
            assert_eq!(decision.decision, "block_invalid_agent_output");
            assert!(decision
                .commands
                .iter()
                .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
        }
    }

    #[test]
    fn empty_artifact_type_fails_closed_instead_of_becoming_evidence() {
        let definition = definition_for(&policy());
        let instance = instance_for(&definition, "reviewing");
        let result = ActivityResult::succeeded("review", "reviewed")
            .with_artifact(ActivityArtifact::new("", json!({})));
        let event = completion_event(&instance, "review", &result);

        let decision = reduce_declarative_completion(&definition, &instance, &event, &result);

        assert_eq!(decision.decision, "block_invalid_agent_output");
        assert_eq!(decision.next_state, "blocked");
        assert!(decision.reason.contains("artifact_type must not be empty"));
    }

    #[test]
    fn reducer_entry_uses_strict_pinning_and_does_not_intercept_builtins() {
        let definition = definition_for(&policy());
        register_declarative_workflow_definitions([definition.clone()])
            .expect("fixture definition should register once");
        let instance = instance_for(&definition, "reviewing");
        let result = ActivityResult::succeeded("review", "reviewed")
            .with_artifact(ActivityArtifact::new("review_report", json!({})));
        let event = completion_event(&instance, "review", &result);
        let decision = reduce_runtime_job_completed(&instance, &event)
            .expect("completion should reduce")
            .expect("declarative completion should produce a decision");
        assert_eq!(decision.next_state, "publishing");

        let missing_hash = WorkflowInstance::new(
            DEFINITION_ID,
            definition.definition_version(),
            "reviewing",
            WorkflowSubject::new("document", "missing-hash"),
        );
        let missing_hash_event = completion_event(&missing_hash, "review", &result);
        let pin_error = reduce_runtime_job_completed(&missing_hash, &missing_hash_event)
            .expect("pin error should reduce")
            .expect("pin error should block");
        assert_eq!(pin_error.decision, "definition_version_missing");
        assert_eq!(pin_error.next_state, "blocked");

        let invalid_hash = instance.clone().with_data(json!({
            "definition_hash": "not-a-canonical-hash"
        }));
        let invalid_hash_event = completion_event(&invalid_hash, "review", &result);
        let invalid_hash_decision = reduce_runtime_job_completed(&invalid_hash, &invalid_hash_event)
            .expect("invalid hash should reduce")
            .expect("invalid hash should block");
        assert_eq!(invalid_hash_decision.decision, "definition_version_missing");
        assert!(invalid_hash_decision.reason.contains("invalid_hash"));

        let mut mismatched_hash = definition.definition_hash().to_string();
        let replacement = if mismatched_hash.ends_with('0') {
            '1'
        } else {
            '0'
        };
        mismatched_hash.pop();
        mismatched_hash.push(replacement);
        let hash_mismatch = instance
            .clone()
            .with_data(json!({ "definition_hash": mismatched_hash }));
        let hash_mismatch_event = completion_event(&hash_mismatch, "review", &result);
        let hash_mismatch_decision =
            reduce_runtime_job_completed(&hash_mismatch, &hash_mismatch_event)
                .expect("hash mismatch should reduce")
                .expect("hash mismatch should block");
        assert_eq!(
            hash_mismatch_decision.decision,
            "definition_version_missing"
        );
        assert!(hash_mismatch_decision.reason.contains("hash_mismatch"));

        let missing_version = WorkflowInstance::new(
            DEFINITION_ID,
            u32::MAX,
            "reviewing",
            WorkflowSubject::new("document", "missing-version"),
        )
        .with_data(json!({ "definition_hash": definition.definition_hash() }));
        let missing_version_event = completion_event(&missing_version, "review", &result);
        let missing_version_decision =
            reduce_runtime_job_completed(&missing_version, &missing_version_event)
                .expect("missing version should reduce")
                .expect("missing version should block");
        assert_eq!(
            missing_version_decision.decision,
            "definition_version_missing"
        );
        assert!(missing_version_decision.reason.contains("missing_version"));

        let builtin = issue_instance("replanning");
        let builtin_result = ActivityResult::succeeded("replan_issue", "replanned");
        let builtin_event = WorkflowEvent::new(
            &builtin.id,
            1,
            RUNTIME_JOB_COMPLETED_EVENT,
            "runtime-test",
        )
        .with_payload(json!({
            "command_id": "builtin-command",
            "activity_result": builtin_result,
        }));
        let builtin_decision = reduce_runtime_job_completed(&builtin, &builtin_event)
            .expect("builtin completion should reduce")
            .expect("builtin completion should retain its reducer");
        assert_eq!(
            builtin_decision.decision,
            "resume_implementation_after_replan"
        );
    }
}
