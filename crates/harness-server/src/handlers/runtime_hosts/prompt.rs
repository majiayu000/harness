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
) -> Result<Option<Value>, ActivityResult> {
    let Some(workspace) = workspace else {
        return Ok(None);
    };
    crate::workflow_runtime_worker::remote_prompt::prepare(state, job, workspace)
        .await
        .map(Some)
        .map_err(|error| {
            ActivityResult::failed(
                completion::runtime_job_activity(job),
                "Remote runtime prompt preparation failed.",
                error.to_string(),
            )
            .with_error_kind(
                crate::workflow_runtime_worker::remote_prompt::error_kind(&error),
            )
        })
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
            ("native-quality", json!({}), "run_quality_gate"),
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
}
