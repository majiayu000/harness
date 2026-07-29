use super::EventStore;
use chrono::{DateTime, Utc};
use harness_core::run_id::RunId;
use harness_core::types::{Decision, Event, EventFilters, EventId, SessionId};
use sqlx::{Postgres, QueryBuilder, Row};
use std::str::FromStr;

struct PreparedEventInsert<'a> {
    id: &'a str,
    ts: DateTime<Utc>,
    session_id: &'a str,
    run_id: Option<&'a str>,
    hook: &'a str,
    tool: &'a str,
    decision: &'static str,
    reason: Option<&'a str>,
    detail: Option<&'a str>,
    duration_ms: Option<i64>,
    content: Option<&'a str>,
    metadata: Option<String>,
}

impl<'a> PreparedEventInsert<'a> {
    fn from_event(event: &'a Event) -> anyhow::Result<Self> {
        let metadata = match &event.metadata {
            Some(metadata) => Some(serde_json::to_string(metadata)?),
            None => None,
        };
        Ok(Self {
            id: event.id.as_str(),
            ts: event.ts,
            session_id: event.session_id.as_str(),
            run_id: event.run_id.as_ref().map(RunId::as_str),
            hook: event.hook.as_str(),
            tool: event.tool.as_str(),
            decision: decision_to_db_str(event.decision),
            reason: event.reason.as_deref(),
            detail: event.detail.as_deref(),
            duration_ms: event.duration_ms.map(|v| v as i64),
            content: event.content.as_deref(),
            metadata,
        })
    }
}

impl EventStore {
    pub(super) async fn insert_event(&self, event: &Event) -> anyhow::Result<()> {
        self.insert_events(std::slice::from_ref(event)).await
    }

    pub(super) async fn insert_events(&self, events: &[Event]) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let rows = events
            .iter()
            .map(PreparedEventInsert::from_event)
            .collect::<anyhow::Result<Vec<_>>>()?;
        for chunk in rows.chunks(1_000) {
            let mut builder = QueryBuilder::<Postgres>::new(
                "INSERT INTO events
                    (store_key, id, ts, session_id, run_id, hook, tool, decision, reason, detail, duration_ms, content, metadata) ",
            );
            builder.push_values(chunk, |mut values, row| {
                values
                    .push_bind(&self.store_key)
                    .push_bind(row.id)
                    .push_bind(row.ts)
                    .push_bind(row.session_id)
                    .push_bind(row.run_id)
                    .push_bind(row.hook)
                    .push_bind(row.tool)
                    .push_bind(row.decision)
                    .push_bind(row.reason)
                    .push_bind(row.detail)
                    .push_bind(row.duration_ms)
                    .push_bind(row.content)
                    .push_bind(row.metadata.as_deref());
            });
            builder.push(" ON CONFLICT (store_key, id) DO NOTHING");
            builder.build().execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn log(&self, event: &Event) -> anyhow::Result<EventId> {
        self.insert_event(event).await?;
        let slot = self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipeline) = slot.as_ref() {
            pipeline.record_event(event);
        }
        Ok(event.id.clone())
    }

    pub async fn log_many(&self, events: &[Event]) -> anyhow::Result<Vec<EventId>> {
        self.insert_events(events).await?;
        let slot = self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipeline) = slot.as_ref() {
            for event in events {
                pipeline.record_event(event);
            }
        }
        Ok(events.iter().map(|event| event.id.clone()).collect())
    }

    pub async fn query(&self, filters: &EventFilters) -> anyhow::Result<Vec<Event>> {
        let content_col = if filters.include_content {
            "content"
        } else {
            "CAST(NULL AS TEXT) as content"
        };
        let mut sql = format!(
            "SELECT id, ts, session_id, run_id, hook, tool, decision, reason, detail, duration_ms, {content_col}, metadata
             FROM events WHERE store_key = $1",
        );
        let mut param_count = 1usize;
        if filters.session_id.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND session_id = ${param_count}"));
        }
        if filters.run_id.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND run_id = ${param_count}"));
        }
        if filters.hook.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND hook = ${param_count}"));
        }
        if filters.tool.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND tool = ${param_count}"));
        }
        if filters.decision.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND decision = ${param_count}"));
        }
        if filters.since.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND ts >= ${param_count}"));
        }
        if filters.until.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND ts <= ${param_count}"));
        }

        sql.push_str(" ORDER BY ts ASC");

        if filters.limit.is_some() {
            param_count += 1;
            sql.push_str(&format!(" LIMIT ${param_count}"));
        }

        let mut q = sqlx::query(&sql).bind(&self.store_key);

        if let Some(ref sid) = filters.session_id {
            q = q.bind(sid.as_str());
        }
        if let Some(ref run_id) = filters.run_id {
            q = q.bind(run_id.as_str());
        }
        if let Some(ref hook) = filters.hook {
            q = q.bind(hook.as_str());
        }
        if let Some(ref tool) = filters.tool {
            q = q.bind(tool.as_str());
        }
        if let Some(decision) = filters.decision {
            q = q.bind(decision_to_db_str(decision));
        }
        if let Some(since) = filters.since {
            q = q.bind(since);
        }
        if let Some(until) = filters.until {
            q = q.bind(until);
        }
        if let Some(limit) = filters.limit {
            let limit = i64::try_from(limit)
                .map_err(|_| anyhow::anyhow!("event query limit exceeds i64::MAX"))?;
            q = q.bind(limit);
        }

        let rows = q.fetch_all(&self.pool).await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event = Self::row_to_event(&row)?;
            events.push(event);
        }
        Ok(events)
    }

    pub async fn policy_events_for_run(&self, run_id: &RunId) -> anyhow::Result<Vec<Event>> {
        self.query(&EventFilters {
            run_id: Some(run_id.clone()),
            ..Default::default()
        })
        .await
    }

    pub async fn policy_events_for_agent_run(
        &self,
        run_id: &RunId,
        agent_tool: &str,
    ) -> anyhow::Result<Vec<Event>> {
        self.query(&EventFilters {
            run_id: Some(run_id.clone()),
            tool: Some(agent_tool.to_string()),
            ..Default::default()
        })
        .await
    }

    fn row_to_event(row: &sqlx::postgres::PgRow) -> anyhow::Result<Event> {
        let id: String = row.try_get("id")?;
        let ts: DateTime<Utc> = row.try_get("ts")?;
        let session_id: String = row.try_get("session_id")?;
        let run_id: Option<String> = row.try_get("run_id")?;
        let hook: String = row.try_get("hook")?;
        let tool: String = row.try_get("tool")?;
        let decision_str: String = row.try_get("decision")?;
        let reason: Option<String> = row.try_get("reason")?;
        let detail: Option<String> = row.try_get("detail")?;
        let content: Option<String> = row.try_get("content")?;
        let metadata_json: Option<String> = row.try_get("metadata")?;
        let duration_ms: Option<i64> = row.try_get("duration_ms")?;

        Ok(Event {
            id: EventId::from_str(&id),
            ts,
            session_id: SessionId::from_str(&session_id),
            run_id: run_id.as_deref().map(RunId::from_str).transpose()?,
            hook,
            tool,
            decision: decision_from_db_str(&decision_str)?,
            reason,
            detail,
            content,
            metadata: metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            duration_ms: duration_ms.map(|v| v as u64),
        })
    }
}

fn decision_to_db_str(decision: Decision) -> &'static str {
    match decision {
        Decision::Pass => "pass",
        Decision::Warn => "warn",
        Decision::Block => "block",
        Decision::Gate => "gate",
        Decision::Escalate => "escalate",
        Decision::Complete => "complete",
    }
}

fn decision_from_db_str(value: &str) -> anyhow::Result<Decision> {
    match value {
        "pass" => Ok(Decision::Pass),
        "warn" => Ok(Decision::Warn),
        "block" => Ok(Decision::Block),
        "gate" => Ok(Decision::Gate),
        "escalate" => Ok(Decision::Escalate),
        "complete" => Ok(Decision::Complete),
        _ => anyhow::bail!("invalid decision '{value}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_db_mapping_covers_all_variants() {
        for (decision, value) in [
            (Decision::Pass, "pass"),
            (Decision::Warn, "warn"),
            (Decision::Block, "block"),
            (Decision::Gate, "gate"),
            (Decision::Escalate, "escalate"),
            (Decision::Complete, "complete"),
        ] {
            assert_eq!(decision_to_db_str(decision), value);
            assert_eq!(decision_from_db_str(value).unwrap(), decision);
        }
        assert!(decision_from_db_str("\"pass\"").is_err());
    }
}
