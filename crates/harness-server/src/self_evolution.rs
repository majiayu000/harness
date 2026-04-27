use crate::handlers::learn;
use crate::http::AppState;
use crate::skill_governor;
use chrono::{Duration as ChronoDuration, Utc};
use harness_core::types::{Decision, DraftStatus, Event, EventFilters, SessionId};
use harness_protocol::methods::RpcResponse;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SelfEvolutionReport {
    pub(crate) rules_learned: usize,
    pub(crate) skills_learned: usize,
    pub(crate) skills_scored: usize,
    pub(crate) quarantined: usize,
    pub(crate) retired: usize,
    pub(crate) drafts_pending: usize,
    pub(crate) drafts_auto_adopted: usize,
    pub(crate) skills_invoked_in_window: usize,
}

/// Start periodic self-evolution ticks.
///
/// Each tick invokes the existing `learn_rules` + `learn_skills` pipeline and
/// logs an aggregate `self_evolution_tick` event for observability.
///
/// The first tick runs immediately after startup (after a brief delay to let
/// the rest of the server finish wiring up). Subsequent ticks wait `interval`
/// between runs. This ensures the learning loop is observable on every restart
/// instead of requiring the server to stay up for a full `interval` (24h by
/// default) before producing any output.
pub fn start(state: Arc<AppState>, interval: Duration) {
    tokio::spawn(async move {
        // Small warm-up delay so AppState finishes initialization (event store
        // migrations, skill discovery) before the first tick queries them.
        sleep(Duration::from_secs(30)).await;
        loop {
            if let Err(err) = run_tick(&state).await {
                tracing::error!("scheduler: periodic self-evolution tick failed: {err}");
            }
            sleep(interval).await;
        }
    });
}

pub(crate) async fn run_tick(state: &Arc<AppState>) -> anyhow::Result<SelfEvolutionReport> {
    let project_root = state.core.project_root.clone();
    let mut report = SelfEvolutionReport::default();
    let mut errors: Vec<String> = Vec::new();

    let rules_resp = learn::learn_rules(state, None, project_root.clone()).await;
    match extract_count(&rules_resp, "rules_learned") {
        Ok(count) => report.rules_learned = count,
        Err(err) => errors.push(format!("learn_rules failed: {err}")),
    }

    let skills_resp = learn::learn_skills(state, None, project_root.clone()).await;
    match extract_count(&skills_resp, "skills_learned") {
        Ok(count) => report.skills_learned = count,
        Err(err) => errors.push(format!("learn_skills failed: {err}")),
    }

    match skill_governor::run_skill_governance_tick(state).await {
        Ok(governance) => {
            report.skills_scored = governance.skills_scored;
            report.quarantined = governance.quarantined;
            report.retired = governance.retired;
        }
        Err(err) => errors.push(format!("skill_governance failed: {err}")),
    }

    // Telemetry: drafts pending at tick start.
    match state.engines.gc_agent.drafts() {
        Ok(drafts) => {
            report.drafts_pending = drafts
                .iter()
                .filter(|d| d.status == DraftStatus::Pending)
                .count();
        }
        Err(err) => errors.push(format!("draft_count failed: {err}")),
    }

    let since_window = Utc::now() - ChronoDuration::hours(24);

    // Telemetry: gc_adopt events with Complete decision in the 24h window.
    match state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("gc_adopt".to_string()),
            decision: Some(Decision::Complete),
            since: Some(since_window),
            ..EventFilters::default()
        })
        .await
    {
        Ok(adopted_events) => report.drafts_auto_adopted = adopted_events.len(),
        Err(err) => errors.push(format!("gc_adopt_count failed: {err}")),
    }

    // Telemetry: skill_used events in the 24h window.
    match state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("skill_used".to_string()),
            since: Some(since_window),
            ..EventFilters::default()
        })
        .await
    {
        Ok(skill_events) => report.skills_invoked_in_window = skill_events.len(),
        Err(err) => errors.push(format!("skill_invoked_count failed: {err}")),
    }

    let decision = if errors.is_empty() {
        Decision::Complete
    } else {
        Decision::Warn
    };
    let mut event = Event::new(
        SessionId::new(),
        "self_evolution_tick",
        "scheduler",
        decision,
    );
    event.reason = Some(format!(
        "rules={} skills={} scored={} quarantine={} retired={} drafts_pending={} drafts_adopted={} skills_invoked={}",
        report.rules_learned,
        report.skills_learned,
        report.skills_scored,
        report.quarantined,
        report.retired,
        report.drafts_pending,
        report.drafts_auto_adopted,
        report.skills_invoked_in_window,
    ));
    event.detail = Some(project_root.display().to_string());
    if !errors.is_empty() {
        event.content = Some(errors.join("\n"));
    }
    if let Err(err) = state.observability.events.log(&event).await {
        tracing::warn!(error = %err, "failed to log self_evolution_tick event");
    }

    if errors.is_empty() {
        Ok(report)
    } else {
        Err(anyhow::anyhow!(errors.join("; ")))
    }
}

fn extract_count(response: &RpcResponse, key: &str) -> anyhow::Result<usize> {
    if let Some(error) = &response.error {
        anyhow::bail!("rpc_error code={} message={}", error.code, error.message);
    }
    let result = response
        .result
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing result payload"))?;
    let raw = result
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing or invalid '{key}'"))?;
    usize::try_from(raw).map_err(|_| anyhow::anyhow!("'{key}' does not fit usize: {raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, EventFilters, ProjectId,
        RemediationType, Signal, SignalType,
    };

    fn make_pending_draft() -> Draft {
        Draft {
            id: DraftId::new(),
            status: DraftStatus::Pending,
            signal: Signal::new(
                SignalType::RepeatedWarn,
                ProjectId::new(),
                serde_json::json!({}),
                RemediationType::Guard,
            ),
            artifacts: vec![Artifact {
                artifact_type: ArtifactType::Guard,
                target_path: std::path::PathBuf::from(".harness/drafts/test.md"),
                content: "test".into(),
            }],
            rationale: "test".into(),
            validation: "test".into(),
            generated_at: chrono::Utc::now(),
            agent_model: "test".into(),
        }
    }

    fn make_test_event(hook: &str, decision: Decision) -> Event {
        let mut e = Event::new(SessionId::new(), hook, "test", decision);
        e.reason = Some(format!("test {hook}"));
        e
    }

    #[tokio::test]
    async fn run_tick_logs_summary_event_when_no_adopted_drafts() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-self-evo-data-")?;
        let project_root = crate::test_helpers::tempdir_in_home("harness-self-evo-project-")?;
        let state = crate::test_helpers::make_test_state_with_project_root(
            data_dir.path(),
            project_root.path(),
        )
        .await?;

        let report = run_tick(&state).await?;
        assert_eq!(report.rules_learned, 0);
        assert_eq!(report.skills_learned, 0);
        assert_eq!(report.skills_scored, 0);
        assert_eq!(report.drafts_pending, 0);
        assert_eq!(report.drafts_auto_adopted, 0);
        assert_eq!(report.skills_invoked_in_window, 0);

        let events = state
            .observability
            .events
            .query(&EventFilters {
                hook: Some("self_evolution_tick".to_string()),
                ..EventFilters::default()
            })
            .await?;
        let latest = events
            .iter()
            .max_by_key(|event| event.ts)
            .ok_or_else(|| anyhow::anyhow!("expected self_evolution_tick event"))?;
        assert_eq!(
            latest.reason.as_deref(),
            Some(
                "rules=0 skills=0 scored=0 quarantine=0 retired=0 drafts_pending=0 drafts_adopted=0 skills_invoked=0"
            )
        );
        let expected_detail = project_root.path().display().to_string();
        assert_eq!(latest.detail.as_deref(), Some(expected_detail.as_str()));
        Ok(())
    }

    #[tokio::test]
    async fn run_tick_counts_pending_drafts() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-self-evo-pending-")?;
        let project_root = crate::test_helpers::tempdir_in_home("harness-self-evo-pending-proj-")?;
        let state = crate::test_helpers::make_test_state_with_project_root(
            data_dir.path(),
            project_root.path(),
        )
        .await?;

        let draft = make_pending_draft();
        state.engines.gc_agent.draft_store().save(&draft)?;

        let report = run_tick(&state).await?;
        assert_eq!(report.drafts_pending, 1, "expected 1 pending draft");
        Ok(())
    }

    #[tokio::test]
    async fn run_tick_counts_adopted_in_window() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-self-evo-adopted-")?;
        let project_root = crate::test_helpers::tempdir_in_home("harness-self-evo-adopted-proj-")?;
        let state = crate::test_helpers::make_test_state_with_project_root(
            data_dir.path(),
            project_root.path(),
        )
        .await?;

        // Log one gc_adopt Complete event inside the 24h window.
        let event_in = make_test_event("gc_adopt", Decision::Complete);
        state.observability.events.log(&event_in).await?;

        // Log one gc_adopt Complete event outside the 24h window — should not be counted.
        let mut event_out = make_test_event("gc_adopt", Decision::Complete);
        event_out.ts = chrono::Utc::now() - chrono::Duration::hours(25);
        state.observability.events.log(&event_out).await?;

        let report = run_tick(&state).await?;
        assert_eq!(
            report.drafts_auto_adopted, 1,
            "only in-window gc_adopt events should be counted"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_tick_counts_skills_in_window() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-self-evo-skills-")?;
        let project_root = crate::test_helpers::tempdir_in_home("harness-self-evo-skills-proj-")?;
        let state = crate::test_helpers::make_test_state_with_project_root(
            data_dir.path(),
            project_root.path(),
        )
        .await?;

        // Log two skill_used events inside the 24h window.
        let e1 = make_test_event("skill_used", Decision::Complete);
        let e2 = make_test_event("skill_used", Decision::Complete);
        state.observability.events.log(&e1).await?;
        state.observability.events.log(&e2).await?;

        let report = run_tick(&state).await?;
        assert_eq!(
            report.skills_invoked_in_window, 2,
            "both skill_used events should be counted"
        );
        Ok(())
    }
}
