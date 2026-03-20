use crate::{http::AppState, validate_root};
use anyhow::anyhow;
use chrono::{DateTime, Utc};
use harness_core::{
    AgentRequest, CapabilityProfile, CodeAgent, Event, EventFilters, SessionId, TaskComplexity,
};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use harness_rules::engine::RuleEngine;
use harness_skills::SkillStore;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Result of a preflight analysis run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightResult {
    pub constraints: Vec<String>,
    pub affected_files: Vec<PathBuf>,
    pub baseline_violations: usize,
    pub baseline_scan_timestamp: DateTime<Utc>,
    pub baseline_scan_source: String,
    pub baseline_scan_session_id: SessionId,
    pub recommended_complexity: TaskComplexity,
}

#[derive(Debug, Clone)]
struct ParsedPreflightOutput {
    constraints: Vec<String>,
    affected_files: Vec<PathBuf>,
    recommended_complexity: TaskComplexity,
}

#[derive(Debug, Clone)]
struct BaselineScanSnapshot {
    violation_count: usize,
    scanned_at: DateTime<Utc>,
    source: String,
    session_id: SessionId,
}

/// Run a preflight analysis for the given task description.
///
/// Loads the 'preflight' skill, gathers active rules, then asks the agent to
/// produce a structured constraint set for the task before any code is written.
pub async fn run_preflight(
    agent: Arc<dyn CodeAgent>,
    skills: Arc<RwLock<SkillStore>>,
    rules: Arc<RwLock<RuleEngine>>,
    events: Arc<harness_observe::EventStore>,
    project_root: PathBuf,
    task_description: String,
) -> anyhow::Result<PreflightResult> {
    let skill_content = {
        let store = skills.read().await;
        match store.list().iter().find(|s| s.name == "preflight") {
            Some(s) => s.content.clone(),
            None => {
                tracing::warn!(
                    "preflight skill not found in store; proceeding with empty skill content"
                );
                String::new()
            }
        }
    };

    let rules_summary = {
        let engine = rules.read().await;
        engine
            .rules()
            .iter()
            .map(|r| format!("- {}: {}", r.id.as_str(), r.title))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let safe_desc = harness_core::prompts::wrap_external_data(&task_description);
    let prompt = format!(
        "{skill_content}\n\n\
         ## Task Description\n\
         {safe_desc}\n\n\
         ## Active Rules\n\
         {rules_summary}\n\n\
         ## Instructions\n\
         Analyze the task description and project context. \
         Respond with EXACTLY these four sections (one item per line prefixed with '- '):\n\
         CONSTRAINTS:\n- <constraint>\n\
         AFFECTED_FILES:\n- <relative file path>\n\
         RISK: <low|medium|high>\n\
         COMPLEXITY: <simple|medium|complex|critical>"
    );

    // Inject capability restriction note — primary enforcement since --allowedTools
    // is not passed to the CLI (issue #483).
    let prompt = if let Some(note) = CapabilityProfile::ReadOnly.prompt_note() {
        format!("{note}\n\n{prompt}")
    } else {
        prompt
    };
    let req = AgentRequest {
        prompt,
        project_root,
        allowed_tools: CapabilityProfile::ReadOnly.tools().unwrap_or_default(),
        ..Default::default()
    };

    let resp = agent.execute(req).await?;
    let parsed = parse_preflight_output(&resp.output)?;
    let baseline = latest_baseline_scan(events.as_ref()).await?;

    Ok(PreflightResult {
        constraints: parsed.constraints,
        affected_files: parsed.affected_files,
        baseline_violations: baseline.violation_count,
        baseline_scan_timestamp: baseline.scanned_at,
        baseline_scan_source: baseline.source,
        baseline_scan_session_id: baseline.session_id,
        recommended_complexity: parsed.recommended_complexity,
    })
}

fn parse_preflight_output(output: &str) -> anyhow::Result<ParsedPreflightOutput> {
    let mut constraints: Vec<String> = Vec::new();
    let mut affected_files: Vec<PathBuf> = Vec::new();
    let mut complexity_str = "simple";
    let mut current_section = "";

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("CONSTRAINTS:") {
            current_section = "constraints";
        } else if trimmed.starts_with("AFFECTED_FILES:") {
            current_section = "affected_files";
        } else if trimmed.starts_with("RISK:") {
            current_section = "";
        } else if let Some(rest) = trimmed.strip_prefix("COMPLEXITY:") {
            complexity_str = rest.trim();
            current_section = "";
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            let item = item.trim();
            match current_section {
                "constraints" => constraints.push(item.to_string()),
                "affected_files" => affected_files.push(PathBuf::from(item)),
                _ => {}
            }
        }
    }

    let recommended_complexity = match complexity_str.to_lowercase().as_str() {
        "complex" => TaskComplexity::Complex,
        "critical" => TaskComplexity::Critical,
        "medium" => TaskComplexity::Medium,
        _ => TaskComplexity::Simple,
    };

    Ok(ParsedPreflightOutput {
        constraints,
        affected_files,
        recommended_complexity,
    })
}

async fn latest_baseline_scan(
    events: &harness_observe::EventStore,
) -> anyhow::Result<BaselineScanSnapshot> {
    let all_events = events
        .query(&EventFilters::default())
        .await
        .map_err(|e| anyhow!("failed to query event store for baseline scan: {e}"))?;

    select_latest_baseline_scan(&all_events).ok_or_else(|| {
        anyhow!("no baseline scan results found in event store; run rule_check before preflight")
    })
}

fn select_latest_baseline_scan(events: &[Event]) -> Option<BaselineScanSnapshot> {
    if let Some(scan) = events.iter().rev().find(|e| e.hook == "rule_scan") {
        let violation_count = events
            .iter()
            .filter(|e| e.hook == "rule_check" && e.session_id == scan.session_id)
            .count();

        return Some(BaselineScanSnapshot {
            violation_count,
            scanned_at: scan.ts,
            source: format!("event_store:{}:{}", scan.hook, scan.tool),
            session_id: scan.session_id.clone(),
        });
    }

    let latest_rule_check = events.iter().rev().find(|e| e.hook == "rule_check")?;
    let session_id = latest_rule_check.session_id.clone();
    let (violation_count, scanned_at) = events
        .iter()
        .filter(|e| e.hook == "rule_check" && e.session_id == session_id)
        .fold((0usize, latest_rule_check.ts), |(count, max_ts), event| {
            (count + 1, max_ts.max(event.ts))
        });

    Some(BaselineScanSnapshot {
        violation_count,
        scanned_at,
        source: "event_store:rule_check_fallback".to_string(),
        session_id,
    })
}

/// RPC handler: validate project_root then run preflight analysis.
pub async fn preflight(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
    task_description: String,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id);

    let agent = match state.core.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };

    match run_preflight(
        agent,
        state.engines.skills.clone(),
        state.engines.rules.clone(),
        state.observability.events.clone(),
        project_root,
        task_description,
    )
    .await
    {
        Ok(result) => match serde_json::to_value(&result) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{
        AgentRequest, AgentResponse, Capability, CodeAgent, Decision, Result as HarnessResult,
        StreamItem, TokenUsage,
    };
    use tokio::sync::mpsc::Sender;

    struct StaticAgent {
        output: String,
    }

    #[async_trait::async_trait]
    impl CodeAgent for StaticAgent {
        fn name(&self) -> &str {
            "static-agent"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: self.output.clone(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    fn sample_output() -> String {
        "\
CONSTRAINTS:\n- No breaking API changes\n- Must add tests\n\
AFFECTED_FILES:\n- src/lib.rs\n- src/handler.rs\n\
RISK: medium\n\
COMPLEXITY: complex"
            .to_string()
    }

    #[test]
    fn parse_output_with_all_sections() {
        let output = sample_output();
        let result = parse_preflight_output(&output).expect("parse");
        assert_eq!(
            result.constraints,
            vec!["No breaking API changes", "Must add tests"]
        );
        assert_eq!(
            result.affected_files,
            vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/handler.rs")]
        );
        assert_eq!(result.recommended_complexity, TaskComplexity::Complex);
    }

    #[test]
    fn parse_output_critical_complexity() {
        let output = "CONSTRAINTS:\nAFFECTED_FILES:\nRISK: high\nCOMPLEXITY: critical";
        let result = parse_preflight_output(output).expect("parse");
        assert_eq!(result.recommended_complexity, TaskComplexity::Critical);
    }

    #[test]
    fn parse_output_low_risk_simple() {
        let output = "CONSTRAINTS:\n- Keep it minimal\nAFFECTED_FILES:\n- README.md\nRISK: low\nCOMPLEXITY: simple";
        let result = parse_preflight_output(output).expect("parse");
        assert_eq!(result.recommended_complexity, TaskComplexity::Simple);
    }

    #[test]
    fn parse_output_empty_sections() {
        let output = "CONSTRAINTS:\nAFFECTED_FILES:\nRISK: low\nCOMPLEXITY: simple";
        let result = parse_preflight_output(output).expect("parse");
        assert!(result.constraints.is_empty());
        assert!(result.affected_files.is_empty());
    }

    #[test]
    fn select_latest_scan_prefers_rule_scan_session() {
        let first_session = SessionId::new();
        let latest_session = SessionId::new();
        let mut events = vec![Event::new(
            first_session.clone(),
            "rule_scan",
            "RuleEngine",
            Decision::Warn,
        )];
        events.push(Event::new(
            first_session.clone(),
            "rule_check",
            "SEC-01",
            Decision::Block,
        ));
        events.push(Event::new(
            latest_session.clone(),
            "rule_scan",
            "RuleEngine",
            Decision::Warn,
        ));
        events.push(Event::new(
            latest_session.clone(),
            "rule_check",
            "PERF-01",
            Decision::Warn,
        ));
        events.push(Event::new(
            latest_session.clone(),
            "rule_check",
            "SEC-02",
            Decision::Block,
        ));

        let snapshot = select_latest_baseline_scan(&events).expect("snapshot");
        assert_eq!(snapshot.violation_count, 2);
        assert_eq!(snapshot.session_id, latest_session);
        assert_eq!(snapshot.source, "event_store:rule_scan:RuleEngine");
    }

    #[test]
    fn select_latest_scan_falls_back_to_rule_check_session() {
        let first_session = SessionId::new();
        let latest_session = SessionId::new();
        let mut events = vec![Event::new(
            first_session.clone(),
            "rule_check",
            "SEC-01",
            Decision::Block,
        )];
        events.push(Event::new(
            latest_session.clone(),
            "rule_check",
            "SEC-02",
            Decision::Block,
        ));
        events.push(Event::new(
            latest_session.clone(),
            "rule_check",
            "PERF-01",
            Decision::Warn,
        ));

        let snapshot = select_latest_baseline_scan(&events).expect("snapshot");
        assert_eq!(snapshot.violation_count, 2);
        assert_eq!(snapshot.session_id, latest_session);
        assert_eq!(snapshot.source, "event_store:rule_check_fallback");
    }

    #[tokio::test]
    async fn run_preflight_uses_scan_result_from_event_store() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let events = Arc::new(harness_observe::EventStore::new(temp.path()).await?);
        let session_id = SessionId::new();

        let scan_event = Event::new(
            session_id.clone(),
            "rule_scan",
            "RuleEngine",
            Decision::Warn,
        );
        events.log(&scan_event).await?;
        events
            .log(&Event::new(
                session_id.clone(),
                "rule_check",
                "SEC-01",
                Decision::Block,
            ))
            .await?;
        events
            .log(&Event::new(
                session_id.clone(),
                "rule_check",
                "SEC-02",
                Decision::Warn,
            ))
            .await?;

        let result = run_preflight(
            Arc::new(StaticAgent {
                output: sample_output(),
            }),
            Arc::new(RwLock::new(SkillStore::new())),
            Arc::new(RwLock::new(RuleEngine::new())),
            events,
            temp.path().to_path_buf(),
            "test task".to_string(),
        )
        .await?;

        assert_eq!(result.baseline_violations, 2);
        assert_eq!(
            result.baseline_scan_source,
            "event_store:rule_scan:RuleEngine"
        );
        assert_eq!(result.baseline_scan_session_id, session_id);
        assert_eq!(result.recommended_complexity, TaskComplexity::Complex);
        Ok(())
    }

    #[tokio::test]
    async fn run_preflight_errors_when_no_scan_result_exists() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let err = run_preflight(
            Arc::new(StaticAgent {
                output: sample_output(),
            }),
            Arc::new(RwLock::new(SkillStore::new())),
            Arc::new(RwLock::new(RuleEngine::new())),
            Arc::new(harness_observe::EventStore::new(temp.path()).await?),
            temp.path().to_path_buf(),
            "test task".to_string(),
        )
        .await
        .expect_err("expected no-scan error");

        assert!(
            err.to_string()
                .contains("no baseline scan results found in event store"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_preflight_errors_when_event_store_query_fails() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;

        let err = run_preflight(
            Arc::new(StaticAgent {
                output: sample_output(),
            }),
            Arc::new(RwLock::new(SkillStore::new())),
            Arc::new(RwLock::new(RuleEngine::new())),
            Arc::new(harness_observe::EventStore::new(temp.path()).await?),
            temp.path().to_path_buf(),
            "test task".to_string(),
        )
        .await
        .expect_err("expected event-store failure");

        assert!(
            err.to_string()
                .contains("no baseline scan results found in event store"),
            "unexpected error: {err}"
        );
        Ok(())
    }
}
