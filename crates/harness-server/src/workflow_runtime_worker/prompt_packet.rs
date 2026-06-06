use harness_core::config::workflow::WorkflowDocument;
use harness_workflow::runtime::{
    ActivityArtifact, DecisionValidator, RuntimeJob, RuntimeProfile, WorkflowInstance,
    ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL, ISSUE_STATE_ARTIFACT,
    PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY, PR_FEEDBACK_SNAPSHOT_ARTIFACT, QUALITY_BLOCKED_SIGNAL,
    QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID,
    QUALITY_PASSED_SIGNAL, SERVER_PR_SNAPSHOT_ARTIFACT,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

use super::activity_contract::activity_contract;
use super::data_helpers::activity_name;

pub(super) fn build_runtime_prompt_packet(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    project_root: &Path,
    source_project_root: &Path,
    runtime_profile: &RuntimeProfile,
    workflow_document: &WorkflowDocument,
) -> Value {
    json!({
        "schema": "harness.runtime.prompt_packet.v1",
        "runtime_job": {
            "id": job.id,
            "command_id": job.command_id,
            "runtime_kind": job.runtime_kind,
            "runtime_profile": job.runtime_profile,
            "activity": activity_name(job),
        },
        "runtime_profile": runtime_profile,
        "project": {
            "root": project_root.display().to_string(),
            "source_root": source_project_root.display().to_string(),
            "repo": workflow
                .and_then(|workflow| workflow.data.get("repo"))
                .and_then(Value::as_str)
                .or_else(|| job.input.get("repo").and_then(Value::as_str)),
        },
        "workflow": workflow.map(|workflow| {
            json!({
                "id": workflow.id,
                "definition_id": workflow.definition_id,
                "definition_version": workflow.definition_version,
                "state": workflow.state,
                "version": workflow.version,
                "subject": workflow.subject,
                "parent_workflow_id": workflow.parent_workflow_id,
                "data": workflow.data,
            })
        }),
        "workflow_file": {
            "source_path": &workflow_document.source_path,
            "config": &workflow_document.config,
            "prompt_template": &workflow_document.prompt_template,
        },
        "command_input": job.input,
        "runtime_contract": {
            "orchestration_source": "workflow_database",
            "agent_must_not_edit_workflow_tables": true,
            "agent_executes_repository_and_github_work": true,
            "follow_project_instructions": true,
        },
        "activity_result_schema": activity_result_schema(job, workflow),
        "required_structured_output": {
            "summary": "Concise final activity summary.",
            "changed_files": "Files changed by this runtime activity, if any.",
            "validation_commands": "Validation commands run and their results.",
            "remaining_blockers": "Any blockers that still require follow-up.",
        },
    })
}

pub(super) fn build_runtime_job_prompt(
    prompt_packet: &Value,
    prompt_task_request: Option<&str>,
) -> String {
    let prompt_packet_json = pretty_json(prompt_packet);
    let activity = prompt_packet
        .get("runtime_job")
        .and_then(|runtime_job| runtime_job.get("activity"))
        .and_then(Value::as_str)
        .unwrap_or("workflow_activity");
    let project_root = prompt_packet
        .get("project")
        .and_then(|project| project.get("root"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let runtime_profile = prompt_packet
        .get("runtime_job")
        .and_then(|runtime_job| runtime_job.get("runtime_profile"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let job_id = prompt_packet
        .get("runtime_job")
        .and_then(|runtime_job| runtime_job.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut prompt = format!(
        "You are executing a Harness workflow runtime job.\n\n\
         Runtime contract:\n\
         - Treat the workflow database as the source of orchestration state, but do not edit workflow tables directly.\n\
         - Harness server only manages lifecycle. You, the agent, perform repository and GitHub work when the activity requires it.\n\
         - Follow the project instructions loaded by the runtime.\n\
         - Use the prompt packet activity_result_schema to shape your final summary.\n\
         - When returning structured activity output, put a JSON object in a final fenced `harness-activity-result` block matching activity_result_schema.\n\
         - The structured result activity field must match this runtime job activity exactly.\n\
         - Return a concise final summary appropriate to the activity. Include changed files and validation commands only when repository code changes were requested; for discovery and planning activities, report inspected inputs, emitted signals, and remaining blockers.\n\n\
         Project root: {project_root}\n\
         Runtime job id: {job_id}\n\
         Runtime profile: {runtime_profile}\n\
         Activity: {activity}\n\n\
         Prompt packet:\n{prompt_packet_json}\n",
    );
    if let Some(prompt_task_request) = prompt_task_request {
        prompt.push_str("\nPrompt task request:\n");
        prompt.push_str(prompt_task_request);
        prompt.push('\n');
    }
    if let Some(template) = prompt_packet
        .get("workflow_file")
        .and_then(|workflow_file| workflow_file.get("prompt_template"))
        .and_then(Value::as_str)
        .filter(|template| !template.trim().is_empty())
    {
        prompt.push_str("\nRepository workflow prompt template:\n");
        prompt.push_str(template);
        prompt.push('\n');
    }
    prompt
}

pub(super) fn activity_result_schema(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Value {
    let activity = activity_name(job);
    let workflow_definition = workflow
        .map(|workflow| workflow.definition_id.as_str())
        .unwrap_or("unknown");
    let activity_contract = activity_contract(workflow_definition, &activity);
    let transition_contract = activity_transition_contract(workflow_definition, &activity);
    let summary_contract = agent_summary_contract(workflow_definition, &activity);
    let decision_contract = workflow_decision_contract(workflow);
    let command_examples = workflow_decision_command_examples(workflow_definition, &activity);
    json!({
        "schema": "harness.runtime.activity_result.v1",
        "activity": activity,
        "workflow_definition": workflow_definition,
        "activity_contract": activity_contract.to_prompt_value(),
        "result_type": "ActivityResult",
        "required_fields": ["activity", "status", "summary"],
        "optional_fields": ["artifacts", "signals", "validation", "error", "error_kind"],
        "allowed_statuses": ["succeeded", "failed", "blocked", "cancelled"],
        "allowed_error_kinds": ["retryable", "timeout", "fatal", "configuration", "external_dependency", "unknown"],
        "optional_artifacts": {
            "workflow_decision": {
                "description": "A proposed WorkflowDecision. Harness validates it before applying any transition or command.",
                "required_fields": ["workflow_id", "observed_state", "decision", "next_state", "reason", "confidence"],
                "allowed_confidence": ["low", "medium", "high"]
            }
        },
        "status_contract": {
            "succeeded": "The activity completed and its output is ready for the workflow reducer.",
            "failed": "The activity hit an execution error. Use error_kind=fatal or configuration when retry would not help.",
            "blocked": "The activity cannot proceed without external input or budget.",
            "cancelled": "The activity was intentionally stopped.",
        },
        "transition_contract": transition_contract,
        "workflow_decision_contract": decision_contract,
        "agent_summary_contract": summary_contract,
        "wire_format_example": {
            "activity": activity,
            "status": "succeeded",
            "summary": "Concise description of what the activity did.",
            "artifacts": [
                {
                    "artifact_type": "workflow_decision",
                    "artifact": {
                        "workflow_id": "...",
                        "observed_state": "...",
                        "decision": "...",
                        "next_state": "...",
                        "reason": "...",
                        "confidence": "high",
                        "commands": command_examples
                    }
                }
            ],
            "signals": [
                {
                    "signal_type": "<one of accepted_signals from transition_contract>",
                    "signal": {
                        "issue_number": 123,
                        "issue_url": "https://example/issues/123",
                        "note": "Per-signal payload goes inside the `signal` object. Do NOT use `kind` as the discriminator; the wire format is `signal_type` + `signal`."
                    }
                }
            ],
            "validation": [
                {"command": "cargo test", "status": "passed"}
            ],
            "_format_rules": [
                "`artifacts` MUST be a JSON array of {artifact_type, artifact} objects. Never emit it as a map keyed by artifact name.",
                "`signals` MUST be a JSON array of {signal_type, signal} objects. Never use `kind` or any other discriminator name.",
                "`validation` MUST be a JSON array of {command, status} objects. Never emit it as a map.",
                "Inside a `workflow_decision` artifact, the next-step activity MUST be expressed as `commands: [{command_type, dedupe_key, command}]` (plural array). Never use a singular `command` field at the artifact level — that field is silently ignored, leaving the workflow stuck in the new state with no follow-up activity enqueued.",
                "For `command_type: start_child_workflow`, the nested `command` object MUST include `definition_id` and `subject_key`; for GitHub issue workflows use `definition_id: github_issue_pr` and `subject_key: issue:<number>`.",
                "Omit `artifacts`, `signals`, or `validation` entirely if there is nothing to report — empty arrays or missing fields are both fine."
            ]
        },
    })
}

fn workflow_decision_command_examples(workflow_definition: &str, activity: &str) -> Value {
    match (workflow_definition, activity) {
        ("repo_backlog", "plan_repo_sprint") => json!([
            {
                "command_type": "start_child_workflow",
                "dedupe_key": "repo-sprint-plan:owner/repo:issue:42:start",
                "command": {
                    "definition_id": "github_issue_pr",
                    "subject_key": "issue:42",
                    "repo": "owner/repo",
                    "issue_number": 42,
                    "issue_url": "https://github.com/owner/repo/issues/42",
                    "title": "Example issue selected for the sprint",
                    "labels": ["bug"],
                    "depends_on": [],
                    "source": "github",
                    "external_id": "42",
                    "auto_submit": true,
                    "note": "All child-workflow payload goes INSIDE this nested `command` Value. Do not omit definition_id or subject_key."
                }
            }
        ]),
        _ => json!([
            {
                "command_type": "enqueue_activity",
                "dedupe_key": "<unique stable string for this command>",
                "command": {
                    "activity": "<next activity name>",
                    "note": "All activity-specific payload (repo, issue_number, signals, etc.) goes INSIDE this nested `command` Value. The outer object MUST have exactly the three fields: command_type, dedupe_key, command."
                }
            }
        ]),
    }
}

fn workflow_decision_contract(workflow: Option<&WorkflowInstance>) -> Value {
    let Some(workflow) = workflow else {
        return json!({
            "available": false,
            "reason": "No workflow instance was loaded for this runtime job."
        });
    };
    let Some(validator) = decision_validator_for_definition(&workflow.definition_id) else {
        return json!({
            "available": false,
            "workflow_id": workflow.id.as_str(),
            "workflow_definition": workflow.definition_id.as_str(),
            "observed_state": workflow.state.as_str(),
            "reason": "No transition validator is registered for this workflow definition."
        });
    };
    let allowed_transitions = validator
        .transition_rules_from(&workflow.state)
        .map(|rule| {
            let allowed_commands = rule
                .allowed_commands
                .iter()
                .map(|command| command.as_str())
                .collect::<Vec<_>>();
            json!({
                "from_state": rule.from_state.as_deref().unwrap_or("*"),
                "next_state": rule.to_state.as_str(),
                "allowed_commands": allowed_commands,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "available": true,
        "workflow_id": workflow.id.as_str(),
        "workflow_definition": workflow.definition_id.as_str(),
        "observed_state": workflow.state.as_str(),
        "allowed_transitions": allowed_transitions,
    })
}

fn decision_validator_for_definition(definition_id: &str) -> Option<DecisionValidator> {
    match definition_id {
        "github_issue_pr" => Some(DecisionValidator::github_issue_pr()),
        QUALITY_GATE_DEFINITION_ID => Some(DecisionValidator::quality_gate()),
        PR_FEEDBACK_DEFINITION_ID => Some(DecisionValidator::pr_feedback()),
        PROMPT_TASK_DEFINITION_ID => Some(DecisionValidator::prompt_task()),
        "repo_backlog" => Some(DecisionValidator::repo_backlog()),
        _ => None,
    }
}

fn activity_transition_contract(workflow_definition: &str, activity: &str) -> Value {
    match (workflow_definition, activity) {
        ("github_issue_pr", "replan_issue") => json!({
            "on_succeeded": {
                "reducer_next_state": "implementing",
                "required_summary": "Explain the revised implementation direction and validation plan."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("github_issue_pr", "address_pr_feedback") => json!({
            "on_succeeded": {
                "reducer_next_state": "local_review_gate",
                "success_requires": "A succeeded address_pr_feedback result MUST include pr_repair_snapshot with final head, observed_at, action proof, and passing validation evidence, unless IssueClosed/IssueAlreadyResolved or issue_state proves the issue or PR is already closed/resolved.",
                "required_summary": "Describe addressed review feedback, pushed/no-code action, validation evidence, or closed issue evidence. Harness will run local review before remote feedback unless terminal closed evidence finishes the workflow."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("github_issue_pr", harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "awaiting_feedback_or_addressing_feedback_or_blocked_from_signals",
                "accepted_signals": [
                    harness_workflow::runtime::LOCAL_REVIEW_PASSED_SIGNAL,
                    harness_workflow::runtime::LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
                    harness_workflow::runtime::LOCAL_REVIEW_BLOCKED_SIGNAL
                ],
                "required_summary": "Describe the local agent review result, blocking findings if any, and validation evidence inspected."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("github_issue_pr", "sweep_pr_feedback")
        | ("github_issue_pr", PR_FEEDBACK_INSPECT_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "derived_from_structured_decision_or_signals",
                "accepted_signals": ["FeedbackFound", "NoFeedbackFound", "PrReadyToMerge", "ChangesRequested", "ChecksFailed"],
                "accepted_artifacts": ["workflow_decision", SERVER_PR_SNAPSHOT_ARTIFACT, PR_FEEDBACK_SNAPSHOT_ARTIFACT],
                "success_requires": "PrReadyToMerge or mark_ready_to_merge requires server_pr_snapshot collected by Harness with final head, observed_at, APPROVED reviewDecision, isDraft=false, SUCCESS checks, CLEAN mergeStateStatus, complete reviewThreads, and zero active unresolved review threads.",
                "required_summary": "Describe inspected PR feedback, review state, checks, mergeability, draft state, unresolved review threads, snapshot source, and next action."
            },
            "structured_decision": {
                "preferred": true,
                "description": "Return a workflow_decision artifact for address_pr_feedback, wait_for_pr_feedback, or mark_ready_to_merge."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "feedback_found_or_no_actionable_feedback_or_ready_to_merge_from_signals",
                "accepted_signals": ["FeedbackFound", "NoFeedbackFound", "PrReadyToMerge", "ChangesRequested", "ChecksFailed"],
                "accepted_artifacts": ["workflow_decision", SERVER_PR_SNAPSHOT_ARTIFACT, PR_FEEDBACK_SNAPSHOT_ARTIFACT],
                "success_requires": "PrReadyToMerge or any workflow_decision with next_state=ready_to_merge requires server_pr_snapshot collected by Harness with final head, observed_at, APPROVED reviewDecision, isDraft=false, SUCCESS checks, CLEAN mergeStateStatus, complete reviewThreads, and zero active unresolved review threads.",
                "parent_propagation": "The same activity result is propagated to the parent github_issue_pr workflow."
            },
            "structured_decision": {
                "optional": true,
                "description": "A workflow_decision artifact may update the pr_feedback child workflow, but ready_to_merge workflow_decisions still require the same server_pr_snapshot evidence as PrReadyToMerge signals."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("repo_backlog", "poll_repo_backlog") => json!({
            "on_succeeded": {
                "reducer_next_state": "planning_batch_when_IssueDiscovered_signals_exist; dispatching_when_only_OpenPrFeedbackDiscovered_signals_exist; otherwise idle",
                "accepted_signals": ["IssueDiscovered", "IssueSkipped", "NoOpenIssueFound", "OpenPrFeedbackDiscovered", "OpenPrFeedbackSkipped", "NoOpenPrFeedbackFound"],
                "success_requires": "At least one accepted signal. Empty signals are invalid even when status is succeeded.",
                "empty_success_allowed": false,
                "required_summary": "Describe the GitHub issue query, open PR feedback query, existing workflow checks, new issue workflow candidates, and standalone PR feedback candidates."
            },
            "structured_decision": {
                "optional": true,
                "description": "Prefer signals. Only emit workflow_decision when you can include every required command; a transition to planning_batch without an enqueue_activity command is invalid."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("repo_backlog", "plan_repo_sprint") => json!({
            "on_succeeded": {
                "reducer_next_state": "dispatching_when_SprintTaskSelected_signals_exist_else_idle",
                "accepted_signals": ["SprintTaskSelected", "IssueSkipped", "NoSprintTaskSelected"],
                "accepted_artifacts": ["sprint_plan"],
                "success_requires": "At least one accepted signal or a sprint_plan artifact. Empty signals and no sprint_plan artifact are invalid even when status is succeeded.",
                "empty_success_allowed": false,
                "required_summary": "Describe dependency planning, skipped issues, and selected issue workflow dispatch order."
            },
            "structured_decision": {
                "optional": true,
                "description": "Prefer signals. Only emit workflow_decision when you can include every required command; a transition to dispatching without start_child_workflow commands is invalid."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("github_issue_pr", "implement_issue") => json!({
            "on_succeeded": {
                "reducer_next_state": "pr_open_with_pull_request_artifact_or_done_with_closed_issue_signal_else_blocked",
                "accepted_signals": [ISSUE_CLOSED_SIGNAL, ISSUE_ALREADY_RESOLVED_SIGNAL],
                "accepted_artifacts": ["pull_request", ISSUE_STATE_ARTIFACT],
                "success_requires": "A succeeded implement_issue result MUST include either a pull_request artifact with pr_number/pr_url, or structured closed-issue evidence with explicit closed/resolved state plus issue_number or issue_url. Empty success is blocked.",
                "required_summary": "Include changed files, validation commands, and the PR URL or closed issue evidence."
            },
            "follow_up_event": "PrDetected can still bind PR metadata, but a runtime result should emit pull_request directly when a PR exists."
        }),
        (PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "done",
                "success_requires": "A succeeded implement_prompt result MUST include validation evidence via validation records or a validation_report artifact.",
                "required_summary": "Include changed files, validation commands, and remaining blockers."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("repo_backlog", "start_child_workflow") => json!({
            "on_succeeded": {
                "reducer_next_state": "idle",
                "required_artifact": "child_workflow"
            }
        }),
        ("repo_backlog", "mark_bound_issue_done") | ("repo_backlog", "recover_issue_workflow") => {
            json!({
                "on_succeeded": {
                    "reducer_next_state": "idle",
                    "required_artifact": "child_workflow"
                }
            })
        }
        (QUALITY_GATE_DEFINITION_ID, QUALITY_GATE_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "passed",
                "output_signal": QUALITY_PASSED_SIGNAL,
                "required_summary": "Describe validation commands run and passing evidence."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "output_signal": QUALITY_FAILED_SIGNAL,
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            },
            "on_blocked": {
                "reducer_next_state": "blocked",
                "output_signal": QUALITY_BLOCKED_SIGNAL,
                "required_summary": "Describe the missing dependency, budget, or external input."
            }
        }),
        _ => json!({
            "on_succeeded": {
                "reducer_next_state": "unchanged",
                "reason": "No reducer transition is registered for this workflow/activity pair."
            }
        }),
    }
}

fn agent_summary_contract(workflow_definition: &str, activity: &str) -> Value {
    match (workflow_definition, activity) {
        ("github_issue_pr", "implement_issue") => json!({
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
                    "required_when": "No validation records are present in the activity result.",
                    "fields": ["commands", "passed", "failed", "blocked"]
                }
            }
        }),
        ("github_issue_pr", "replan_issue") => json!({
            "must_include": ["reason for replan", "new implementation plan", "validation plan"],
            "must_not_include": ["direct workflow state changes"],
        }),
        ("github_issue_pr", "address_pr_feedback") => json!({
            "must_include": ["review feedback addressed or explicit no-code reason", "changed files or explicit no-code-change reason", "validation commands or closed issue evidence", "fresh PR state checked before final response", "final PR head or closed issue evidence"],
            "must_not_include": ["claiming review approval without a fresh review signal", "marking review threads resolved without current GitHub evidence"],
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
            "must_include": ["PR diff reviewed", "blocking findings or explicit approval", "validation evidence checked", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "remote review approval claims"],
            "signals": {
                "LocalReviewPassed": "Use only when the local agent review finds no blocking issues.",
                "LocalReviewChangesRequested": "Use when local review finds blocking code, test, regression, or security issues that need a fix round.",
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
                    "allowed_decisions": ["address_pr_feedback", "wait_for_pr_feedback", "mark_ready_to_merge"]
                },
                "server_pr_snapshot": {
                    "required_when": "Using PrReadyToMerge or mark_ready_to_merge.",
                    "source": "Harness server GitHub GraphQL collector",
                    "fields": ["schema", "snapshot_source", "pr_number", "pr_url", "head_oid", "observed_at", "active_unresolved_review_threads", "active_unresolved_review_threads_count", "review_threads_complete", "status_check_rollup_state", "merge_state_status", "review_decision", "is_draft", "changed_files"]
                },
                "pr_feedback_snapshot": {
                    "required_when": "Harness server emits normalized PR feedback evidence.",
                    "source": "Normalized view of server_pr_snapshot."
                }
            },
            "signals": {
                "FeedbackFound": "Use when actionable feedback, requested changes, or failed checks require a fix round.",
                "NoFeedbackFound": "Use when no actionable feedback is present yet.",
                "PrReadyToMerge": "Use only with server_pr_snapshot proving APPROVED reviewDecision, isDraft=false, SUCCESS checks, CLEAN mergeStateStatus, complete reviewThreads, and zero active unresolved review threads for the final head."
            }
        }),
        ("repo_backlog", "poll_repo_backlog") => json!({
            "must_include": ["repo and label queried", "open issues inspected", "open PR feedback inspected", "existing workflow checks", "new issue workflow candidates", "open PR feedback candidates", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "server-side GitHub polling changes"],
            "success_rule": "A succeeded result MUST emit at least one accepted issue or open PR feedback signal. Do not return succeeded with empty signals.",
            "signals": {
                "IssueDiscovered": "Use for each open GitHub issue that should be considered by the runtime sprint planner. Include issue_number, issue_url, repo, title, and labels when available.",
                "IssueSkipped": "Use for open GitHub issues that already have a workflow, are PRs, or should not be started. Include issue_number and reason. When an issue already has a workflow, include workflow_state.",
                "NoOpenIssueFound": "Use when the repo/label query found no candidate issues.",
                "OpenPrFeedbackDiscovered": "Use for each open PR with unresolved actionable review feedback and no active bound workflow. Include pr_number, pr_url, repo, title, feedback_count, and summary.",
                "OpenPrFeedbackSkipped": "Use for open PRs intentionally skipped because they are already covered, have no actionable feedback, are draft/closed, or are otherwise not candidates. Include pr_number and reason.",
                "NoOpenPrFeedbackFound": "Use when the open PR review query found no standalone PR feedback candidates."
            },
            "artifacts": {
                "workflow_decision": {
                    "optional": true,
                    "allowed_decisions": ["plan_repo_sprint_from_scan", "finish_repo_backlog_scan"]
                }
            }
        }),
        ("repo_backlog", "plan_repo_sprint") => json!({
            "must_include": ["issues considered", "dependency reasoning", "selected tasks", "skipped issues", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "server-side task queue changes"],
            "success_rule": "A succeeded result MUST emit SprintTaskSelected, IssueSkipped, NoSprintTaskSelected, or a sprint_plan artifact. Do not return succeeded with empty signals and no sprint_plan artifact.",
            "signals": {
                "SprintTaskSelected": "Use once for each issue selected for execution. Include issue_number, issue_url, repo, labels, and depends_on as issue numbers.",
                "IssueSkipped": "Use for discovered issues intentionally skipped by planning. Include issue_number and reason.",
                "NoSprintTaskSelected": "Use when no issue should be dispatched from this sprint plan."
            },
            "artifacts": {
                "sprint_plan": {
                    "optional": true,
                    "fields": ["tasks", "skip"],
                    "task_fields": ["issue", "depends_on"]
                },
                "workflow_decision": {
                    "optional": true,
                    "allowed_decisions": ["start_issue_workflows_from_sprint_plan", "finish_repo_sprint_plan"]
                }
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
        _ => json!({
            "must_include": ["summary", "validation commands", "remaining blockers"],
            "must_not_include": ["direct workflow table mutations"],
        }),
    }
}

pub(super) fn prompt_packet_digest(prompt_packet: &Value) -> String {
    let bytes = serde_json::to_vec(prompt_packet).unwrap_or_else(|_| Vec::new());
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn workflow_prompt_artifact(prompt_packet_digest: &str) -> ActivityArtifact {
    ActivityArtifact::new(
        "runtime_prompt_packet",
        json!({
            "digest": prompt_packet_digest,
            "schema": "harness.runtime.prompt_packet.v1",
        }),
    )
}

fn pretty_json<T>(value: &T) -> String
where
    T: serde::Serialize,
{
    serde_json::to_string_pretty(value).unwrap_or_else(|error| {
        json!({
            "serialization_error": error.to_string()
        })
        .to_string()
    })
}

#[cfg(test)]
#[path = "prompt_packet_tests.rs"]
mod tests;
