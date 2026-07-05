fn issue_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", "123"),
    )
}

fn quality_gate_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("quality_gate", "issue:123"),
    )
}

fn prompt_task_instance(state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("prompt", "task-123"),
    )
}

fn project_issue_instance(
    project_id: &str,
    issue_number: u64,
    state: &str,
) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(format!("{project_id}::issue:{issue_number}"))
    .with_data(json!({
        "project_id": project_id,
        "issue_number": issue_number,
    }))
}

async fn enqueue_test_runtime_job(
    store: &WorkflowRuntimeStore,
    command_key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
) -> anyhow::Result<RuntimeJob> {
    enqueue_test_runtime_job_with_not_before(
        store,
        command_key,
        runtime_kind,
        runtime_profile,
        input,
        None,
    )
    .await
}

async fn enqueue_test_runtime_job_with_not_before(
    store: &WorkflowRuntimeStore,
    command_key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
    not_before: Option<DateTime<Utc>>,
) -> anyhow::Result<RuntimeJob> {
    let workflow = issue_instance("implementing").with_id(format!("test-workflow-{command_key}"));
    store.upsert_instance(&workflow).await?;
    enqueue_workflow_runtime_job(
        store,
        &workflow.id,
        command_key,
        runtime_kind,
        runtime_profile,
        input,
        not_before,
    )
    .await
}

async fn enqueue_workflow_runtime_job(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    command_key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
    not_before: Option<DateTime<Utc>>,
) -> anyhow::Result<RuntimeJob> {
    let activity = input
        .get("activity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("test_activity")
        .to_string();
    let command =
        WorkflowCommand::enqueue_activity(activity, format!("test-command-{command_key}"));
    let command_id = store.enqueue_command(workflow_id, None, &command).await?;
    store
        .enqueue_runtime_job_with_not_before(
            &command_id,
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
}

fn runtime_completion_event(
    instance: &WorkflowInstance,
    activity: &str,
    activity_result: ActivityResult,
) -> WorkflowEvent {
    WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "command": WorkflowCommand::enqueue_activity(activity, "activity-1"),
        "runtime_job_id": "job-1",
        "activity_result": activity_result,
    }))
}

fn leased_issue_instance(state: &str) -> WorkflowInstance {
    issue_instance(state).with_lease("controller-1", Utc::now() + Duration::minutes(5))
}

fn validation_context() -> ValidationContext {
    ValidationContext::new("controller-1", Utc::now())
}

struct StaticRuntimeExecutor {
    result: ActivityResult,
}

#[async_trait]
impl RuntimeJobExecutor for StaticRuntimeExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.result.clone()
    }
}

struct CountingRuntimeExecutor {
    result: ActivityResult,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl RuntimeJobExecutor for CountingRuntimeExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

struct BlockingRuntimeExecutor {
    result: ActivityResult,
    calls: Arc<AtomicUsize>,
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    finish: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl RuntimeJobExecutor for BlockingRuntimeExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = self.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        let finish = self
            .finish
            .lock()
            .unwrap()
            .take()
            .expect("blocking executor should only run once");
        let _ = finish.await;
        self.result.clone()
    }
}
