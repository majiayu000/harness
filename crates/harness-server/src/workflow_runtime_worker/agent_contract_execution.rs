//! Durable attempt and correction loop for pinned agent contracts.

use crate::http::AppState;
use harness_core::agent::AgentBackend;
use harness_core::types::TurnId;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus,
    AgentContractAttemptReservation, RuntimeJob, AGENT_CONTRACT_VERDICT_ARTIFACT,
};
use std::sync::Arc;

use super::agent_contract_assessment::attach_server_assessment;
use super::agent_contract_attempt::{contract_violations, parse_contract_verdict};
use super::agent_contract_enforcement::{turn_observation_artifact, PinnedJobAgentContract};
use super::agent_contract_stream::{
    budget_stop_artifact, enforced_budget_cost_error, execute_agent_contract_attempt,
    ContractAttemptFailure,
};
use super::turn_engine::helpers::RuntimeUsageContext;

pub(super) async fn execute_contract_attempts(
    state: &AppState,
    job: &RuntimeJob,
    backend: Arc<dyn AgentBackend>,
    pinned: &PinnedJobAgentContract,
    activity: &str,
    model: Option<String>,
    reasoning_effort: Option<String>,
    timeout_secs: u64,
    max_turns: Option<u32>,
    runtime_usage: Option<&RuntimeUsageContext>,
    lease_lost: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<ActivityResult> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("workflow runtime store is unavailable"))?;
    if let Some(error) = runtime_usage
        .and_then(|context| enforced_budget_cost_error(backend.as_ref(), &context.budget_policy))
    {
        return Ok(ActivityResult {
            activity: activity.to_string(),
            status: ActivityStatus::Blocked,
            summary: "Agent contract execution requires observable USD cost for the enforced workflow budget.".to_string(),
            artifacts: Vec::new(),
            signals: Vec::new(),
            validation: Vec::new(),
            error: Some(error),
            error_kind: Some(ActivityErrorKind::Configuration),
        });
    }
    let mut observations = Vec::new();
    let mut corrections_used = 0;
    let mut last_validation_error = "agent contract produced no valid verdict".to_string();
    let mut previous_output = String::new();
    for primary_attempt in 1..=pinned.contract.max_primary_attempts {
        for correction_attempt in 0..=pinned.contract.max_corrections {
            match store
                .reserve_agent_contract_attempt(job, max_turns, primary_attempt, correction_attempt)
                .await?
            {
                AgentContractAttemptReservation::Reserved => {}
                AgentContractAttemptReservation::BudgetExhausted => {
                    return Ok(ActivityResult {
                        activity: activity.to_string(),
                        status: ActivityStatus::Blocked,
                        summary: "Agent contract workflow turn budget was exhausted.".to_string(),
                        artifacts: observations,
                        signals: Vec::new(),
                        validation: Vec::new(),
                        error: Some("the next pinned agent-contract attempt would exceed the runtime profile max_turns".to_string()),
                        error_kind: None,
                    });
                }
                AgentContractAttemptReservation::AlreadyReserved
                | AgentContractAttemptReservation::StaleLease => {
                    let mut result = ActivityResult::failed(
                        activity,
                        "Agent contract attempt reservation is no longer available.",
                        format!(
                            "attempt reservation primary:{primary_attempt}:correction:{correction_attempt} was already consumed or the runtime lease is stale; refusing to invoke the model"
                        ),
                    )
                    .with_error_kind(ActivityErrorKind::Fatal);
                    result.artifacts = observations;
                    return Ok(result);
                }
            }
            if correction_attempt > 0 {
                corrections_used += 1;
            }
            let correction = (correction_attempt > 0)
                .then_some((previous_output.as_str(), last_validation_error.as_str()));
            let turn_number = (primary_attempt - 1)
                .saturating_mul(pinned.contract.max_corrections + 1)
                .saturating_add(correction_attempt)
                .saturating_add(1);
            let turn_id = TurnId::from_str(&format!("agent-contract:{}:{turn_number}", job.id));
            let attempt = match execute_agent_contract_attempt(
                Arc::clone(&backend),
                pinned,
                model.clone(),
                reasoning_effort.clone(),
                timeout_secs,
                correction,
                runtime_usage.map(|context| (context, &turn_id)),
                Some(lease_lost.clone()),
            )
            .await
            {
                Ok(attempt) => attempt,
                Err(error) => {
                    let (error, output, budget_stop) =
                        match error.downcast::<ContractAttemptFailure>() {
                            Ok(failure) => {
                                let (attempt, error, budget_stop) = failure.into_parts();
                                observations.push(turn_observation_artifact(
                                    turn_number,
                                    &attempt.items,
                                    &attempt.observations,
                                ));
                                (error, attempt.output, budget_stop)
                            }
                            Err(error) => (error, String::new(), None),
                        };
                    let error = error.to_string();
                    record_attempt_completed(
                        store,
                        job,
                        primary_attempt,
                        correction_attempt,
                        if budget_stop.is_some() {
                            "budget_exhausted"
                        } else {
                            "execution_error"
                        },
                        &output,
                        Some(&error),
                    )
                    .await?;
                    if let Some(stop) = budget_stop {
                        return Ok(ActivityResult {
                            activity: activity.to_string(),
                            status: ActivityStatus::Blocked,
                            summary:
                                "Agent contract attempt stopped at the enforced workflow budget."
                                    .to_string(),
                            artifacts: observations,
                            signals: Vec::new(),
                            validation: Vec::new(),
                            error: Some(error),
                            error_kind: None,
                        }
                        .with_artifact(budget_stop_artifact(&stop)));
                    }
                    let mut result = ActivityResult::failed(
                        activity,
                        "Agent contract attempt failed before producing a verdict.",
                        error,
                    )
                    .with_error_kind(ActivityErrorKind::Fatal);
                    result.artifacts = observations;
                    return Ok(result);
                }
            };
            observations.push(turn_observation_artifact(
                turn_number,
                &attempt.items,
                &attempt.observations,
            ));
            let violations = contract_violations(&attempt);
            if !violations.is_empty() {
                record_attempt_completed(
                    store,
                    job,
                    primary_attempt,
                    correction_attempt,
                    "contract_violation",
                    &attempt.output,
                    Some(&violations.join("; ")),
                )
                .await?;
                let mut result = ActivityResult::failed(
                    activity,
                    "Agent contract attempt is invalid: the agent used a forbidden surface.",
                    format!("contract violations: {}", violations.join("; ")),
                )
                .with_error_kind(ActivityErrorKind::Fatal);
                result.artifacts = observations;
                return Ok(result);
            }
            match parse_contract_verdict(&attempt.output, &pinned.contract) {
                Ok(verdict) => {
                    record_attempt_completed(
                        store,
                        job,
                        primary_attempt,
                        correction_attempt,
                        "valid",
                        &attempt.output,
                        None,
                    )
                    .await?;
                    let mut result = ActivityResult::succeeded(
                        activity,
                        format!("Agent contract verdict: {}.", verdict.outcome),
                    );
                    result.artifacts = observations;
                    result.artifacts.push(ActivityArtifact::new(
                        AGENT_CONTRACT_VERDICT_ARTIFACT,
                        serde_json::json!({
                            "output_schema": pinned.contract.output_schema,
                            "definition_hash": pinned.definition_hash,
                            "outcome": verdict.outcome,
                            "verdict": verdict.raw,
                        }),
                    ));
                    return attach_server_assessment(
                        job,
                        pinned,
                        result,
                        primary_attempt,
                        corrections_used,
                    );
                }
                Err(reason) => {
                    record_attempt_completed(
                        store,
                        job,
                        primary_attempt,
                        correction_attempt,
                        "invalid_verdict",
                        &attempt.output,
                        Some(&reason),
                    )
                    .await?;
                    previous_output = attempt.output;
                    last_validation_error = reason;
                }
            }
        }
    }
    let mut result = ActivityResult::failed(
        activity,
        "Agent contract exhausted its pinned attempt budget without a valid verdict.",
        last_validation_error,
    )
    .with_error_kind(ActivityErrorKind::Fatal);
    result.artifacts = observations;
    Ok(result)
}

async fn record_attempt_completed(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    job: &RuntimeJob,
    primary_attempt: u32,
    correction_attempt: u32,
    status: &str,
    output: &str,
    validation_error: Option<&str>,
) -> anyhow::Result<()> {
    store
        .record_runtime_event(
            &job.id,
            "AgentContractAttemptCompleted",
            serde_json::json!({
                "primary_attempt": primary_attempt,
                "correction_attempt": correction_attempt,
                "status": status,
                "output": output,
                "validation_error": validation_error,
            }),
        )
        .await
        .map(|_| ())
}
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::config::workflow::{
        AgentContractMutationPolicy, AgentContractToolPolicy, AgentContractWorkspacePolicy,
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowAgentContract,
        WorkflowDefinitionPolicy,
    };
    use harness_workflow::runtime::{
        build_declarative_definition, build_declarative_submission_decision, DataProvenance,
        RuntimeKind, RuntimeProfile, WorkflowDecisionRecord, WorkflowDefinitionRegistry,
        WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Mutex;

    struct LoopAgent {
        requests: Mutex<usize>,
    }

    #[async_trait]
    impl AgentBackend for LoopAgent {
        fn name(&self) -> &str {
            "loop-agent"
        }

        fn agent_contract_capabilities(&self) -> harness_core::agent::AgentContractCapabilities {
            harness_core::agent::AgentContractCapabilities {
                prompt_only_launch: true,
                pinned_output_schema: true,
                attempt_observation_stream: true,
            }
        }

        fn reports_usage_cost(&self) -> bool {
            true
        }

        async fn execute_stream(
            &self,
            _request: harness_core::agent::AgentRequest,
            tx: tokio::sync::mpsc::Sender<harness_core::agent::AgentEvent>,
        ) -> harness_core::error::Result<()> {
            *self.requests.lock().await += 1;
            tx.send(harness_core::agent::AgentEvent::TokenUsage {
                usage: harness_core::types::TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    total_tokens: 18,
                    cost_usd: 0.125,
                },
            })
            .await
            .map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(error.to_string())
            })?;
            tx.send(harness_core::agent::AgentEvent::TurnCompleted {
                output: json!({
                    "schema": "harness.semantic_verdict.v1",
                    "outcome": "small",
                    "rationale": "The supplied facts describe one bounded change.",
                    "evidence_refs": []
                })
                .to_string(),
            })
            .await
            .map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(error.to_string())
            })?;
            Ok(())
        }
    }

    fn loop_definition() -> harness_workflow::runtime::DeclarativeWorkflowDefinition {
        let policy = WorkflowDefinitionPolicy {
            id: "agent_contract_loop".to_string(),
            initial: "classifying".to_string(),
            states: BTreeMap::from([
                (
                    "classifying".to_string(),
                    DeclaredState {
                        activity: Some("classify_scope".to_string()),
                        on_failure: Some("blocked".to_string()),
                        on_signal: BTreeMap::from([
                            ("small".to_string(), "implementing".to_string()),
                            ("large".to_string(), "blocked".to_string()),
                        ]),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "implementing".to_string(),
                    DeclaredState {
                        activity: Some("implement_change".to_string()),
                        on_success: Some("done".to_string()),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
                ("cancelled".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["classifying".to_string()],
            intake: None,
        };
        let contract = WorkflowAgentContract {
            input_schema: "harness.semantic_activity_input.v1".to_string(),
            output_schema: "harness.semantic_verdict.v1".to_string(),
            allowed_outcomes: vec!["small".to_string(), "large".to_string()],
            tools: AgentContractToolPolicy::None,
            mutation: AgentContractMutationPolicy::Forbidden,
            workspace: AgentContractWorkspacePolicy::EphemeralEmpty,
            fresh_context: true,
            max_primary_attempts: 1,
            max_corrections: 1,
        };
        build_declarative_definition(
            &policy,
            &BTreeMap::from([
                (
                    "classify_scope".to_string(),
                    WorkflowActivityPolicy {
                        prompt: Some("Classify only the supplied facts.".to_string()),
                        agent_contract: Some(contract),
                        ..WorkflowActivityPolicy::default()
                    },
                ),
                (
                    "implement_change".to_string(),
                    WorkflowActivityPolicy::default(),
                ),
            ]),
        )
        .expect("loop definition should compile")
    }

    fn loop_registry() -> WorkflowDefinitionRegistry {
        let mut registry = WorkflowDefinitionRegistry::with_builtins();
        registry
            .register_declarative_current(loop_definition())
            .expect("loop definition should register");
        registry
    }

    fn pinned_contract() -> PinnedJobAgentContract {
        let definition = loop_definition();
        let pinned_activity = definition
            .agent_contract("classify_scope")
            .expect("classifier contract");
        let contract_value = serde_json::to_value(&pinned_activity.contract).expect("contract");
        PinnedJobAgentContract {
            contract: pinned_activity.contract.clone(),
            prompt: pinned_activity.prompt.clone(),
            input: json!({
                "schema": "harness.semantic_activity_input.v1",
                "subject": {"kind": "test", "identity": "contract-execution"},
                "facts": {"scope": "small"},
                "provenance": {"/scope": "server"},
                "contract_hash": harness_workflow::runtime::stable_remote_fact_hash(&contract_value),
            }),
            definition_hash: definition.definition_hash().to_string(),
        }
    }

    #[tokio::test]
    async fn lease_loss_cancels_the_contract_backend_before_returning() {
        struct CancellationMarker(Arc<AtomicBool>);
        impl Drop for CancellationMarker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct HangingAgent {
            started: Arc<tokio::sync::Notify>,
            cancelled: Arc<AtomicBool>,
        }
        #[async_trait]
        impl AgentBackend for HangingAgent {
            fn name(&self) -> &str {
                "hanging-contract-agent"
            }
            fn agent_contract_capabilities(
                &self,
            ) -> harness_core::agent::AgentContractCapabilities {
                harness_core::agent::AgentContractCapabilities {
                    prompt_only_launch: true,
                    pinned_output_schema: true,
                    attempt_observation_stream: true,
                }
            }
            async fn execute_stream(
                &self,
                _request: harness_core::agent::AgentRequest,
                _tx: tokio::sync::mpsc::Sender<harness_core::agent::AgentEvent>,
            ) -> harness_core::error::Result<()> {
                let _marker = CancellationMarker(Arc::clone(&self.cancelled));
                self.started.notify_one();
                std::future::pending().await
            }
        }

        let pinned = pinned_contract();
        let started = Arc::new(tokio::sync::Notify::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(HangingAgent {
            started: Arc::clone(&started),
            cancelled: Arc::clone(&cancelled),
        });
        let (lease_lost, receiver) = tokio::sync::watch::channel(false);
        let attempt = tokio::spawn(async move {
            super::super::agent_contract_stream::execute_agent_contract_attempt(
                backend,
                &pinned,
                None,
                None,
                30,
                None,
                None,
                Some(receiver),
            )
            .await
        });
        started.notified().await;
        lease_lost.send_replace(true);
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), attempt)
            .await
            .expect("lease-loss cancellation should not hang")
            .expect("attempt task should not panic")
            .expect_err("lease loss must fail the attempt");
        assert!(error.to_string().contains("lease loss"), "{error}");
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn closed_event_channel_does_not_escape_attempt_timeout() {
        struct CancellationMarker(Arc<AtomicBool>);
        impl Drop for CancellationMarker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct ClosedChannelBackend {
            cancelled: Arc<AtomicBool>,
        }
        #[async_trait]
        impl AgentBackend for ClosedChannelBackend {
            fn name(&self) -> &str {
                "closed-channel-contract-backend"
            }
            fn agent_contract_capabilities(
                &self,
            ) -> harness_core::agent::AgentContractCapabilities {
                harness_core::agent::AgentContractCapabilities {
                    prompt_only_launch: true,
                    pinned_output_schema: true,
                    attempt_observation_stream: true,
                }
            }
            async fn execute_stream(
                &self,
                _request: harness_core::agent::AgentRequest,
                tx: tokio::sync::mpsc::Sender<harness_core::agent::AgentEvent>,
            ) -> harness_core::error::Result<()> {
                let _marker = CancellationMarker(Arc::clone(&self.cancelled));
                drop(tx);
                std::future::pending().await
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(ClosedChannelBackend {
            cancelled: Arc::clone(&cancelled),
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::super::agent_contract_stream::execute_agent_contract_attempt(
                backend,
                &pinned_contract(),
                None,
                None,
                1,
                None,
                None,
                None,
            ),
        )
        .await
        .expect("the attempt timeout must remain active after its event channel closes");
        let error = result.expect_err("the closed-channel backend must time out");

        assert!(error.to_string().contains("timed out after 1s"), "{error}");
        assert!(cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn continuous_event_stream_does_not_starve_attempt_timeout() {
        struct StreamingBackend;
        #[async_trait]
        impl AgentBackend for StreamingBackend {
            fn name(&self) -> &str {
                "streaming-contract-backend"
            }
            fn agent_contract_capabilities(
                &self,
            ) -> harness_core::agent::AgentContractCapabilities {
                harness_core::agent::AgentContractCapabilities {
                    prompt_only_launch: true,
                    pinned_output_schema: true,
                    attempt_observation_stream: true,
                }
            }
            async fn execute_stream(
                &self,
                _request: harness_core::agent::AgentRequest,
                tx: tokio::sync::mpsc::Sender<harness_core::agent::AgentEvent>,
            ) -> harness_core::error::Result<()> {
                loop {
                    tx.send(harness_core::agent::AgentEvent::TurnStarted)
                        .await
                        .map_err(|error| {
                            harness_core::error::HarnessError::AgentExecution(error.to_string())
                        })?;
                }
            }
        }

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            super::super::agent_contract_stream::execute_agent_contract_attempt(
                Arc::new(StreamingBackend),
                &pinned_contract(),
                None,
                None,
                1,
                None,
                None,
                None,
            ),
        )
        .await
        .expect("continuous events must not starve the attempt deadline");
        let error = result.expect_err("continuous event stream must time out");

        assert!(error.to_string().contains("timed out after 1s"), "{error}");
    }

    #[tokio::test]
    async fn real_submission_assessment_routes_and_reopens_without_model_replay(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let agent = Arc::new(LoopAgent {
            requests: Mutex::new(0),
        });
        let mut agent_registry = harness_agents::registry::AgentRegistry::new("codex");
        agent_registry.register("codex", agent.clone());
        let mut state = crate::test_helpers::make_test_state_with_project_root_and_registry(
            dir.path(),
            &project_root,
            agent_registry,
        )
        .await?;
        let store_path =
            harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime");
        let database_url = crate::test_helpers::test_database_url()?;
        let store = Arc::new(
            WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url))
                .await?
                .with_definition_registry(loop_registry().into_shared()),
        );
        state.core.workflow_runtime_store = Some(Arc::clone(&store));
        let state = Arc::new(state);
        let definition = loop_definition();
        store
            .persist_definition_version(
                &harness_workflow::runtime::persisted_declarative_definition(&definition, None),
            )
            .await?;
        let instance = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "classifying",
            WorkflowSubject::new("test", "loop-e2e"),
        )
        .with_id("agent-contract-loop-e2e")
        .with_classified_data(
            json!({
                "definition_hash": definition.definition_hash(),
                "project_id": project_root,
                "task_id": "loop-e2e-task",
                "scope": "bounded"
            }),
            DataProvenance::Server,
        );
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &instance)
            .await?;
        let submission = build_declarative_submission_decision(&definition, &instance)?;
        let record = WorkflowDecisionRecord::accepted(submission.clone(), None);
        store.record_decision(&record).await?;
        let command_id = store
            .enqueue_command(&instance.id, Some(&record.id), &submission.commands[0])
            .await?;
        let mut profile = RuntimeProfile::new("codex-contract", RuntimeKind::CodexExec);
        profile.timeout_secs = Some(30);
        let runtime_job = store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::CodexExec,
                &profile.name,
                json!({
                    "workflow_id": instance.id,
                    "command_id": command_id,
                    "command_type": submission.commands[0].command_type,
                    "dedupe_key": submission.commands[0].dedupe_key,
                    "activity": "classify_scope",
                    "command": submission.commands[0].command,
                    "runtime_profile": profile,
                }),
            )
            .await?;

        let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
            &state,
            "loop-worker",
            chrono::Duration::minutes(5),
        )
        .await?;

        assert_eq!(tick.succeeded, 1);
        assert_eq!(*agent.requests.lock().await, 1);
        let completed = store
            .get_runtime_job(&runtime_job.id)
            .await?
            .expect("runtime job should persist");
        let output: ActivityResult =
            serde_json::from_value(completed.output.expect("completed job should carry output"))?;
        assert_eq!(
            output
                .artifacts
                .iter()
                .filter(|artifact| artifact.artifact_type == "agent_contract_assessment")
                .count(),
            1
        );
        assert_eq!(
            store
                .get_instance(&instance.id)
                .await?
                .expect("workflow should persist")
                .state,
            "implementing"
        );
        let workflow_events = store.events_for(&instance.id).await?;
        let completion_event = workflow_events
            .iter()
            .find(|event| event.event_type == "RuntimeJobCompleted")
            .expect("contract completion event should persist");
        assert_eq!(
            completion_event.event["runtime_job_profile"],
            "codex-contract"
        );
        assert_eq!(completion_event.event["runtime_job_kind"], "codex_exec");
        assert_eq!(
            completion_event.event["agent_contract_attempts"],
            json!([{"primary_attempt": 1, "correction_attempt": 0}])
        );
        let usage = store
            .runtime_usage_for_workflow(&instance.id)
            .await?
            .expect("contract attempt usage should persist");
        assert_eq!(usage.metrics.input_tokens, 11);
        assert_eq!(usage.metrics.output_tokens, 7);
        assert_eq!(usage.metrics.reported_total_tokens, Some(18));
        assert_eq!(usage.cost_usd_micros, 125_000);
        let usage_records = store
            .runtime_usage_between(
                chrono::Utc::now() - chrono::Duration::minutes(1),
                chrono::Utc::now() + chrono::Duration::minutes(1),
            )
            .await?;
        let usage_record = usage_records
            .iter()
            .find(|record| record.runtime_job_id == runtime_job.id)
            .expect("contract attempt usage record should persist");
        assert_eq!(usage_record.project, project_root.to_string_lossy());
        assert_eq!(usage_record.task_id.as_deref(), Some("loop-e2e-task"));
        drop(state);
        drop(store);

        let reopened =
            WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url))
                .await?
                .with_definition_registry(loop_registry().into_shared());
        assert_eq!(
            reopened
                .get_instance(&instance.id)
                .await?
                .expect("reopened workflow should persist")
                .state,
            "implementing"
        );
        assert_eq!(reopened.decisions_for(&instance.id).await?.len(), 2);
        assert_eq!(*agent.requests.lock().await, 1);
        Ok(())
    }
}
