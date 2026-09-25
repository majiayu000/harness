use super::*;

pub(super) fn validate_workspace(workspace: Option<&str>) -> Result<(), &'static str> {
    if workspace.is_some_and(|path| {
        !path.starts_with('/') || path.trim() != path || path.chars().any(char::is_control)
    }) {
        return Err(
            "execution_workspace must be an absolute remote path without control characters",
        );
    }
    Ok(())
}

pub(super) async fn prepare_claim_prompt(
    state: &AppState,
    job: &RuntimeJob,
    workspace: Option<&str>,
) -> Result<
    Option<crate::workflow_runtime_worker::remote_prompt::PreparedRemotePrompt>,
    Box<ActivityResult>,
> {
    let Some(workspace) = workspace else {
        return Ok(None);
    };
    // The next activity is selected by the server after the host asks for work.
    // Native validation has no model prompt; deliver its existing command intact.
    // Explicit contract/replay jobs still require their own raw-claim protocol.
    if completion::runtime_job_activity(job) == harness_workflow::runtime::QUALITY_GATE_ACTIVITY
        && job.input.pointer("/command/agent_contract").is_none()
        && job.input.pointer("/command/exact_replay").is_none()
    {
        return Ok(None);
    }
    crate::workflow_runtime_worker::remote_prompt::prepare(state, job, workspace)
        .await
        .map(Some)
        .map_err(|error| {
            Box::new(
                ActivityResult::failed(
                    completion::runtime_job_activity(job),
                    "Remote runtime prompt preparation failed.",
                    error.to_string(),
                )
                .with_error_kind(
                    crate::workflow_runtime_worker::remote_prompt::error_kind(&error),
                ),
            )
        })
}

pub(super) async fn record_claim_delivery(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
    prepared: Option<&crate::workflow_runtime_worker::remote_prompt::PreparedRemotePrompt>,
    credential_audit: Option<Value>,
    resource_audit: Option<Value>,
    network_audit: Option<Value>,
) -> Result<(), (StatusCode, Value)> {
    let result = store
        .record_remote_host_claim_delivery(
            job,
            prepared.map(|prompt| prompt.evidence.clone()),
            credential_audit,
            resource_audit,
            network_audit,
        )
        .await;
    match result {
        Ok(true) => check_claim_fence(store, job).await,
        Ok(false) => {
            check_claim_fence(store, job).await?;
            let (status, body) = lease::lease_lost_response();
            Err((status, body.0))
        }
        Err(error) => {
            tracing::error!(runtime_job_id = %job.id, %error, "remote claim delivery audit failed");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"failed to persist remote claim delivery audit"}),
            ))
        }
    }
}

pub(super) async fn check_claim_fence(
    store: &WorkflowRuntimeStore,
    expected: &RuntimeJob,
) -> Result<(), (StatusCode, Value)> {
    let current = store.get_runtime_job(&expected.id).await.map_err(|error| {
        tracing::error!(runtime_job_id = %expected.id, %error, "remote prompt lease check failed");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "workflow runtime store unavailable"}),
        )
    })?;
    if current.as_ref().is_some_and(|current| {
        current.status == harness_workflow::runtime::RuntimeJobStatus::Running
            && current.lease_generation == expected.lease_generation
            && current
                .lease
                .as_ref()
                .zip(expected.lease.as_ref())
                .is_some_and(|(current, expected)| {
                    current.owner == expected.owner
                        && current.expires_at == expected.expires_at
                        && current.expires_at > Utc::now()
                })
    }) {
        if current
            .as_ref()
            .is_some_and(|job| job.input.get("cancellation_requested").is_some())
        {
            return Err((StatusCode::CONFLICT, lease::cancellation_requested_body()));
        }
        return Ok(());
    }
    let (status, body) = lease::lease_lost_response();
    Err((status, body.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::runtime_hosts_workflow_api_tests::{
        enqueue_runtime_host_test_job, make_test_state_with_runtime_store, post_json,
        post_json_with_status, register_host, runtime_hosts_workflow_app,
    };
    use harness_workflow::runtime::RuntimeKind;

    #[tokio::test]
    async fn remote_prompt_claim_renders_builtin_instructions_and_audits_without_launch_claims(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "prompt-host").await?;
        for activity in ["implement_issue", "run_local_review"] {
            let job = enqueue_runtime_host_test_job(&store, activity, RuntimeKind::RemoteHost, "remote", json!({
                "activity": activity, "command": {"activity": activity, "additional_prompt": "Use the recorded base and a draft PR.", "base_commit":"9c0099ad458e82fd377fd20a8e288a46722762ef", "branch_prefix":"harness-eval/", "pull_request_mode":"draft"},
            })).await?;
            let response = post_json(
                &app,
                "/api/runtime-hosts/prompt-host/runtime-jobs/claim".into(),
                json!({"execution_workspace":"/nonexistent-remote-only/workspace"}),
            )
            .await?;
            assert_eq!(response["claimed"], true);
            let prepared = &response["prepared_prompt"];
            let prompt = prepared["prompt"].as_str().expect("rendered prompt");
            assert!(prompt.contains("Project root: /nonexistent-remote-only/workspace"));
            assert!(prompt.contains(activity));
            assert!(prompt.contains("Use the recorded base and a draft PR."));
            if activity == "run_local_review" {
                for field in [
                    "LocalReviewPassed",
                    "reviewed_head_sha",
                    "working_tree_clean",
                ] {
                    assert!(prompt.contains(field), "missing review contract {field}");
                }
            } else {
                for value in [
                    "9c0099ad458e82fd377fd20a8e288a46722762ef",
                    "harness-eval/",
                    "draft",
                ] {
                    assert!(prompt.contains(value), "missing eval instruction {value}");
                }
            }
            assert!(prompt.contains("harness-activity-result"));
            assert!(!prompt.contains("source_root"));
            assert!(!prompt.contains("source_path"));
            assert_eq!(prepared["activity_result_schema"]["activity"], activity);
            if activity == "run_local_review" {
                let wire = json!({
                    "activity":activity, "status":"succeeded", "summary":"Reviewed the requested commit",
                    "artifacts":[{"artifact_type":"findings", "artifact":{"items":[{"severity":"info"}]}}],
                    "signals":[{"signal_type":"LocalReviewPassed", "signal":{"reviewed_head_sha":"expected-head", "working_tree_clean":true}}],
                    "validation":[], "error":null, "error_kind":null,
                });
                let validator =
                    jsonschema::validator_for(&prepared["activity_result_schema"]["json_schema"])?;
                assert!(validator.is_valid(&wire));
                // The remote completion endpoint uses this direct deserialization.
                let result: ActivityResult = serde_json::from_value(wire)?;
                assert_eq!(result.artifacts[0].artifact["items"][0]["severity"], "info");
                let mut instance = store
                    .get_instance(
                        &store
                            .get_command(&job.command_id)
                            .await?
                            .expect("command")
                            .workflow_id,
                    )
                    .await?
                    .expect("workflow");
                instance.state = "local_review_gate".into();
                instance = instance.with_server_data(
                    json!({"pr_number":77,"merge_review_head_sha":"expected-head"}),
                );
                let event = harness_workflow::runtime::WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "remote-test")
                    .with_payload(json!({"command_id":"command-1", "command":harness_workflow::runtime::WorkflowCommand::enqueue_activity(activity,"review-1"), "runtime_job_id":job.id, "activity_result":result}));
                let decision =
                    harness_workflow::runtime::reduce_runtime_job_completed(&instance, &event)?
                        .expect("review decision");
                assert_eq!(decision.next_state, "ready_to_merge");
            }

            assert_eq!(
                prepared["prompt_packet_digest"]
                    .as_str()
                    .expect("digest")
                    .len(),
                64
            );
            let events = store.runtime_events_for(&job.id).await?;
            let prepared_event = events
                .iter()
                .find(|event| event.event_type == "RuntimePromptPrepared")
                .expect("durable prompt evidence");
            let packet = &prepared_event.event["prompt_packet"];
            assert!(packet.get("resolved_runtime_settings").is_none());
            assert!(!packet["context_provenance"]
                .to_string()
                .contains("workflow_runtime_profile_selected"));
            assert!(!packet["context_provenance"]["entries"]
                .as_array()
                .expect("provenance")
                .is_empty());
            assert_eq!(packet["runtime_job"]["runtime_kind"], "remote_host");
        }
        Ok(())
    }

    #[tokio::test]
    async fn remote_prompt_claim_rejects_bad_workspace_before_leasing_and_raw_still_works(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "raw-host").await?;
        let job = enqueue_runtime_host_test_job(
            &store,
            "raw-quality",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity":"run_quality_gate"}),
        )
        .await?;
        for path in ["", "relative", "/workspace\n", " /workspace"] {
            let (status, _) = post_json_with_status(
                &app,
                "/api/runtime-hosts/raw-host/runtime-jobs/claim".into(),
                json!({"execution_workspace":path}),
            )
            .await?;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(store
                .get_runtime_job(&job.id)
                .await?
                .expect("job")
                .lease
                .is_none());
        }
        let raw = post_json(
            &app,
            "/api/runtime-hosts/raw-host/runtime-jobs/claim".into(),
            json!({}),
        )
        .await?;
        assert_eq!(raw["claimed"], true);
        assert!(raw.get("prepared_prompt").is_none());
        assert!(store
            .runtime_events_for(&job.id)
            .await?
            .iter()
            .all(|event| event.event_type != "RuntimePromptPrepared"));
        Ok(())
    }

    #[tokio::test]
    async fn remote_prompt_claim_delivers_native_quality_gate_without_rendering(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "mixed-host").await?;
        let expected_head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let command = json!({
            "activity": "run_quality_gate",
            "expected_head_sha": expected_head,
            "validation_commands_argv": [["python3", "-I", "/trusted/verify.py"]],
        });
        let job = enqueue_runtime_host_test_job(
            &store,
            "native-quality-mixed",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity":"run_quality_gate", "command":command}),
        )
        .await?;
        let response = post_json(
            &app,
            "/api/runtime-hosts/mixed-host/runtime-jobs/claim".into(),
            json!({"execution_workspace":"/workspace"}),
        )
        .await?;
        assert_eq!(response["claimed"], true);
        assert_eq!(response["runtime_job_id"], job.id);
        assert!(response.get("prepared_prompt").is_none());
        assert_eq!(response["runtime_job"]["input"]["command"], command);
        assert!(response["lease_proof"].as_str().is_some());
        let running = store.get_runtime_job(&job.id).await?.expect("job retained");
        assert_eq!(
            running.status,
            harness_workflow::runtime::RuntimeJobStatus::Running
        );
        let events = store.runtime_events_for(&job.id).await?;
        assert!(events
            .iter()
            .all(|event| event.event_type != "RuntimePromptPrepared"));
        super::check_claim_fence(&store, &running)
            .await
            .expect("native lease remains live");
        Ok(())
    }

    #[tokio::test]
    async fn remote_prompt_claim_failure_never_falls_back_to_raw() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "failed-prompt-host").await?;
        for (key, command, activity) in [
            (
                "missing-payload",
                json!({"prompt_ref":"prompt-memory:missing"}),
                "implement_issue",
            ),
            (
                "native-pinned-contract",
                json!({"agent_contract":{}}),
                "run_quality_gate",
            ),
            (
                "native-exact-replay",
                json!({"exact_replay":{}}),
                "run_quality_gate",
            ),
            ("server-child", json!({}), "start_child_workflow"),
            ("server-feedback", json!({}), "inspect_pr_feedback"),
            (
                "pinned-contract",
                json!({"agent_contract":{}}),
                "implement_issue",
            ),
            (
                "exact-replay",
                json!({"exact_replay":{}}),
                "implement_issue",
            ),
        ] {
            let job = enqueue_runtime_host_test_job(
                &store,
                key,
                RuntimeKind::RemoteHost,
                "remote",
                json!({"activity":activity,"command":command}),
            )
            .await?;
            let (_, response) = post_json_with_status(
                &app,
                "/api/runtime-hosts/failed-prompt-host/runtime-jobs/claim".into(),
                json!({"execution_workspace":"/workspace"}),
            )
            .await?;
            assert!(response.get("prepared_prompt").is_none());
            assert_ne!(response["claimed"], true);
            let failed = store.get_runtime_job(&job.id).await?.expect("job retained");
            assert_eq!(
                failed.status,
                harness_workflow::runtime::RuntimeJobStatus::Failed
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn remote_prompt_claim_fences_cancelled_reclaimed_and_expired_jobs() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "fence-host").await?;
        let job = enqueue_runtime_host_test_job(
            &store,
            "prompt-fence",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity":"implement_issue"}),
        )
        .await?;
        let claim = post_json(
            &app,
            "/api/runtime-hosts/fence-host/runtime-jobs/claim".into(),
            json!({}),
        )
        .await?;
        let expected: RuntimeJob = serde_json::from_value(claim["runtime_job"].clone())?;
        check_claim_fence(&store, &expected)
            .await
            .expect("owned lease");
        let mut current = expected.clone();
        current.input["cancellation_requested"] = json!({"reason":"operator"});
        store.persist_runtime_job_data(&current).await?;
        let (_, body) = check_claim_fence(&store, &expected)
            .await
            .expect_err("cancelled");
        assert_eq!(body["cleanup_ack_required"], true);
        let mut stale = expected.clone();
        stale.lease_generation += 1;
        let (_, body) = check_claim_fence(&store, &stale)
            .await
            .expect_err("generation mismatch");
        assert!(body.get("cleanup_ack_required").is_none());
        let expired_job = enqueue_runtime_host_test_job(
            &store,
            "prompt-expired",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity":"implement_issue"}),
        )
        .await?;
        let expired = store
            .claim_next_remote_host_runtime_job(
                "expired-host",
                Utc::now() - chrono::Duration::seconds(1),
                true,
                true,
            )
            .await?
            .expect("expired lease issued through the proof-aware store");
        assert_eq!(expired.id, expired_job.id);
        assert!(check_claim_fence(&store, &expired).await.is_err());
        assert_eq!(job.id, current.id);
        Ok(())
    }
    #[tokio::test]
    async fn remote_prompt_claim_delivers_durable_request_and_declarative_policy(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("WORKFLOW.md"),
            include_str!("../../../../../config/workflows/review.md"),
        )?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let document = harness_core::config::workflow::load_workflow_document(dir.path())?;
        let definition = harness_workflow::runtime::build_declarative_definition(
            document
                .config
                .definition
                .as_ref()
                .expect("declared definition"),
            &document.config.activities,
        )?;
        let mut registry = harness_workflow::runtime::WorkflowDefinitionRegistry::with_builtins();
        registry.register_declarative_current(definition)?;
        let store = Arc::new(
            (*store)
                .clone()
                .with_definition_registry(registry.into_shared()),
        );
        let mut state = state;
        Arc::get_mut(&mut state)
            .expect("unique test state")
            .core
            .workflow_runtime_store = Some(store.clone());
        let task_id = harness_core::types::TaskId::from_str("remote-prompt-durable");
        let submission = crate::workflow_runtime_submission::record_declarative_submission(
            &store,
            crate::workflow_runtime_submission::DeclarativeSubmissionRuntimeContext {
                project_root: dir.path(),
                definition_id: "repository_review",
                repo: None,
                task_id: &task_id,
                prompt: "Inspect the actual lease renewal boundary without edits.",
                depends_on: &[],
                serialization_depends_on: &[],
                source: Some("test"),
                external_id: Some("remote-prompt-durable"),
                subject_key: None,
                author_trust_class: None,
                classification_input_provenance: harness_workflow::runtime::DataProvenance::Server,
            },
        )
        .await?;
        let workflow = store
            .get_instance(&submission.workflow_id)
            .await?
            .expect("submitted workflow");
        let prompt_ref = workflow.data["prompt_ref"]
            .as_str()
            .expect("durable prompt reference");
        crate::workflow_runtime_submission::clear_prompt_submission_prompt_cache_for_test(
            prompt_ref,
        );
        let job = enqueue_runtime_host_test_job(
            &store,
            "durable-policy",
            RuntimeKind::RemoteHost,
            "remote",
            json!({
                "activity":"inspect_repository", "workflow_id":submission.workflow_id,
                "command":{"activity":"inspect_repository", "prompt_ref":prompt_ref},
            }),
        )
        .await?;
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "durable-host").await?;
        let response = post_json(
            &app,
            "/api/runtime-hosts/durable-host/runtime-jobs/claim".into(),
            json!({"execution_workspace":"/isolated/candidate"}),
        )
        .await?;
        assert_eq!(response["claimed"], true);
        let prompt = response["prepared_prompt"]["prompt"]
            .as_str()
            .expect("durable rendered prompt");
        assert!(prompt.contains("Inspect the actual lease renewal boundary without edits."));
        assert!(prompt.contains("Conduct a read-only review of the requested scope."));
        assert!(prompt.contains("Project root: /isolated/candidate"));
        assert!(prompt.contains("Use native JSON values for artifact and signal payloads"));
        assert!(!prompt.contains("The wrapper `json` field MUST"));
        let events = store.runtime_events_for(&job.id).await?;
        let packet = &events
            .iter()
            .find(|event| event.event_type == "RuntimePromptPrepared")
            .expect("prepared event")
            .event["prompt_packet"];
        assert_eq!(packet["workflow"]["definition_id"], "repository_review");
        assert!(packet.get("activity_policy").is_some());
        assert!(packet.get("prompt_task_request").is_some());
        Ok(())
    }
    #[tokio::test]
    async fn remote_prompt_claim_waiting_for_payload_does_not_block_other_lease_renewal(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        // This test needs concurrent queries; ordinary test stores intentionally use one connection.
        let context = harness_core::db::PgStoreContext::from_legacy_path_schema(
            &dir.path().join("concurrent.db"),
            None,
        )?;
        context.ensure_schema(store.pool()).await?;
        let pool = context.open_runtime_pool_with_max_connections(8).await?;
        let store = Arc::new(WorkflowRuntimeStore::open_with_shared_pool(pool).await?);
        let mut state = state;
        Arc::get_mut(&mut state)
            .expect("unique state")
            .core
            .workflow_runtime_store = Some(store.clone());
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "parallel-host").await?;
        enqueue_runtime_host_test_job(
            &store,
            "already-running",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity":"implement_issue"}),
        )
        .await?;
        let first = post_json(
            &app,
            "/api/runtime-hosts/parallel-host/runtime-jobs/claim".into(),
            json!({}),
        )
        .await?;
        let prompt_ref = "prompt-memory:blocked-durable-read";
        store
            .insert_prompt_payload(prompt_ref, "Do the independently queued task.")
            .await?;
        enqueue_runtime_host_test_job(
            &store,
            "waiting-payload",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity":"implement_issue", "command":{"prompt_ref":prompt_ref}}),
        )
        .await?;
        let mut blocked = store.pool().begin().await?;
        sqlx::query("LOCK TABLE workflow_prompt_payloads IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocked)
            .await?;
        let waiting_app = app.clone();
        let waiting = tokio::spawn(async move {
            post_json_with_status(
                &waiting_app,
                "/api/runtime-hosts/parallel-host/runtime-jobs/claim".into(),
                json!({"execution_workspace":"/workspace"}),
            )
            .await
        });
        let observed_wait = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='workflow_prompt_payloads'::regclass AND mode='AccessShareLock' AND NOT granted)").fetch_one(store.pool()).await?;
                if waiting { return Ok::<_, anyhow::Error>(()); }
                tokio::task::yield_now().await;
            }
        }).await;
        let renewal = tokio::time::timeout(std::time::Duration::from_secs(3), post_json_with_status(
            &app, format!("/api/runtime-hosts/parallel-host/runtime-jobs/{}/lease/renew", first["runtime_job_id"].as_str().expect("job id")),
            json!({"lease_generation":first["lease_generation"], "lease_expires_at":first["lease_expires_at"], "lease_proof":first["lease_proof"], "renewal_id":uuid::Uuid::new_v4(), "lease_secs":120}),
        )).await;
        blocked.rollback().await?;
        let (_, prepared) = waiting.await??;
        observed_wait.map_err(|error| {
            anyhow::anyhow!("durable lookup did not reach table wait: {error}")
        })??;
        let (status, _) =
            renewal.map_err(|error| anyhow::anyhow!("other lease renewal blocked: {error}"))??;
        assert_eq!(status, StatusCode::OK);
        assert!(prepared["prepared_prompt"]["prompt"]
            .as_str()
            .expect("prompt")
            .contains("Do the independently queued task."));
        Ok(())
    }
    #[tokio::test]
    async fn remote_prompt_claim_optional_memory_failure_keeps_audited_prompt() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("WORKFLOW.md"),
            "---\nmemory:\n  enabled: true\n---\nFollow the activity instructions.\n",
        )?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let app = runtime_hosts_workflow_app(state);
        register_host(&app, "memory-host").await?;
        let job = enqueue_runtime_host_test_job(
            &store,
            "memory-unavailable",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity":"implement_issue", "repo":"owner/repo"}),
        )
        .await?;
        sqlx::query("ALTER TABLE workflow_repo_memory RENAME TO unavailable_repo_memory")
            .execute(store.pool())
            .await?;
        let response = post_json_with_status(
            &app,
            "/api/runtime-hosts/memory-host/runtime-jobs/claim".into(),
            json!({"execution_workspace":"/workspace"}),
        )
        .await;
        sqlx::query("ALTER TABLE unavailable_repo_memory RENAME TO workflow_repo_memory")
            .execute(store.pool())
            .await?;
        let (status, response) = response?;
        assert_eq!(status, StatusCode::OK);
        assert!(response.get("prepared_prompt").is_some());
        let events = store.runtime_events_for(&job.id).await?;
        let event = events
            .iter()
            .find(|event| event.event_type == "RuntimePromptPrepared")
            .expect("prepared event");
        assert_eq!(
            event.event["repo_memory_degradation"]["artifact"]["reason"],
            "repo_memory_retrieval_failed"
        );
        Ok(())
    }
    #[tokio::test]
    async fn remote_prompt_claim_enforcement_audits_require_successful_delivery(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
            return Ok(());
        };
        let app = runtime_hosts_workflow_app(state);
        crate::handlers::runtime_hosts_workflow_api_tests::register_host_with_capabilities(
            &app,
            "policy-host",
            vec!["eval_resource_limits", "eval_network_policy"],
        )
        .await?;
        for (key, render, fail) in [
            ("raw-policy", false, false),
            ("rendered-policy", true, false),
            ("failed-policy", true, true),
        ] {
            let mut command = json!({"activity":"implement_issue", "eval":{"eval_run_id":"run-1", "case_id":"case-1", "timeout_secs":45}});
            if fail {
                command["prompt_ref"] = json!("prompt-memory:missing-policy-task");
            }
            let job = enqueue_runtime_host_test_job(&store, key, RuntimeKind::RemoteHost, "remote", json!({"activity":"implement_issue", "isolation":{"network_allowlist":["api.github.com"]}, "command":command})).await?;
            let request = if render {
                json!({"execution_workspace":"/workspace"})
            } else {
                json!({})
            };
            let (_, response) = post_json_with_status(
                &app,
                "/api/runtime-hosts/policy-host/runtime-jobs/claim".into(),
                request,
            )
            .await?;
            let events = store.runtime_events_for(&job.id).await?;
            for (kind, field) in [
                ("EvalResourceLimitsApplied", "resource_limits"),
                ("EvalNetworkPolicyApplied", "network_policy"),
            ] {
                let applied: Vec<_> = events
                    .iter()
                    .filter(|event| event.event_type == kind)
                    .collect();
                if fail {
                    assert!(applied.is_empty());
                    assert!(response.get(field).is_none());
                    assert_ne!(response["claimed"], true);
                } else {
                    assert_eq!(response["claimed"], true);
                    assert_eq!(applied.len(), 1);
                    assert_eq!(applied[0].event[field], response[field]);
                }
            }
        }
        Ok(())
    }
}
