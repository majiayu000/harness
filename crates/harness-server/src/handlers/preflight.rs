use crate::http::AppState;
use harness_core::{AgentRequest, CodeAgent, TaskComplexity};
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
    pub recommended_complexity: TaskComplexity,
}

/// Run a preflight analysis for the given task description.
///
/// Loads the 'preflight' skill, gathers active rules, then asks the agent to
/// produce a structured constraint set for the task before any code is written.
pub async fn run_preflight(
    agent: Arc<dyn CodeAgent>,
    skills: Arc<RwLock<SkillStore>>,
    rules: Arc<RwLock<RuleEngine>>,
    project_root: PathBuf,
    task_description: String,
) -> anyhow::Result<PreflightResult> {
    let skill_content = {
        let store = skills.read().await;
        store
            .list()
            .iter()
            .find(|s| s.name == "preflight")
            .map(|s| s.content.clone())
            .unwrap_or_default()
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

    let req = AgentRequest {
        prompt,
        project_root,
        allowed_tools: vec!["Read".to_string()],
        model: None,
        max_budget_usd: None,
        context: vec![],
    };

    let resp = agent.execute(req).await?;
    parse_preflight_output(&resp.output)
}

fn parse_preflight_output(output: &str) -> anyhow::Result<PreflightResult> {
    let mut constraints: Vec<String> = Vec::new();
    let mut affected_files: Vec<PathBuf> = Vec::new();
    let mut risk_level = "low";
    let mut complexity_str = "simple";
    let mut current_section = "";

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("CONSTRAINTS:") {
            current_section = "constraints";
        } else if trimmed.starts_with("AFFECTED_FILES:") {
            current_section = "affected_files";
        } else if let Some(rest) = trimmed.strip_prefix("RISK:") {
            risk_level = rest.trim();
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

    let baseline_violations = match risk_level.to_lowercase().as_str() {
        "high" => 3,
        "medium" => 1,
        _ => 0,
    };

    let recommended_complexity = match complexity_str.to_lowercase().as_str() {
        "complex" => TaskComplexity::Complex,
        "critical" => TaskComplexity::Critical,
        "medium" => TaskComplexity::Medium,
        _ => TaskComplexity::Simple,
    };

    Ok(PreflightResult {
        constraints,
        affected_files,
        baseline_violations,
        recommended_complexity,
    })
}

/// RPC handler: validate project_root then run preflight analysis.
pub async fn preflight(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
    task_description: String,
) -> RpcResponse {
    let project_root = match crate::handlers::validate_project_root(&project_root) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };

    let agent = match state.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };

    match run_preflight(
        agent,
        state.skills.clone(),
        state.rules.clone(),
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
    use harness_core::TaskComplexity;

    #[test]
    fn parse_output_with_all_sections() {
        let output = "\
CONSTRAINTS:\n- No breaking API changes\n- Must add tests\n\
AFFECTED_FILES:\n- src/lib.rs\n- src/handler.rs\n\
RISK: medium\n\
COMPLEXITY: complex";
        let result = parse_preflight_output(output).expect("parse");
        assert_eq!(result.constraints, vec!["No breaking API changes", "Must add tests"]);
        assert_eq!(result.affected_files, vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/handler.rs")]);
        assert_eq!(result.baseline_violations, 1);
        assert_eq!(result.recommended_complexity, TaskComplexity::Complex);
    }

    #[test]
    fn parse_output_high_risk() {
        let output = "CONSTRAINTS:\nAFFECTED_FILES:\nRISK: high\nCOMPLEXITY: critical";
        let result = parse_preflight_output(output).expect("parse");
        assert_eq!(result.baseline_violations, 3);
        assert_eq!(result.recommended_complexity, TaskComplexity::Critical);
    }

    #[test]
    fn parse_output_low_risk_simple() {
        let output = "CONSTRAINTS:\n- Keep it minimal\nAFFECTED_FILES:\n- README.md\nRISK: low\nCOMPLEXITY: simple";
        let result = parse_preflight_output(output).expect("parse");
        assert_eq!(result.baseline_violations, 0);
        assert_eq!(result.recommended_complexity, TaskComplexity::Simple);
    }

    #[test]
    fn parse_output_empty_sections() {
        let output = "CONSTRAINTS:\nAFFECTED_FILES:\nRISK: low\nCOMPLEXITY: simple";
        let result = parse_preflight_output(output).expect("parse");
        assert!(result.constraints.is_empty());
        assert!(result.affected_files.is_empty());
    }
}
