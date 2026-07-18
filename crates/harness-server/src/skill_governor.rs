use crate::http::AppState;
use crate::task_runner::TaskStatus;
use chrono::{Duration as ChronoDuration, Utc};
use harness_core::types::{Decision, Event, EventFilters, SessionId, SkillId};
use harness_skills::store::{SkillGovernanceInput, SkillGovernanceStatus};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub(crate) struct SkillGovernanceReport {
    pub(crate) skills_scored: usize,
    pub(crate) samples: u64,
    pub(crate) activated: usize,
    pub(crate) watched: usize,
    pub(crate) quarantined: usize,
    pub(crate) retired: usize,
}

pub(crate) async fn run_skill_governance_tick(
    state: &Arc<AppState>,
) -> anyhow::Result<SkillGovernanceReport> {
    let since = governance_window_start(state).await?;
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("skill_used".to_string()),
            since: Some(since),
            ..EventFilters::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to query skill_used events: {e}"))?;

    let task_statuses: HashMap<String, TaskStatus> = state
        .core
        .tasks
        .list_all_statuses_with_terminal()
        .await
        .map_err(|e| anyhow::anyhow!("failed to list tasks for governance scoring: {e}"))?
        .into_iter()
        .map(|(id, status)| (id.as_str().to_string(), status))
        .collect();

    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    let mut outcomes: HashMap<SkillId, SkillGovernanceInput> = HashMap::new();

    for event in events {
        let Some(detail) = event.detail.as_deref() else {
            continue;
        };
        let Some((task_id, skill_id)) = parse_skill_used_detail(detail) else {
            continue;
        };
        if !seen_pairs.insert((task_id.clone(), skill_id.as_str().to_string())) {
            continue;
        }
        let entry = outcomes.entry(skill_id).or_default();
        match task_statuses.get(task_id.as_str()) {
            Some(status) if status.is_success() => {
                entry.success = entry.success.saturating_add(1);
            }
            Some(status) if status.is_failure() => {
                entry.fail = entry.fail.saturating_add(1);
            }
            Some(_) | None => entry.unknown = entry.unknown.saturating_add(1),
        }
    }

    let mut report = SkillGovernanceReport::default();
    let mut status_changes: Vec<String> = Vec::new();
    let mut skills = state.engines.skills.write().await;
    for (skill_id, input) in outcomes {
        let Some(update) = skills.apply_governance_outcome(&skill_id, input) else {
            continue;
        };
        report.skills_scored += 1;
        report.samples = report.samples.saturating_add(input.scored_total());
        match update.current_status {
            SkillGovernanceStatus::Active => report.activated += 1,
            SkillGovernanceStatus::Watch => report.watched += 1,
            SkillGovernanceStatus::Quarantine => report.quarantined += 1,
            SkillGovernanceStatus::Retired => report.retired += 1,
        }
        if update.previous_status != update.current_status {
            status_changes.push(format!(
                "{}: {:?} -> {:?} (score={:.3}, samples={}, canary={:.2})",
                update.name,
                update.previous_status,
                update.current_status,
                update.quality_score,
                update.scored_samples,
                update.canary_ratio
            ));
        }
    }

    let mut event = Event::new(
        SessionId::new(),
        "skill_governance_tick",
        "scheduler",
        Decision::Complete,
    );
    event.reason = Some(format!(
        "scored={} samples={} active={} watch={} quarantine={} retired={}",
        report.skills_scored,
        report.samples,
        report.activated,
        report.watched,
        report.quarantined,
        report.retired
    ));
    if !status_changes.is_empty() {
        event.content = Some(status_changes.join("\n"));
    }
    if let Err(err) = state.observability.events.log(&event).await {
        tracing::warn!(error = %err, "failed to log skill_governance_tick event");
    }

    Ok(report)
}

async fn governance_window_start(state: &Arc<AppState>) -> anyhow::Result<chrono::DateTime<Utc>> {
    let fallback_since = Utc::now() - ChronoDuration::days(14);
    let ticks = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("skill_governance_tick".to_string()),
            since: Some(fallback_since),
            ..EventFilters::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to query skill_governance_tick events: {e}"))?;

    let latest_tick = ticks.iter().map(|event| event.ts).max();
    Ok(match latest_tick {
        Some(ts) => std::cmp::max(fallback_since, ts + ChronoDuration::microseconds(1)),
        None => fallback_since,
    })
}

fn parse_skill_used_detail(detail: &str) -> Option<(String, SkillId)> {
    let mut task_id: Option<String> = None;
    let mut skill_id: Option<SkillId> = None;
    for token in detail.split_whitespace() {
        if let Some(value) = token.strip_prefix("task_id=") {
            if !value.is_empty() {
                task_id = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = token.strip_prefix("skill_id=") {
            if !value.is_empty() {
                skill_id = Some(SkillId::from_str(value));
            }
        }
    }
    match (task_id, skill_id) {
        (Some(task_id), Some(skill_id)) => Some((task_id, skill_id)),
        _ => None,
    }
}
