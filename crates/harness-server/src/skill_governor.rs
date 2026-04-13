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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::{register_pending_task, update_status, CreateTaskRequest};
    use harness_core::types::{Decision, Event, SessionId};

    #[tokio::test]
    async fn governance_tick_quarantines_poor_performing_skill() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-data-")?;
        let project_root = crate::test_helpers::tempdir_in_home("harness-skill-gov-project-")?;
        let state = crate::test_helpers::make_test_state_with_project_root(
            data_dir.path(),
            project_root.path(),
        )
        .await?;

        let skill_id = {
            let mut store = state.engines.skills.write().await;
            store.create(
                "review-skill".to_string(),
                "# review\n<!-- trigger-patterns: review -->".to_string(),
            );
            store
                .get_by_name("review-skill")
                .expect("skill should exist")
                .id
                .clone()
        };

        for _ in 0..12 {
            let req = CreateTaskRequest {
                prompt: Some("review me".to_string()),
                project: Some(project_root.path().to_path_buf()),
                ..Default::default()
            };
            let task_id = register_pending_task(state.core.tasks.clone(), &req).await;
            update_status(&state.core.tasks, &task_id, TaskStatus::Failed, 1).await?;

            let mut event = Event::new(
                SessionId::new(),
                "skill_used",
                "task_runner",
                Decision::Pass,
            );
            event.detail = Some(format!(
                "task_id={} skill_id={}",
                task_id.as_str(),
                skill_id.as_str()
            ));
            state.observability.events.log(&event).await?;
        }

        let report = run_skill_governance_tick(&state).await?;
        assert_eq!(report.skills_scored, 1);
        assert!(report.quarantined >= 1);

        let store = state.engines.skills.read().await;
        let skill = store.get(&skill_id).expect("skill should be present");
        assert_eq!(skill.governance_status, SkillGovernanceStatus::Quarantine);
        assert!(skill.quality_score < 0.45);
        assert_eq!(skill.canary_ratio, 0.1);
        Ok(())
    }

    #[tokio::test]
    async fn governance_tick_treats_cancelled_as_unknown() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-cancel-data-")?;
        let project_root =
            crate::test_helpers::tempdir_in_home("harness-skill-gov-cancel-project-")?;
        let state = crate::test_helpers::make_test_state_with_project_root(
            data_dir.path(),
            project_root.path(),
        )
        .await?;

        let skill_id = {
            let mut store = state.engines.skills.write().await;
            store.create(
                "cancel-skill".to_string(),
                "# cancel\n<!-- trigger-patterns: cancel -->".to_string(),
            );
            store
                .get_by_name("cancel-skill")
                .expect("skill should exist")
                .id
                .clone()
        };

        let req = CreateTaskRequest {
            prompt: Some("cancel me".to_string()),
            project: Some(project_root.path().to_path_buf()),
            ..Default::default()
        };
        let task_id = register_pending_task(state.core.tasks.clone(), &req).await;
        update_status(&state.core.tasks, &task_id, TaskStatus::Cancelled, 1).await?;

        let mut used = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        used.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        state.observability.events.log(&used).await?;

        let report = run_skill_governance_tick(&state).await?;
        assert_eq!(report.skills_scored, 0);
        assert_eq!(report.samples, 0);
        assert_eq!(report.activated, 0);
        assert_eq!(report.watched, 0);
        assert_eq!(report.quarantined, 0);
        assert_eq!(report.retired, 0);

        let store = state.engines.skills.read().await;
        let skill = store.get(&skill_id).expect("skill should exist");
        assert_eq!(skill.scored_samples, 0);
        assert_eq!(skill.quality_score, 0.5);
        Ok(())
    }

    #[tokio::test]
    async fn governance_tick_does_not_double_count_without_new_events() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-dedup-data-")?;
        let project_root =
            crate::test_helpers::tempdir_in_home("harness-skill-gov-dedup-project-")?;
        let state = crate::test_helpers::make_test_state_with_project_root(
            data_dir.path(),
            project_root.path(),
        )
        .await?;

        let skill_id = {
            let mut store = state.engines.skills.write().await;
            store.create(
                "dedup-skill".to_string(),
                "# dedup\n<!-- trigger-patterns: dedup -->".to_string(),
            );
            store
                .get_by_name("dedup-skill")
                .expect("skill should exist")
                .id
                .clone()
        };

        let req = CreateTaskRequest {
            prompt: Some("dedup me".to_string()),
            project: Some(project_root.path().to_path_buf()),
            ..Default::default()
        };
        let task_id = register_pending_task(state.core.tasks.clone(), &req).await;
        update_status(&state.core.tasks, &task_id, TaskStatus::Failed, 1).await?;

        let mut used = Event::new(
            SessionId::new(),
            "skill_used",
            "task_runner",
            Decision::Pass,
        );
        used.detail = Some(format!(
            "task_id={} skill_id={}",
            task_id.as_str(),
            skill_id.as_str()
        ));
        state.observability.events.log(&used).await?;

        let first = run_skill_governance_tick(&state).await?;
        assert_eq!(first.skills_scored, 1);
        assert_eq!(first.samples, 1);

        let second = run_skill_governance_tick(&state).await?;
        assert_eq!(second.skills_scored, 0);
        assert_eq!(second.samples, 0);

        let store = state.engines.skills.read().await;
        let skill = store.get(&skill_id).expect("skill should exist");
        assert_eq!(skill.scored_samples, 1);
        Ok(())
    }
}
