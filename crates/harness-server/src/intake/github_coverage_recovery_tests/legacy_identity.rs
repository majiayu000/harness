use super::*;
use harness_workflow::project_lifecycle::ProjectWorkflowStore;

#[tokio::test]
async fn canonical_intake_reuses_legacy_issue_and_project_workflows() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = crate::test_helpers::tempdir_in_home("harness-test-legacy-intake-")?;
    let database_url = crate::test_helpers::test_database_url()?;
    let issue_store = IssueWorkflowStore::open_with_database_url(
        &dir.path().join("issue-workflows"),
        Some(&database_url),
    )
    .await?;
    let project_store = ProjectWorkflowStore::open_with_database_url(
        &dir.path().join("project-workflows"),
        Some(&database_url),
    )
    .await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1708;
    let legacy_issue = issue_store
        .record_issue_scheduled(
            &project_id,
            Some("Owner/Repo"),
            issue_number,
            "legacy-issue-task",
            &[],
            false,
        )
        .await?;
    let legacy_project = project_store
        .record_poll_started(&project_id, Some("Owner/Repo"))
        .await?;

    let coverage = check_github_issue_coverage(
        Some(&issue_store),
        None,
        &project_root,
        &project_id,
        REPO,
        issue_number,
        IsolationTrustClass::Trusted,
        None,
    )
    .await?;
    assert!(matches!(
        coverage,
        GitHubIssueCoverage::Covered {
            source: "issue_workflow",
            ..
        }
    ));
    let idle = project_store.record_idle(&project_id, Some(REPO)).await?;

    assert_eq!(idle.id, legacy_project.id);
    assert_eq!(project_store.row_count().await?, 1);
    assert_eq!(issue_store.row_count().await?, 1);
    assert_eq!(
        issue_store
            .get_by_issue(&project_id, Some(REPO), issue_number)
            .await?
            .map(|workflow| workflow.id),
        Some(legacy_issue.id)
    );
    Ok(())
}
