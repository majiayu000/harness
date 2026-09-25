//! Append-only writes for the decision audit trail (GH-1865 W1).
//!
//! A decision row is the record of what was authorized, by which decision, on
//! which observed state. Writing it with an `ON CONFLICT DO UPDATE` made that
//! record mutable: a later write reusing an existing decision id silently
//! replaced `accepted`, the serialized decision, and the rejection reason, so
//! the audit trail described the newest write rather than what actually
//! happened. Retries and replays reuse decision ids by design, which is exactly
//! when the history matters most.
//!
//! Every write now goes in once. A repeat of the same row is an idempotent
//! replay; a repeat that differs is a provenance conflict and fails closed
//! rather than overwriting the original.

use super::*;

/// A decision id that already exists with different content. The write is
/// refused: the stored row is the authoritative record of what happened.
#[derive(Debug, Clone)]
pub struct DecisionProvenanceConflict {
    pub decision_id: String,
    pub workflow_id: String,
    /// The fields that differ between the stored row and the attempted write.
    pub changed_fields: Vec<&'static str>,
}

impl std::fmt::Display for DecisionProvenanceConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "decision `{}` on workflow `{}` already exists with different provenance; \
             refusing to overwrite (differs in: {})",
            self.decision_id,
            self.workflow_id,
            self.changed_fields.join(", ")
        )
    }
}

impl std::error::Error for DecisionProvenanceConflict {}

/// The stored row a conflicting insert is compared against.
#[derive(sqlx::FromRow)]
struct StoredDecisionRow {
    workflow_id: String,
    event_id: Option<String>,
    accepted: bool,
    #[sqlx(rename = "data")]
    data_json: String,
    rejection_reason: Option<String>,
}

/// Insert a decision record once.
///
/// A row that already exists with identical content is an idempotent replay
/// and succeeds without writing; one that exists with anything else is a
/// [`DecisionProvenanceConflict`].
pub(in crate::runtime) async fn insert_decision_record_once_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &WorkflowDecisionRecord,
) -> anyhow::Result<()> {
    let data = to_jsonb_string(record)?;
    let inserted = sqlx::query(
        "INSERT INTO workflow_decisions
            (id, workflow_id, event_id, accepted, data, rejection_reason)
         VALUES ($1, $2, $3, $4, $5::jsonb, $6)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&record.id)
    .bind(&record.workflow_id)
    .bind(&record.event_id)
    .bind(record.accepted)
    .bind(&data)
    .bind(&record.rejection_reason)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        return Ok(());
    }

    let stored: Option<StoredDecisionRow> = sqlx::query_as(
        "SELECT workflow_id, event_id, accepted, data::text AS data, rejection_reason
         FROM workflow_decisions
         WHERE id = $1",
    )
    .bind(&record.id)
    .fetch_optional(&mut **tx)
    .await?;
    // The row is inserted or read back inside this transaction, so a missing
    // row here means the conflict was resolved by a delete that this store
    // never performs. Fail closed rather than retrying into a race.
    let Some(stored) = stored else {
        anyhow::bail!(
            "decision `{}` conflicted on insert but could not be read back",
            record.id
        );
    };

    let mut changed_fields = Vec::new();
    if stored.workflow_id != record.workflow_id {
        changed_fields.push("workflow_id");
    }
    if stored.event_id != record.event_id {
        changed_fields.push("event_id");
    }
    if stored.accepted != record.accepted {
        changed_fields.push("accepted");
    }
    if stored.rejection_reason != record.rejection_reason {
        changed_fields.push("rejection_reason");
    }
    // The columns are a projection of `data`, so compare the record itself
    // rather than its serialization: jsonb normalizes key order and numeric
    // formatting, which would make a byte comparison report false conflicts.
    match serde_json::from_str::<WorkflowDecisionRecord>(&stored.data_json) {
        Ok(stored_record) if stored_record != *record => changed_fields.push("data"),
        Ok(_) => {}
        Err(_) => changed_fields.push("data"),
    }

    if changed_fields.is_empty() {
        return Ok(());
    }
    Err(DecisionProvenanceConflict {
        decision_id: record.id.clone(),
        workflow_id: record.workflow_id.clone(),
        changed_fields,
    }
    .into())
}
