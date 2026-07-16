fn declarative_recovery_definition(
) -> anyhow::Result<super::declarative::DeclarativeWorkflowDefinition> {
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;

    let policy = WorkflowDefinitionPolicy {
        id: "declarative_recovery_integration".to_string(),
        initial: "running".to_string(),
        states: BTreeMap::from([
            (
                "blocked".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::OperatorGate),
                    ..DeclaredState::default()
                },
            ),
            (
                "running".to_string(),
                DeclaredState {
                    activity: Some("run".to_string()),
                    on_success: Some("done".to_string()),
                    on_failure: Some("failed".to_string()),
                    on_signal: BTreeMap::from([("cancel".to_string(), "cancelled".to_string())]),
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
            ("done".to_string(), "succeeded".to_string()),
            ("failed".to_string(), "failed".to_string()),
            ("cancelled".to_string(), "cancelled".to_string()),
        ]),
        evidence_required: BTreeMap::from([
            (
                "running".to_string(),
                vec!["operator_ticket".to_string()],
            ),
            ("done".to_string(), vec!["release_report".to_string()]),
        ]),
        recovery_targets: vec!["running".to_string(), "waiting".to_string()],
        intake: None,
    };
    super::build_declarative_definition(
        &policy,
        &BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]),
    )
}

#[tokio::test]
async fn declarative_recovery_is_atomic_and_persists_exact_driver_status(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let definition = declarative_recovery_definition()?;
    super::register_declarative_workflow_definitions([definition.clone()])?;
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let blocked = |id: &str| {
        WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "blocked",
            WorkflowSubject::new("test", id),
        )
        .with_id(id)
        .with_data(json!({ "definition_hash": definition.definition_hash() }))
    };

    let running = blocked("declarative-recovery-running");
    store.upsert_instance(&running).await?;
    let missing_evidence = store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &running.id,
            action: super::WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator repaired the dependency",
            actor: "operator",
            target_state: Some("running"),
            evidence: &[],
        })
        .await?;
    assert!(matches!(
        missing_evidence,
        super::WorkflowRuntimeRecoveryOutcome::MissingRequiredEvidence { ref detail, .. }
            if detail.contains("missing required evidence")
    ));
    assert_eq!(store.get_instance(&running.id).await?.unwrap().state, "blocked");
    let rejection_events = store.events_for(&running.id).await?;
    assert_eq!(rejection_events.len(), 1);
    assert_eq!(rejection_events[0].event_type, "WorkflowRuntimeRecoveryRejected");
    assert_eq!(rejection_events[0].event["reason_code"], "missing_required_evidence");
    assert!(store.decisions_for(&running.id).await?.is_empty());
    assert!(store.commands_for(&running.id).await?.is_empty());

    let evidence = [WorkflowEvidence::new("operator_ticket", "approved")];
    let recovered = store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &running.id,
            action: super::WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator repaired the dependency",
            actor: "operator",
            target_state: Some("running"),
            evidence: &evidence,
        })
        .await?;
    assert!(matches!(
        recovered,
        super::WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));
    let running_commands = store.commands_for(&running.id).await?;
    assert_eq!(running_commands.len(), 1);
    assert_eq!(running_commands[0].command.command_type, WorkflowCommandType::EnqueueActivity);
    assert_eq!(running_commands[0].status, WorkflowCommandStatus::Pending);
    assert_eq!(store.decisions_for(&running.id).await?[0].decision.evidence, evidence);

    let waiting = blocked("declarative-recovery-waiting");
    store.upsert_instance(&waiting).await?;
    store
        .recover_stopped_instance(super::WorkflowRuntimeRecoveryRequest {
            workflow_id: &waiting.id,
            action: super::WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator selected external wait",
            actor: "operator",
            target_state: Some("waiting"),
            evidence: &[],
        })
        .await?;
    let waiting_commands = store.commands_for(&waiting.id).await?;
    assert_eq!(waiting_commands.len(), 1);
    assert_eq!(waiting_commands[0].command.command_type, WorkflowCommandType::Wait);
    assert_eq!(waiting_commands[0].status, WorkflowCommandStatus::HandledInline);
    assert!(store.pending_commands(10).await?.iter().all(|command| command.workflow_id != waiting.id));

    let completion = WorkflowInstance::new(
        definition.policy().id.clone(),
        definition.definition_version(),
        "running",
        WorkflowSubject::new("test", "declarative-completion-missing-evidence"),
    )
    .with_id("declarative-completion-missing-evidence")
    .with_data(json!({ "definition_hash": definition.definition_hash() }));
    store.upsert_instance(&completion).await?;
    let result = ActivityResult::succeeded("run", "completed without the release report");
    let command = WorkflowCommand::enqueue_activity("run", "declarative-completion-command");
    let policy = store
        .commit_parent_runtime_completion(
            &completion.id,
            "workflow_runtime_worker",
            json!({
                "runtime_job_id": "declarative-completion-job",
                "command": command,
                "activity_result": result,
            }),
        )
        .await?
        .expect("completion should persist a blocked policy decision");
    assert!(policy.accepted);
    assert_eq!(policy.decision.decision, "block_invalid_agent_output");
    assert_eq!(store.get_instance(&completion.id).await?.unwrap().state, "blocked");
    let completion_decisions = store.decisions_for(&completion.id).await?;
    assert_eq!(completion_decisions.len(), 2);
    assert!(!completion_decisions[0].accepted);
    assert!(completion_decisions[0]
        .rejection_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("MissingRequiredEvidence")));
    assert!(completion_decisions[1].accepted);
    let completion_commands = store.commands_for(&completion.id).await?;
    assert_eq!(completion_commands.len(), 2);
    assert!(completion_commands.iter().all(|record| {
        record.status == WorkflowCommandStatus::HandledInline
            && matches!(
                record.command.command_type,
                WorkflowCommandType::MarkBlocked
                    | WorkflowCommandType::RequestOperatorAttention
            )
    }));
    Ok(())
}
