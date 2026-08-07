use super::TaskId;
use harness_workflow::runtime::{
    activity_result_value_has_closed_issue_evidence, build_issue_submission_decision,
    build_prompt_submission_decision, value_has_closed_issue_evidence,
    IssueSubmissionDecisionInput, PromptSubmissionDecisionInput, SubmissionMode, WorkflowCommand,
    WorkflowCommandType, WorkflowDecision, WorkflowEvent, WorkflowInstance, WorkflowRuntimeStore,
    RUNTIME_JOB_COMPLETED_EVENT,
};
use serde_json::json;

use super::prompt_memory::remove_prompt_submission_prompt_durable;
use super::{
    commit_runtime_decision, depends_on_strings, issue_submission_fields,
    lookup_prompt_submission_prompt_durable, optional_string_field, prompt_submission_fields,
    set_data_bool, set_data_string, task_ids_from_data, GITHUB_ISSUE_PR_DEFINITION_ID,
    PROMPT_TASK_DEFINITION_ID,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptDependencyReleaseOutcome {
    Released,
    MissingPrompt,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DependencyReleaseSummary {
    pub released: usize,
    pub failed: usize,
    pub waiting: usize,
    pub skipped: usize,
    /// Issues failed because their dependency chain forms a cycle that can
    /// never self-resolve (GH-1885).
    pub deadlocked: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeDependencyStatus {
    Done,
    Failed,
    Cancelled,
    Waiting,
}

pub(crate) async fn resolve_issue_dependency_status(
    store: Option<&WorkflowRuntimeStore>,
    task_id: &TaskId,
) -> anyhow::Result<RuntimeDependencyStatus> {
    resolve_issue_dependency_status_by_exact_id(store, task_id)
        .await
        .map(|status| status.unwrap_or(RuntimeDependencyStatus::Waiting))
}

async fn resolve_issue_dependency_status_by_exact_id(
    store: Option<&WorkflowRuntimeStore>,
    task_id: &TaskId,
) -> anyhow::Result<Option<RuntimeDependencyStatus>> {
    let Some(store) = store else {
        return Ok(None);
    };
    let Some(instance) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(None);
    };
    runtime_dependency_status_from_instance(store, &instance)
        .await
        .map(Some)
}

/// Resolve a dependency task id to its workflow instance: exact task-id
/// match first, then the canonical `github-issue:<repo>:issue:<n>` mapping.
async fn resolve_issue_dependency_instance(
    store: &WorkflowRuntimeStore,
    waiting_instance: &WorkflowInstance,
    task_id: &TaskId,
) -> anyhow::Result<Option<WorkflowInstance>> {
    if let Some(instance) = store.get_instance_by_task_id(task_id.as_str()).await? {
        return Ok(Some(instance));
    }
    canonical_github_issue_dependency_instance(store, waiting_instance, task_id).await
}

async fn runtime_dependency_status_from_instance(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
) -> anyhow::Result<RuntimeDependencyStatus> {
    Ok(match instance.state.as_str() {
        "done" => RuntimeDependencyStatus::Done,
        "failed" => RuntimeDependencyStatus::Failed,
        "cancelled" => RuntimeDependencyStatus::Cancelled,
        "blocked" if blocked_issue_dependency_is_satisfied(store, instance).await? => {
            RuntimeDependencyStatus::Done
        }
        _ => RuntimeDependencyStatus::Waiting,
    })
}

async fn blocked_issue_dependency_is_satisfied(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
) -> anyhow::Result<bool> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID || instance.state != "blocked" {
        return Ok(false);
    }
    if instance_data_has_closed_issue_evidence(&instance.data) {
        return Ok(true);
    }
    Ok(store
        .latest_event_for_type(&instance.id, RUNTIME_JOB_COMPLETED_EVENT)
        .await?
        .as_ref()
        .is_some_and(runtime_event_has_closed_issue_evidence))
}

fn instance_data_has_closed_issue_evidence(data: &serde_json::Value) -> bool {
    ["closed_issue_evidence", "issue_state"]
        .iter()
        .filter_map(|field| data.get(field))
        .any(value_has_closed_issue_evidence)
}

fn runtime_event_has_closed_issue_evidence(event: &WorkflowEvent) -> bool {
    if event.event_type != RUNTIME_JOB_COMPLETED_EVENT {
        return false;
    }
    event
        .event
        .get("activity_result")
        .is_some_and(activity_result_value_has_closed_issue_evidence)
}

async fn canonical_github_issue_dependency_instance(
    store: &WorkflowRuntimeStore,
    waiting_instance: &WorkflowInstance,
    task_id: &TaskId,
) -> anyhow::Result<Option<WorkflowInstance>> {
    let Some((dependency_repo, issue_number)) = parse_github_issue_dependency(task_id.as_str())
    else {
        return Ok(None);
    };
    let Some(project_id) = waiting_instance
        .data
        .get("project_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    let repo = waiting_instance
        .data
        .get("repo")
        .and_then(serde_json::Value::as_str);
    if dependency_repo != repo.unwrap_or("<none>") {
        return Ok(None);
    }
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(project_id, repo, issue_number);
    store.get_instance(&workflow_id).await
}

fn parse_github_issue_dependency(task_id: &str) -> Option<(&str, u64)> {
    let rest = task_id.strip_prefix("github-issue:")?;
    let (repo_key, issue_number) = rest.rsplit_once(":issue:")?;
    if repo_key.is_empty() {
        return None;
    }
    Some((repo_key, issue_number.parse().ok()?))
}

pub(crate) async fn release_ready_issue_dependencies(
    store: &WorkflowRuntimeStore,
    limit: i64,
) -> anyhow::Result<DependencyReleaseSummary> {
    let instances = store
        .list_instances_by_state(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "awaiting_dependencies",
            limit,
        )
        .await?;
    let mut summary = DependencyReleaseSummary::default();
    // Waiting instances and the edges between them, for cycle detection
    // (GH-1885): edge A -> B when A waits on B and B is itself awaiting
    // dependencies. A cycle in this subgraph can never self-resolve.
    let mut waiting_instances: Vec<WorkflowInstance> = Vec::new();
    let mut waiting_edges: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for instance in instances {
        let depends_on = match task_ids_from_data(&instance.data, "depends_on") {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed issue data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let mut all_done = true;
        let mut terminal_failure: Option<(TaskId, &'static str)> = None;
        for dep_id in &depends_on {
            let dep_instance = resolve_issue_dependency_instance(store, &instance, dep_id).await?;
            let status = match dep_instance.as_ref() {
                Some(dep) => runtime_dependency_status_from_instance(store, dep).await?,
                None => RuntimeDependencyStatus::Waiting,
            };
            match status {
                RuntimeDependencyStatus::Done => {}
                RuntimeDependencyStatus::Failed => {
                    terminal_failure = Some((dep_id.clone(), "failed"));
                    break;
                }
                RuntimeDependencyStatus::Cancelled => {
                    terminal_failure = Some((dep_id.clone(), "cancelled"));
                    break;
                }
                RuntimeDependencyStatus::Waiting => {
                    all_done = false;
                    if let Some(dep) = dep_instance {
                        if dep.state == "awaiting_dependencies" {
                            waiting_edges
                                .entry(instance.id.clone())
                                .or_default()
                                .push(dep.id.clone());
                        }
                    }
                }
            }
        }
        if let Some((dep_id, label)) = terminal_failure {
            fail_issue_for_dependency(store, instance, &dep_id, label).await?;
            summary.failed += 1;
        } else if all_done {
            release_issue_after_dependencies(store, instance, &depends_on).await?;
            summary.released += 1;
        } else {
            store.touch_instance(&instance.id).await?;
            waiting_instances.push(instance);
        }
    }
    let deadlocked = find_dependency_cycle_members(&waiting_edges);
    for instance in waiting_instances {
        if deadlocked.contains(&instance.id) {
            let cycle: Vec<&str> = deadlocked.iter().map(String::as_str).collect();
            fail_issue_for_dependency_cycle(store, instance, &cycle).await?;
            summary.deadlocked += 1;
        } else {
            summary.waiting += 1;
        }
    }
    Ok(summary)
}

/// Workflow ids that sit on a dependency cycle among mutually-waiting
/// instances. Downstream waiters that merely point *into* a cycle are left
/// alone: once cycle members fail, the existing terminal-dependency path
/// fails them on the next tick with precise evidence.
fn find_dependency_cycle_members(
    edges: &std::collections::BTreeMap<String, Vec<String>>,
) -> std::collections::BTreeSet<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: std::collections::BTreeMap<&str, Color> = edges
        .iter()
        .flat_map(|(node, targets)| {
            std::iter::once(node.as_str()).chain(targets.iter().map(String::as_str))
        })
        .map(|node| (node, Color::White))
        .collect();
    let mut members = std::collections::BTreeSet::new();

    fn visit<'a>(
        node: &'a str,
        edges: &'a std::collections::BTreeMap<String, Vec<String>>,
        color: &mut std::collections::BTreeMap<&'a str, Color>,
        stack: &mut Vec<&'a str>,
        members: &mut std::collections::BTreeSet<String>,
    ) {
        color.insert(node, Color::Gray);
        stack.push(node);
        for target in edges.get(node).map(Vec::as_slice).unwrap_or_default() {
            match color.get(target.as_str()).copied().unwrap_or(Color::White) {
                Color::White => visit(target, edges, color, stack, members),
                Color::Gray => {
                    // Back edge: everything on the stack from `target` up is
                    // on a cycle.
                    if let Some(start) = stack.iter().position(|frame| *frame == target) {
                        for frame in &stack[start..] {
                            members.insert((*frame).to_string());
                        }
                    }
                }
                Color::Black => {}
            }
        }
        stack.pop();
        color.insert(node, Color::Black);
    }

    let nodes: Vec<&str> = color.keys().copied().collect();
    for node in nodes {
        if color.get(node).copied() == Some(Color::White) {
            let mut stack = Vec::new();
            visit(node, edges, &mut color, &mut stack, &mut members);
        }
    }
    members
}

/// Fail an issue whose dependency chain forms a cycle (GH-1885). Cycles are
/// configuration errors that can never self-resolve; failing loudly with
/// evidence beats idling forever, and `failed -> scheduled` recovery edges
/// plus force-execute remain available to the operator.
async fn fail_issue_for_dependency_cycle(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    cycle_members: &[&str],
) -> anyhow::Result<()> {
    let event_payload = json!({
        "dependency_cycle_members": cycle_members,
        "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "dependency_cycle",
        "failed",
        format!(
            "issue dependency chain deadlocked: {} mutually-waiting workflows form a cycle ({})",
            cycle_members.len(),
            cycle_members.join(" -> ")
        ),
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("issue-submit:{}:dependency-cycle", instance.id),
        json!({
            "dependency_cycle_members": cycle_members,
        }),
    ))
    .high_confidence();
    let data = set_data_string(
        instance.data.clone(),
        "dependency_failure_status",
        "dependency_cycle",
    );
    commit_runtime_decision(
        store,
        instance,
        decision,
        "IssueDependencyCycleDetected",
        "workflow_runtime_submission",
        event_payload,
        Some(data),
    )
    .await?;
    Ok(())
}

pub(crate) async fn release_ready_prompt_dependencies(
    store: &WorkflowRuntimeStore,
    limit: i64,
) -> anyhow::Result<DependencyReleaseSummary> {
    let instances = store
        .list_instances_by_state(PROMPT_TASK_DEFINITION_ID, "awaiting_dependencies", limit)
        .await?;
    let mut summary = DependencyReleaseSummary::default();
    for instance in instances {
        let depends_on = match task_ids_from_data(&instance.data, "depends_on") {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed prompt data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let serialization_depends_on = match task_ids_from_data(
            &instance.data,
            "serialization_depends_on",
        ) {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed prompt serialization data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let required_depends_on = match task_ids_from_data(&instance.data, "required_depends_on") {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed prompt required dependency data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let mut all_done = true;
        let mut terminal_failure: Option<(TaskId, &'static str)> = None;
        for dep_id in &depends_on {
            match resolve_issue_dependency_status(Some(store), dep_id).await? {
                RuntimeDependencyStatus::Done => {}
                RuntimeDependencyStatus::Failed
                    if serialization_depends_on.iter().any(|id| id == dep_id)
                        && !required_depends_on.iter().any(|id| id == dep_id) => {}
                RuntimeDependencyStatus::Failed => {
                    terminal_failure = Some((dep_id.clone(), "failed"));
                    break;
                }
                RuntimeDependencyStatus::Cancelled
                    if serialization_depends_on.iter().any(|id| id == dep_id)
                        && !required_depends_on.iter().any(|id| id == dep_id) => {}
                RuntimeDependencyStatus::Cancelled => {
                    terminal_failure = Some((dep_id.clone(), "cancelled"));
                    break;
                }
                RuntimeDependencyStatus::Waiting => all_done = false,
            }
        }
        if let Some((dep_id, label)) = terminal_failure {
            fail_prompt_for_dependency(store, instance, &dep_id, label).await?;
            summary.failed += 1;
        } else if all_done {
            match release_prompt_after_dependencies(store, instance, &depends_on).await? {
                PromptDependencyReleaseOutcome::Released => summary.released += 1,
                PromptDependencyReleaseOutcome::MissingPrompt => summary.failed += 1,
            }
        } else {
            store.touch_instance(&instance.id).await?;
            summary.waiting += 1;
        }
    }
    Ok(summary)
}

async fn release_issue_after_dependencies(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    depends_on: &[TaskId],
) -> anyhow::Result<()> {
    let fields = issue_submission_fields(&instance)?;
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: &fields.task_id,
            repo: fields.repo.as_deref(),
            issue_number: fields.issue_number,
            labels: &fields.labels,
            force_execute: fields.force_execute,
            additional_prompt: fields.additional_prompt.as_deref(),
            depends_on: &depends_on_strings(depends_on),
            dependencies_blocked: false,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: fields.candidate_fanout,
        },
    );
    let event_payload = json!({
        "task_id": fields.task_id,
        "repo": fields.repo,
        "issue_number": fields.issue_number,
        "depends_on": depends_on_strings(depends_on),
        "tracker_source": fields.tracker_source,
        "tracker_external_id": fields.tracker_external_id,
        "author_trust_class": fields.author_trust_class,
        "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    let data = set_data_bool(instance.data.clone(), "dependencies_blocked", false);
    commit_runtime_decision(
        store,
        instance,
        output.decision,
        "IssueDependenciesSatisfied",
        "workflow_runtime_submission",
        event_payload,
        Some(data),
    )
    .await?;
    Ok(())
}

async fn fail_issue_for_dependency(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    dependency_id: &TaskId,
    dependency_status: &str,
) -> anyhow::Result<()> {
    let event_payload = json!({
        "dependency_task_id": dependency_id.as_str(),
        "dependency_status": dependency_status,
        "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "dependency_failed",
        "failed",
        format!(
            "dependency task {} {} before runtime issue submission could start",
            dependency_id.as_str(),
            dependency_status
        ),
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("issue-submit:{}:dependency-failed", dependency_id.as_str()),
        json!({
            "dependency_task_id": dependency_id.as_str(),
            "dependency_status": dependency_status,
        }),
    ))
    .high_confidence();
    let data = set_data_string(
        set_data_string(
            instance.data.clone(),
            "dependency_failure_task_id",
            dependency_id.as_str(),
        ),
        "dependency_failure_status",
        dependency_status,
    );
    commit_runtime_decision(
        store,
        instance,
        decision,
        "IssueDependencyFailed",
        "workflow_runtime_submission",
        event_payload,
        Some(data),
    )
    .await?;
    Ok(())
}

async fn release_prompt_after_dependencies(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    depends_on: &[TaskId],
) -> anyhow::Result<PromptDependencyReleaseOutcome> {
    let fields = prompt_submission_fields(&instance)?;
    let Some(prompt) = lookup_prompt_submission_prompt_durable(store, &fields.prompt_ref).await?
    else {
        fail_prompt_for_missing_prompt(store, instance, &fields).await?;
        return Ok(PromptDependencyReleaseOutcome::MissingPrompt);
    };
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: &fields.task_id,
            prompt: &prompt,
            prompt_ref: &fields.prompt_ref,
            source: fields.source.as_deref(),
            external_id: fields.external_id.as_deref(),
            depends_on: &depends_on_strings(depends_on),
            dependencies_blocked: false,
            continuation: fields.continuation.as_ref(),
        },
    )?;
    let event_payload = json!({
        "task_id": fields.task_id,
        "depends_on": depends_on_strings(depends_on),
        "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    let data = set_data_bool(instance.data.clone(), "dependencies_blocked", false);
    commit_runtime_decision(
        store,
        instance,
        output.decision,
        "PromptDependenciesSatisfied",
        "workflow_runtime_submission",
        event_payload,
        Some(data),
    )
    .await?;
    Ok(PromptDependencyReleaseOutcome::Released)
}

async fn fail_prompt_for_dependency(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    dependency_id: &TaskId,
    dependency_status: &str,
) -> anyhow::Result<()> {
    let prompt_ref = optional_string_field(&instance.data, "prompt_ref");
    let event_payload = json!({
        "dependency_task_id": dependency_id.as_str(),
        "dependency_status": dependency_status,
        "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "dependency_failed",
        "failed",
        format!(
            "dependency task {} {} before runtime prompt submission could start",
            dependency_id.as_str(),
            dependency_status
        ),
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("prompt-submit:{}:dependency-failed", dependency_id.as_str()),
        json!({
            "dependency_task_id": dependency_id.as_str(),
            "dependency_status": dependency_status,
        }),
    ))
    .high_confidence();
    let data = set_data_string(
        set_data_string(
            instance.data.clone(),
            "dependency_failure_task_id",
            dependency_id.as_str(),
        ),
        "dependency_failure_status",
        dependency_status,
    );
    commit_runtime_decision(
        store,
        instance,
        decision,
        "PromptDependencyFailed",
        "workflow_runtime_submission",
        event_payload,
        Some(data),
    )
    .await?;
    remove_prompt_submission_prompt_durable(store, prompt_ref.as_deref()).await?;
    Ok(())
}

async fn fail_prompt_for_missing_prompt(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    fields: &super::PromptSubmissionFields,
) -> anyhow::Result<()> {
    let event_payload = json!({
        "task_id": fields.task_id.as_str(),
        "prompt_ref": fields.prompt_ref.as_str(),
        "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "prompt_unavailable",
        "failed",
        "prompt-only runtime submission cannot resume because its in-memory prompt is unavailable",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("prompt-submit:{}:prompt-missing", fields.task_id),
        json!({
            "task_id": fields.task_id.as_str(),
            "prompt_ref": fields.prompt_ref.as_str(),
        }),
    ))
    .high_confidence();
    let data = set_data_string(
        instance.data.clone(),
        "failure_reason",
        "prompt unavailable",
    );
    commit_runtime_decision(
        store,
        instance,
        decision,
        "PromptSubmissionPromptMissing",
        "workflow_runtime_submission",
        event_payload,
        Some(data),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod cycle_tests {
    use super::find_dependency_cycle_members;
    use std::collections::BTreeMap;

    fn edges(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(node, targets)| {
                (
                    node.to_string(),
                    targets.iter().map(|target| target.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn two_node_cycle_is_detected() {
        let members = find_dependency_cycle_members(&edges(&[("a", &["b"]), ("b", &["a"])]));
        assert_eq!(
            members.into_iter().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn chain_without_cycle_is_empty() {
        let members =
            find_dependency_cycle_members(&edges(&[("a", &["b"]), ("b", &["c"]), ("c", &[])]));
        assert!(members.is_empty());
    }

    #[test]
    fn waiter_pointing_into_cycle_is_not_a_member() {
        // d waits on the a<->b cycle but is not on it; it fails via the
        // terminal-dependency path once the cycle members fail.
        let members =
            find_dependency_cycle_members(&edges(&[("a", &["b"]), ("b", &["a"]), ("d", &["a"])]));
        assert!(members.contains("a"));
        assert!(members.contains("b"));
        assert!(!members.contains("d"));
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let members = find_dependency_cycle_members(&edges(&[("a", &["a"])]));
        assert!(members.contains("a"));
    }

    #[test]
    fn disjoint_cycles_are_all_detected() {
        let members = find_dependency_cycle_members(&edges(&[
            ("a", &["b"]),
            ("b", &["a"]),
            ("x", &["y"]),
            ("y", &["z"]),
            ("z", &["x"]),
            ("lone", &[]),
        ]));
        assert_eq!(members.len(), 5);
        assert!(!members.contains("lone"));
    }
}
