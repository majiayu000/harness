use super::*;

#[test]
fn intake_trust_issue_submission_data_records_author_trust_class() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("intake-trust");
    let labels = vec!["harness".to_string()];
    let ctx = IssueSubmissionRuntimeContext {
        project_root: &project_root,
        repo: Some("owner/repo"),
        issue_number: 42,
        task_id: &task_id,
        labels: &labels,
        force_execute: true,
        additional_prompt: None,
        depends_on: &[],
        dependencies_blocked: false,
        source: Some("github"),
        external_id: Some("issue:42"),
        remote_fact_hash: None,
        author_trust_class: Some(IsolationTrustClass::NonCollaborator),
    };

    let data = issue_submission_data(&ctx, &project_root.to_string_lossy(), &json!({}), None);

    assert_eq!(
        data["author_trust_class"],
        json!(IsolationTrustClass::NonCollaborator)
    );
    Ok(())
}

#[test]
fn issue_submission_data_preserves_the_definition_pin() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("definition-pin");
    let labels = Vec::new();
    let ctx = IssueSubmissionRuntimeContext {
        project_root: &project_root,
        repo: Some("owner/repo"),
        issue_number: 42,
        task_id: &task_id,
        labels: &labels,
        force_execute: false,
        additional_prompt: None,
        depends_on: &[],
        dependencies_blocked: false,
        source: Some("github"),
        external_id: Some("issue:42"),
        remote_fact_hash: None,
        author_trust_class: None,
    };
    let definition_hash = github_issue_pr_definition_hash();

    let data = issue_submission_data(
        &ctx,
        &project_root.to_string_lossy(),
        &json!({"definition_hash": definition_hash}),
        None,
    );

    assert_eq!(data["definition_hash"], definition_hash);
    Ok(())
}
