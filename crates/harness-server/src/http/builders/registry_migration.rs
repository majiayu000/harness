use std::path::Path;

/// Walk all `issue_workflows` rows. For each row whose `project_id` contains
/// `/workspaces/` (a corrupt worktree path), replace it with `canonical_root`.
/// Returns `(rewritten, failed, skipped)` counts.
pub(super) async fn repair_corrupt_project_ids(
    store: &harness_workflow::issue_lifecycle::IssueWorkflowStore,
    canonical_root: &Path,
) -> (u64, u64, u64) {
    let corrupt_rows = match store.list_with_worktree_project_ids().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("startup repair: failed to list corrupt issue workflows: {e}");
            return (0, 0, 0);
        }
    };
    let total_count = store.row_count().await.unwrap_or(0);

    let new_project_id = canonical_root.to_string_lossy().into_owned();
    let (mut rewritten, mut failed) = (0u64, 0u64);
    let skipped = (total_count as u64).saturating_sub(corrupt_rows.len() as u64);

    for workflow in corrupt_rows {
        tracing::info!(
            row_id = %workflow.id,
            old_project_id = %workflow.project_id,
            new_project_id = %new_project_id,
            "startup repair: rewriting corrupt workflow project_id"
        );
        match store.repair_project_id(&workflow.id, &new_project_id).await {
            Ok(()) => rewritten += 1,
            Err(e) => {
                tracing::error!(
                    row_id = %workflow.id,
                    "startup repair: failed to rewrite project_id: {e}; marking workflow failed"
                );
                if let Err(e2) = store
                    .mark_workflow_failed_with_reason(
                        &workflow.id,
                        "failed to repair project_id during startup",
                    )
                    .await
                {
                    tracing::error!(
                        row_id = %workflow.id,
                        "startup repair: also failed to mark workflow as failed: {e2}"
                    );
                }
                failed += 1;
            }
        }
    }

    (rewritten, failed, skipped)
}

pub(super) async fn migrate_issue_workflows_if_needed(
    configured_database_url: Option<&str>,
    legacy_path: &Path,
    target_schema: &str,
    target_store: &harness_workflow::issue_lifecycle::IssueWorkflowStore,
) -> anyhow::Result<()> {
    let legacy_schema = harness_workflow::issue_lifecycle::legacy_schema_for_path(legacy_path)?;
    if legacy_schema == target_schema {
        return Ok(());
    }

    let legacy_store =
        harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url(
            legacy_path,
            configured_database_url,
        )
        .await?;
    let legacy_rows = legacy_store.list().await?;
    if legacy_rows.is_empty() {
        return Ok(());
    }

    let mut copied = 0usize;
    let mut skipped_existing = 0usize;
    for workflow in &legacy_rows {
        if target_store.insert_if_absent(workflow).await? {
            copied += 1;
        } else {
            skipped_existing += 1;
        }
    }
    tracing::info!(
        copied,
        skipped_existing,
        legacy_schema = %legacy_schema,
        target_schema = %target_schema,
        "workflow migration: backfilled legacy issue workflow rows into namespaced schema"
    );
    Ok(())
}

pub(super) async fn migrate_project_workflows_if_needed(
    configured_database_url: Option<&str>,
    legacy_path: &Path,
    target_schema: &str,
    target_store: &harness_workflow::project_lifecycle::ProjectWorkflowStore,
) -> anyhow::Result<()> {
    let legacy_schema = harness_workflow::project_lifecycle::legacy_schema_for_path(legacy_path)?;
    if legacy_schema == target_schema {
        return Ok(());
    }

    let legacy_store =
        harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url(
            legacy_path,
            configured_database_url,
        )
        .await?;
    let legacy_rows = legacy_store.list().await?;
    if legacy_rows.is_empty() {
        return Ok(());
    }

    let mut copied = 0usize;
    let mut skipped_existing = 0usize;
    for workflow in &legacy_rows {
        if target_store.insert_if_absent(workflow).await? {
            copied += 1;
        } else {
            skipped_existing += 1;
        }
    }
    tracing::info!(
        copied,
        skipped_existing,
        legacy_schema = %legacy_schema,
        target_schema = %target_schema,
        "workflow migration: backfilled legacy project workflow rows into namespaced schema"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::{pg_schema_for_path, resolve_database_url};

    async fn open_test_issue_store(
    ) -> anyhow::Result<Option<harness_workflow::issue_lifecycle::IssueWorkflowStore>> {
        if harness_core::db::resolve_database_url(None).is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        match harness_workflow::issue_lifecycle::IssueWorkflowStore::open(
            &dir.path().join("issue_workflows.db"),
        )
        .await
        {
            Ok(store) => Ok(Some(store)),
            Err(e) => {
                tracing::warn!("issue workflow store test skipped: {e}");
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn migration_rewrites_worktree_paths() -> anyhow::Result<()> {
        let Some(store) = open_test_issue_store().await? else {
            return Ok(());
        };
        let corrupt = "/data/workspaces/abc-uuid-reg-test";
        let canonical = std::path::Path::new("/real/canonical/reg-root");

        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 8001, "task-reg1", &[], false)
            .await?;
        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 8002, "task-reg2", &[], false)
            .await?;

        let (rewritten, failed, skipped) = repair_corrupt_project_ids(&store, canonical).await;
        assert_eq!(rewritten, 2, "both worktree rows should be rewritten");
        assert_eq!(failed, 0);
        assert_eq!(skipped, 0);

        let canonical_str = canonical.to_string_lossy();
        let r1 = store
            .get_by_issue(&canonical_str, Some("owner/repo"), 8001)
            .await?;
        assert!(r1.is_some(), "row 8001 should have canonical project_id");
        let r2 = store
            .get_by_issue(&canonical_str, Some("owner/repo"), 8002)
            .await?;
        assert!(r2.is_some(), "row 8002 should have canonical project_id");
        Ok(())
    }

    #[tokio::test]
    async fn migration_skips_canonical_rows() -> anyhow::Result<()> {
        let Some(store) = open_test_issue_store().await? else {
            return Ok(());
        };
        let canonical_id = "/tmp/already-canonical-skip-test";
        let canonical_root = std::path::Path::new("/any/root");

        store
            .record_issue_scheduled(
                canonical_id,
                Some("owner/repo"),
                8003,
                "task-skip1",
                &[],
                false,
            )
            .await?;

        let (rewritten, failed, skipped) = repair_corrupt_project_ids(&store, canonical_root).await;
        assert_eq!(skipped, 1, "canonical row should be skipped");
        assert_eq!(rewritten, 0);
        assert_eq!(failed, 0);

        // Row untouched: project_id unchanged
        let row = store
            .get_by_issue(canonical_id, Some("owner/repo"), 8003)
            .await?
            .expect("row should still exist");
        assert_eq!(row.project_id, canonical_id);
        Ok(())
    }

    #[tokio::test]
    async fn migration_counts_all_categories() -> anyhow::Result<()> {
        let Some(store) = open_test_issue_store().await? else {
            return Ok(());
        };
        let corrupt = "/data/workspaces/mix-uuid-reg-test";
        let canonical_id = "/tmp/canonical-mix-test";
        let canonical_root = std::path::Path::new("/real/mix-root");

        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 8010, "task-mix1", &[], false)
            .await?;
        store
            .record_issue_scheduled(
                canonical_id,
                Some("owner/repo"),
                8011,
                "task-mix2",
                &[],
                false,
            )
            .await?;

        let (rewritten, failed, skipped) = repair_corrupt_project_ids(&store, canonical_root).await;
        assert_eq!(rewritten, 1);
        assert_eq!(failed, 0);
        assert_eq!(skipped, 1);
        Ok(())
    }

    #[tokio::test]
    async fn issue_workflow_migration_backfills_partial_target() -> anyhow::Result<()> {
        let configured_database_url = match resolve_database_url(None) {
            Ok(url) => Some(url),
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let legacy_path = dir.path().join("issue_workflows.db");
        let target_schema = pg_schema_for_path(&dir.path().join("issue_workflows_target.db"))?;

        let legacy_store =
            harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url(
                &legacy_path,
                configured_database_url.as_deref(),
            )
            .await?;
        let target_store =
            harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url_and_schema(
                configured_database_url.as_deref(),
                &target_schema,
            )
            .await?;

        legacy_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9101,
                "task-9101",
                &[],
                false,
            )
            .await?;
        legacy_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9102,
                "task-9102",
                &[],
                false,
            )
            .await?;
        target_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9101,
                "target-task-9101",
                &[],
                false,
            )
            .await?;
        target_store
            .record_pr_detected(
                "/tmp/project",
                Some("owner/repo"),
                9101,
                "target-pr-task-9101",
                99101,
                "https://github.com/owner/repo/pull/99101",
            )
            .await?;

        migrate_issue_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;

        let existing = target_store
            .get_by_issue("/tmp/project", Some("owner/repo"), 9101)
            .await?
            .expect("existing target row should remain present");
        assert_eq!(
            existing.state,
            harness_workflow::issue_lifecycle::IssueLifecycleState::PrOpen,
            "existing target row state should not be overwritten"
        );
        assert_eq!(existing.pr_number, Some(99101));
        assert_eq!(
            existing.active_task_id.as_deref(),
            Some("target-pr-task-9101")
        );
        assert!(
            target_store
                .get_by_issue("/tmp/project", Some("owner/repo"), 9102)
                .await?
                .is_some(),
            "missing legacy row should be backfilled even when target is non-empty"
        );
        Ok(())
    }

    #[tokio::test]
    async fn project_workflow_migration_backfills_partial_target() -> anyhow::Result<()> {
        let configured_database_url = match resolve_database_url(None) {
            Ok(url) => Some(url),
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let legacy_path = dir.path().join("project_workflows.db");
        let target_schema = pg_schema_for_path(&dir.path().join("project_workflows_target.db"))?;

        let legacy_store =
            harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url(
                &legacy_path,
                configured_database_url.as_deref(),
            )
            .await?;
        let target_store = harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url_and_schema(
            configured_database_url.as_deref(),
            &target_schema,
        )
        .await?;

        legacy_store
            .record_poll_started("/tmp/project", Some("owner/repo"))
            .await?;
        legacy_store
            .record_poll_started("/tmp/project", Some("owner/repo-two"))
            .await?;
        target_store
            .record_poll_started("/tmp/project", Some("owner/repo"))
            .await?;
        target_store
            .record_degraded("/tmp/project", Some("owner/repo"), "target is canonical")
            .await?;

        migrate_project_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;

        let existing = target_store
            .get_by_project("/tmp/project", Some("owner/repo"))
            .await?
            .expect("existing target row should remain present");
        assert_eq!(
            existing.state,
            harness_workflow::project_lifecycle::ProjectWorkflowState::Degraded,
            "existing target row state should not be overwritten"
        );
        assert_eq!(
            existing.degraded_reason.as_deref(),
            Some("target is canonical")
        );
        assert!(
            target_store
                .get_by_project("/tmp/project", Some("owner/repo-two"))
                .await?
                .is_some(),
            "missing legacy row should be backfilled even when target is non-empty"
        );
        Ok(())
    }

    // Regression for #928: rows written to the legacy schema after the first
    // migration boot (e.g. by a still-old node during a rolling upgrade) must
    // be picked up on subsequent boots. A non-empty target schema is not
    // sufficient proof that migration is complete.
    #[tokio::test]
    async fn issue_workflow_migration_picks_up_late_legacy_rows_across_boots() -> anyhow::Result<()>
    {
        let configured_database_url = match resolve_database_url(None) {
            Ok(url) => Some(url),
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let legacy_path = dir.path().join("issue_workflows.db");
        let target_schema = pg_schema_for_path(&dir.path().join("issue_workflows_target.db"))?;

        let legacy_store =
            harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url(
                &legacy_path,
                configured_database_url.as_deref(),
            )
            .await?;
        let target_store =
            harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url_and_schema(
                configured_database_url.as_deref(),
                &target_schema,
            )
            .await?;

        // Boot 1: one legacy row exists; migration copies it.
        legacy_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9201,
                "task-9201",
                &[],
                false,
            )
            .await?;
        migrate_issue_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;
        assert!(
            target_store
                .get_by_issue("/tmp/project", Some("owner/repo"), 9201)
                .await?
                .is_some(),
            "boot 1 should backfill the first legacy row"
        );

        // Between boots, a still-old node writes a second row into the legacy
        // schema. Target is now non-empty, but migration must still run.
        legacy_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9202,
                "task-9202",
                &[],
                false,
            )
            .await?;

        // Boot 2: target already has rows; migration must still copy the
        // late-arriving legacy row instead of returning early.
        migrate_issue_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;
        assert!(
            target_store
                .get_by_issue("/tmp/project", Some("owner/repo"), 9202)
                .await?
                .is_some(),
            "boot 2 must backfill late-arriving legacy rows even when the target schema is non-empty"
        );
        assert!(
            target_store
                .get_by_issue("/tmp/project", Some("owner/repo"), 9201)
                .await?
                .is_some(),
            "previously migrated rows must remain after the second boot"
        );
        Ok(())
    }

    // Regression for #928: same two-boot scenario for project workflows.
    #[tokio::test]
    async fn project_workflow_migration_picks_up_late_legacy_rows_across_boots(
    ) -> anyhow::Result<()> {
        let configured_database_url = match resolve_database_url(None) {
            Ok(url) => Some(url),
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let legacy_path = dir.path().join("project_workflows.db");
        let target_schema = pg_schema_for_path(&dir.path().join("project_workflows_target.db"))?;

        let legacy_store =
            harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url(
                &legacy_path,
                configured_database_url.as_deref(),
            )
            .await?;
        let target_store = harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url_and_schema(
            configured_database_url.as_deref(),
            &target_schema,
        )
        .await?;

        legacy_store
            .record_poll_started("/tmp/project", Some("owner/repo-a"))
            .await?;
        migrate_project_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;
        assert!(
            target_store
                .get_by_project("/tmp/project", Some("owner/repo-a"))
                .await?
                .is_some(),
            "boot 1 should backfill the first legacy project row"
        );

        legacy_store
            .record_poll_started("/tmp/project", Some("owner/repo-b"))
            .await?;

        migrate_project_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;
        assert!(
            target_store
                .get_by_project("/tmp/project", Some("owner/repo-b"))
                .await?
                .is_some(),
            "boot 2 must backfill late-arriving legacy project rows even when target is non-empty"
        );
        assert!(
            target_store
                .get_by_project("/tmp/project", Some("owner/repo-a"))
                .await?
                .is_some(),
            "previously migrated project rows must remain after the second boot"
        );
        Ok(())
    }
}
