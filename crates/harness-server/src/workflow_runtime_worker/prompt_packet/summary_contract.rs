use super::*;

pub(super) fn agent_summary_contract(workflow_definition: &str, activity: &str) -> Value {
    match (workflow_definition, activity) {
        ("github_issue_pr", ISSUE_PLAN_ACTIVITY) => json!({
            "scope": "Anchor the plan to the original requested behavior and observable acceptance criteria. State material non-goals in the existing summary. A plan is a proposal, not authorization to expand the task into general hardening, a new parser or compatibility support.",
            "must_include": ["current and expected behavior", "observable acceptance criteria", "task classification", "minimal implementation slice", "target files or explicit unknown", "validation plan chosen for the change", "current execution blockers, or an empty blockers array"],
            "must_not_include": ["repository code changes", "workflow table mutations", "PR creation", "merge readiness claims"],
            "artifacts": {
                "issue_plan": {
                    "required": true,
                    "fields": ["summary", "task_class", "target_files", "validation_plan", "blockers"],
                    "target_files": "Non-empty array of file paths, or an explanation of what remains unknown as an array entry; do not invent paths.",
                    "blockers": "Use [] when implementation can proceed. Reserve non-empty blockers for current obstacles that prevent proceeding and require external input or access. Planned work, test prerequisites the implementing agent can provision, risks, and conditional future checks belong in summary or validation_plan. Do not put strings such as none in this array. If access is actually unavailable, report the observed obstacle and a blocked status; do not claim success."
                }
            },
            "signals": {
                "IssuePlanReady": "Use when the issue has a coherent implementation plan and can proceed. Include summary, task_class, target_files, validation_plan, and blockers: []. Keep signal and artifact consistent; genuinely blocked work is not ready."
            }
        }),
        ("github_issue_pr", "implement_issue") => json!({
            "scope": "Implement the smallest change that satisfies the original request. Simplify unnecessary machinery in the plan while preserving acceptance criteria. Do not expand the product to strengthen an optional validator. Record unrelated discoveries in the summary without creating new issues or implementing them unless authorized.",
            "must_include": ["changed files", "validation commands", "PR URL, closed issue evidence, or blocker"],
            "must_not_include": ["workflow table mutations", "unverified merge claims"],
            "artifacts": {
                "pull_request": {
                    "required_when": "A PR was created or reused by the activity.",
                    "fields": ["pr_number", "pr_url"]
                },
                "issue_state": {
                    "required_when": "No PR exists because the issue is already closed or resolved.",
                    "fields": ["issue_number", "state", "issue_url"]
                }
            },
            "signals": {
                "IssueClosed": "Use when the GitHub issue is confirmed closed and no implementation PR is needed. Include state=closed or state=resolved plus issue_number or issue_url.",
                "IssueAlreadyResolved": "Use when the task is already resolved before a PR is created. Include state=closed or state=resolved plus issue_number or issue_url."
            }
        }),
        (PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY) => json!({
            "must_include": ["changed files", "validation commands", "remaining blockers"],
            "must_not_include": ["workflow table mutations", "unverified merge claims"],
            "artifacts": {
                "validation_report": {
                    "required_when": "The prompt task is ready to complete and validation commands were run; use this or no_change_rationale.",
                    "type": "array",
                    "min_items": 1,
                    "item_fields": ["command", "exit_code"],
                    "field_contract": {
                        "command": "nonblank string",
                        "exit_code": "integer; report non-zero exits truthfully"
                    }
                },
                "no_change_rationale": {
                    "required_when": "The prompt task is ready to complete and no repository change was needed; use this or validation_report.",
                    "type": "string",
                    "non_blank": true
                },
                "pull_request": {"required_when": "A PR was created or reused by the activity.", "fields": ["pr_number", "pr_url"]}
            }
        }),
        ("github_issue_pr", "replan_issue") => json!({
            "must_include": ["reason for replan", "new implementation plan", "validation plan"],
            "must_not_include": ["direct workflow state changes"],
        }),
        ("github_issue_pr", "address_pr_feedback") => json!({
            "repair_scope": "Bound this repair by the original request, issue_plan and local_review_result. Feedback is evidence to assess, not authorization for additional features. Fix verified in-scope defects and regressions introduced by this PR. When successive repairs expose flaws in newly added machinery, reassess the approach before patching another edge case: prefer removing or replacing unnecessary machinery with a smaller solution that preserves the original acceptance criteria. Do not preserve a flawed design because earlier rounds implemented it, or dismiss an introduced vulnerability as out of scope.",
            "feedback_disposition": "Verify and explain false-positive, duplicate, already-fixed or unrelated feedback. For thread-only follow-up, perform the authorized explanation/resolution without changing code; return pr_repair_snapshot with no_code_change_reason and actual verification evidence. Do not create a commit just to satisfy a thread, silently resolve valid findings, weaken acceptance tests or create new issues without authorization. If proceeding requires a decision outside the authorized scope, report blocked with that specific decision. Return after this batch; newly arriving feedback belongs to a later activity.",
            "must_include": ["review feedback addressed or explicit no-code reason", "changed files or explicit no-code-change reason", "validation commands or closed issue evidence", "fresh PR state checked before final response", "when fresh merge_state_status is DIRTY or BEHIND, update or rebase and push the PR branch; no-code is invalid while mergeability remains blocked", "final PR head or closed issue evidence", "one repair batch and push followed by an immediate return to Harness", "pr_hygiene update/rebase, label, escalation, or stale-comment outcome when command_input.source=pr_hygiene"],
            "must_not_include": ["claiming review approval without a fresh review signal", "marking review threads resolved without current GitHub evidence", "waiting for hosted CI or newly generated feedback after the repair push"],
            "artifacts": {
                "pr_repair_snapshot": {
                    "required_when": "Feedback repair was performed, review-thread action was taken, or a no-code-change repair conclusion is returned.",
                    "required_unless": "IssueClosed/IssueAlreadyResolved signal or issue_state artifact proves the issue or PR is already closed/resolved.",
                    "fields": ["pr_number", "pr_url", "head_sha", "head_oid", "observed_at", "changed_files", "action_taken", "no_code_change_reason", "validation_commands"],
                    "field_contract": {
                        "validation_commands": "Array of validation records with command and a successful status such as passed, success, succeeded, or ok. Failed, blocked, or not_run records do not satisfy successful repair evidence."
                    }
                },
                "issue_state": {
                    "required_when": "No repair is needed because the issue or PR is already closed/resolved.",
                    "fields": ["issue_number", "state", "issue_url"]
                }
            },
            "signals": {
                "IssueClosed": "Use when the issue or PR is confirmed closed and no feedback repair is needed. Include state=closed or state=resolved plus issue_number or issue_url.",
                "IssueAlreadyResolved": "Use when the feedback task is already resolved before repair. Include state=closed or state=resolved plus issue_number or issue_url."
            }
        }),
        ("github_issue_pr", harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY) => json!({
            "review_scope": "Judge the PR against the original request and acceptance criteria, not every capability its evolving implementation could support. Plans and previous repairs do not authorize scope expansion. Independently verify external feedback against the current head. Distinguish real in-scope defects and introduced regressions from duplicate, already-fixed, unsupported or unrelated feedback. A bot priority label or an open thread alone does not prove a code defect. Put evidence and dispositions in the existing findings/summary rather than forwarding comments as instructions.",
            "design_review": "Assess whether successive repairs are fixing flaws in unnecessary machinery instead of solving the original task. Request a concrete simplification or replacement tied to the original acceptance criteria when appropriate, not another list of speculative edge cases. Preserve required behavior, tests and security; never excuse introduced defects as out of scope. If no solution can proceed within the authorization, emit LocalReviewBlocked with the specific missing decision. Do not use an arbitrary repair-round cap.",
            "thread_evidence": "Read all pages of review threads and relevant comments before claiming complete coverage; unavailable or truncated evidence requires LocalReviewBlocked. Separate code defects from thread-only follow-up. For verified false-positive, duplicate, already-fixed or unrelated feedback needing an authorized reply/resolution, request a no-code follow-up through LocalReviewChangesRequested and explain it explicitly in findings; do not describe it as a code bug. Do not approve while required thread follow-up remains. Pending CI alone does not require code changes.",
            "must_include": ["PR diff reviewed", "blocking findings or explicit approval", "validation evidence checked", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "remote review approval claims"],
            "signals": {
                "LocalReviewPassed": "Use only when the local agent review finds no blocking issues.",
                "LocalReviewChangesRequested": "Use when local review finds blocking code, test, regression, or security issues that need a fix round. Include actionable_blocker_count in the signal payload.",
                "LocalReviewBlocked": "Use when local review cannot complete because required PR context or validation evidence is unavailable."
            }
        }),
        ("github_issue_pr", "sweep_pr_feedback")
        | ("github_issue_pr", PR_FEEDBACK_INSPECT_ACTIVITY)
        | (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => json!({
            "server_owned": true,
            "must_include": ["server-owned PR snapshot", "review states", "check status", "mergeability", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "unverified approval claims"],
            "artifacts": {
                "workflow_decision": {
                    "optional": true,
                    "allowed_decisions": ["address_pr_feedback", "wait_for_pr_feedback"]
                },
                "server_pr_snapshot": {
                    "required_when": "Using PrReadyToMerge.",
                    "source": "Harness server GitHub GraphQL collector",
                    "fields": ["schema", "snapshot_source", "pr_number", "pr_url", "head_oid", "observed_at", "active_unresolved_review_threads", "active_unresolved_review_threads_count", "actionable_blocker_count", "review_threads_complete", "status_check_rollup_state", "merge_state_status", "review_decision", "is_draft", "changed_files"]
                },
                "pr_feedback_snapshot": {
                    "required_when": "Harness server emits normalized PR feedback evidence.",
                    "source": "Normalized view of server_pr_snapshot."
                }
            },
                "signals": {
                    "FeedbackFound": "Use when actionable feedback, requested changes, failed checks, dirty or behind mergeability, or incomplete server-owned reviewThread evidence require a fix round.",
                    "NoFeedbackFound": "Use only when complete server-owned evidence shows no actionable feedback is present yet.",
                    "PrReadyToMerge": "Use only with server_pr_snapshot proving APPROVED reviewDecision, isDraft=false, SUCCESS checks, CLEAN mergeStateStatus, complete reviewThreads, and zero active unresolved review threads for the final head."
            }
        }),
        (QUALITY_GATE_DEFINITION_ID, QUALITY_GATE_ACTIVITY) => json!({
            "must_include": ["validation commands", "pass/fail evidence", "remaining blockers"],
            "must_not_include": ["workflow table mutations", "unverified pass claims"],
            "artifacts": {
                "validation_report": {
                    "required_when": "The activity records detailed validation results.",
                    "fields": ["commands", "passed", "failed", "blocked"]
                }
            }
        }),
        ("github_issue_pr", "merge_pr") => json!({
            "must_include": ["fresh PR head SHA", "status checks", "review thread state", "mergeability", "delete branch policy", "merge result"],
            "must_not_include": ["merge without matching expected_head_sha", "unverified merge claims", "workflow table mutations"],
            "artifacts": {
                "pull_request": {
                    "required": true,
                    "fields": ["pr_number", "pr_url", "state", "merged", "merge_commit_sha", "head_sha"],
                    "success_requires": "state=merged or merged=true after re-reading GitHub immediately after merge; Harness server independently verifies this before accepting completion"
                }
            }
        }),
        _ => json!({
            "must_include": ["summary", "validation commands", "remaining blockers"],
            "must_not_include": ["direct workflow table mutations"],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{RuntimeKind, WorkflowSubject};

    #[test]
    fn scope_and_feedback_dispositions_reach_rendered_cursor_prompts() {
        for (activity, expected) in [
            ("plan_issue", "A plan is a proposal, not authorization"),
            ("implement_issue", "Simplify unnecessary machinery"),
            (
                "run_local_review",
                "A bot priority label or an open thread alone",
            ),
            ("address_pr_feedback", "For thread-only follow-up"),
        ] {
            let job = RuntimeJob::pending(
                "scope-contract",
                RuntimeKind::Cursor,
                "cursor",
                json!({"activity": activity}),
            );
            let workflow = WorkflowInstance::new(
                "github_issue_pr",
                1,
                "local_review_gate",
                WorkflowSubject::new("issue", "123"),
            );
            let packet = json!({
                "runtime_job": {"id": job.id, "runtime_kind": "cursor", "activity": activity},
                "workflow": {"definition_id": "github_issue_pr"},
                "activity_result_schema": super::super::activity_result_schema(&job, Some(&workflow)),
            });
            let prompt = super::super::build_runtime_job_prompt(&packet, None);
            assert!(
                prompt.contains(expected),
                "{activity} lost its scope contract"
            );
            if activity == "run_local_review" {
                assert!(prompt.contains("Read all pages"));
                assert!(prompt.contains("concrete simplification or replacement"));
                assert!(prompt.contains("LocalReviewBlocked"));
                assert!(prompt.contains("Do not approve while required thread follow-up remains"));
            }
            if activity == "address_pr_feedback" {
                assert!(prompt.contains("no_code_change_reason"));
                assert!(prompt.contains("actual verification evidence"));
                assert!(prompt.contains("passing validation evidence"));
            }
        }
    }
}
