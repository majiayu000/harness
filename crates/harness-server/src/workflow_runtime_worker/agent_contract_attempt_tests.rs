use super::*;
use async_trait::async_trait;
use harness_workflow::runtime::ActivityStatus;
use std::sync::Mutex;

fn pinned_contract() -> PinnedJobAgentContract {
    let contract: WorkflowAgentContract = serde_json::from_value(json!({
        "input_schema": "harness.semantic_activity_input.v1",
        "output_schema": "harness.semantic_verdict.v1",
        "allowed_outcomes": ["small", "large"],
        "tools": "none",
        "mutation": "forbidden",
        "workspace": "ephemeral_empty",
        "fresh_context": true,
    }))
    .expect("valid contract");
    PinnedJobAgentContract {
        contract,
        prompt: "Classify only the supplied facts.".to_string(),
        definition_hash: "sha256:pinned".to_string(),
    }
}

fn valid_verdict_reply() -> String {
    json!({
        "schema": "harness.semantic_verdict.v1",
        "outcome": "small",
        "rationale": "Touches a single function.",
    })
    .to_string()
}

/// Records the launch request and workspace state, then replays a script.
struct ScriptedBackend {
    conforming: bool,
    script: Mutex<Vec<AgentEvent>>,
    requests: Mutex<Vec<AgentRequest>>,
    workspace_entry_counts: Mutex<Vec<usize>>,
}

impl ScriptedBackend {
    fn conforming(script: Vec<AgentEvent>) -> Arc<Self> {
        Arc::new(Self {
            conforming: true,
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
            workspace_entry_counts: Mutex::new(Vec::new()),
        })
    }

    fn without_claims() -> Arc<Self> {
        Arc::new(Self {
            conforming: false,
            script: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            workspace_entry_counts: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl AgentBackend for ScriptedBackend {
    fn name(&self) -> &str {
        "scripted-contract-backend"
    }

    fn agent_contract_capabilities(&self) -> harness_core::agent::AgentContractCapabilities {
        if self.conforming {
            harness_core::agent::AgentContractCapabilities {
                prompt_only_launch: true,
                pinned_output_schema: true,
                attempt_observation_stream: true,
            }
        } else {
            harness_core::agent::AgentContractCapabilities::default()
        }
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        let entries = std::fs::read_dir(&req.project_root)
            .map(|entries| entries.count())
            .unwrap_or(usize::MAX);
        self.workspace_entry_counts
            .lock()
            .expect("lock")
            .push(entries);
        self.requests.lock().expect("lock").push(req);
        let script = std::mem::take(&mut *self.script.lock().expect("lock"));
        for event in script {
            tx.send(event).await.map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(error.to_string())
            })?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn attempt_rejects_backend_without_capability_claims() {
    let backend = ScriptedBackend::without_claims();
    let error = execute_agent_contract_attempt(backend.clone(), &pinned_contract(), None, None)
        .await
        .expect_err("a backend claiming nothing must be rejected before launch");
    assert!(error.to_string().contains("cannot enforce"), "{error}");
    assert!(
        backend.requests.lock().expect("lock").is_empty(),
        "rejection must happen before anything is launched"
    );
}

#[tokio::test]
async fn attempt_launches_pinned_deny_all_request_in_empty_workspace() {
    let backend = ScriptedBackend::conforming(vec![AgentEvent::TurnCompleted {
        output: valid_verdict_reply(),
    }]);
    let attempt = execute_agent_contract_attempt(
        backend.clone(),
        &pinned_contract(),
        Some("gpt-5.4".to_string()),
        Some("high".to_string()),
    )
    .await
    .expect("scripted attempt succeeds");

    let requests = backend.requests.lock().expect("lock");
    assert_eq!(requests.len(), 1, "exactly one launch");
    let request = &requests[0];
    assert_eq!(request.prompt, "Classify only the supplied facts.");
    assert_eq!(request.allowed_tools.as_deref(), Some(&[][..]));
    assert_eq!(request.sandbox_mode, Some(SandboxMode::ReadOnly));
    assert_eq!(request.approval_policy.as_deref(), Some("never"));
    assert_eq!(request.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        backend.workspace_entry_counts.lock().expect("lock")[0],
        0,
        "the contract workspace must be empty at launch"
    );
    let schema_path = request
        .env_vars
        .get(AGENT_OUTPUT_SCHEMA_PATH_ENV)
        .expect("pinned output schema path is handed to the backend");
    // The schema temp file is cleaned up with the attempt; the launch-time
    // request must have pointed at the canonical document outside the
    // workspace.
    assert!(
        !schema_path.starts_with(&request.project_root.display().to_string()),
        "schema file must not live inside the empty workspace"
    );
    assert_eq!(attempt.output, valid_verdict_reply());
}

#[tokio::test]
async fn attempt_schema_file_carries_the_canonical_document() {
    // The schema temp file is deleted with the attempt, so its content must
    // be captured at launch time, from inside the stream call.
    struct SchemaRecordingBackend {
        inner: Arc<ScriptedBackend>,
        schema_contents: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl AgentBackend for SchemaRecordingBackend {
        fn name(&self) -> &str {
            "schema-recording-backend"
        }
        fn agent_contract_capabilities(&self) -> harness_core::agent::AgentContractCapabilities {
            self.inner.agent_contract_capabilities()
        }
        async fn execute_stream(
            &self,
            req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> harness_core::error::Result<()> {
            let path = req
                .env_vars
                .get(AGENT_OUTPUT_SCHEMA_PATH_ENV)
                .expect("schema env set");
            self.schema_contents
                .lock()
                .expect("lock")
                .push(std::fs::read_to_string(path).expect("schema file readable at launch"));
            self.inner.execute_stream(req, tx).await
        }
    }
    let recording = Arc::new(SchemaRecordingBackend {
        inner: ScriptedBackend::conforming(vec![AgentEvent::TurnCompleted {
            output: valid_verdict_reply(),
        }]),
        schema_contents: Mutex::new(Vec::new()),
    });
    execute_agent_contract_attempt(recording.clone(), &pinned_contract(), None, None)
        .await
        .expect("scripted attempt succeeds");
    assert_eq!(
        recording.schema_contents.lock().expect("lock")[0],
        agent_contract_output_schema_document("harness.semantic_verdict.v1")
            .expect("canonical verdict schema document exists"),
        "the backend must receive the canonical pinned schema document"
    );
}

#[tokio::test]
async fn attempt_records_stream_observations_and_items() {
    let backend = ScriptedBackend::conforming(vec![
        AgentEvent::ModelReported {
            model: "gpt-5.2-codex".to_string(),
            source: harness_core::agent::ModelIdentitySource::LaunchDerived,
        },
        AgentEvent::ItemCompleted {
            item: Item::ShellCommand {
                command: "cat /etc/passwd".to_string(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        },
        AgentEvent::TurnCompleted {
            output: valid_verdict_reply(),
        },
    ]);
    let attempt = execute_agent_contract_attempt(backend, &pinned_contract(), None, None)
        .await
        .expect("scripted attempt succeeds");
    assert_eq!(attempt.items.len(), 1);
    assert_eq!(
        attempt.observations.reported_models,
        vec![(
            "gpt-5.2-codex".to_string(),
            harness_core::agent::ModelIdentitySource::LaunchDerived
        )]
    );
    let violations = contract_violations(&attempt);
    assert_eq!(violations, vec!["shell_command `cat /etc/passwd`"]);
}

#[test]
fn violations_fail_the_attempt_with_observation_artifact() {
    let mut observations = TurnStreamObservations::default();
    observations.record_stream_item(&AgentEvent::ApprovalRequest {
        id: "approval-1".to_string(),
        command: "rm -rf".to_string(),
    });
    let attempt = ContractAttempt {
        output: valid_verdict_reply(),
        items: vec![Item::ToolCall {
            name: "web_search".to_string(),
            input: json!({}),
            output: None,
        }],
        observations,
    };
    let result = contract_attempt_activity_result("classify_scope", &pinned_contract(), &attempt);
    assert_eq!(result.status, ActivityStatus::Failed);
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Fatal));
    let error = result.error.as_deref().unwrap_or_default();
    assert!(error.contains("tool_call `web_search`"), "{error}");
    assert!(error.contains("approval request"), "{error}");
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type
            == harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_TURN_OBSERVATIONS));
}

#[test]
fn clean_attempt_with_valid_verdict_succeeds() {
    let attempt = ContractAttempt {
        output: valid_verdict_reply(),
        items: Vec::new(),
        observations: TurnStreamObservations::default(),
    };
    let result = contract_attempt_activity_result("classify_scope", &pinned_contract(), &attempt);
    assert_eq!(result.status, ActivityStatus::Succeeded);
    assert!(result.summary.contains("small"), "{}", result.summary);
    let verdict = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "agent_contract_verdict")
        .expect("verdict artifact attached");
    assert_eq!(verdict.artifact["outcome"], "small");
    assert_eq!(verdict.artifact["definition_hash"], "sha256:pinned");
}

#[test]
fn off_vocabulary_or_malformed_verdicts_fail() {
    for (reply, expected) in [
        ("not json".to_string(), "not valid JSON"),
        (
            json!({"schema": "harness.semantic_verdict.v1", "outcome": "medium", "rationale": "x"})
                .to_string(),
            "not in the pinned allowed_outcomes",
        ),
        (
            json!({"schema": "other.schema.v1", "outcome": "small", "rationale": "x"}).to_string(),
            "instead of the pinned",
        ),
        (
            json!({"schema": "harness.semantic_verdict.v1", "outcome": "small"}).to_string(),
            "rationale",
        ),
    ] {
        let attempt = ContractAttempt {
            output: reply.clone(),
            items: Vec::new(),
            observations: TurnStreamObservations::default(),
        };
        let result =
            contract_attempt_activity_result("classify_scope", &pinned_contract(), &attempt);
        assert_eq!(result.status, ActivityStatus::Failed, "{reply}");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(expected),
            "reply {reply} should fail with `{expected}`, got {:?}",
            result.error
        );
    }
}

#[test]
fn started_but_never_completed_tool_items_are_violations() {
    let mut observations = TurnStreamObservations::default();
    observations.record_stream_item(&AgentEvent::ItemStarted {
        item: Item::FileEdit {
            path: std::path::PathBuf::from("/tmp/x"),
            before: String::new(),
            after: String::new(),
        },
    });
    observations.record_stream_item(&AgentEvent::ItemStartedKind {
        item_type: "novel_side_effect".to_string(),
    });
    let attempt = ContractAttempt {
        output: valid_verdict_reply(),
        items: Vec::new(),
        observations,
    };
    let violations = contract_violations(&attempt);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("file_edit")),
        "{violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("unknown event kind `novel_side_effect`")),
        "{violations:?}"
    );
}
