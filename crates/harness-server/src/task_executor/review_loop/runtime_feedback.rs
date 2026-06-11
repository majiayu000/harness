pub(crate) async fn record_runtime_pr_feedback(
    issue_workflow_store: Option<&harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    workflow_runtime_store: Option<&harness_workflow::runtime::WorkflowRuntimeStore>,
    project_root: &Path,
    req: &CreateTaskRequest,
    task_id: &TaskId,
    pr_num: u64,
    pr_url: Option<&str>,
    outcome: PrFeedbackOutcome,
    summary: &str,
) {
    let issue_number = resolve_runtime_feedback_issue_number(
        issue_workflow_store,
        project_root,
        req.repo.as_deref(),
        req.issue,
        pr_num,
    )
    .await;

    crate::workflow_runtime_pr_feedback::record_pr_feedback(
        workflow_runtime_store,
        crate::workflow_runtime_pr_feedback::PrFeedbackRuntimeContext {
            project_root,
            repo: req.repo.as_deref(),
            issue_number,
            task_id,
            pr_number: pr_num,
            pr_url,
            outcome,
            summary,
        },
    )
    .await;
}

pub(crate) async fn record_runtime_pr_merged(
    issue_workflow_store: Option<&harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    workflow_runtime_store: Option<&harness_workflow::runtime::WorkflowRuntimeStore>,
    project_root: &Path,
    req: &CreateTaskRequest,
    task_id: &TaskId,
    pr_num: u64,
    pr_url: Option<&str>,
    summary: &str,
) {
    let project_id = project_root.to_string_lossy();
    if let Some(workflows) = issue_workflow_store {
        if let Err(error) = workflows
            .record_pr_merged(
                project_id.as_ref(),
                req.repo.as_deref(),
                pr_num,
                Some(summary),
            )
            .await
        {
            tracing::warn!(
                project = %project_id,
                repo = req.repo.as_deref().unwrap_or(""),
                pr = pr_num,
                "failed to record merged PR in issue workflow store: {error}"
            );
        }
    }

    let issue_number = resolve_runtime_feedback_issue_number(
        issue_workflow_store,
        project_root,
        req.repo.as_deref(),
        req.issue,
        pr_num,
    )
    .await;
    crate::workflow_runtime_pr_feedback::record_pr_merged(
        workflow_runtime_store,
        crate::workflow_runtime_pr_feedback::PrMergedRuntimeContext {
            project_root,
            repo: req.repo.as_deref(),
            issue_number,
            task_id,
            pr_number: pr_num,
            pr_url,
            summary,
        },
    )
    .await;
}

async fn resolve_runtime_feedback_issue_number(
    issue_workflow_store: Option<&harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    project_root: &Path,
    repo: Option<&str>,
    explicit_issue: Option<u64>,
    pr_num: u64,
) -> Option<u64> {
    if explicit_issue.is_some() {
        return explicit_issue;
    }

    let workflows = issue_workflow_store?;
    let project_id = project_root.to_string_lossy();
    match workflows.get_by_pr(project_id.as_ref(), repo, pr_num).await {
        Ok(Some(workflow)) => Some(workflow.issue_number),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                project = %project_id,
                repo = repo.unwrap_or(""),
                pr = pr_num,
                "failed to resolve runtime feedback issue number: {error}"
            );
            None
        }
    }
}
