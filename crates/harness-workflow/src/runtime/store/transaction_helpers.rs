use super::*;

pub(in crate::runtime) fn to_jsonb_string(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?.replace("\\u0000", ""))
}

pub(in crate::runtime) fn enum_str(value: &impl Serialize) -> anyhow::Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("serialized enum did not produce a string"))
}

pub(super) async fn runtime_job_for_command_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
) -> anyhow::Result<Option<RuntimeJob>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT data::text FROM runtime_jobs
         WHERE command_id = $1
         ORDER BY created_at DESC, (data->>'created_at')::timestamptz DESC
         LIMIT 1",
    )
    .bind(command_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}

pub(super) async fn insert_decision_record_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &WorkflowDecisionRecord,
) -> anyhow::Result<()> {
    let data = to_jsonb_string(record)?;
    sqlx::query(
        "INSERT INTO workflow_decisions
            (id, workflow_id, event_id, accepted, data, rejection_reason)
         VALUES ($1, $2, $3, $4, $5::jsonb, $6)
         ON CONFLICT (id) DO UPDATE SET
            accepted = EXCLUDED.accepted,
            data = EXCLUDED.data,
            rejection_reason = EXCLUDED.rejection_reason",
    )
    .bind(&record.id)
    .bind(&record.workflow_id)
    .bind(&record.event_id)
    .bind(record.accepted)
    .bind(&data)
    .bind(&record.rejection_reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn load_or_insert_initial_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    expected_state: &str,
    create_if_missing: Option<&WorkflowInstance>,
) -> anyhow::Result<Option<WorkflowInstance>> {
    if let Some(instance) = select_instance_for_update_tx(tx, workflow_id).await? {
        return Ok(Some(instance));
    }

    let Some(initial_instance) = create_if_missing else {
        return Ok(None);
    };
    if initial_instance.id != workflow_id {
        anyhow::bail!(
            "initial workflow instance `{}` does not match workflow `{}`",
            initial_instance.id,
            workflow_id
        );
    }
    if initial_instance.state != expected_state {
        return Ok(None);
    }

    if insert_instance_if_absent_tx(tx, initial_instance).await? {
        return Ok(Some(initial_instance.clone()));
    }

    select_instance_for_update_tx(tx, workflow_id).await
}

pub(super) async fn select_instance_for_update_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<Option<WorkflowInstance>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
            .bind(workflow_id)
            .fetch_optional(&mut **tx)
            .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}

pub(in crate::runtime) async fn insert_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    event_type: &str,
    source: &str,
    payload: Value,
) -> anyhow::Result<WorkflowEvent> {
    insert_event_tx_with_id(tx, workflow_id, event_type, source, payload, None).await
}

pub(super) async fn insert_event_tx_with_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    event_type: &str,
    source: &str,
    payload: Value,
    event_id: Option<&str>,
) -> anyhow::Result<WorkflowEvent> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("workflow_events:{workflow_id}"))
        .execute(&mut **tx)
        .await?;
    let (next_sequence,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_one(&mut **tx)
    .await?;
    let mut event = WorkflowEvent::new(workflow_id, next_sequence as u64, event_type, source)
        .with_payload(payload);
    if let Some(event_id) = event_id {
        event.id = event_id.to_string();
    }
    let event_data = to_jsonb_string(&event)?;
    sqlx::query(
        "INSERT INTO workflow_events
            (id, workflow_id, sequence, event_type, source, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind(&event.id)
    .bind(&event.workflow_id)
    .bind(event.sequence as i64)
    .bind(&event.event_type)
    .bind(&event.source)
    .bind(&event_data)
    .execute(&mut **tx)
    .await?;
    Ok(event)
}

pub(super) async fn insert_instance_if_absent_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<bool> {
    let data = to_jsonb_string(instance)?;
    let result = sqlx::query(
        "INSERT INTO workflow_instances
            (id, definition_id, state, subject_type, subject_key, parent_workflow_id, data, version)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&instance.id)
    .bind(&instance.definition_id)
    .bind(&instance.state)
    .bind(&instance.subject.subject_type)
    .bind(&instance.subject.subject_key)
    .bind(&instance.parent_workflow_id)
    .bind(&data)
    .bind(instance.version as i64)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn upsert_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<()> {
    let data = to_jsonb_string(instance)?;
    sqlx::query(
        "INSERT INTO workflow_instances
            (id, definition_id, state, subject_type, subject_key, parent_workflow_id, data, version)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
         ON CONFLICT (id) DO UPDATE SET
            definition_id = EXCLUDED.definition_id,
            state = EXCLUDED.state,
            subject_type = EXCLUDED.subject_type,
            subject_key = EXCLUDED.subject_key,
            parent_workflow_id = EXCLUDED.parent_workflow_id,
            data = EXCLUDED.data,
            version = EXCLUDED.version,
            updated_at = CURRENT_TIMESTAMP",
    )
    .bind(&instance.id)
    .bind(&instance.definition_id)
    .bind(&instance.state)
    .bind(&instance.subject.subject_type)
    .bind(&instance.subject.subject_key)
    .bind(&instance.parent_workflow_id)
    .bind(&data)
    .bind(instance.version as i64)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) fn apply_inline_command_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    match command.command_type {
        WorkflowCommandType::BindPr => apply_bind_pr_side_effect(instance, command),
        WorkflowCommandType::MarkDone => apply_mark_done_side_effect(instance, command),
        WorkflowCommandType::MarkFailed
        | WorkflowCommandType::MarkBlocked
        | WorkflowCommandType::MarkCancelled => {
            super::super::worker::apply_failure_reason_side_effect(instance, command)
        }
        _ => Ok(()),
    }
}

fn apply_bind_pr_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    let pr_number = command
        .command
        .get("pr_number")
        .and_then(Value::as_u64)
        .context("bind_pr command missing pr_number")?;
    let pr_url = command
        .command
        .get("pr_url")
        .and_then(Value::as_str)
        .context("bind_pr command missing pr_url")?;

    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("pr_number".to_string(), json!(pr_number));
    data.insert("pr_url".to_string(), json!(pr_url));
    Ok(())
}

fn apply_mark_done_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    let Some(closed_issue_evidence) = command.command.get("closed_issue_evidence").cloned() else {
        return Ok(());
    };
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("closed_issue_evidence".to_string(), closed_issue_evidence);
    Ok(())
}
