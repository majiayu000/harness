//! Command attempts supersede each other instead of being rewritten in place
//! (GH-1865 W2).
//!
//! A command row records an intent: this decision asked for this command with
//! this payload. Enqueueing on top of a live row used to rewrite `decision_id`,
//! `command_type`, and `data` under the same row id, so one command id could
//! describe an intent it was never minted for — and a cancelled row could be
//! fully resurrected, dispatch fields nulled, with nothing recording that it
//! had been cancelled at all.
//!
//! Intent fields are now write-once per row. A new attempt for the same dedupe
//! key marks the live row `superseded`, links it to its replacement, and
//! inserts a fresh row with the next `attempt_generation`. Dispatch status
//! stays mutable on the row that owns it.

use super::*;
use uuid::Uuid;

/// How a live row for the same dedupe key is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AttemptReplacement {
    /// Only a pending row may be replaced, and an identical re-enqueue is an
    /// idempotent replay rather than a new attempt.
    PendingIntent,
    /// A cancelled row is being re-enqueued. Re-enqueueing is itself the new
    /// attempt, so it supersedes even when the intent is unchanged — the
    /// cancelled row must stay cancelled in the history.
    ReactivateCancelled,
}

impl AttemptReplacement {
    fn replaceable_status(self) -> WorkflowCommandStatus {
        match self {
            Self::PendingIntent => WorkflowCommandStatus::Pending,
            Self::ReactivateCancelled => WorkflowCommandStatus::Cancelled,
        }
    }
}

#[derive(sqlx::FromRow)]
struct LiveAttemptRow {
    id: String,
    status: String,
    decision_id: Option<String>,
    command_type: String,
    #[sqlx(rename = "data")]
    data_json: String,
    attempt_generation: i32,
}

/// Insert a command attempt, superseding the live attempt it replaces.
///
/// Returns the id of the row that now carries the intent: the existing row when
/// this was an idempotent replay or a row in a status that cannot be replaced,
/// and the new row when an attempt was superseded.
pub(super) async fn insert_command_attempt_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    decision_id: Option<&str>,
    command: &WorkflowCommand,
    status: WorkflowCommandStatus,
    replacement: AttemptReplacement,
) -> anyhow::Result<String> {
    let data = to_jsonb_string(command)?;
    let command_type = enum_str(&command.command_type)?;

    let live: Option<LiveAttemptRow> = sqlx::query_as(
        "SELECT id, status, decision_id, command_type, data::text AS data, attempt_generation
         FROM workflow_commands
         WHERE workflow_id = $1 AND dedupe_key = $2 AND status <> $3
         FOR UPDATE",
    )
    .bind(workflow_id)
    .bind(&command.dedupe_key)
    .bind(WorkflowCommandStatus::Superseded.as_str())
    .fetch_optional(&mut **tx)
    .await?;

    let Some(live) = live else {
        return insert_attempt_row_tx(
            tx,
            &Uuid::new_v4().to_string(),
            workflow_id,
            decision_id,
            &command_type,
            &command.dedupe_key,
            status,
            &data,
            1,
        )
        .await;
    };

    if live.status != replacement.replaceable_status().as_str() {
        // Dispatched, completed, failed, blocked, skipped: the attempt is
        // already past the point where its intent could be restated, so the
        // enqueue resolves to the row that owns it.
        return Ok(live.id);
    }

    if replacement == AttemptReplacement::PendingIntent
        && intent_is_unchanged(&live, decision_id, &command_type, command)
    {
        // Same intent from the same decision. The row already carries it; only
        // its dispatch status may advance.
        if live.status != status.as_str() {
            sqlx::query(
                "UPDATE workflow_commands
                 SET status = $2, updated_at = CURRENT_TIMESTAMP
                 WHERE id = $1",
            )
            .bind(&live.id)
            .bind(status.as_str())
            .execute(&mut **tx)
            .await?;
        }
        return Ok(live.id);
    }

    // Retire the live row first: the dedupe key is unique across every row
    // that is not superseded, so the replacement cannot be inserted while the
    // row it replaces still holds that key.
    let new_id = Uuid::new_v4().to_string();
    sqlx::query(
        "UPDATE workflow_commands
         SET status = $2, superseded_by_command_id = $3, updated_at = CURRENT_TIMESTAMP
         WHERE id = $1",
    )
    .bind(&live.id)
    .bind(WorkflowCommandStatus::Superseded.as_str())
    .bind(&new_id)
    .execute(&mut **tx)
    .await?;
    insert_attempt_row_tx(
        tx,
        &new_id,
        workflow_id,
        decision_id,
        &command_type,
        &command.dedupe_key,
        status,
        &data,
        live.attempt_generation.saturating_add(1),
    )
    .await
}

/// Compare the stored payload as a command rather than as raw jsonb, which
/// normalizes key order and would report identical intents as different.
fn intent_is_unchanged(
    live: &LiveAttemptRow,
    decision_id: Option<&str>,
    command_type: &str,
    command: &WorkflowCommand,
) -> bool {
    if live.decision_id.as_deref() != decision_id || live.command_type != command_type {
        return false;
    }
    serde_json::from_str::<WorkflowCommand>(&live.data_json).is_ok_and(|stored| stored == *command)
}

#[allow(clippy::too_many_arguments)]
async fn insert_attempt_row_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
    workflow_id: &str,
    decision_id: Option<&str>,
    command_type: &str,
    dedupe_key: &str,
    status: WorkflowCommandStatus,
    data: &str,
    attempt_generation: i32,
) -> anyhow::Result<String> {
    let (id,): (String,) = sqlx::query_as(
        "INSERT INTO workflow_commands
            (id, workflow_id, decision_id, command_type, dedupe_key, status, data,
             attempt_generation)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
         RETURNING id",
    )
    .bind(id)
    .bind(workflow_id)
    .bind(decision_id)
    .bind(command_type)
    .bind(dedupe_key)
    .bind(status.as_str())
    .bind(data)
    .bind(attempt_generation)
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}
