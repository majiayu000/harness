use super::*;
use futures::FutureExt;
use harness_core::db::{pg_open_pool, resolve_database_url, PgStoreContext};
use serde_json::json;
use tempfile::tempdir;

fn unique_test_schema(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("{prefix}_{nanos}_{count}")
}

#[test]
fn shared_schema_context_uses_fixed_eval_store_schema() -> anyhow::Result<()> {
    let context =
        EvalStore::shared_schema_context(Some("postgres://user:pass@localhost:5432/harness"))?;
    assert_eq!(context.schema(), EVAL_STORE_SCHEMA);
    assert!(
        context.ownership().is_none(),
        "shared eval_store schema must not register path-derived ownership"
    );
    Ok(())
}

#[test]
fn store_key_for_missing_data_dir_errors() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let missing = dir.path().join("missing-data-dir");
    let err = EvalStore::store_key_for_data_dir(&missing)
        .expect_err("missing data_dir should not produce a fallback store key");
    let msg = err.to_string();
    assert!(
        msg.contains("failed to canonicalize eval_store data_dir"),
        "error should mention eval_store data_dir canonicalization, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn shared_schema_eval_store_keeps_store_rows_isolated() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempdir()?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let shared_schema = unique_test_schema("eval_store_scope_test");
    let shared_context = PgStoreContext::from_schema(&shared_schema, Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    std::fs::create_dir_all(&store_a_dir)?;
    std::fs::create_dir_all(&store_b_dir)?;
    let store_a =
        EvalStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir).await?;
    let store_b =
        EvalStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir).await?;

    let result = std::panic::AssertUnwindSafe(async {
        assert_eq!(store_a.schema(), shared_schema);
        assert_eq!(store_b.schema(), shared_schema);
        assert_ne!(store_a.store_key(), store_b.store_key());

        let run_a = store_a.create_run(pr_repair_run()).await?;
        assert!(
            store_b.get_run(&run_a.id).await?.is_none(),
            "store B must not see store A runs in the shared schema"
        );
        assert!(
            store_b
                .add_artifact(&run_a.id, artifact("foreign"))
                .await?
                .is_none(),
            "store B must not attach artifacts to store A runs"
        );

        let run_b = store_b.create_run(pr_repair_run()).await?;
        let artifact_a = store_a
            .add_artifact(&run_a.id, artifact("store-a"))
            .await?
            .expect("store A artifact");
        let artifact_b = store_b
            .add_artifact(&run_b.id, artifact("store-b"))
            .await?
            .expect("store B artifact");

        assert_eq!(store_a.list_runs(10).await?.len(), 1);
        assert_eq!(store_b.list_runs(10).await?.len(), 1);
        assert_eq!(store_a.list_artifacts(&run_a.id).await?, vec![artifact_a]);
        assert_eq!(store_b.list_artifacts(&run_b.id).await?, vec![artifact_b]);

        let score_a = store_a
            .score_run(&run_a.id, pr_repair_input())
            .await?
            .expect("score store A run");
        let score_b = store_b
            .score_run(&run_b.id, pr_repair_input())
            .await?
            .expect("score store B run");
        assert_eq!(
            store_a
                .get_quality_snapshot(&score_a.quality_snapshot.id)
                .await?
                .expect("store A snapshot")
                .id,
            score_a.quality_snapshot.id
        );
        assert!(
            store_b
                .get_quality_snapshot(&score_a.quality_snapshot.id)
                .await?
                .is_none(),
            "store B must not see store A snapshots"
        );

        let snapshots_a = store_a
            .list_quality_snapshots_for_pr("owner/repo", 7, 10)
            .await?;
        let snapshots_b = store_b
            .list_quality_snapshots_for_pr("owner/repo", 7, 10)
            .await?;
        assert_eq!(snapshots_a.len(), 1);
        assert_eq!(snapshots_b.len(), 1);
        assert_eq!(snapshots_a[0].id, score_a.quality_snapshot.id);
        assert_eq!(snapshots_b[0].id, score_b.quality_snapshot.id);

        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    store_a.pool.close().await;
    store_b.pool.close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{shared_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn legacy_eval_store_migration_backfills_once() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    let other_data_dir = dir.path().join("other-data");
    std::fs::create_dir_all(&target_data_dir)?;
    std::fs::create_dir_all(&other_data_dir)?;
    let legacy_path = target_data_dir.join("evals.db");
    let legacy_schema = PgStoreContext::from_legacy_path_schema(&legacy_path, Some(&database_url))?
        .schema()
        .to_owned();
    let target_schema = unique_test_schema("eval_store_migration_test");
    let setup_pool = pg_open_pool(&database_url).await?;
    let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
    let target_store =
        EvalStore::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir)
            .await?;
    let other_store =
        EvalStore::open_shared_with_data_dir(&target_context, &setup_pool, &other_data_dir).await?;
    let legacy_store = EvalStore::open_with_database_url(&legacy_path, Some(&database_url)).await?;

    let result = std::panic::AssertUnwindSafe(async {
        let legacy_run = legacy_store.create_run(pr_repair_run()).await?;
        let legacy_artifact = legacy_store
            .add_artifact(&legacy_run.id, artifact("legacy"))
            .await?
            .expect("legacy artifact");
        let legacy_score = legacy_store
            .score_run(&legacy_run.id, pr_repair_input())
            .await?
            .expect("legacy score");

        let copied =
            migrate_legacy_eval_store_if_needed(&legacy_path, Some(&database_url), &target_store)
                .await?;
        assert_eq!(
            copied, 3,
            "one run, one artifact, and one quality snapshot should be copied"
        );

        let copied_again =
            migrate_legacy_eval_store_if_needed(&legacy_path, Some(&database_url), &target_store)
                .await?;
        assert_eq!(copied_again, 0, "migration must be idempotent");

        assert_eq!(
            target_store
                .get_run(&legacy_run.id)
                .await?
                .expect("legacy run should be backfilled")
                .id,
            legacy_run.id
        );
        assert_eq!(
            target_store.list_artifacts(&legacy_run.id).await?,
            vec![legacy_artifact]
        );
        assert_eq!(
            target_store
                .get_quality_snapshot(&legacy_score.quality_snapshot.id)
                .await?
                .expect("legacy snapshot should be backfilled")
                .id,
            legacy_score.quality_snapshot.id
        );
        assert!(
            other_store.get_run(&legacy_run.id).await?.is_none(),
            "other data_dir scopes must not hydrate legacy eval runs"
        );
        assert!(
            other_store
                .list_quality_snapshots_for_pr("owner/repo", 7, 10)
                .await?
                .is_empty(),
            "other data_dir scopes must not hydrate legacy eval snapshots"
        );

        let stale_legacy_run = legacy_store.create_run(pr_repair_run()).await?;
        let copied_after_stale_update =
            migrate_legacy_eval_store_if_needed(&legacy_path, Some(&database_url), &target_store)
                .await?;
        assert_eq!(copied_after_stale_update, 0);
        assert!(
            target_store.get_run(&stale_legacy_run.id).await?.is_none(),
            "backfill marker should prevent stale legacy reimport on later startup"
        );

        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    legacy_store.pool.close().await;
    target_store.pool.close().await;
    other_store.pool.close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{legacy_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn legacy_eval_store_migration_ignores_missing_legacy_schema() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    std::fs::create_dir_all(&target_data_dir)?;
    let missing_legacy_path = dir.path().join("missing-legacy").join("evals.db");
    let target_schema = unique_test_schema("eval_store_missing_legacy_test");
    let setup_pool = pg_open_pool(&database_url).await?;
    let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
    let target_store =
        EvalStore::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir)
            .await?;

    let result = std::panic::AssertUnwindSafe(async {
        let copied = migrate_legacy_eval_store_if_needed(
            &missing_legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(copied, 0, "missing legacy schema should be a no-op");
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    target_store.pool.close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn pr_repair_run() -> CreateEvalRun {
    CreateEvalRun {
        scenario: EvalScenario::PrRepair,
        target: EvalTarget::PullRequest {
            repo: "owner/repo".to_string(),
            pr_number: 7,
            base_ref: Some("main".to_string()),
            head_ref: Some("fix/review".to_string()),
        },
        source_task_id: Some("task-7".to_string()),
    }
}

fn artifact(label: &str) -> AddEvalArtifact {
    AddEvalArtifact {
        artifact_type: "pr_repair_eval_input".to_string(),
        label: Some(label.to_string()),
        content_type: Some("application/json".to_string()),
        body: pr_repair_input_json().to_string(),
    }
}

fn pr_repair_input() -> PrRepairEvalInput {
    serde_json::from_value(pr_repair_input_json()).expect("valid PR repair input")
}

fn pr_repair_input_json() -> serde_json::Value {
    json!({
        "scenario": "pr_repair",
        "run_mode": "live_run",
        "target": {
            "kind": "pull_request",
            "repo": "owner/repo",
            "pr_number": 7,
            "base_ref": "main",
            "head_ref": "fix/review"
        },
        "baseline_pr": pr_snapshot_json("base-head", true),
        "final_pr": pr_snapshot_json("final-head", false),
        "final_evidence_head_oid": "final-head",
        "runtime": {
            "task_id": "task-7",
            "workflow_id": "workflow-7",
            "workflow_state": "done",
            "runtime_jobs": [{
                "runtime_job_id": "job-7",
                "state": "completed",
                "artifact_count": 1,
                "terminal_state": "succeeded"
            }],
            "latest_activity": "address_pr_feedback",
            "terminal_state": "done",
            "collected_at": "2026-06-05T00:00:00Z"
        },
        "usage": [],
        "reviewer_judgment": {
            "reviewer_kind": "llm",
            "judged_head_oid": "final-head",
            "code_quality_score": 90,
            "trajectory_score": 90,
            "findings": [],
            "residual_risks": []
        },
        "created_unrelated_pr": false,
        "scope_violations": []
    })
}

fn pr_snapshot_json(head_oid: &str, unresolved_thread: bool) -> serde_json::Value {
    let active_unresolved_review_threads = if unresolved_thread {
        json!([{
            "id": "thread-1",
            "path": "src/lib.rs",
            "is_resolved": false,
            "is_outdated": false
        }])
    } else {
        json!([])
    };
    json!({
        "repo": "owner/repo",
        "pr_number": 7,
        "url": "https://github.com/owner/repo/pull/7",
        "title": "Fix review feedback",
        "base_ref": "main",
        "head_ref": "fix/review",
        "head_oid": head_oid,
        "is_draft": false,
        "merge_state": "clean",
        "check_state": "passing",
        "review_decision": "approved",
        "active_unresolved_review_threads": active_unresolved_review_threads,
        "review_threads_complete": true,
        "changed_files": [{
            "path": "src/lib.rs",
            "additions": 4,
            "deletions": 1,
            "status": "modified"
        }],
        "changed_files_complete": true,
        "collected_at": "2026-06-05T00:00:00Z"
    })
}
