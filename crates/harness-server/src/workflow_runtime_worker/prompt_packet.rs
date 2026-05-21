use harness_core::config::workflow::WorkflowDocument;
use harness_workflow::runtime::{
    ActivityArtifact, DecisionValidator, RuntimeJob, RuntimeProfile, WorkflowInstance,
    ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL, ISSUE_STATE_ARTIFACT,
    PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY, QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL,
    QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL,
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
                        "commands": [
                            {
                                "command_type": "enqueue_activity",
                                "dedupe_key": "<unique stable string for this command>",
                                "command": {
                                    "activity": "<next activity name>",
                                    "note": "All activity-specific payload (repo, issue_number, signals, etc.) goes INSIDE this nested `command` Value. The outer object MUST have exactly the three fields: command_type, dedupe_key, command."
                                }
                            }
                        ]
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
                "Omit `artifacts`, `signals`, or `validation` entirely if there is nothing to report — empty arrays or missing fields are both fine."
            ]
        },
    })
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
                "reducer_next_state": "awaiting_feedback",
                "required_summary": "Describe addressed review feedback and validation evidence."
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
                "required_summary": "Describe inspected PR feedback, review state, checks, and mergeability."
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
                "parent_propagation": "The same activity result is propagated to the parent github_issue_pr workflow."
            },
            "structured_decision": {
                "optional": true,
                "description": "A workflow_decision artifact may update the pr_feedback child workflow, but signals are sufficient."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("repo_backlog", "poll_repo_backlog") => json!({
            "on_succeeded": {
                "reducer_next_state": "planning_batch_when_IssueDiscovered_signals_exist_else_idle",
                "accepted_signals": ["IssueDiscovered", "IssueSkipped", "NoOpenIssueFound"],
                "success_requires": "At least one accepted signal. Empty signals are invalid even when status is succeeded.",
                "empty_success_allowed": false,
                "required_summary": "Describe the GitHub issue query, existing workflow checks, and new issue workflow candidates."
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
                "success_requires": "A succeeded implement_issue result MUST include either a pull_request artifact with pr_number/pr_url, or structured closed-issue evidence via IssueClosed/IssueAlreadyResolved signal or issue_state artifact. Empty success is blocked.",
                "required_summary": "Include changed files, validation commands, and the PR URL or closed issue evidence."
            },
            "follow_up_event": "PrDetected can still bind PR metadata, but a runtime result should emit pull_request directly when a PR exists."
        }),
        (PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "done",
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
                "IssueClosed": "Use when the GitHub issue is confirmed closed and no implementation PR is needed. Include issue_number and state or issue_url.",
                "IssueAlreadyResolved": "Use when the task is already resolved before a PR is created. Include issue_number and state or issue_url."
            }
        }),
        (PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY) => json!({
            "must_include": ["changed files", "validation commands", "remaining blockers"],
            "must_not_include": ["workflow table mutations", "unverified merge claims"],
            "artifacts": {
                "validation_report": {
                    "optional": true,
                    "fields": ["commands", "passed", "failed", "blocked"]
                }
            }
        }),
        ("github_issue_pr", "replan_issue") => json!({
            "must_include": ["reason for replan", "new implementation plan", "validation plan"],
            "must_not_include": ["direct workflow state changes"],
        }),
        ("github_issue_pr", "address_pr_feedback") => json!({
            "must_include": ["review feedback addressed", "changed files", "validation commands"],
            "must_not_include": ["claiming review approval without a fresh review signal"],
        }),
        ("github_issue_pr", "sweep_pr_feedback")
        | ("github_issue_pr", PR_FEEDBACK_INSPECT_ACTIVITY)
        | (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => json!({
            "must_include": ["PR comments reviewed", "review states", "check status", "mergeability", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "unverified approval claims"],
            "artifacts": {
                "workflow_decision": {
                    "preferred": true,
                    "allowed_decisions": ["address_pr_feedback", "wait_for_pr_feedback", "mark_ready_to_merge"]
                }
            },
            "signals": {
                "FeedbackFound": "Use when actionable feedback, requested changes, or failed checks require a fix round.",
                "NoFeedbackFound": "Use when no actionable feedback is present yet.",
                "PrReadyToMerge": "Use only when review, checks, and mergeability are all ready."
            }
        }),
        ("repo_backlog", "poll_repo_backlog") => json!({
            "must_include": ["repo and label queried", "open issues inspected", "existing workflow checks", "new issue workflow candidates", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "server-side GitHub polling changes"],
            "success_rule": "A succeeded result MUST emit IssueDiscovered, IssueSkipped, or NoOpenIssueFound. Do not return succeeded with empty signals.",
            "signals": {
                "IssueDiscovered": "Use for each open GitHub issue that should be considered by the runtime sprint planner. Include issue_number, issue_url, repo, title, and labels when available.",
                "IssueSkipped": "Use for open GitHub issues that already have a workflow, are PRs, or should not be started. Include issue_number and reason. When an issue already has a workflow, include workflow_state.",
                "NoOpenIssueFound": "Use when the repo/label query found no candidate issues."
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
mod tests {
    use super::*;
    use harness_workflow::runtime::{
        RuntimeKind, WorkflowSubject, REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
    };

    #[test]
    fn activity_result_schema_describes_issue_implementation_terminal_evidence_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let workflow = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:123"),
        )
        .with_id("issue-123");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
            "pr_open_with_pull_request_artifact_or_done_with_closed_issue_signal_else_blocked"
        );
        assert_eq!(
            schema["activity_contract"]["accepted_signals"][0],
            ISSUE_CLOSED_SIGNAL
        );
        assert_eq!(
            schema["activity_contract"]["success_requires"],
            "pull_request_artifact_or_closed_issue_signal"
        );
        assert_eq!(
            schema["agent_summary_contract"]["artifacts"]["issue_state"]["fields"][1],
            "state"
        );
        assert_eq!(
            schema["agent_summary_contract"]["signals"]["IssueAlreadyResolved"],
            "Use when the task is already resolved before a PR is created. Include issue_number and state or issue_url."
        );
    }

    #[test]
    fn activity_result_schema_describes_quality_gate_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": QUALITY_GATE_ACTIVITY
            }),
        );
        let workflow = WorkflowInstance::new(
            QUALITY_GATE_DEFINITION_ID,
            1,
            "checking",
            WorkflowSubject::new("quality_gate", "issue:123"),
        )
        .with_id("quality-gate-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], QUALITY_GATE_DEFINITION_ID);
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
            "passed"
        );
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["output_signal"],
            QUALITY_PASSED_SIGNAL
        );
        assert_eq!(
            schema["agent_summary_contract"]["must_include"][0],
            "validation commands"
        );
        assert_eq!(
            schema["activity_contract"]["accepted_signals"][0],
            QUALITY_PASSED_SIGNAL
        );
        assert_eq!(
            schema["activity_contract"]["success_requires"],
            "quality_gate_status_signal"
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "passed"));
    }

    #[test]
    fn activity_result_schema_describes_repo_backlog_poll_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "poll_repo_backlog"
            }),
        );
        let workflow = WorkflowInstance::new(
            "repo_backlog",
            1,
            "scanning",
            WorkflowSubject::new("repo", "owner/repo"),
        )
        .with_id("repo-backlog-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], "repo_backlog");
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
            "IssueDiscovered"
        );
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["empty_success_allowed"],
            false
        );
        assert_eq!(
            schema["activity_contract"]["success_requires"],
            "at_least_one_accepted_signal"
        );
        assert_eq!(
            schema["activity_contract"]["explicit_noop_signals"][0],
            "NoOpenIssueFound"
        );
        assert_eq!(
            schema["agent_summary_contract"]["signals"]["IssueDiscovered"],
            "Use for each open GitHub issue that should be considered by the runtime sprint planner. Include issue_number, issue_url, repo, title, and labels when available."
        );
        assert_eq!(
            schema["agent_summary_contract"]["success_rule"],
            "A succeeded result MUST emit IssueDiscovered, IssueSkipped, or NoOpenIssueFound. Do not return succeeded with empty signals."
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "planning_batch"));
    }

    #[test]
    fn activity_result_schema_describes_repo_sprint_plan_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": REPO_BACKLOG_SPRINT_PLAN_ACTIVITY
            }),
        );
        let workflow = WorkflowInstance::new(
            "repo_backlog",
            1,
            "planning_batch",
            WorkflowSubject::new("repo", "owner/repo"),
        )
        .with_id("repo-backlog-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], "repo_backlog");
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
            "SprintTaskSelected"
        );
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["empty_success_allowed"],
            false
        );
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["accepted_artifacts"][0],
            "sprint_plan"
        );
        assert_eq!(
            schema["activity_contract"]["success_requires"],
            "at_least_one_accepted_signal_or_artifact"
        );
        assert_eq!(
            schema["activity_contract"]["accepted_artifacts"][0],
            "sprint_plan"
        );
        assert_eq!(
            schema["agent_summary_contract"]["signals"]["SprintTaskSelected"],
            "Use once for each issue selected for execution. Include issue_number, issue_url, repo, labels, and depends_on as issue numbers."
        );
        assert_eq!(
            schema["agent_summary_contract"]["success_rule"],
            "A succeeded result MUST emit SprintTaskSelected, IssueSkipped, NoSprintTaskSelected, or a sprint_plan artifact. Do not return succeeded with empty signals and no sprint_plan artifact."
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "dispatching"));
    }

    #[test]
    fn activity_result_schema_describes_pr_feedback_child_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": PR_FEEDBACK_INSPECT_ACTIVITY
            }),
        );
        let workflow = WorkflowInstance::new(
            PR_FEEDBACK_DEFINITION_ID,
            1,
            "inspecting",
            WorkflowSubject::new("pr", "pr:77"),
        )
        .with_id("pr-feedback-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], PR_FEEDBACK_DEFINITION_ID);
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
            "FeedbackFound"
        );
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["parent_propagation"],
            "The same activity result is propagated to the parent github_issue_pr workflow."
        );
        assert_eq!(
            schema["activity_contract"]["child_outcome_contract"],
            "pr_feedback_outcome"
        );
        assert_eq!(
            schema["agent_summary_contract"]["signals"]["PrReadyToMerge"],
            "Use only when review, checks, and mergeability are all ready."
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "feedback_found"));
    }

    #[test]
    fn runtime_prompt_packet_includes_workflow_file_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue",
                "runtime_profile": RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc)
            }),
        );
        let workflow_document = WorkflowDocument {
            prompt_template: "Follow the repository workflow prompt.".to_string(),
            source_path: Some("/repo/WORKFLOW.md".to_string()),
            ..Default::default()
        };
        let runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

        let packet = build_runtime_prompt_packet(
            &job,
            None,
            Path::new("/workspaces/job-1"),
            Path::new("/repo"),
            &runtime_profile,
            &workflow_document,
        );
        assert_eq!(packet["project"]["root"], "/workspaces/job-1");
        assert_eq!(packet["project"]["source_root"], "/repo");
        assert_eq!(
            packet["workflow_file"]["prompt_template"],
            "Follow the repository workflow prompt."
        );

        let prompt = build_runtime_job_prompt(&packet, None);
        assert!(prompt.contains("Repository workflow prompt template:"));
        assert!(prompt.contains("Follow the repository workflow prompt."));
    }
}
