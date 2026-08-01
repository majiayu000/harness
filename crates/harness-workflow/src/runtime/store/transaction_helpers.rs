use super::*;

pub(in crate::runtime) fn to_jsonb_string(value: &impl Serialize) -> anyhow::Result<String> {
    crate::jsonb::to_jsonb_string(value)
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

    if insert_validated_observed_instance_tx(tx, initial_instance).await? {
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

async fn lock_instance_for_event_sequence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    // The workflow_events FK will take the same parent KEY SHARE lock during
    // INSERT. Take it before the sequence advisory lock so this writer cannot
    // deadlock with a transition that already holds the instance FOR UPDATE
    // and is waiting to allocate its event sequence.
    let _: Option<(String,)> =
        sqlx::query_as("SELECT id FROM workflow_instances WHERE id = $1 FOR KEY SHARE")
            .bind(workflow_id)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(())
}

pub(super) async fn insert_event_tx_with_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    event_type: &str,
    source: &str,
    payload: Value,
    event_id: Option<&str>,
) -> anyhow::Result<WorkflowEvent> {
    lock_instance_for_event_sequence_tx(tx, workflow_id).await?;
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

pub(super) async fn insert_validated_observed_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<bool> {
    if instance.version != 0 {
        anyhow::bail!(
            "initial workflow instance `{}` must start at version 0, got {}",
            instance.id,
            instance.version
        );
    }
    if crate::runtime::workflow_state_definition_for_instance(instance, &instance.state).is_none()
        && !persisted_declarative_state_exists_tx(tx, instance).await?
    {
        anyhow::bail!(
            "initial workflow instance `{}` uses unknown state `{}` for definition `{}` version {}",
            instance.id,
            instance.state,
            instance.definition_id,
            instance.definition_version
        );
    }
    insert_instance_row_if_absent_tx(tx, instance).await
}

pub(super) async fn insert_validated_canonical_initial_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<bool> {
    if instance.version != 0 {
        anyhow::bail!(
            "initial workflow instance `{}` must start at version 0, got {}",
            instance.id,
            instance.version
        );
    }
    let expected_state = match instance.definition_id.as_str() {
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID => Some("discovered".to_string()),
        crate::runtime::PROMPT_TASK_DEFINITION_ID => Some("submitted".to_string()),
        crate::runtime::QUALITY_GATE_DEFINITION_ID => Some("pending".to_string()),
        crate::runtime::PR_FEEDBACK_DEFINITION_ID => Some("pending".to_string()),
        _ => persisted_declarative_initial_state_tx(tx, instance).await?,
    };
    let Some(expected_state) = expected_state else {
        anyhow::bail!(
            "workflow instance `{}` has no canonical initial state for definition `{}` version {}",
            instance.id,
            instance.definition_id,
            instance.definition_version
        );
    };
    if instance.state != expected_state {
        anyhow::bail!(
            "workflow instance `{}` must use canonical initial state `{}` for definition `{}` version {}, got `{}`",
            instance.id,
            expected_state,
            instance.definition_id,
            instance.definition_version,
            instance.state
        );
    }
    insert_instance_row_if_absent_tx(tx, instance).await
}

async fn persisted_declarative_state_exists_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<bool> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT data::text
         FROM workflow_definitions
         WHERE id = $1 AND version = $2
         FOR SHARE",
    )
    .bind(&instance.definition_id)
    .bind(instance.definition_version as i64)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((data,)) = row else {
        return Ok(false);
    };
    let definition = serde_json::from_str::<crate::runtime::WorkflowDefinition>(&data)?;
    let instance_hash = instance.data.get("definition_hash").and_then(Value::as_str);
    if instance_hash != Some(definition.definition_hash.as_str()) {
        return Ok(false);
    }
    let definition =
        crate::runtime::declarative_pinning::hydrate_persisted_declarative_definition(&definition)?;
    Ok(definition
        .registered()
        .states
        .iter()
        .any(|state| state.key.state.as_ref() == instance.state))
}

async fn persisted_declarative_initial_state_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT data::text
         FROM workflow_definitions
         WHERE id = $1 AND version = $2
         FOR SHARE",
    )
    .bind(&instance.definition_id)
    .bind(instance.definition_version as i64)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((data,)) = row else {
        return Ok(None);
    };
    let definition = serde_json::from_str::<crate::runtime::WorkflowDefinition>(&data)?;
    let instance_hash = instance.data.get("definition_hash").and_then(Value::as_str);
    if instance_hash != Some(definition.definition_hash.as_str()) {
        return Ok(None);
    }
    let definition =
        crate::runtime::declarative_pinning::hydrate_persisted_declarative_definition(&definition)?;
    Ok(Some(definition.policy().initial.clone()))
}

async fn insert_instance_row_if_absent_tx(
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

pub(super) async fn commit_same_state_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    current: &WorkflowInstance,
    target: &WorkflowInstance,
) -> anyhow::Result<()> {
    ensure_instance_identity_fields_match(current, target)?;
    if current.state != target.state {
        anyhow::bail!(
            "same-state workflow write cannot change state from `{}` to `{}`",
            current.state,
            target.state
        );
    }
    if current.parent_workflow_id != target.parent_workflow_id {
        anyhow::bail!("same-state workflow write cannot change parent_workflow_id");
    }
    if current.lease != target.lease {
        anyhow::bail!("same-state workflow write cannot change lease");
    }
    require_next_instance_version(current, target)?;
    upsert_instance_row_tx(tx, target).await
}

pub(super) async fn commit_parent_attachment_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    current: &WorkflowInstance,
    target: &WorkflowInstance,
) -> anyhow::Result<()> {
    ensure_instance_identity_fields_match(current, target)?;
    if current.state != target.state || current.data != target.data || current.lease != target.lease
    {
        anyhow::bail!("parent attachment write changed fields outside parent_workflow_id");
    }
    if current.parent_workflow_id.is_some() || target.parent_workflow_id.is_none() {
        anyhow::bail!(
            "parent attachment write requires a missing current parent and a target parent"
        );
    }
    require_next_instance_version(current, target)?;
    upsert_instance_row_tx(tx, target).await
}

pub(super) async fn commit_decision_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    current: &WorkflowInstance,
    target: &WorkflowInstance,
    record: &WorkflowDecisionRecord,
    allow_idempotent_replay: bool,
) -> anyhow::Result<()> {
    if !record.accepted {
        anyhow::bail!(
            "workflow decision `{}` is rejected and cannot authorize an instance write",
            record.id
        );
    }
    if record.workflow_id != current.id
        || target.id != current.id
        || record.decision.workflow_id != current.id
    {
        anyhow::bail!("workflow decision instance write identifiers do not match");
    }
    if target == current {
        if allow_idempotent_replay && current.state == record.decision.next_state {
            return Ok(());
        }
        anyhow::bail!(
            "workflow decision `{}` cannot authorize a no-op instance write",
            record.decision.decision
        );
    }
    if current.state != record.decision.observed_state {
        anyhow::bail!(
            "workflow decision `{}` observed `{}` but current state is `{}`",
            record.decision.decision,
            record.decision.observed_state,
            current.state
        );
    }
    if target.state != record.decision.next_state {
        anyhow::bail!(
            "workflow decision `{}` authorizes `{}` but target state is `{}`",
            record.decision.decision,
            record.decision.next_state,
            target.state
        );
    }
    ensure_instance_identity_fields_match(current, target)?;
    if current.parent_workflow_id != target.parent_workflow_id {
        anyhow::bail!("workflow decision instance write cannot change parent_workflow_id");
    }
    if current.lease != target.lease && target.lease.is_some() {
        anyhow::bail!("workflow decision instance write can only preserve or release its lease");
    }
    require_next_instance_version(current, target)?;
    upsert_instance_row_tx(tx, target).await
}

pub(super) async fn commit_rejected_initial_failure_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    current: &WorkflowInstance,
    target: &WorkflowInstance,
    record: &WorkflowDecisionRecord,
) -> anyhow::Result<()> {
    if record.accepted
        || record.workflow_id != current.id
        || target.id != current.id
        || record.decision.workflow_id != current.id
    {
        anyhow::bail!("rejected initial failure instance write is not linked to its decision");
    }
    if current.state != record.decision.observed_state {
        anyhow::bail!(
            "rejected initial failure decision observed `{}` but current state is `{}`",
            record.decision.observed_state,
            current.state
        );
    }
    if current.version != 0
        || target.terminal_state() != Some(crate::runtime::WorkflowTerminalState::Failed)
    {
        anyhow::bail!(
            "rejected initial failure write requires a version-0 instance and failed target"
        );
    }
    ensure_instance_identity_fields_match(current, target)?;
    if current.parent_workflow_id != target.parent_workflow_id || current.lease != target.lease {
        anyhow::bail!("rejected initial failure write changed protected instance fields");
    }
    require_next_instance_version(current, target)?;
    upsert_instance_row_tx(tx, target).await
}

fn ensure_instance_identity_fields_match(
    current: &WorkflowInstance,
    target: &WorkflowInstance,
) -> anyhow::Result<()> {
    let mut changed_fields = Vec::new();
    if current.id != target.id {
        changed_fields.push("id");
    }
    if current.definition_id != target.definition_id {
        changed_fields.push("definition_id");
    }
    if current.definition_version != target.definition_version {
        changed_fields.push("definition_version");
    }
    if current.subject != target.subject {
        changed_fields.push("subject");
    }
    if current.created_at != target.created_at {
        changed_fields.push("created_at");
    }
    if !changed_fields.is_empty() {
        anyhow::bail!(
            "workflow instance write changes identity fields: {}",
            changed_fields.join(", ")
        );
    }
    Ok(())
}

fn require_next_instance_version(
    current: &WorkflowInstance,
    target: &WorkflowInstance,
) -> anyhow::Result<()> {
    let expected = current.version.checked_add(1).ok_or_else(|| {
        anyhow::anyhow!(
            "workflow instance `{}` version cannot advance beyond {}",
            current.id,
            current.version
        )
    })?;
    if target.version != expected {
        anyhow::bail!(
            "workflow instance `{}` target version {} must equal next version {expected}",
            current.id,
            target.version
        );
    }
    Ok(())
}

async fn upsert_instance_row_tx(
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

#[cfg(test)]
pub(super) async fn force_upsert_instance_for_test_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<()> {
    upsert_instance_row_tx(tx, instance).await
}

#[cfg(test)]
impl WorkflowRuntimeStore {
    pub(crate) async fn force_upsert_instance_for_test(
        &self,
        instance: &WorkflowInstance,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        upsert_instance_row_tx(&mut tx, instance).await?;
        tx.commit().await?;
        Ok(())
    }
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
