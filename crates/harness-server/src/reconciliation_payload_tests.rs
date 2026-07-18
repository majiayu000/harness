use super::*;

#[test]
fn reconciliation_payload_has_no_workspace_paths() {
    let report = ReconciliationReport {
        candidates: 3,
        skipped_terminal: 1,
        transitions: vec![ReconciliationTransition {
            task_id: "task-abc123".to_string(),
            from: "implementing".to_string(),
            to: "done".to_string(),
            reason: "PR merged".to_string(),
            applied: true,
        }],
        workflow_transitions: vec![WorkflowReconciliationTransition {
            workflow_id: "project::repo:owner/repo::issue:42".to_string(),
            from: "pr_open".to_string(),
            to: "done".to_string(),
            reason: "PR merged".to_string(),
            applied: true,
            repo: Some("owner/repo".to_string()),
            issue_number: Some(42),
            pr_number: Some(77),
            pr_url: Some("https://github.com/owner/repo/pull/77".to_string()),
        }],
        workflow_alerts: vec![],
    };
    let json = serde_json::to_string(&report).expect("ReconciliationReport must serialise to JSON");
    assert!(
        !json.contains("/workspaces/"),
        "ReconciliationReport JSON must not contain a workspace path, got: {json}"
    );
}
