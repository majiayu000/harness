use super::{RuntimeRecoveryTargetProjection, RuntimeStoppedActionEligibility};
use harness_workflow::runtime::{
    workflow_declarative_definition, ActivityErrorKind, WorkflowCommand, WorkflowCommandType,
    WorkflowInstance, WorkflowRuntimeStore, GITHUB_ISSUE_PR_DEFINITION_ID, LOCAL_REVIEW_ACTIVITY,
    PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) async fn stopped_action_eligibility_for_workflows(
    store: Option<&WorkflowRuntimeStore>,
    workflows: &[WorkflowInstance],
) -> anyhow::Result<HashMap<String, RuntimeStoppedActionEligibility>> {
    let Some(store) = store else {
        return Ok(HashMap::new());
    };

    let mut plans = Vec::new();
    let mut runtime_job_ids = Vec::new();
    for workflow in workflows {
        let Some(plan) = stopped_action_plan(workflow) else {
            continue;
        };
        if let Some(runtime_job_id) = plan.runtime_job_id.as_ref() {
            runtime_job_ids.push(runtime_job_id.clone());
        }
        plans.push((workflow.id.clone(), plan));
    }

    let command_sources = store
        .command_sources_for_runtime_jobs(&runtime_job_ids)
        .await?;
    let mut by_workflow = HashMap::new();
    for (workflow_id, plan) in plans {
        let allowed = match plan.runtime_job_id.as_ref() {
            None => plan.legacy_fallback,
            Some(runtime_job_id) => command_sources
                .get(runtime_job_id)
                .filter(|source| source.workflow_id == workflow_id)
                .is_some_and(|source| {
                    command_matches_recovery_target(&source.command, plan.target)
                }),
        };
        if allowed {
            by_workflow.insert(
                workflow_id,
                match plan.action {
                    StoppedRecoveryAction::Unblock => RuntimeStoppedActionEligibility {
                        can_unblock: true,
                        can_retry: false,
                    },
                    StoppedRecoveryAction::Retry => RuntimeStoppedActionEligibility {
                        can_unblock: false,
                        can_retry: true,
                    },
                },
            );
        }
    }
    Ok(by_workflow)
}

pub(super) fn pinned_recovery_targets(
    workflow: &WorkflowInstance,
) -> Vec<RuntimeRecoveryTargetProjection> {
    if workflow.state != "blocked" {
        return Vec::new();
    }
    let Some(definition) =
        workflow_declarative_definition(&workflow.definition_id, workflow.definition_version)
    else {
        return Vec::new();
    };
    recovery_targets_for_definition(workflow, &definition)
}

fn recovery_targets_for_definition(
    workflow: &WorkflowInstance,
    definition: &harness_workflow::runtime::DeclarativeWorkflowDefinition,
) -> Vec<RuntimeRecoveryTargetProjection> {
    if workflow.state != "blocked" {
        return Vec::new();
    }
    let Some(pinned_hash) = workflow.data.get("definition_hash").and_then(Value::as_str) else {
        return Vec::new();
    };
    if pinned_hash != definition.definition_hash() {
        return Vec::new();
    }
    definition
        .policy()
        .recovery_targets
        .iter()
        .map(|state| RuntimeRecoveryTargetProjection {
            state: state.clone(),
            required_evidence: definition
                .policy()
                .evidence_required
                .get(state)
                .cloned()
                .unwrap_or_default(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoppedActionPlan {
    action: StoppedRecoveryAction,
    target: RecoveryDispatchTarget,
    runtime_job_id: Option<String>,
    legacy_fallback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoppedRecoveryAction {
    Unblock,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecoveryDispatchTarget {
    activity: &'static str,
}

fn stopped_action_plan(workflow: &WorkflowInstance) -> Option<StoppedActionPlan> {
    if workflow.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
        if workflow.state != "blocked" || pinned_recovery_targets(workflow).is_empty() {
            return None;
        }
        return Some(StoppedActionPlan {
            action: StoppedRecoveryAction::Unblock,
            target: RecoveryDispatchTarget { activity: "" },
            runtime_job_id: None,
            legacy_fallback: true,
        });
    }
    let action = match workflow.state.as_str() {
        "blocked" => StoppedRecoveryAction::Unblock,
        "failed" => StoppedRecoveryAction::Retry,
        _ => return None,
    };
    let metadata = StoppedRecoveryMetadata::from_workflow_data(&workflow.data)?;
    if action == StoppedRecoveryAction::Retry
        && matches!(
            metadata.error_kind,
            Some(ActivityErrorKind::Fatal | ActivityErrorKind::Configuration)
        )
    {
        return None;
    }

    let (target, runtime_job_id, legacy_fallback) = match metadata.activity.as_deref() {
        Some(activity) => (
            recovery_dispatch_target(activity)?,
            Some(metadata.runtime_job_id?),
            false,
        ),
        None if metadata.has_no_structured_stop_metadata => (
            RecoveryDispatchTarget {
                activity: "implement_issue",
            },
            None,
            true,
        ),
        None => return None,
    };
    Some(StoppedActionPlan {
        action,
        target,
        runtime_job_id,
        legacy_fallback,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoppedRecoveryMetadata {
    activity: Option<String>,
    runtime_job_id: Option<String>,
    error_kind: Option<ActivityErrorKind>,
    has_no_structured_stop_metadata: bool,
}

impl StoppedRecoveryMetadata {
    fn from_workflow_data(data: &Value) -> Option<Self> {
        if data
            .get("last_stop")
            .filter(|value| !value.is_null())
            .is_some_and(|value| !value.is_object())
        {
            return None;
        }
        let error_kind = stopped_error_kind(data)?;
        let _state = optional_metadata_string(data.pointer("/last_stop/state"))?;
        Some(Self {
            activity: optional_metadata_string(data.pointer("/last_stop/activity"))?,
            runtime_job_id: optional_metadata_string(data.pointer("/last_stop/runtime_job_id"))?,
            has_no_structured_stop_metadata: data.get("last_stop").is_none_or(Value::is_null)
                && error_kind.is_none(),
            error_kind,
        })
    }
}

fn stopped_error_kind(data: &Value) -> Option<Option<ActivityErrorKind>> {
    let root = optional_error_kind(data.get("error_kind"))?;
    let last_stop = optional_error_kind(data.pointer("/last_stop/error_kind"))?;
    Some(root.or(last_stop))
}

fn optional_metadata_string(value: Option<&Value>) -> Option<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Some(None);
    };
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Some(value.to_string()))
}

fn optional_error_kind(value: Option<&Value>) -> Option<Option<ActivityErrorKind>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Some(None);
    };
    if value.as_str().is_some_and(|value| value.trim().is_empty()) {
        return None;
    }
    serde_json::from_value(value.clone()).ok().map(Some)
}

fn recovery_dispatch_target(activity: &str) -> Option<RecoveryDispatchTarget> {
    match activity {
        "implement_issue" => Some(RecoveryDispatchTarget {
            activity: "implement_issue",
        }),
        "replan_issue" => Some(RecoveryDispatchTarget {
            activity: "replan_issue",
        }),
        "merge_pr" => Some(RecoveryDispatchTarget {
            activity: "merge_pr",
        }),
        LOCAL_REVIEW_ACTIVITY => Some(RecoveryDispatchTarget {
            activity: LOCAL_REVIEW_ACTIVITY,
        }),
        "sweep_pr_feedback" => Some(RecoveryDispatchTarget {
            activity: "sweep_pr_feedback",
        }),
        PR_FEEDBACK_INSPECT_ACTIVITY => Some(RecoveryDispatchTarget {
            activity: PR_FEEDBACK_INSPECT_ACTIVITY,
        }),
        "start_child_workflow" => Some(RecoveryDispatchTarget {
            activity: "start_child_workflow",
        }),
        "address_pr_feedback" => Some(RecoveryDispatchTarget {
            activity: "address_pr_feedback",
        }),
        _ => None,
    }
}

fn command_matches_recovery_target(
    command: &WorkflowCommand,
    target: RecoveryDispatchTarget,
) -> bool {
    match command.command_type {
        WorkflowCommandType::EnqueueActivity => {
            command.activity_name() == Some(target.activity)
                && enqueue_payload_matches_target(&command.command)
        }
        WorkflowCommandType::StartChildWorkflow => {
            let payload = &command.command;
            matches!(
                target.activity,
                "start_child_workflow" | "sweep_pr_feedback"
            ) && payload.get("definition_id").and_then(Value::as_str)
                == Some(PR_FEEDBACK_DEFINITION_ID)
                && payload.get("child_activity").and_then(Value::as_str)
                    == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
                && payload.get("pr_number").and_then(Value::as_u64).is_some()
                && payload
                    .get("subject_key")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
        }
        _ => false,
    }
}

fn enqueue_payload_matches_target(payload: &Value) -> bool {
    if !payload.is_object() {
        return false;
    }
    let review_summary = payload
        .get("review_summary")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let hygiene = payload
        .get("hygiene")
        .or_else(|| payload.get("hygiene_context"))
        .is_some_and(|value| !value.is_null());
    payload.get("source").and_then(Value::as_str) != Some("pr_hygiene")
        || (payload.get("pr_number").and_then(Value::as_u64).is_some() && review_summary && hygiene)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use harness_workflow::runtime::build_declarative_definition;
    use harness_workflow::runtime::{RuntimeKind, WorkflowSubject};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn operator_monitor_enqueue_payload_rejects_non_objects() {
        for payload in [Value::Null, Value::String("implement_issue".to_string())] {
            assert!(!enqueue_payload_matches_target(&payload));
        }
    }

    #[test]
    fn declarative_projection_exposes_only_exact_pinned_recovery_metadata() {
        let policy = WorkflowDefinitionPolicy {
            id: "projection_recovery".to_string(),
            initial: "running".to_string(),
            states: BTreeMap::from([
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "running".to_string(),
                    DeclaredState {
                        activity: Some("run".to_string()),
                        on_success: Some("done".to_string()),
                        on_failure: Some("failed".to_string()),
                        on_signal: BTreeMap::from([(
                            "cancel".to_string(),
                            "cancelled".to_string(),
                        )]),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
                ("cancelled".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::from([(
                "running".to_string(),
                vec!["operator_ticket".to_string()],
            )]),
            recovery_targets: vec!["running".to_string()],
        };
        let definition = build_declarative_definition(
            &policy,
            &BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]),
        )
        .expect("fixture definition should compile");
        let pinned = WorkflowInstance::new(
            "projection_recovery",
            definition.definition_version(),
            "blocked",
            WorkflowSubject::new("test", "one"),
        )
        .with_data(json!({ "definition_hash": definition.definition_hash() }));
        assert_eq!(
            recovery_targets_for_definition(&pinned, &definition),
            [RuntimeRecoveryTargetProjection {
                state: "running".to_string(),
                required_evidence: vec!["operator_ticket".to_string()],
            }]
        );
        let missing_hash = WorkflowInstance {
            data: json!({}),
            ..pinned.clone()
        };
        assert!(recovery_targets_for_definition(&missing_hash, &definition).is_empty());
        let mismatch = WorkflowInstance {
            data: json!({ "definition_hash": "sha256:wrong" }),
            ..pinned.clone()
        };
        assert!(recovery_targets_for_definition(&mismatch, &definition).is_empty());
        let nonblocked = WorkflowInstance {
            state: "failed".to_string(),
            ..pinned
        };
        assert!(recovery_targets_for_definition(&nonblocked, &definition).is_empty());
    }

    #[tokio::test]
    async fn operator_monitor_malformed_payloads_never_grant_stopped_actions() -> anyhow::Result<()>
    {
        if !test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-malformed-stop-payload-")?;
        let store = WorkflowRuntimeStore::open_with_database_url(
            &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
            Some(&test_helpers::test_database_url()?),
        )
        .await?;
        let mut workflows = Vec::new();
        for (id, state, payload) in [
            ("malformed-blocked", "blocked", Value::Null),
            (
                "malformed-failed",
                "failed",
                Value::String("implement_issue".to_string()),
            ),
        ] {
            let mut workflow = WorkflowInstance::new(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                1,
                state,
                WorkflowSubject::new("issue", format!("issue:{id}")),
            )
            .with_id(id.to_string())
            .with_data(json!({
                "error_kind": "timeout",
                "last_stop": { "state": state, "activity": "implement_issue" },
            }));
            store.upsert_instance(&workflow).await?;
            let command = WorkflowCommand::new(
                WorkflowCommandType::EnqueueActivity,
                format!("{id}-source"),
                payload,
            );
            let command_id = store.enqueue_command(id, None, &command).await?;
            let job = store
                .enqueue_runtime_job(
                    &command_id,
                    RuntimeKind::CodexJsonrpc,
                    "codex-test",
                    command.command,
                )
                .await?;
            workflow.data["last_stop"]["runtime_job_id"] = json!(job.id);
            store.upsert_instance(&workflow).await?;
            workflows.push(workflow);
        }

        let eligibility =
            stopped_action_eligibility_for_workflows(Some(&store), &workflows).await?;
        assert!(workflows.iter().all(|workflow| eligibility
            .get(&workflow.id)
            .copied()
            .unwrap_or_default()
            == RuntimeStoppedActionEligibility::default()));
        Ok(())
    }
}
