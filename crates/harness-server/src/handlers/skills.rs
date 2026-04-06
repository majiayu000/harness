use crate::http::AppState;
use harness_core::types::{EventFilters, SkillId};
use harness_protocol::{methods::RpcResponse, methods::INTERNAL_ERROR, methods::NOT_FOUND};
use harness_skills::store::SkillGovernanceStatus;
use serde::{Deserialize, Serialize};

pub async fn skill_create(
    state: &AppState,
    id: Option<serde_json::Value>,
    name: String,
    content: String,
) -> RpcResponse {
    // Reject names that could traverse outside the skills directory when used as a filename.
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.is_empty() {
        return RpcResponse::error(
            id,
            INTERNAL_ERROR,
            "skill name must not contain path separators or '..'",
        );
    }
    let mut skills = state.engines.skills.write().await;
    let skill = skills.create(name, content).clone();
    match serde_json::to_value(&skill) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn skill_list(
    state: &AppState,
    id: Option<serde_json::Value>,
    query: Option<String>,
) -> RpcResponse {
    let skills = state.engines.skills.read().await;
    let result = match query {
        Some(q) => skills.search(&q).into_iter().cloned().collect::<Vec<_>>(),
        None => skills.list().to_vec(),
    };
    match serde_json::to_value(&result) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn skill_get(
    state: &AppState,
    id: Option<serde_json::Value>,
    skill_id: SkillId,
) -> RpcResponse {
    let skills = state.engines.skills.read().await;
    match skills.get(&skill_id) {
        Some(skill) => match serde_json::to_value(skill) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        None => RpcResponse::error(id, NOT_FOUND, "skill not found"),
    }
}

pub async fn skill_delete(
    state: &AppState,
    id: Option<serde_json::Value>,
    skill_id: SkillId,
) -> RpcResponse {
    let mut skills = state.engines.skills.write().await;
    let deleted = skills.delete(&skill_id);
    RpcResponse::success(id, serde_json::json!({ "deleted": deleted }))
}

/// Governance fields projected from a single skill — authoritative current state.
#[derive(Debug, Serialize, Deserialize)]
struct GovernanceView {
    skill_id: SkillId,
    name: String,
    governance_status: SkillGovernanceStatus,
    quality_score: f64,
    scored_samples: u64,
    canary_ratio: f64,
    last_scored: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn skill_governance_view(
    state: &AppState,
    id: Option<serde_json::Value>,
    skill_id: SkillId,
) -> RpcResponse {
    let skills = state.engines.skills.read().await;
    match skills.get(&skill_id) {
        Some(skill) => {
            let view = GovernanceView {
                skill_id: skill.id.clone(),
                name: skill.name.clone(),
                governance_status: skill.governance_status,
                quality_score: skill.quality_score,
                scored_samples: skill.scored_samples,
                canary_ratio: skill.canary_ratio,
                last_scored: skill.last_scored,
            };
            match serde_json::to_value(&view) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        None => RpcResponse::error(id, NOT_FOUND, "skill not found"),
    }
}

/// A parsed status transition from a `skill_governance_tick` event.
///
/// Format written by `skill_governor.rs`:
/// `"{name}: {OldStatus:?} -> {NewStatus:?} (score={f64:.3}, samples={u64}, canary={f64:.2})"`
#[derive(Debug, Serialize, Deserialize)]
pub struct GovernanceTransition {
    pub tick_ts: chrono::DateTime<chrono::Utc>,
    pub skill_name: String,
    pub from_status: SkillGovernanceStatus,
    pub to_status: SkillGovernanceStatus,
    pub quality_score: f64,
    pub scored_samples: u64,
    pub canary_ratio: f64,
}

/// Parse one content line from a `skill_governance_tick` event.
///
/// Expected format (written by `skill_governor.rs`):
/// `"<name>: <OldStatus> -> <NewStatus> (score=<f64>, samples=<u64>, canary=<f64>)"`
///
/// Returns `None` for any line that does not match the expected structure.
fn parse_transition_line(
    line: &str,
) -> Option<(
    String,
    SkillGovernanceStatus,
    SkillGovernanceStatus,
    f64,
    u64,
    f64,
)> {
    // Split at ": " to get skill name and remainder
    let (name, rest) = line.split_once(": ")?;
    // rest: "OldStatus -> NewStatus (score=X, samples=Y, canary=Z)"
    let (statuses, params) = rest.split_once(" (")?;
    let params = params.strip_suffix(')')?;

    let (from_str, to_str) = statuses.split_once(" -> ")?;
    let from_status = parse_governance_status(from_str.trim())?;
    let to_status = parse_governance_status(to_str.trim())?;

    let mut quality_score: Option<f64> = None;
    let mut scored_samples: Option<u64> = None;
    let mut canary_ratio: Option<f64> = None;

    for token in params.split(", ") {
        if let Some(v) = token.strip_prefix("score=") {
            quality_score = v.parse().ok();
        } else if let Some(v) = token.strip_prefix("samples=") {
            scored_samples = v.parse().ok();
        } else if let Some(v) = token.strip_prefix("canary=") {
            canary_ratio = v.parse().ok();
        }
    }

    Some((
        name.to_string(),
        from_status,
        to_status,
        quality_score?,
        scored_samples?,
        canary_ratio?,
    ))
}

fn parse_governance_status(s: &str) -> Option<SkillGovernanceStatus> {
    match s {
        "Active" => Some(SkillGovernanceStatus::Active),
        "Watch" => Some(SkillGovernanceStatus::Watch),
        "Quarantine" => Some(SkillGovernanceStatus::Quarantine),
        "Retired" => Some(SkillGovernanceStatus::Retired),
        _ => None,
    }
}

pub async fn skill_governance_history(
    state: &AppState,
    id: Option<serde_json::Value>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    limit: Option<usize>,
) -> RpcResponse {
    // Direct store access is safe here: skill_governance_tick events contain only
    // governance metadata (scores, status names), never user content or secrets.
    let events = match state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("skill_governance_tick".to_string()),
            include_content: true,
            since,
            until,
            limit,
            ..EventFilters::default()
        })
        .await
    {
        Ok(evts) => evts,
        Err(e) => {
            return RpcResponse::error(id, INTERNAL_ERROR, e.to_string());
        }
    };

    let mut transitions: Vec<GovernanceTransition> = Vec::new();
    for event in events {
        let Some(content) = event.content.as_deref() else {
            continue;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match parse_transition_line(line) {
                Some((
                    skill_name,
                    from_status,
                    to_status,
                    quality_score,
                    scored_samples,
                    canary_ratio,
                )) => {
                    transitions.push(GovernanceTransition {
                        tick_ts: event.ts,
                        skill_name,
                        from_status,
                        to_status,
                        quality_score,
                        scored_samples,
                        canary_ratio,
                    });
                }
                None => {
                    tracing::warn!(
                        line,
                        "skill_governance_history: unparseable transition line"
                    );
                }
            }
        }
    }

    // Events are already returned in ascending ts order by the store; sort within each event
    // is not needed since all transitions from one event share the same tick_ts.
    transitions.sort_by_key(|t| t.tick_ts);

    match serde_json::to_value(&transitions) {
        Ok(v) => RpcResponse::success(id, v),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Parser unit tests ===

    #[test]
    fn parse_transition_line_happy() {
        let line = "my-skill: Active -> Quarantine (score=0.123, samples=42, canary=0.10)";
        let (name, from, to, score, samples, canary) =
            parse_transition_line(line).expect("should parse");
        assert_eq!(name, "my-skill");
        assert_eq!(from, SkillGovernanceStatus::Active);
        assert_eq!(to, SkillGovernanceStatus::Quarantine);
        assert!((score - 0.123).abs() < 1e-9);
        assert_eq!(samples, 42);
        assert!((canary - 0.10).abs() < 1e-9);
    }

    #[test]
    fn parse_transition_line_malformed() {
        assert!(parse_transition_line("").is_none());
        assert!(parse_transition_line("no colon here").is_none());
        assert!(parse_transition_line(
            "name: BadStatus -> Active (score=0.5, samples=1, canary=1.0)"
        )
        .is_none());
        assert!(
            parse_transition_line("name: Active -> Active (score=bad, samples=1, canary=1.0)")
                .is_none()
        );
    }

    #[test]
    fn parse_transition_line_all_statuses() {
        let statuses = ["Active", "Watch", "Quarantine", "Retired"];
        for from_s in statuses {
            for to_s in statuses {
                let line =
                    format!("skill-x: {from_s} -> {to_s} (score=0.500, samples=10, canary=1.00)");
                let (_, from, to, _, _, _) = parse_transition_line(&line)
                    .unwrap_or_else(|| panic!("should parse {from_s} -> {to_s}"));
                let expected_from = parse_governance_status(from_s).unwrap();
                let expected_to = parse_governance_status(to_s).unwrap();
                assert_eq!(from, expected_from);
                assert_eq!(to, expected_to);
            }
        }
    }

    // === Integration tests via in-process state ===

    #[tokio::test]
    async fn skill_governance_view_returns_fields() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-view-")?;
        let state = crate::test_helpers::make_test_state(data_dir.path()).await?;

        let skill_id = {
            let mut store = state.engines.skills.write().await;
            store.create(
                "view-skill".to_string(),
                "# view\n<!-- trigger-patterns: view -->".to_string(),
            );
            store
                .get_by_name("view-skill")
                .expect("skill should exist")
                .id
                .clone()
        };

        let resp = skill_governance_view(&state, Some(serde_json::json!(1)), skill_id).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.expect("result should be present");
        assert_eq!(result["name"], "view-skill");
        assert!(result["governance_status"].as_str().is_some());
        assert!(result["quality_score"].is_number());
        assert!(result["scored_samples"].is_number());
        assert!(result["canary_ratio"].is_number());
        Ok(())
    }

    #[tokio::test]
    async fn skill_governance_view_not_found() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-notfound-")?;
        let state = crate::test_helpers::make_test_state(data_dir.path()).await?;

        let missing_id = SkillId::from_str("00000000-0000-0000-0000-000000000000");
        let resp = skill_governance_view(&state, Some(serde_json::json!(1)), missing_id).await;
        assert!(resp.result.is_none());
        let err = resp.error.expect("error should be present");
        assert_eq!(err.code, NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn skill_governance_history_empty() -> anyhow::Result<()> {
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-hist-empty-")?;
        let state = crate::test_helpers::make_test_state(data_dir.path()).await?;

        let resp =
            skill_governance_history(&state, Some(serde_json::json!(1)), None, None, None).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.expect("result should be present");
        assert_eq!(result, serde_json::json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn skill_governance_history_parses_transitions() -> anyhow::Result<()> {
        use harness_core::types::{Decision, Event, SessionId};

        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-hist-parse-")?;
        let state = crate::test_helpers::make_test_state(data_dir.path()).await?;

        let mut event = Event::new(
            SessionId::new(),
            "skill_governance_tick",
            "scheduler",
            Decision::Complete,
        );
        event.content =
            Some("alpha-skill: Active -> Watch (score=0.400, samples=5, canary=1.00)".to_string());
        state.observability.events.log(&event).await?;

        let resp =
            skill_governance_history(&state, Some(serde_json::json!(1)), None, None, None).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.expect("result should be present");
        let arr = result.as_array().expect("result should be array");
        assert_eq!(arr.len(), 1);
        let t = &arr[0];
        assert_eq!(t["skill_name"], "alpha-skill");
        assert_eq!(t["from_status"], "active");
        assert_eq!(t["to_status"], "watch");
        Ok(())
    }

    #[tokio::test]
    async fn skill_governance_history_limit_respected() -> anyhow::Result<()> {
        use harness_core::types::{Decision, Event, SessionId};

        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let data_dir = crate::test_helpers::tempdir_in_home("harness-skill-gov-hist-limit-")?;
        let state = crate::test_helpers::make_test_state(data_dir.path()).await?;

        for i in 0..5u64 {
            let mut event = Event::new(
                SessionId::new(),
                "skill_governance_tick",
                "scheduler",
                Decision::Complete,
            );
            event.content = Some(format!(
                "skill-{i}: Active -> Watch (score=0.400, samples={i}, canary=1.00)"
            ));
            state.observability.events.log(&event).await?;
        }

        let resp =
            skill_governance_history(&state, Some(serde_json::json!(1)), None, None, Some(3)).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result = resp.result.expect("result should be present");
        let arr = result.as_array().expect("result should be array");
        // limit=3 means at most 3 tick events are fetched, each with 1 transition
        assert_eq!(arr.len(), 3);
        Ok(())
    }
}
