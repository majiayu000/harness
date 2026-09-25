pub(super) use super::agent_contract_dispatch::{
    execute_contract_job, pinned_agent_contract_for_execution, AgentContractExecutionError,
};

#[cfg(test)]
mod tests {
    //! Worker-tick behavior for pinned agent contracts (Slice B): capability
    //! preflight, the pinned deny-all launch in an empty ephemeral workspace, and
    //! observation-based invalidation.

    use async_trait::async_trait;
    use harness_core::agent::{
        AgentBackend, AgentRequest, StreamItem, AGENT_OUTPUT_SCHEMA_PATH_ENV,
    };
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, RuntimeBudgetEnforcement, RuntimeBudgetPolicy,
        WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Contract-conforming scripted backend: claims every enforcement capability,
    /// records the launch request and workspace state, and replays a script.
    struct ContractStreamAgent {
        scripts: Mutex<std::collections::VecDeque<Vec<harness_core::agent::AgentEvent>>>,
        requests: Mutex<Vec<AgentRequest>>,
        workspace_entry_counts: Mutex<Vec<usize>>,
        reports_usage_cost: bool,
    }

    impl ContractStreamAgent {
        fn new(script: Vec<harness_core::agent::AgentEvent>) -> Arc<Self> {
            Self::with_scripts_and_cost_reporting(vec![script], true)
        }

        fn with_scripts(scripts: Vec<Vec<harness_core::agent::AgentEvent>>) -> Arc<Self> {
            Self::with_scripts_and_cost_reporting(scripts, true)
        }

        fn cost_blind() -> Arc<Self> {
            Self::with_scripts_and_cost_reporting(Vec::new(), false)
        }

        fn with_scripts_and_cost_reporting(
            scripts: Vec<Vec<harness_core::agent::AgentEvent>>,
            reports_usage_cost: bool,
        ) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                requests: Mutex::new(Vec::new()),
                workspace_entry_counts: Mutex::new(Vec::new()),
                reports_usage_cost,
            })
        }
    }

    #[async_trait]
    impl AgentBackend for ContractStreamAgent {
        fn name(&self) -> &str {
            "contract-stream-agent"
        }

        fn agent_contract_capabilities(&self) -> harness_core::agent::AgentContractCapabilities {
            harness_core::agent::AgentContractCapabilities {
                prompt_only_launch: true,
                pinned_output_schema: true,
                attempt_observation_stream: true,
            }
        }

        fn reports_usage_cost(&self) -> bool {
            self.reports_usage_cost
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            let entries = std::fs::read_dir(&req.project_root)
                .map(|entries| entries.count())
                .unwrap_or(usize::MAX);
            self.workspace_entry_counts.lock().await.push(entries);
            self.requests.lock().await.push(req);
            let script = self.scripts.lock().await.pop_front().unwrap_or_default();
            for event in script {
                tx.send(event).await.map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(error.to_string())
                })?;
            }
            Ok(())
        }
    }

    struct UnclaimingStreamAgent {
        requests: Mutex<Vec<AgentRequest>>,
    }

    impl UnclaimingStreamAgent {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl AgentBackend for UnclaimingStreamAgent {
        fn name(&self) -> &str {
            "unclaiming-stream-agent"
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            self.requests.lock().await.push(req);
            Ok(())
        }
    }

    fn contract_definition(
        activity: &str,
    ) -> anyhow::Result<harness_workflow::runtime::DeclarativeWorkflowDefinition> {
        let contract = serde_json::from_value(serde_json::json!({
            "input_schema": "harness.semantic_activity_input.v1",
            "output_schema": "harness.semantic_verdict.v1",
            "allowed_outcomes": ["small", "large"],
            "tools": "none",
            "mutation": "forbidden",
            "workspace": "ephemeral_empty",
            "fresh_context": true,
        }))?;
        let policy = WorkflowDefinitionPolicy {
            id: format!("contract_test_{activity}"),
            initial: "classifying".to_string(),
            states: BTreeMap::from([
                (
                    "classifying".to_string(),
                    DeclaredState {
                        activity: Some(activity.to_string()),
                        on_failure: Some("blocked".to_string()),
                        on_blocked: Some("done".to_string()),
                        on_signal: BTreeMap::from([
                            ("small".to_string(), "done".to_string()),
                            ("large".to_string(), "blocked".to_string()),
                        ]),
                        ..Default::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..Default::default()
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
        harness_workflow::runtime::build_declarative_definition(
            &policy,
            &BTreeMap::from([(
                activity.to_string(),
                WorkflowActivityPolicy {
                    prompt: Some("Classify only the supplied facts.".to_string()),
                    agent_contract: Some(contract),
                    ..Default::default()
                },
            )]),
        )
    }

    async fn make_contract_test_state(
        config_root: &std::path::Path,
        project_root: &std::path::Path,
        agent_registry: harness_agents::registry::AgentRegistry,
        activity: &str,
        enforce_budget: bool,
    ) -> anyhow::Result<Arc<crate::http::AppState>> {
        let mut state = crate::test_helpers::make_test_state_with_project_root_and_registry(
            config_root,
            project_root,
            agent_registry,
        )
        .await?;
        let definition = contract_definition(activity)?;
        let mut definition_registry =
            harness_workflow::runtime::WorkflowDefinitionRegistry::with_builtins();
        definition_registry.register_declarative_current(definition)?;
        let store_path =
            harness_core::config::dirs::default_db_path(config_root, "workflow_runtime");
        let database_url = crate::test_helpers::test_database_url()?;
        let mut store = harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &store_path,
            Some(&database_url),
        )
        .await?
        .with_definition_registry(definition_registry.into_shared());
        if enforce_budget {
            store = store.with_budget_policy(RuntimeBudgetPolicy {
                enforcement: RuntimeBudgetEnforcement::Enforce,
                ..RuntimeBudgetPolicy::default()
            });
        }
        state.core.workflow_runtime_store = Some(Arc::new(store));
        Ok(Arc::new(state))
    }

    fn valid_verdict_reply() -> String {
        serde_json::json!({
            "schema": "harness.semantic_verdict.v1",
            "outcome": "small",
            "rationale": "Touches a single function.",
            "evidence_refs": [],
        })
        .to_string()
    }

    async fn enqueue_contract_job(
        store: &harness_workflow::runtime::WorkflowRuntimeStore,
        project_root: &std::path::Path,
        activity: &str,
    ) -> anyhow::Result<harness_workflow::runtime::RuntimeJob> {
        let definition = contract_definition(activity)?;
        let workflow = harness_workflow::runtime::WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            definition.policy().initial.clone(),
            harness_workflow::runtime::WorkflowSubject::new("issue", "issue:126"),
        )
        .with_id("issue-126")
        .with_classified_data(
            serde_json::json!({
                "project_id": project_root,
                "repo": "owner/repo",
                "issue_number": 126,
                "changed_files": ["src/lib.rs"],
                "definition_hash": definition.definition_hash(),
            }),
            harness_workflow::runtime::DataProvenance::Server,
        );
        store
            .persist_definition_version(
                &harness_workflow::runtime::persisted_declarative_definition(&definition, None),
            )
            .await?;
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow)
            .await?;
        let command = harness_workflow::runtime::build_declarative_submission_decision(
            &definition,
            &workflow,
        )?
        .commands
        .into_iter()
        .next()
        .expect("contract submission must enqueue its initial activity");
        let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
        let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
            "codex-default",
            harness_workflow::runtime::RuntimeKind::CodexExec,
        );
        runtime_profile.timeout_secs = Some(30);
        runtime_profile.max_turns = Some(2);
        store
            .enqueue_runtime_job(
                &command_id,
                harness_workflow::runtime::RuntimeKind::CodexExec,
                "codex-default",
                serde_json::json!({
                    "workflow_id": workflow.id,
                    "command_id": command_id,
                    "command_type": command.command_type,
                    "dedupe_key": command.dedupe_key,
                    "command": command.command,
                    "activity": activity,
                    "runtime_profile": runtime_profile,
                }),
            )
            .await
    }

    #[tokio::test]
    async fn contract_job_on_unclaiming_backend_fails_without_spawning() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        // RuntimeStreamAgent claims no contract capabilities (the default).
        let agent = UnclaimingStreamAgent::new();
        let mut registry = harness_agents::registry::AgentRegistry::new("codex");
        registry.register("codex", agent.clone());
        let state =
            make_contract_test_state(dir.path(), &project_root, registry, "classify_scope", false)
                .await?;
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .expect("workflow runtime store should be configured");
        let runtime_job = enqueue_contract_job(store, &project_root, "classify_scope").await?;

        let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
            &state,
            "worker-test",
            chrono::Duration::minutes(5),
        )
        .await?;

        assert_eq!(tick.failed, 1, "contract job must fail explicitly");
        assert!(
            agent.requests.lock().await.is_empty(),
            "no agent process may be spawned for an unenforceable contract job"
        );
        let completed = store
            .get_runtime_job(&runtime_job.id)
            .await?
            .expect("runtime job should exist");
        assert_eq!(
            completed.status,
            harness_workflow::runtime::RuntimeJobStatus::Failed
        );
        let output: harness_workflow::runtime::ActivityResult =
            serde_json::from_value(completed.output.clone().expect("failed job carries output"))?;
        let error = output.error.as_deref().unwrap_or_default();
        assert!(
            error.contains("cannot enforce an agent_contract"),
            "{error}"
        );
        assert!(error.contains("prompt_only_launch"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn enforced_budget_rejects_cost_blind_contract_backend_as_failure() -> anyhow::Result<()>
    {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let agent = ContractStreamAgent::cost_blind();
        let mut registry = harness_agents::registry::AgentRegistry::new("codex");
        registry.register("codex", agent.clone());
        let state =
            make_contract_test_state(dir.path(), &project_root, registry, "classify_cost", true)
                .await?;
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workflow runtime store should be configured"))?;
        let runtime_job = enqueue_contract_job(store, &project_root, "classify_cost").await?;

        let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
            &state,
            "worker-test",
            chrono::Duration::minutes(5),
        )
        .await?;

        assert_eq!(tick.failed, 1);
        assert!(agent.requests.lock().await.is_empty());
        let completed = store
            .get_runtime_job(&runtime_job.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("runtime job should exist"))?;
        assert_eq!(
            completed.status,
            harness_workflow::runtime::RuntimeJobStatus::Failed
        );
        let workflow = store
            .get_instance("issue-126")
            .await?
            .ok_or_else(|| anyhow::anyhow!("workflow should exist"))?;
        assert_eq!(workflow.state, "failed");
        Ok(())
    }

    #[tokio::test]
    async fn contract_job_named_like_server_activity_still_executes_pinned_contract(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let agent =
            ContractStreamAgent::new(vec![harness_core::agent::AgentEvent::TurnCompleted {
                output: valid_verdict_reply(),
            }]);
        let mut registry = harness_agents::registry::AgentRegistry::new("codex");
        registry.register("codex", agent.clone());
        let state = make_contract_test_state(
            dir.path(),
            &project_root,
            registry,
            harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            false,
        )
        .await?;
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .expect("workflow runtime store should be configured");
        let runtime_job = enqueue_contract_job(
            store,
            &project_root,
            harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
        )
        .await?;

        let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
            &state,
            "worker-test",
            chrono::Duration::minutes(5),
        )
        .await?;
        let completed = store
            .get_runtime_job(&runtime_job.id)
            .await?
            .expect("runtime job should exist");
        assert_eq!(
            tick.succeeded, 1,
            "clean contract attempt must succeed; persisted status={:?}, output={:?}",
            completed.status, completed.output
        );

        let requests = agent.requests.lock().await;
        assert_eq!(requests.len(), 1, "exactly one attempt launch");
        let request = &requests[0];
        let payload = &runtime_job.input["command"];
        let expected_prompt = format!(
            "{}\n\nAgent contract input (JSON):\n{}",
            payload["prompt"].as_str().expect("pinned prompt"),
            serde_json::to_string_pretty(&payload["agent_contract_input"])?
        );
        assert_eq!(request.prompt, expected_prompt);
        assert_eq!(request.timeout_secs, Some(30));
        assert_eq!(
            request.allowed_tools.as_deref(),
            Some(&[][..]),
            "the launch is deny-all-tools"
        );
        assert_eq!(
            request.sandbox_mode,
            Some(harness_core::config::agents::SandboxMode::ReadOnly)
        );
        assert_eq!(request.approval_policy.as_deref(), Some("never"));
        assert!(
            !request.project_root.starts_with(&project_root),
            "the contract workspace must not be a repository checkout"
        );
        assert_eq!(
            agent.workspace_entry_counts.lock().await[0],
            0,
            "the contract workspace must be empty at launch"
        );
        assert!(
            request.env_vars.contains_key(AGENT_OUTPUT_SCHEMA_PATH_ENV),
            "the pinned output schema is handed to the backend"
        );

        assert_eq!(
            completed.status,
            harness_workflow::runtime::RuntimeJobStatus::Succeeded
        );
        let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
            completed
                .output
                .clone()
                .expect("succeeded job carries output"),
        )?;
        let verdict = output
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "agent_contract_verdict")
            .expect("verdict artifact attached");
        assert_eq!(verdict.artifact["outcome"], "small");
        let assessment = output
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "agent_contract_assessment")
            .expect("server assessment attached");
        assert_eq!(
            assessment.artifact["assessment_id"],
            format!("{}:agent-contract-assessment", runtime_job.id)
        );
        assert_eq!(assessment.artifact["command_id"], runtime_job.command_id);
        assert_eq!(assessment.artifact["outcome"], "small");
        assert_eq!(assessment.artifact["budget"]["primary_attempts_used"], 1);
        assert_eq!(assessment.artifact["budget"]["corrections_used"], 0);
        assert!(
            output.artifacts.iter().any(|artifact| {
                artifact.artifact_type
                    == harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_TURN_OBSERVATIONS
            }),
            "the server-observed attempt record is attached"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_verdict_consumes_durable_correction_then_assesses_success(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let agent = ContractStreamAgent::with_scripts(vec![
            vec![harness_core::agent::AgentEvent::TurnCompleted {
                output: "not valid JSON".to_string(),
            }],
            vec![harness_core::agent::AgentEvent::TurnCompleted {
                output: valid_verdict_reply(),
            }],
        ]);
        let mut registry = harness_agents::registry::AgentRegistry::new("codex");
        registry.register("codex", agent.clone());
        let state =
            make_contract_test_state(dir.path(), &project_root, registry, "classify_scope", false)
                .await?;
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .expect("workflow runtime store should be configured");
        let runtime_job = enqueue_contract_job(store, &project_root, "classify_scope").await?;

        let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
            &state,
            "worker-test",
            chrono::Duration::minutes(5),
        )
        .await?;

        assert_eq!(
            tick.succeeded, 1,
            "a valid bounded correction should succeed"
        );
        let requests = agent.requests.lock().await;
        assert_eq!(requests.len(), 2, "one primary plus one correction");
        assert!(requests[1].prompt.contains("not valid JSON"));
        assert!(requests[1].prompt.contains("failed server validation"));
        drop(requests);
        let events = store.runtime_events_for(&runtime_job.id).await?;
        let reservations = events
            .iter()
            .filter(|event| event.event_type == "AgentContractAttemptStarted")
            .collect::<Vec<_>>();
        assert_eq!(reservations.len(), 2);
        assert_eq!(reservations[0].event["primary_attempt"], 1);
        assert_eq!(reservations[0].event["correction_attempt"], 0);
        assert_eq!(reservations[1].event["primary_attempt"], 1);
        assert_eq!(reservations[1].event["correction_attempt"], 1);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "RuntimeTurnStarted")
                .count(),
            2,
            "each model invocation must consume one workflow runtime turn"
        );
        let completed = store
            .get_runtime_job(&runtime_job.id)
            .await?
            .expect("runtime job should exist");
        let output: harness_workflow::runtime::ActivityResult =
            serde_json::from_value(completed.output.expect("successful job carries output"))?;
        let assessment = output
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "agent_contract_assessment")
            .expect("server assessment attached");
        assert_eq!(assessment.artifact["budget"]["primary_attempts_used"], 1);
        assert_eq!(assessment.artifact["budget"]["corrections_used"], 1);
        Ok(())
    }

    /// Any tool-surface activity observed during a contract attempt invalidates
    /// it, even when the reply itself is a valid verdict.
    #[tokio::test]
    async fn contract_attempt_with_tool_activity_is_invalidated() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let agent = ContractStreamAgent::new(vec![
            harness_core::agent::AgentEvent::ItemCompleted {
                item: harness_core::types::Item::ShellCommand {
                    command: "uname -a".to_string(),
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                },
            },
            harness_core::agent::AgentEvent::TurnCompleted {
                output: valid_verdict_reply(),
            },
        ]);
        let mut registry = harness_agents::registry::AgentRegistry::new("codex");
        registry.register("codex", agent.clone());
        let state =
            make_contract_test_state(dir.path(), &project_root, registry, "classify_scope", false)
                .await?;
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .expect("workflow runtime store should be configured");
        let runtime_job = enqueue_contract_job(store, &project_root, "classify_scope").await?;

        let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
            &state,
            "worker-test",
            chrono::Duration::minutes(5),
        )
        .await?;
        assert_eq!(tick.failed, 1, "a violating attempt must be invalidated");

        let completed = store
            .get_runtime_job(&runtime_job.id)
            .await?
            .expect("runtime job should exist");
        assert_eq!(
            completed.status,
            harness_workflow::runtime::RuntimeJobStatus::Failed
        );
        let output: harness_workflow::runtime::ActivityResult =
            serde_json::from_value(completed.output.clone().expect("failed job carries output"))?;
        let error = output.error.as_deref().unwrap_or_default();
        assert!(error.contains("contract violations"), "{error}");
        assert!(error.contains("shell_command `uname -a`"), "{error}");
        Ok(())
    }
}

#[cfg(test)]
mod dogfood_tests {
    //! Explicit, opt-in dogfood against the installed Codex CLI and live model.
    //!
    //! This is ignored in ordinary test runs because it consumes a real model
    //! turn and requires local Codex authentication. Run it by exact name when
    //! validating a Codex/model combination before enabling contract dispatch.

    use super::super::agent_contract_attempt::{contract_violations, parse_contract_verdict};
    use super::super::agent_contract_enforcement::PinnedJobAgentContract;
    use super::super::agent_contract_stream::execute_agent_contract_attempt;
    use harness_agents::codex::CodexAgent;
    use harness_core::config::agents::SandboxMode;
    use harness_core::config::workflow::WorkflowAgentContract;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn dogfood_contract() -> PinnedJobAgentContract {
        let contract: WorkflowAgentContract = serde_json::from_value(json!({
            "input_schema": "harness.semantic_activity_input.v1",
            "output_schema": "harness.semantic_verdict.v1",
            "allowed_outcomes": ["small", "large"],
            "tools": "none",
            "mutation": "forbidden",
            "workspace": "ephemeral_empty",
            "fresh_context": true,
        }))
        .expect("dogfood contract is valid");
        let contract_hash = harness_workflow::runtime::stable_remote_fact_hash(
            &serde_json::to_value(&contract).expect("serialize dogfood contract"),
        );
        let facts = json!({"changed_files": ["docs/agent-contract.md"]});
        let facts_digest = harness_workflow::runtime::stable_remote_fact_hash(&facts);
        PinnedJobAgentContract {
            contract,
            prompt: concat!(
                "Classify the supplied change as small or large. ",
                "It is small when exactly one documentation file changes. ",
                "Use only the supplied JSON facts, call no tools, and return the structured verdict."
            )
            .to_string(),
            input: json!({
                "schema": "harness.semantic_activity_input.v1",
                "subject": {"kind": "pull_request", "identity": "owner/repo#2020"},
                "facts": facts,
                "provenance": {
                    "schema": "harness.workflow.data_provenance.v1",
                    "entries": {"": "server"},
                    "value_digests": {"": facts_digest}
                },
                "contract_hash": contract_hash,
            }),
            definition_hash: "sha256:dogfood-definition".to_string(),
        }
    }

    #[tokio::test]
    #[ignore = "requires installed/authenticated Codex CLI and consumes a live GPT-5.6 Sol turn"]
    async fn real_codex_gpt_5_6_sol_contract_dogfood() -> anyhow::Result<()> {
        let backend = Arc::new(CodexAgent::new(
            PathBuf::from("codex"),
            SandboxMode::ReadOnly,
        ));
        let pinned = dogfood_contract();
        let attempt = execute_agent_contract_attempt(
            backend,
            &pinned,
            Some("gpt-5.6-sol".to_string()),
            Some("high".to_string()),
            300,
            None,
            None,
            None,
        )
        .await?;

        let violations = contract_violations(&attempt);
        assert!(
            violations.is_empty(),
            "live attempt violations: {violations:?}"
        );
        let verdict = parse_contract_verdict(&attempt.output, &pinned.contract, &pinned.input)
            .map_err(anyhow::Error::msg)?;
        assert_eq!(verdict.outcome, "small");
        assert!(
            attempt
                .observations
                .reported_models
                .iter()
                .any(|(model, _)| model == "gpt-5.6-sol"),
            "the live attempt must observe the exact requested model identity"
        );
        Ok(())
    }
}
