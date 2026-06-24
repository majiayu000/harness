use super::IssueWorkflowStore;
use crate::issue_lifecycle::{
    IssueLifecycleEvent, IssueLifecycleEventKind, IssueMergeMethod, IssueMergePolicy,
    IssueWorkflowInstance,
};

impl IssueWorkflowStore {
    pub async fn record_issue_dependencies_detected(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        detail: Option<&str>,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            let mut event = IssueLifecycleEvent::new(IssueLifecycleEventKind::DependenciesDetected);
            if let Some(detail) = detail {
                event = event.with_detail(detail.to_string());
            }
            workflow.apply_event(event);
        })
        .await
    }

    pub async fn record_issue_remote_fact(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        issue_url: Option<&str>,
        labels_snapshot: &[String],
        fact_hash: &str,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            if let Some(issue_url) = issue_url {
                workflow.set_issue_url(Some(issue_url.to_string()));
            }
            workflow.labels_snapshot = labels_snapshot.to_vec();
            workflow.set_remote_fact_hash(Some(fact_hash.to_string()));
        })
        .await
    }

    pub async fn record_pr_remote_fact(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
        pr_head_sha: Option<&str>,
        fact_hash: &str,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
            if let Some(pr_head_sha) = pr_head_sha {
                workflow.set_pr_head_sha(Some(pr_head_sha.to_string()));
            }
            workflow.set_remote_fact_hash(Some(fact_hash.to_string()));
        })
        .await
    }

    pub async fn record_merge_policy_for_issue(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        merge_policy: Option<IssueMergePolicy>,
        merge_method: Option<IssueMergeMethod>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_existing_issue(project_id, repo, issue_number, |workflow| {
            workflow.set_merge_policy(merge_policy, merge_method);
        })
        .await
    }

    pub async fn record_merge_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
        task_id: &str,
        pr_head_sha: &str,
        fact_hash: Option<&str>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
            let mut event = IssueLifecycleEvent::new(IssueLifecycleEventKind::MergeStarted)
                .with_task_id(task_id.to_string())
                .with_pr_head_sha(pr_head_sha.to_string());
            if let Some(fact_hash) = fact_hash {
                event = event.with_remote_fact_hash(fact_hash.to_string());
            }
            workflow.apply_event(event);
        })
        .await
    }
}
