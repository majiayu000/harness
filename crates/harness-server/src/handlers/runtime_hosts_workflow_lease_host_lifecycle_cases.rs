#[tokio::test]
async fn runtime_job_lease_renewal_is_fenced_idempotent_and_sanitized() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    register_host(&app, "host-b").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "renewal",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    let request = json!({
        "lease_generation": claimed["lease_generation"],
        "lease_expires_at": claimed["lease_expires_at"],
        "lease_proof": claimed["lease_proof"],
        "renewal_id": uuid::Uuid::new_v4(),
        "lease_secs": 120,
    });
    let uri = format!(
        "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
        job.id
    );
    let renewed = post_json(&app, uri.clone(), request.clone()).await?;
    assert_eq!(renewed["renewed"], true);
    assert_eq!(renewed["replayed"], false);
    assert_eq!(renewed["lease_generation"], claimed["lease_generation"]);

    let replayed = post_json(&app, uri, request.clone()).await?;
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["lease_expires_at"], renewed["lease_expires_at"]);

    let lease_generation = claimed["lease_generation"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("claim must return a numeric lease generation"))?;
    let result = ActivityResult::failed(
        "remote_check",
        "Remote host reported a failed activity.",
        "remote execution failed",
    );
    let (status, _) = post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": lease_generation + 1,
            "lease_expires_at": renewed["lease_expires_at"],
            "lease_proof": renewed["lease_proof"],
            "result": result,
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, lost) = post_json_with_status(
        &app,
        format!(
            "/api/runtime-hosts/host-b/runtime-jobs/{}/lease/renew",
            job.id
        ),
        request,
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        lost,
        json!({ "error_code": "lease_lost", "must_stop": true })
    );
    assert!(!lost.to_string().contains("host-a"));

    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": lease_generation,
            "lease_expires_at": renewed["lease_expires_at"],
            "lease_proof": renewed["lease_proof"],
            "result": ActivityResult::failed(
                "remote_check",
                "Remote host reported a failed activity.",
                "remote execution failed",
            ),
        }),
    )
    .await?;
    assert_eq!(completed["completed"], true);
    Ok(())
}

#[tokio::test]
async fn runtime_job_lease_renewal_rejects_invalid_duration_as_bad_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, _store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    for lease_secs in [json!(0), json!(3601), json!(null)] {
        let (status, _) = post_json_with_status(
            &app,
            "/api/runtime-hosts/host-a/runtime-jobs/missing/lease/renew".to_string(),
            json!({
                "lease_generation": 1,
                "lease_expires_at": Utc::now(),
                "renewal_id": uuid::Uuid::new_v4(),
                "lease_secs": lease_secs,
            }),
        )
        .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    Ok(())
}

#[tokio::test]
async fn runtime_host_deregister_revokes_workflow_job_before_removal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state.clone());
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "deregister-revocation",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/deregister")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!state.runtime_hosts.hosts.contains_key("host-a"));
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job should remain persisted"))?;
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert!(persisted.lease.is_none());
    let events = store.runtime_events_for(&job.id).await?;
    assert_eq!(
        events.last().map(|event| event.event_type.as_str()),
        Some("RuntimeJobLeaseRevoked")
    );
    Ok(())
}

#[tokio::test]
async fn register_runtime_host_rejects_required_missing_runtime_state_store() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
    let state_mut =
        Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("runtime_state_store")
                .failed("pool timed out while waiting for an open connection"),
        ];
    state_mut.degraded_subsystems = vec!["runtime_state_store"];
    let app = runtime_hosts_workflow_app(state.clone());

    let (status, body) = post_json_with_status(
        &app,
        "/api/runtime-hosts/register".to_string(),
        json!({
            "host_id": "host-a",
            "display_name": null,
            "capabilities": [],
        }),
    )
    .await?;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "runtime state persistence unavailable");
    assert!(
        !state.runtime_hosts.hosts.contains_key("host-a"),
        "host registration must not mutate memory when required persistence is unavailable"
    );
    assert!(state.is_runtime_state_dirty());
    Ok(())
}

#[tokio::test]
async fn deregister_runtime_host_rejects_required_missing_runtime_state_store_before_lookup(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
    let state_mut =
        Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("runtime_state_store")
                .failed("pool timed out while waiting for an open connection"),
        ];
    state_mut.degraded_subsystems = vec!["runtime_state_store"];
    let app = runtime_hosts_workflow_app(state.clone());

    let (status, body) = post_json_with_status(
        &app,
        "/api/runtime-hosts/ghost-host/deregister".to_string(),
        json!({}),
    )
    .await?;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "runtime state persistence unavailable");
    assert!(state.is_runtime_state_dirty());
    Ok(())
}
