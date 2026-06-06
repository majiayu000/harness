use crate::model::{
    ChangedFileSnapshot, CheckState, EvalScenario, EvalTarget, MergeState, PrRepairEvalInput,
    PullRequestSnapshot, ReviewDecision, ReviewThreadSnapshot, ReviewerJudgment,
    RuntimeJobSnapshot, RuntimeSnapshot,
};
use serde_json::Value;

pub struct PrRepairEvalIngest<'a> {
    pub repo: &'a str,
    pub pr_number: u64,
    pub baseline_collected_at: &'a str,
    pub final_collected_at: &'a str,
    pub baseline: &'a Value,
    pub final_pr: &'a Value,
    pub submission: Option<&'a Value>,
    pub task_detail: Option<&'a Value>,
    pub reviewer_judgment: Option<ReviewerJudgment>,
}

pub fn github_pr_snapshot_from_value(
    repo: &str,
    collected_at: &str,
    value: &Value,
) -> PullRequestSnapshot {
    let review_threads = value
        .pointer("/reviewThreads/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|thread| {
            !thread
                .get("isResolved")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !thread
                    .get("isOutdated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
        .map(|thread| ReviewThreadSnapshot {
            id: string_field(thread, "id"),
            path: optional_string_field(thread, "path"),
            is_resolved: thread
                .get("isResolved")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_outdated: thread
                .get("isOutdated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect();

    let changed_files = value
        .pointer("/files/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|file| ChangedFileSnapshot {
            path: string_field(file, "path"),
            additions: u64_field(file, "additions"),
            deletions: u64_field(file, "deletions"),
            status: string_field(file, "changeType"),
        })
        .collect();

    PullRequestSnapshot {
        repo: repo.to_string(),
        pr_number: u64_field(value, "number"),
        url: optional_string_field(value, "url"),
        title: optional_string_field(value, "title"),
        base_ref: string_field(value, "baseRefName"),
        head_ref: string_field(value, "headRefName"),
        head_oid: string_field(value, "headRefOid"),
        is_draft: value
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        merge_state: merge_state_from_github(value.get("mergeStateStatus")),
        check_state: check_state_from_github(value.pointer("/statusCheckRollup/state")),
        review_decision: review_decision_from_github(value.get("reviewDecision")),
        active_unresolved_review_threads: review_threads,
        changed_files,
        collected_at: collected_at.to_string(),
    }
}

pub fn runtime_snapshot_from_values(
    submission: &Value,
    task_detail: &Value,
    collected_at: &str,
) -> Option<RuntimeSnapshot> {
    let task_id = optional_string_field(submission, "task_id")
        .or_else(|| optional_string_field(submission, "submission_id"))
        .or_else(|| optional_string_field(task_detail, "id"));
    let workflow_id = optional_string_field(submission, "workflow_id")
        .or_else(|| optional_string_at_pointer(task_detail, "/workflow/id"));
    let terminal_state = terminal_string_field(task_detail, "status")
        .or_else(|| terminal_string_at_pointer(task_detail, "/workflow/state"))
        .or_else(|| terminal_string_field(submission, "status"));
    let workflow_state = optional_string_at_pointer(task_detail, "/workflow/state");
    let latest_activity = optional_string_field(task_detail, "latest_activity")
        .or_else(|| optional_string_at_pointer(task_detail, "/workflow/latest_activity"));
    let runtime_jobs = runtime_jobs_from_value(task_detail);

    if task_id.is_none()
        && workflow_id.is_none()
        && terminal_state.is_none()
        && workflow_state.is_none()
        && runtime_jobs.is_empty()
    {
        return None;
    }

    Some(RuntimeSnapshot {
        task_id,
        workflow_id,
        workflow_state,
        runtime_jobs,
        latest_activity,
        terminal_state,
        collected_at: collected_at.to_string(),
    })
}

pub fn pr_repair_eval_input_from_values(input: PrRepairEvalIngest<'_>) -> PrRepairEvalInput {
    let baseline_pr =
        github_pr_snapshot_from_value(input.repo, input.baseline_collected_at, input.baseline);
    let final_pr_snapshot =
        github_pr_snapshot_from_value(input.repo, input.final_collected_at, input.final_pr);
    let scenario = scenario_from_baseline(&baseline_pr);
    let runtime = match (input.submission, input.task_detail) {
        (Some(submission), Some(task_detail)) => {
            runtime_snapshot_from_values(submission, task_detail, input.final_collected_at)
        }
        _ => None,
    };

    PrRepairEvalInput {
        scenario,
        target: EvalTarget::PullRequest {
            repo: input.repo.to_string(),
            pr_number: input.pr_number,
            base_ref: Some(baseline_pr.base_ref.clone()),
            head_ref: Some(baseline_pr.head_ref.clone()),
        },
        final_evidence_head_oid: Some(final_pr_snapshot.head_oid.clone()),
        baseline_pr,
        final_pr: final_pr_snapshot,
        runtime,
        usage: Vec::new(),
        reviewer_judgment: input.reviewer_judgment,
        created_unrelated_pr: false,
        scope_violations: Vec::new(),
    }
}

fn scenario_from_baseline(snapshot: &PullRequestSnapshot) -> EvalScenario {
    if !snapshot.is_draft
        && review_decision_allows_noop(snapshot.review_decision.as_ref())
        && snapshot.active_unresolved_review_threads.is_empty()
        && snapshot.check_state == CheckState::Passing
        && snapshot.merge_state == MergeState::Clean
    {
        EvalScenario::ReadyNoopControl
    } else {
        EvalScenario::PrRepair
    }
}

fn review_decision_allows_noop(review_decision: Option<&ReviewDecision>) -> bool {
    !matches!(
        review_decision,
        Some(ReviewDecision::ChangesRequested | ReviewDecision::ReviewRequired)
    )
}

fn runtime_jobs_from_value(value: &Value) -> Vec<RuntimeJobSnapshot> {
    let Some(jobs) = value
        .get("runtime_jobs")
        .or_else(|| value.get("runtimeJobs"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    jobs.iter()
        .map(|job| RuntimeJobSnapshot {
            runtime_job_id: string_field(job, "runtime_job_id")
                .or_else_empty(|| string_field(job, "runtimeJobId"))
                .or_else_empty(|| string_field(job, "id")),
            state: string_field(job, "state").or_else_empty(|| string_field(job, "status")),
            artifact_count: job
                .get("artifact_count")
                .or_else(|| job.get("artifactCount"))
                .and_then(json_u64)
                .unwrap_or(0),
            terminal_state: terminal_string_field(job, "terminal_state")
                .or_else(|| terminal_string_field(job, "terminalState"))
                .or_else(|| terminal_string_field(job, "status")),
        })
        .collect()
}

fn merge_state_from_github(value: Option<&Value>) -> MergeState {
    match value.and_then(Value::as_str).unwrap_or_default() {
        "CLEAN" => MergeState::Clean,
        "DIRTY" => MergeState::Dirty,
        "BLOCKED" => MergeState::Blocked,
        "BEHIND" => MergeState::Behind,
        _ => MergeState::Unknown,
    }
}

fn check_state_from_github(value: Option<&Value>) -> CheckState {
    match value.and_then(Value::as_str).unwrap_or_default() {
        "SUCCESS" => CheckState::Passing,
        "PENDING" | "EXPECTED" => CheckState::Pending,
        "FAILURE" | "ERROR" => CheckState::Failing,
        _ => CheckState::Unknown,
    }
}

fn review_decision_from_github(value: Option<&Value>) -> Option<ReviewDecision> {
    match value.and_then(Value::as_str).unwrap_or_default() {
        "APPROVED" => Some(ReviewDecision::Approved),
        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
        _ => None,
    }
}

fn optional_string_at_pointer(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(json_string)
}

fn optional_string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(json_string)
}

fn terminal_string_at_pointer(value: &Value, pointer: &str) -> Option<String> {
    optional_string_at_pointer(value, pointer).filter(|state| is_terminal_state(state))
}

fn terminal_string_field(value: &Value, field: &str) -> Option<String> {
    optional_string_field(value, field).filter(|state| is_terminal_state(state))
}

fn is_terminal_state(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "done"
            | "completed"
            | "succeeded"
            | "success"
            | "passed"
            | "ready_to_merge"
            | "blocked"
            | "failed"
            | "cancelled"
            | "canceled"
            | "timed_out"
            | "timeout"
            | "errored"
            | "error"
    )
}

fn string_field(value: &Value, field: &str) -> String {
    optional_string_field(value, field).unwrap_or_default()
}

fn u64_field(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(json_u64).unwrap_or(0)
}

fn json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
}

trait NonEmptyStringExt {
    fn or_else_empty<F>(self, fallback: F) -> String
    where
        F: FnOnce() -> String;
}

impl NonEmptyStringExt for String {
    fn or_else_empty<F>(self, fallback: F) -> String
    where
        F: FnOnce() -> String,
    {
        if self.trim().is_empty() {
            fallback()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_github_pr_snapshot_to_eval_model() {
        let snapshot = github_pr_snapshot_from_value(
            "owner/repo",
            "2026-06-06T00:00:00Z",
            &json!({
                "number": 7,
                "url": "https://github.com/owner/repo/pull/7",
                "title": "Fix CI",
                "headRefName": "fix/ci",
                "headRefOid": "abc123",
                "baseRefName": "main",
                "isDraft": false,
                "mergeStateStatus": "CLEAN",
                "reviewDecision": "APPROVED",
                "statusCheckRollup": {"state": "SUCCESS"},
                "reviewThreads": {
                    "nodes": [
                        {"id": "active", "path": "src/lib.rs", "isResolved": false, "isOutdated": false},
                        {"id": "resolved", "path": "src/main.rs", "isResolved": true, "isOutdated": false},
                        {"id": "old", "path": "src/old.rs", "isResolved": false, "isOutdated": true}
                    ]
                },
                "files": {
                    "nodes": [
                        {"path": "src/lib.rs", "additions": 3, "deletions": 1, "changeType": "MODIFIED"}
                    ]
                }
            }),
        );

        assert_eq!(snapshot.repo, "owner/repo");
        assert_eq!(snapshot.pr_number, 7);
        assert_eq!(snapshot.check_state, CheckState::Passing);
        assert_eq!(snapshot.merge_state, MergeState::Clean);
        assert_eq!(snapshot.active_unresolved_review_threads.len(), 1);
        assert_eq!(snapshot.changed_files.len(), 1);
    }

    #[test]
    fn builds_ready_noop_input_from_clean_baseline() {
        let pr = json!({
            "number": 7,
            "headRefName": "ready",
            "headRefOid": "same",
            "baseRefName": "main",
            "isDraft": false,
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": {"state": "SUCCESS"},
            "reviewThreads": {"nodes": []},
            "files": {"nodes": []}
        });

        let input = pr_repair_eval_input_from_values(PrRepairEvalIngest {
            repo: "owner/repo",
            pr_number: 7,
            baseline_collected_at: "2026-06-06T00:00:00Z",
            final_collected_at: "2026-06-06T00:01:00Z",
            baseline: &pr,
            final_pr: &pr,
            submission: None,
            task_detail: None,
            reviewer_judgment: None,
        });

        assert_eq!(input.scenario, EvalScenario::ReadyNoopControl);
        assert!(input.runtime.is_none());
    }

    #[test]
    fn review_required_baseline_is_pr_repair() {
        let pr = json!({
            "number": 7,
            "url": "https://github.com/owner/repo/pull/7",
            "title": "Example",
            "baseRefName": "main",
            "headRefName": "feature",
            "headRefOid": "abc123",
            "isDraft": false,
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": {"state": "SUCCESS"},
            "reviewDecision": "CHANGES_REQUESTED",
            "reviewThreads": {"nodes": []},
            "files": {"nodes": []}
        });

        let input = pr_repair_eval_input_from_values(PrRepairEvalIngest {
            repo: "owner/repo",
            pr_number: 7,
            baseline_collected_at: "2026-06-06T00:00:00Z",
            final_collected_at: "2026-06-06T00:01:00Z",
            baseline: &pr,
            final_pr: &pr,
            submission: None,
            task_detail: None,
            reviewer_judgment: None,
        });

        assert_eq!(input.scenario, EvalScenario::PrRepair);
    }

    #[test]
    fn preserves_reviewer_judgment_from_ingest() {
        let pr = json!({
            "number": 7,
            "headRefName": "feature",
            "headRefOid": "abc123",
            "baseRefName": "main",
            "isDraft": false,
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": {"state": "SUCCESS"},
            "reviewThreads": {"nodes": []},
            "files": {"nodes": []}
        });
        let judgment = ReviewerJudgment {
            reviewer_kind: crate::model::ReviewerKind::Llm,
            judged_head_oid: "abc123".to_string(),
            code_quality_score: 92,
            trajectory_score: 88,
            findings: Vec::new(),
            residual_risks: Vec::new(),
        };

        let input = pr_repair_eval_input_from_values(PrRepairEvalIngest {
            repo: "owner/repo",
            pr_number: 7,
            baseline_collected_at: "2026-06-06T00:00:00Z",
            final_collected_at: "2026-06-06T00:01:00Z",
            baseline: &pr,
            final_pr: &pr,
            submission: None,
            task_detail: None,
            reviewer_judgment: Some(judgment.clone()),
        });

        assert_eq!(input.reviewer_judgment, Some(judgment));
    }

    #[test]
    fn draft_baseline_is_pr_repair() {
        let pr = json!({
            "number": 7,
            "url": "https://github.com/owner/repo/pull/7",
            "title": "Example",
            "baseRefName": "main",
            "headRefName": "feature",
            "headRefOid": "abc123",
            "isDraft": true,
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": {"state": "SUCCESS"},
            "reviewDecision": "APPROVED",
            "reviewThreads": {"nodes": []},
            "files": {"nodes": []}
        });

        let input = pr_repair_eval_input_from_values(PrRepairEvalIngest {
            repo: "owner/repo",
            pr_number: 7,
            baseline_collected_at: "2026-06-06T00:00:00Z",
            final_collected_at: "2026-06-06T00:01:00Z",
            baseline: &pr,
            final_pr: &pr,
            submission: None,
            task_detail: None,
            reviewer_judgment: None,
        });

        assert_eq!(input.scenario, EvalScenario::PrRepair);
    }

    #[test]
    fn running_runtime_status_is_not_terminal_evidence() {
        let submission = json!({
            "task_id": "task-1",
            "workflow_id": "workflow-1",
            "status": "implementing"
        });
        let task_detail = json!({
            "id": "task-1",
            "status": "running",
            "workflow": {
                "id": "workflow-1",
                "state": "addressing_feedback",
                "latest_activity": "implementing"
            },
            "runtime_jobs": [
                {
                    "id": "job-1",
                    "state": "running",
                    "status": "running",
                    "artifact_count": 0
                }
            ]
        });

        let Some(snapshot) =
            runtime_snapshot_from_values(&submission, &task_detail, "2026-06-06T00:01:00Z")
        else {
            panic!("runtime identity should still produce a runtime snapshot");
        };

        assert_eq!(snapshot.task_id.as_deref(), Some("task-1"));
        assert_eq!(snapshot.workflow_id.as_deref(), Some("workflow-1"));
        assert_eq!(
            snapshot.workflow_state.as_deref(),
            Some("addressing_feedback")
        );
        assert_eq!(snapshot.terminal_state, None);
        assert_eq!(snapshot.runtime_jobs.len(), 1);
        assert_eq!(snapshot.runtime_jobs[0].terminal_state, None);
    }

    #[test]
    fn pr_repair_terminal_workflow_states_are_runtime_evidence() {
        for state in ["ready_to_merge", "blocked"] {
            let submission = json!({
                "task_id": "task-1",
                "workflow_id": "workflow-1"
            });
            let task_detail = json!({
                "id": "task-1",
                "status": state,
                "workflow": {
                    "id": "workflow-1",
                    "state": state
                }
            });

            let Some(snapshot) =
                runtime_snapshot_from_values(&submission, &task_detail, "2026-06-06T00:01:00Z")
            else {
                panic!("runtime identity should produce a runtime snapshot");
            };

            assert_eq!(snapshot.terminal_state.as_deref(), Some(state));
        }
    }
}
