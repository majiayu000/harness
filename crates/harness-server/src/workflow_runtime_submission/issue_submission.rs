use super::*;

pub(crate) struct IssueSubmissionRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub task_id: &'a TaskId,
    pub labels: &'a [String],
    pub force_execute: bool,
    pub additional_prompt: Option<&'a str>,
    pub depends_on: &'a [TaskId],
    pub dependencies_blocked: bool,
    pub source: Option<&'a str>,
    pub external_id: Option<&'a str>,
    pub remote_fact_hash: Option<&'a str>,
    pub author_trust_class: Option<IsolationTrustClass>,
}

pub(crate) async fn record_issue_submission(
    store: &WorkflowRuntimeStore,
    ctx: IssueSubmissionRuntimeContext<'_>,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    record_issue_submission_with_admission(store, ctx, || async { Ok(()) }).await
}

pub(crate) async fn record_issue_submission_with_admission<F, Fut>(
    store: &WorkflowRuntimeStore,
    ctx: IssueSubmissionRuntimeContext<'_>,
    admission: F,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    persist_issue_submission(store, &ctx, admission).await
}

async fn persist_issue_submission<F, Fut>(
    store: &WorkflowRuntimeStore,
    ctx: &IssueSubmissionRuntimeContext<'_>,
    admission: F,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, ctx.repo, ctx.issue_number);
    upsert_github_issue_pr_definition(store).await?;
    let (instance, new_instance) = match store
        .get_instance_by_issue(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            &project_id,
            ctx.repo,
            ctx.issue_number,
        )
        .await?
    {
        Some(instance) => (instance, false),
        None => (
            issue_instance(
                workflow_id,
                project_id.clone(),
                ctx.repo.map(ToOwned::to_owned),
                ctx.issue_number,
            ),
            true,
        ),
    };
    let workflow_cfg = harness_core::config::workflow::load_workflow_config(ctx.project_root)?;
    let candidate_fanout = candidate_fanout_from_policy(
        &instance.id,
        ctx.issue_number,
        ctx.labels,
        &workflow_cfg.candidates,
    )?;
    let submitted_data =
        issue_submission_data(ctx, &project_id, &instance.data, candidate_fanout.as_ref());
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: ctx.task_id.as_str(),
            repo: ctx.repo,
            issue_number: ctx.issue_number,
            labels: ctx.labels,
            force_execute: ctx.force_execute,
            additional_prompt: ctx.additional_prompt,
            depends_on: &depends_on_strings(ctx.depends_on),
            dependencies_blocked: ctx.dependencies_blocked,
            remote_fact_hash: ctx.remote_fact_hash,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout,
        },
    );
    apply_decision(
        store,
        instance,
        new_instance,
        output.decision,
        ctx,
        submitted_data,
        admission,
    )
    .await
}

async fn upsert_github_issue_pr_definition(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "GitHub issue PR workflow",
        ))
        .await
}

pub(super) fn issue_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
) -> WorkflowInstance {
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "discovered",
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id)
    .with_classified_data(
        json!({
            "project_id": project_id,
            "repo": repo,
            "issue_number": issue_number,
        }),
        DataProvenance::Server,
    )
}

pub(super) fn issue_submission_data(
    ctx: &IssueSubmissionRuntimeContext<'_>,
    project_id: &str,
    existing_data: &serde_json::Value,
    candidate_fanout: Option<&CandidateFanoutRequest>,
) -> serde_json::Value {
    let last_remote_fact_hash = ctx
        .remote_fact_hash
        .map(ToOwned::to_owned)
        .or_else(|| optional_string_field(existing_data, "last_remote_fact_hash"));
    let mut data = json!({
        "project_id": project_id,
        "repo": ctx.repo,
        "issue_number": ctx.issue_number,
        "submission_id": submission_id_for_data(existing_data, ctx.task_id),
        "task_id": ctx.task_id.as_str(),
        "task_ids": task_id_history(existing_data, ctx.task_id),
        "labels": ctx.labels,
        "force_execute": ctx.force_execute,
        "additional_prompt": ctx.additional_prompt,
        "depends_on": depends_on_strings(ctx.depends_on),
        "dependencies_blocked": ctx.dependencies_blocked,
        "source": ctx.source,
        "external_id": ctx.external_id,
        "last_remote_fact_hash": last_remote_fact_hash,
        "tracker_source": issue_tracker_source(ctx),
        "tracker_external_id": issue_tracker_external_id(ctx),
    });
    if let (Some(object), Some(candidate_fanout)) = (data.as_object_mut(), candidate_fanout) {
        object.insert("candidate_fanout".to_string(), json!(candidate_fanout));
    }
    insert_author_trust_class(&mut data, ctx.author_trust_class);
    crate::workflow_runtime_policy::merge_runtime_retry_policy(ctx.project_root, data)
}

pub(super) fn insert_author_trust_class(
    data: &mut serde_json::Value,
    author_trust_class: Option<IsolationTrustClass>,
) {
    if let (Some(object), Some(author_trust_class)) = (data.as_object_mut(), author_trust_class) {
        object.insert("author_trust_class".to_string(), json!(author_trust_class));
    }
}

pub(super) fn issue_tracker_source(
    ctx: &IssueSubmissionRuntimeContext<'_>,
) -> Option<&'static str> {
    ctx.source
        .filter(|source| source.eq_ignore_ascii_case(GITHUB_TRACKER_SOURCE))
        .map(|_| GITHUB_TRACKER_SOURCE)
}

pub(super) fn issue_tracker_external_id(ctx: &IssueSubmissionRuntimeContext<'_>) -> Option<String> {
    issue_tracker_source(ctx)?;
    Some(canonical_issue_external_id(
        ctx.external_id,
        ctx.issue_number,
    ))
}

fn canonical_issue_external_id(external_id: Option<&str>, issue_number: u64) -> String {
    let external_id = external_id
        .map(str::trim)
        .filter(|external_id| !external_id.is_empty())
        .unwrap_or("");
    if external_id.is_empty() {
        return format!("issue:{issue_number}");
    }
    if external_id.starts_with("issue:") {
        external_id.to_string()
    } else if external_id.chars().all(|ch| ch.is_ascii_digit()) {
        format!("issue:{external_id}")
    } else {
        external_id.to_string()
    }
}

#[derive(Debug)]
pub(super) struct IssueSubmissionFields {
    pub(super) task_id: String,
    pub(super) repo: Option<String>,
    pub(super) issue_number: u64,
    pub(super) labels: Vec<String>,
    pub(super) force_execute: bool,
    pub(super) additional_prompt: Option<String>,
    pub(super) tracker_source: Option<String>,
    pub(super) tracker_external_id: Option<String>,
    pub(super) author_trust_class: Option<IsolationTrustClass>,
    pub(super) candidate_fanout: Option<CandidateFanoutRequest>,
}

pub(super) fn issue_submission_fields(
    instance: &WorkflowInstance,
) -> anyhow::Result<IssueSubmissionFields> {
    Ok(IssueSubmissionFields {
        task_id: string_field(&instance.data, "task_id")?,
        repo: optional_string_field(&instance.data, "repo"),
        issue_number: instance
            .data
            .get("issue_number")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| anyhow::anyhow!("runtime issue workflow is missing issue_number"))?,
        labels: string_array_field(&instance.data, "labels")?,
        force_execute: instance
            .data
            .get("force_execute")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        additional_prompt: optional_string_field(&instance.data, "additional_prompt"),
        tracker_source: optional_string_field(&instance.data, "tracker_source"),
        tracker_external_id: optional_string_field(&instance.data, "tracker_external_id"),
        author_trust_class: author_trust_class_field(&instance.data)?,
        candidate_fanout: candidate_fanout_from_value(&instance.data)?,
    })
}

fn author_trust_class_field(
    data: &serde_json::Value,
) -> anyhow::Result<Option<IsolationTrustClass>> {
    let Some(value) = data.get("author_trust_class") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| {
            anyhow::anyhow!("runtime issue workflow has invalid author_trust_class: {error}")
        })
}
