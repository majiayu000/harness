//! Real execution path for one pinned agent-contract attempt.
//!
//! Production dispatch reaches this path only after the server authorizes the
//! exact selected profile against the concrete backend's capability claims.
//!
//! Enforcement model: a conforming backend cannot switch its tool surface off
//! (codex-cli has no such flag), so the contract is enforced by construction
//! plus observation —
//! - the launch input is the pinned prompt plus pinned semantic input envelope:
//!   no prompt packet, repo memory, workflow document, user config, or rule files;
//! - the workspace is a fresh empty temp directory with no repository
//!   checkout (`workspace: ephemeral_empty`);
//! - the launch is deny-all-tools (`allowed_tools = []`), read-only sandbox,
//!   approvals never;
//! - the pinned output schema document is handed to the backend's structured
//!   output channel;
//! - the server records the whole event stream, and any tool, mutation,
//!   approval, or unknown event invalidates the attempt.

use harness_core::agent::{AgentBackend, AgentEvent, AgentRequest, AGENT_OUTPUT_SCHEMA_PATH_ENV};
use harness_core::config::agents::{AgentPermissionMode, SandboxMode};
use harness_core::config::workflow::{
    agent_contract_output_schema_document, validate_agent_contract_output, WorkflowAgentContract,
};
use harness_core::types::Item;
#[cfg(test)]
use harness_workflow::runtime::{ActivityErrorKind, ActivityResult};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;

#[cfg(test)]
use super::agent_contract_enforcement::turn_observation_artifact;
use super::agent_contract_enforcement::{
    ensure_backend_can_enforce_contract, PinnedJobAgentContract, TurnStreamObservations,
};
use super::agent_contract_prompt::contract_attempt_prompt;

/// Everything the server observed and received from one contract attempt.
#[derive(Debug)]
pub(super) struct ContractAttempt {
    /// Final structured reply text from the backend.
    pub(super) output: String,
    /// Completed transcript items, recorded from the stream.
    pub(super) items: Vec<Item>,
    /// Server-observed stream facts for the whole attempt.
    pub(super) observations: TurnStreamObservations,
}

/// Structured verdict parsed from a contract attempt's reply.
pub(super) struct ContractVerdict {
    pub(super) outcome: String,
    pub(super) raw: Value,
}

/// Launches one contract attempt against `backend` and records the whole
/// stream. This is the enforcement primitive itself; it performs the
/// capability preflight again so a direct caller can never skip it.
pub(super) async fn execute_agent_contract_attempt(
    backend: Arc<dyn AgentBackend>,
    pinned: &PinnedJobAgentContract,
    model: Option<String>,
    reasoning_effort: Option<String>,
    timeout_secs: u64,
    correction: Option<(&str, &str)>,
) -> anyhow::Result<ContractAttempt> {
    if timeout_secs == 0 {
        anyhow::bail!("agent contract attempt timeout_secs must be positive");
    }
    ensure_backend_can_enforce_contract(backend.as_ref())?;
    let schema_document = agent_contract_output_schema_document(&pinned.contract.output_schema)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "output schema `{}` has no canonical schema document to enforce",
                pinned.contract.output_schema
            )
        })?;
    let base_prompt = contract_attempt_prompt(pinned)?;
    let prompt = match correction {
        Some((previous_output, validation_error)) => format!(
            "{base_prompt}\n\nThe previous reply failed server validation. Return a corrected verdict using the same pinned facts and no tools.\n\nValidation error:\n{validation_error}\n\nPrevious reply:\n{previous_output}"
        ),
        None => base_prompt,
    };

    // The workspace stays literally empty: the schema file lives in its own
    // temp directory so nothing but the pinned prompt reaches the attempt.
    let workspace = tempfile::Builder::new()
        .prefix("harness-agent-contract-workspace-")
        .tempdir()?;
    let schema_dir = tempfile::Builder::new()
        .prefix("harness-agent-contract-schema-")
        .tempdir()?;
    let schema_path = schema_dir.path().join("output-schema.json");
    std::fs::write(&schema_path, schema_document)?;

    let request = AgentRequest {
        prompt,
        project_root: workspace.path().to_path_buf(),
        // The Codex process needs provider network access even though the
        // model-facing tool allowlist is empty. `Full` controls host egress
        // resolution here; the explicit read-only sandbox, approvals=never,
        // empty tool declaration, and attempt observations remain pinned.
        permission_mode: AgentPermissionMode::Full,
        allowed_tools: Some(Vec::new()),
        sandbox_mode: Some(SandboxMode::ReadOnly),
        approval_policy: Some("never".to_string()),
        model,
        reasoning_effort,
        timeout_secs: Some(timeout_secs),
        env_vars: std::iter::once((
            AGENT_OUTPUT_SCHEMA_PATH_ENV.to_string(),
            schema_path.display().to_string(),
        ))
        .collect(),
        ..AgentRequest::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let stream_backend = Arc::clone(&backend);
    let stream = tokio::spawn(async move { stream_backend.execute_stream(request, tx).await });

    let mut observations = TurnStreamObservations::default();
    let mut items = Vec::new();
    let mut output = String::new();
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                observations.record_stream_item(&event);
                match event {
                    AgentEvent::ItemCompleted { item } => items.push(item),
                    AgentEvent::TurnCompleted { output: reply } => output = reply,
                    _ => {}
                }
            }
            _ = &mut deadline => {
                stream.abort();
                match stream.await {
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        anyhow::bail!(
                            "agent contract attempt timed out after {timeout_secs}s and its stream task failed during cancellation: {error}"
                        );
                    }
                    Ok(Err(error)) => {
                        anyhow::bail!(
                            "agent contract attempt timed out after {timeout_secs}s while its stream failed: {error}"
                        );
                    }
                    Ok(Ok(())) => {}
                }
                anyhow::bail!("agent contract attempt timed out after {timeout_secs}s");
            }
        }
    }
    stream
        .await
        .map_err(|error| anyhow::anyhow!("contract attempt stream task panicked: {error}"))?
        .map_err(|error| anyhow::anyhow!("contract attempt launch failed: {error}"))?;

    Ok(ContractAttempt {
        output,
        items,
        observations,
    })
}

/// Converts an observed attempt into the job's `ActivityResult`: violations
/// invalidate the attempt, an unparsable or off-vocabulary verdict fails it,
/// and the server-authored observation artifact is attached either way.
#[cfg(test)]
pub(super) fn contract_attempt_activity_result(
    activity: &str,
    pinned: &PinnedJobAgentContract,
    attempt: &ContractAttempt,
) -> ActivityResult {
    let observation_artifact = turn_observation_artifact(1, &attempt.items, &attempt.observations);
    let violations = contract_violations(attempt);
    if !violations.is_empty() {
        return ActivityResult::failed(
            activity,
            "Agent contract attempt is invalid: the agent used a surface the pinned contract forbids.",
            format!("contract violations: {}", violations.join("; ")),
        )
        .with_error_kind(ActivityErrorKind::Fatal)
        .with_artifact(observation_artifact);
    }
    match parse_contract_verdict(&attempt.output, &pinned.contract) {
        Ok(verdict) => ActivityResult::succeeded(
            activity,
            format!("Agent contract verdict: {}.", verdict.outcome),
        )
        .with_artifact(observation_artifact)
        .with_artifact(harness_workflow::runtime::ActivityArtifact::new(
            "agent_contract_verdict",
            json!({
                "output_schema": pinned.contract.output_schema,
                "definition_hash": pinned.definition_hash,
                "outcome": verdict.outcome,
                "verdict": verdict.raw,
            }),
        )),
        Err(reason) => ActivityResult::failed(
            activity,
            "Agent contract attempt did not return a valid structured verdict.",
            reason,
        )
        .with_error_kind(ActivityErrorKind::Fatal)
        .with_artifact(observation_artifact),
    }
}

/// Everything observed during the attempt that the pinned contract forbids.
/// Fail closed: unknown event kinds count as violations because an
/// unrecognized surface cannot be proven benign.
pub(super) fn contract_violations(attempt: &ContractAttempt) -> Vec<String> {
    let mut violations = BTreeSet::new();
    for item in &attempt.items {
        match item {
            Item::ShellCommand { command, .. } => {
                violations.insert(format!("shell_command `{command}`"));
            }
            Item::ToolCall { name, .. } => {
                violations.insert(format!("tool_call `{name}`"));
            }
            Item::FileEdit { path, .. } => {
                violations.insert(format!("file_edit `{}`", path.display()));
            }
            Item::FileRead { path, .. } => {
                violations.insert(format!("file_read `{}`", path.display()));
            }
            Item::ApprovalRequest { action, .. } => {
                violations.insert(format!("approval_request `{action}`"));
            }
            Item::UserMessage { .. } | Item::AgentReasoning { .. } | Item::Error { .. } => {}
        }
    }
    // Stream-level kinds cover items that started but never completed.
    for kind in &attempt.observations.started_item_kinds {
        if !matches!(kind.as_str(), "user_message" | "agent_reasoning" | "error") {
            violations.insert(format!("started item of kind `{kind}`"));
        }
    }
    for kind in &attempt.observations.unknown_item_kinds {
        violations.insert(format!("unknown event kind `{kind}`"));
    }
    if attempt.observations.approval_requests > 0 {
        violations.insert(format!(
            "{} approval request(s)",
            attempt.observations.approval_requests
        ));
    }
    if attempt.observations.tool_output_deltas > 0 {
        violations.insert(format!(
            "{} tool output delta(s)",
            attempt.observations.tool_output_deltas
        ));
    }
    violations.into_iter().collect()
}

/// Validates the attempt's reply against the pinned contract: it must be a
/// JSON object naming the pinned output schema, with an `outcome` from the
/// pinned vocabulary and a non-empty `rationale`.
pub(super) fn parse_contract_verdict(
    output: &str,
    contract: &WorkflowAgentContract,
) -> Result<ContractVerdict, String> {
    let raw: Value = serde_json::from_str(output.trim())
        .map_err(|error| format!("reply is not valid JSON: {error}"))?;
    validate_agent_contract_output(&contract.output_schema, &raw)?;
    let outcome = raw
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or_else(|| "validated reply is missing a string outcome".to_string())?;
    if !contract
        .allowed_outcomes
        .iter()
        .any(|allowed| allowed == outcome)
    {
        return Err(format!(
            "outcome `{outcome}` is not in the pinned allowed_outcomes [{}]",
            contract.allowed_outcomes.join(", ")
        ));
    }
    Ok(ContractVerdict {
        outcome: outcome.to_string(),
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_workflow::runtime::ActivityStatus;
    use std::sync::atomic::{AtomicBool, Ordering};
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
        let contract_hash = harness_workflow::runtime::stable_remote_fact_hash(
            &serde_json::to_value(&contract).expect("serialize contract"),
        );
        PinnedJobAgentContract {
            contract,
            prompt: "Classify only the supplied facts.".to_string(),
            input: json!({
                "schema": "harness.semantic_activity_input.v1",
                "subject": {"kind": "issue", "identity": "owner/repo#126"},
                "facts": {"changed_files": ["src/lib.rs"]},
                "provenance": {"/changed_files": "server"},
                "contract_hash": contract_hash,
            }),
            definition_hash: "sha256:pinned".to_string(),
        }
    }

    fn valid_verdict_reply() -> String {
        json!({
            "schema": "harness.semantic_verdict.v1",
            "outcome": "small",
            "rationale": "Touches a single function.",
            "evidence_refs": [],
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
        let error = execute_agent_contract_attempt(
            backend.clone(),
            &pinned_contract(),
            None,
            None,
            30,
            None,
        )
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
        let pinned = pinned_contract();
        let backend = ScriptedBackend::conforming(vec![AgentEvent::TurnCompleted {
            output: valid_verdict_reply(),
        }]);
        let attempt = execute_agent_contract_attempt(
            backend.clone(),
            &pinned,
            Some("gpt-5.4".to_string()),
            Some("high".to_string()),
            30,
            None,
        )
        .await
        .expect("scripted attempt succeeds");

        let requests = backend.requests.lock().expect("lock");
        assert_eq!(requests.len(), 1, "exactly one launch");
        let request = &requests[0];
        assert_eq!(request.prompt, contract_attempt_prompt(&pinned).unwrap());
        assert!(request.prompt_layers.is_none());
        assert!(request.context.is_empty());
        assert_eq!(request.allowed_tools.as_deref(), Some(&[][..]));
        assert_eq!(request.permission_mode, AgentPermissionMode::Full);
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
            fn agent_contract_capabilities(
                &self,
            ) -> harness_core::agent::AgentContractCapabilities {
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
        execute_agent_contract_attempt(recording.clone(), &pinned_contract(), None, None, 30, None)
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
            AgentEvent::ToolCall {
                name: "direct_tool".to_string(),
                input: json!({"argument": true}),
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
        let attempt =
            execute_agent_contract_attempt(backend, &pinned_contract(), None, None, 30, None)
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
        assert_eq!(
            violations,
            vec![
                "shell_command `cat /etc/passwd`",
                "started item of kind `tool_call:direct_tool`",
            ]
        );
    }

    #[tokio::test]
    async fn attempt_timeout_cancels_the_backend_stream() {
        struct CancellationMarker(Arc<AtomicBool>);
        impl Drop for CancellationMarker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct HangingBackend {
            cancelled: Arc<AtomicBool>,
        }
        #[async_trait]
        impl AgentBackend for HangingBackend {
            fn name(&self) -> &str {
                "hanging-contract-backend"
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
                _req: AgentRequest,
                _tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> harness_core::error::Result<()> {
                let _marker = CancellationMarker(Arc::clone(&self.cancelled));
                std::future::pending().await
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(HangingBackend {
            cancelled: Arc::clone(&cancelled),
        });
        let error =
            execute_agent_contract_attempt(backend, &pinned_contract(), None, None, 1, None)
                .await
                .expect_err("a hung backend must hit the pinned wall-clock boundary");

        assert!(error.to_string().contains("timed out after 1s"), "{error}");
        assert!(
            cancelled.load(Ordering::SeqCst),
            "the timed-out stream task must be cancelled before returning"
        );
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
        let result =
            contract_attempt_activity_result("classify_scope", &pinned_contract(), &attempt);
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
        let result =
            contract_attempt_activity_result("classify_scope", &pinned_contract(), &attempt);
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
                json!({"schema": "harness.semantic_verdict.v1", "outcome": "medium", "rationale": "x", "evidence_refs": []})
                    .to_string(),
                "not in the pinned allowed_outcomes",
            ),
            (
                json!({"schema": "other.schema.v1", "outcome": "small", "rationale": "x", "evidence_refs": []}).to_string(),
                "does not match schema",
            ),
            (
                json!({"schema": "harness.semantic_verdict.v1", "outcome": "small"}).to_string(),
                "does not match schema",
            ),
            (
                json!({
                    "schema": "harness.semantic_verdict.v1",
                    "outcome": "small",
                    "rationale": "x",
                    "evidence_refs": [],
                    "unexpected": true,
                })
                .to_string(),
                "does not match schema",
            ),
            (
                json!({
                    "schema": "harness.semantic_verdict.v1",
                    "outcome": "small",
                    "rationale": "x",
                    "evidence_refs": "not-an-array",
                })
                .to_string(),
                "does not match schema",
            ),
            (
                json!({
                    "schema": "harness.semantic_verdict.v1",
                    "outcome": "small",
                    "rationale": "x",
                    "evidence_refs": [""],
                })
                .to_string(),
                "does not match schema",
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
}
