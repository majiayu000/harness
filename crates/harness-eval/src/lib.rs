pub mod benchmark;
pub mod ingest;
pub mod model;
pub mod scoring;

pub use benchmark::*;
pub use ingest::*;
pub use model::*;
pub use scoring::{score_pr_repair_eval, ScoringError};

#[cfg(test)]
mod usage_probe_tests {
    use super::*;
    use serde_json::json;

    fn harness_eval_count() -> u64 {
        harness_core::usage_probe::snapshot()
            .into_iter()
            .find(|entry| entry.surface == "harness_eval")
            .map(|entry| entry.count)
            .unwrap_or(0)
    }

    #[test]
    fn ingest_public_entrypoints_record_harness_eval_usage() {
        let before = harness_eval_count();
        let pr = json!({
            "number": 7,
            "baseRefName": "main",
            "headRefName": "feature",
            "headRefOid": "abc123",
            "isDraft": false,
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": {"state": "SUCCESS"},
            "reviewThreads": {"nodes": []},
            "files": {"nodes": []}
        });
        let submission = json!({"task_id": "task-1", "workflow_id": "workflow-1"});
        let task_detail = json!({"id": "task-1", "status": "done"});

        let pr_snapshot = github_pr_snapshot_from_value("owner/repo", "2026-07-04T00:00:00Z", &pr);
        let runtime_snapshot =
            runtime_snapshot_from_values(&submission, &task_detail, "2026-07-04T00:00:00Z");
        let eval_input = pr_repair_eval_input_from_values(PrRepairEvalIngest {
            repo: "owner/repo",
            pr_number: 7,
            baseline_collected_at: "2026-07-04T00:00:00Z",
            final_collected_at: "2026-07-04T00:01:00Z",
            baseline: &pr,
            final_pr: &pr,
            run_mode: EvalRunMode::LiveRun,
            submission: Some(&submission),
            task_detail: Some(&task_detail),
            reviewer_judgment: None,
        });

        assert_eq!(pr_snapshot.pr_number, 7);
        assert!(runtime_snapshot.is_some());
        assert_eq!(eval_input.final_pr.pr_number, 7);
        assert!(harness_eval_count() >= before + 3);
    }
}
