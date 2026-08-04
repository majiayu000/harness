use super::{
    commit_same_state_instance_tx, insert_event_tx, insert_validated_canonical_initial_instance_tx,
    select_instance_for_update_tx, WorkflowRuntimeStore,
};
use crate::runtime::{WorkflowCommand, WorkflowCommandType, WorkflowInstance};
use serde_json::Value;

pub struct WorkflowChildStart<'a> {
    pub instance: &'a WorkflowInstance,
    pub command_id: &'a str,
    pub source: &'a str,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowChildStartOutcome {
    pub instance: WorkflowInstance,
    pub event_created: bool,
}

impl WorkflowRuntimeStore {
    pub async fn ensure_child_workflow_started(
        &self,
        request: WorkflowChildStart<'_>,
    ) -> anyhow::Result<WorkflowChildStartOutcome> {
        let payload_command_id = request.payload.get("command_id").and_then(Value::as_str);
        if payload_command_id != Some(request.command_id) {
            anyhow::bail!(
                "child workflow `{}` start payload command_id does not match `{}`",
                request.instance.id,
                request.command_id
            );
        }

        let mut tx = self.pool.begin().await?;
        let instance = match select_instance_for_update_tx(&mut tx, &request.instance.id).await? {
            Some(current) => {
                persist_existing_child_start_tx(&mut tx, current, request.instance).await?
            }
            None => {
                if insert_validated_canonical_initial_instance_tx(&mut tx, request.instance).await?
                {
                    request.instance.clone()
                } else {
                    let current = select_instance_for_update_tx(&mut tx, &request.instance.id)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "child workflow `{}` disappeared after its concurrent insert",
                                request.instance.id
                            )
                        })?;
                    persist_existing_child_start_tx(&mut tx, current, request.instance).await?
                }
            }
        };
        validate_start_command_provenance_tx(
            &mut tx,
            &instance,
            request.command_id,
            &request.payload,
        )
        .await?;

        let event_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                FROM workflow_events
                WHERE workflow_id = $1
                  AND event_type = 'ChildWorkflowStarted'
                  AND data->'event'->>'command_id' = $2
            )",
        )
        .bind(&instance.id)
        .bind(request.command_id)
        .fetch_one(&mut *tx)
        .await?;
        if !event_exists {
            insert_event_tx(
                &mut tx,
                &instance.id,
                "ChildWorkflowStarted",
                request.source,
                request.payload,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(WorkflowChildStartOutcome {
            instance,
            event_created: !event_exists,
        })
    }
}

async fn validate_start_command_provenance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    child: &WorkflowInstance,
    command_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT workflow_id, data::text
         FROM workflow_commands
         WHERE id = $1
         FOR SHARE",
    )
    .bind(command_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((parent_workflow_id, data)) = row else {
        anyhow::bail!(
            "child workflow `{}` requires persisted StartChildWorkflow command `{}`",
            child.id,
            command_id
        );
    };
    let command = serde_json::from_str::<WorkflowCommand>(&data)?;
    if command.command_type != WorkflowCommandType::StartChildWorkflow {
        anyhow::bail!(
            "child workflow `{}` command `{}` is not a StartChildWorkflow command",
            child.id,
            command_id
        );
    }
    if child.parent_workflow_id.as_deref() != Some(parent_workflow_id.as_str()) {
        anyhow::bail!(
            "child workflow `{}` parent does not match StartChildWorkflow command `{}`",
            child.id,
            command_id
        );
    }
    if payload.get("parent_workflow_id").and_then(Value::as_str)
        != Some(parent_workflow_id.as_str())
    {
        anyhow::bail!(
            "child workflow `{}` payload parent does not match StartChildWorkflow command `{}`",
            child.id,
            command_id
        );
    }
    let command_definition_id = command.command.get("definition_id").and_then(Value::as_str);
    let command_subject_key = command.command.get("subject_key").and_then(Value::as_str);
    let instance_subject_key = if child.definition_id == crate::runtime::PROMPT_TASK_DEFINITION_ID {
        command
            .command
            .get("external_id")
            .and_then(Value::as_str)
            .or(command_subject_key)
    } else {
        command_subject_key
    };
    if command_definition_id != Some(child.definition_id.as_str())
        || instance_subject_key != Some(child.subject.subject_key.as_str())
    {
        anyhow::bail!(
            "child workflow `{}` identity does not match StartChildWorkflow command `{}`",
            child.id,
            command_id
        );
    }
    if payload.get("definition_id").and_then(Value::as_str) != command_definition_id
        || payload.get("subject_key").and_then(Value::as_str) != command_subject_key
    {
        anyhow::bail!(
            "child workflow `{}` start payload does not match StartChildWorkflow command `{}`",
            child.id,
            command_id
        );
    }
    let runtime_job_id = payload
        .get("runtime_job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "child workflow `{}` start payload is missing runtime_job_id",
                child.id
            )
        })?;
    let runtime_job_command_id: Option<String> = sqlx::query_scalar(
        "SELECT command_id
         FROM runtime_jobs
         WHERE id = $1
         FOR SHARE",
    )
    .bind(runtime_job_id)
    .fetch_optional(&mut **tx)
    .await?;
    if runtime_job_command_id.as_deref() != Some(command_id) {
        anyhow::bail!(
            "child workflow `{}` runtime job `{}` does not belong to StartChildWorkflow command `{}`",
            child.id,
            runtime_job_id,
            command_id
        );
    }
    Ok(())
}

async fn persist_existing_child_start_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    current: WorkflowInstance,
    incoming: &WorkflowInstance,
) -> anyhow::Result<WorkflowInstance> {
    if current.id != incoming.id
        || current.definition_id != incoming.definition_id
        || current.definition_version != incoming.definition_version
        || current.state != incoming.state
        || current.subject != incoming.subject
        || current.parent_workflow_id != incoming.parent_workflow_id
        || current.lease != incoming.lease
    {
        anyhow::bail!(
            "child workflow `{}` start snapshot changes identity or state fields",
            incoming.id
        );
    }
    if current.version != incoming.version {
        anyhow::bail!(
            "child workflow `{}` changed from version {} to {} before start persistence",
            incoming.id,
            incoming.version,
            current.version
        );
    }
    if current.data == incoming.data {
        return Ok(current);
    }
    let mut target = current.clone();
    target.adopt_classified_data_from(incoming)?;
    target.version = current
        .version
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("child workflow `{}` version cannot advance", current.id))?;
    commit_same_state_instance_tx(tx, &current, &target).await?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};
    use harness_core::db::resolve_database_url;
    use serde_json::json;

    async fn insert_parent_start_command(
        store: &WorkflowRuntimeStore,
        parent_id: &str,
        definition_id: &str,
        subject_key: &str,
    ) -> anyhow::Result<(String, String)> {
        let parent = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "discovered",
            WorkflowSubject::new("issue_batch", parent_id),
        )
        .with_id(parent_id);
        store.upsert_instance(&parent).await?;
        let command_id = store
            .enqueue_command(
                parent_id,
                None,
                &WorkflowCommand::start_child_workflow(
                    definition_id,
                    subject_key,
                    format!("{parent_id}:start-child"),
                ),
            )
            .await?;
        let runtime_job = store
            .enqueue_runtime_job(
                &command_id,
                crate::runtime::RuntimeKind::CodexJsonrpc,
                "test",
                json!({}),
            )
            .await?;
        Ok((command_id, runtime_job.id))
    }

    #[tokio::test]
    async fn child_start_rejects_missing_start_command_provenance() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let child = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "discovered",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id("child-start-missing-command")
        .with_parent("missing-parent");

        let error = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: "missing-command",
                source: "test",
                payload: json!({
                    "command_id": "missing-command",
                    "definition_id": GITHUB_ISSUE_PR_DEFINITION_ID,
                    "subject_key": "issue:1784",
                }),
            })
            .await
            .expect_err("child creation must require a persisted StartChildWorkflow command");

        assert!(error.to_string().contains("StartChildWorkflow command"));
        assert!(store.get_instance(&child.id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn child_start_rejects_payload_parent_mismatch() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let (command_id, _runtime_job_id) = insert_parent_start_command(
            &store,
            "payload-parent",
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "issue:1784",
        )
        .await?;
        let child = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "discovered",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id("child-start-payload-parent-mismatch")
        .with_parent("payload-parent");

        let error = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: &command_id,
                source: "test",
                payload: json!({
                    "parent_workflow_id": "other-parent",
                    "runtime_job_id": "missing-job",
                    "command_id": command_id,
                    "definition_id": GITHUB_ISSUE_PR_DEFINITION_ID,
                    "subject_key": "issue:1784",
                }),
            })
            .await
            .expect_err("child start payload must identify the command parent");

        assert!(error.to_string().contains("payload parent"));
        assert!(store.get_instance(&child.id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn child_start_rejects_runtime_job_from_another_command() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let parent = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "discovered",
            WorkflowSubject::new("issue_batch", "runtime-job-parent"),
        )
        .with_id("runtime-job-parent");
        store.upsert_instance(&parent).await?;
        let first_command = WorkflowCommand::start_child_workflow(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "issue:1784",
            "runtime-job-parent:first",
        );
        let first_command_id = store
            .enqueue_command(&parent.id, None, &first_command)
            .await?;
        let other_command = WorkflowCommand::start_child_workflow(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "issue:1784",
            "runtime-job-parent:other",
        );
        let other_command_id = store
            .enqueue_command(&parent.id, None, &other_command)
            .await?;
        let other_job = store
            .enqueue_runtime_job(
                &other_command_id,
                crate::runtime::RuntimeKind::CodexJsonrpc,
                "test",
                json!({}),
            )
            .await?;
        let child = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "discovered",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id("child-start-runtime-job-mismatch")
        .with_parent(parent.id.clone());

        let error = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: &first_command_id,
                source: "test",
                payload: json!({
                    "parent_workflow_id": parent.id,
                    "runtime_job_id": other_job.id,
                    "command_id": first_command_id,
                    "definition_id": GITHUB_ISSUE_PR_DEFINITION_ID,
                    "subject_key": "issue:1784",
                }),
            })
            .await
            .expect_err("runtime job provenance must belong to the start command");

        assert!(error.to_string().contains("runtime job"));
        assert!(store.get_instance(&child.id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn child_start_is_atomic_idempotent_and_version_guarded() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let (command_id, runtime_job_id) = insert_parent_start_command(
            &store,
            "atomic-child-parent",
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "issue:1784",
        )
        .await?;
        let child = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "discovered",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id("atomic-child-start")
        .with_parent("atomic-child-parent")
        .with_server_data(json!({"started_by_command_id": command_id}));
        let payload = json!({
            "command_id": command_id,
            "runtime_job_id": runtime_job_id,
            "parent_workflow_id": "atomic-child-parent",
            "definition_id": GITHUB_ISSUE_PR_DEFINITION_ID,
            "subject_key": "issue:1784",
        });

        let created = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: &command_id,
                source: "test",
                payload: payload.clone(),
            })
            .await?;
        assert!(created.event_created);
        assert_eq!(created.instance.version, 0);
        assert_eq!(store.events_for(&child.id).await?.len(), 1);

        let replay = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: &command_id,
                source: "test",
                payload: payload.clone(),
            })
            .await?;
        assert!(!replay.event_created);
        assert_eq!(replay.instance.version, 0);
        assert_eq!(store.events_for(&child.id).await?.len(), 1);

        let updated = child
            .clone()
            .with_server_data(json!({"started_by_command_id": command_id, "attempt": 2}));
        let updated = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &updated,
                command_id: &command_id,
                source: "test",
                payload: payload.clone(),
            })
            .await?;
        assert_eq!(updated.instance.version, 1);
        assert_eq!(updated.instance.data["attempt"], 2);
        assert_eq!(store.events_for(&child.id).await?.len(), 1);

        let error = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: &command_id,
                source: "test",
                payload,
            })
            .await
            .expect_err("a stale child snapshot must not overwrite the current generation");
        assert!(error.to_string().contains("changed from version 0 to 1"));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_child_creation_converges_on_one_instance_and_event() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let (command_id, runtime_job_id) = insert_parent_start_command(
            &store,
            "concurrent-child-parent",
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "issue:1784",
        )
        .await?;
        let first = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "discovered",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id("concurrent-child-start")
        .with_parent("concurrent-child-parent")
        .with_server_data(json!({"started_by_command_id": command_id}));
        let mut second = first.clone();
        second.created_at += chrono::Duration::milliseconds(1);
        second.updated_at = second.created_at;
        let payload = json!({
            "command_id": command_id,
            "runtime_job_id": runtime_job_id,
            "parent_workflow_id": "concurrent-child-parent",
            "definition_id": GITHUB_ISSUE_PR_DEFINITION_ID,
            "subject_key": "issue:1784",
        });

        let first_start = store.ensure_child_workflow_started(WorkflowChildStart {
            instance: &first,
            command_id: &command_id,
            source: "test",
            payload: payload.clone(),
        });
        let second_start = store.ensure_child_workflow_started(WorkflowChildStart {
            instance: &second,
            command_id: &command_id,
            source: "test",
            payload,
        });
        let (first_outcome, second_outcome) = tokio::join!(first_start, second_start);
        let first_outcome = first_outcome?;
        let second_outcome = second_outcome?;

        assert_ne!(
            first_outcome.event_created, second_outcome.event_created,
            "exactly one concurrent starter should append the event"
        );
        assert_eq!(store.events_for(&first.id).await?.len(), 1);
        assert!(store.get_instance(&first.id).await?.is_some());
        Ok(())
    }
}
