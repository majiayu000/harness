use crate::handlers::learn;
use crate::http::AppState;
use crate::skill_governor;
use harness_core::types::{Decision, Event, SessionId};
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
}

/// Start periodic self-evolution ticks.
///
/// Each tick invokes the existing `learn_rules` + `learn_skills` pipeline and
/// logs an aggregate `self_evolution_tick` event for observability.
pub fn start(state: Arc<AppState>, interval: Duration) {
    tokio::spawn(async move {
        loop {
            sleep(interval).await;
            if let Err(err) = run_tick(&state).await {
                tracing::error!("scheduler: periodic self-evolution tick failed: {err}");
            }
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
        "rules={} skills={} scored={} quarantine={} retired={}",
        report.rules_learned,
        report.skills_learned,
        report.skills_scored,
        report.quarantined,
        report.retired
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
    use harness_core::types::EventFilters;

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
            Some("rules=0 skills=0 scored=0 quarantine=0 retired=0")
        );
        let expected_detail = project_root.path().display().to_string();
        assert_eq!(latest.detail.as_deref(), Some(expected_detail.as_str()));
        Ok(())
    }
}
