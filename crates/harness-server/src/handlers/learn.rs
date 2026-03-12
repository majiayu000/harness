use crate::{http::AppState, validate_root};
use harness_core::{AgentRequest, Category, DraftStatus, RuleId, Severity};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use harness_rules::engine::Rule;
use std::path::PathBuf;

pub async fn learn_rules(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id);

    let draft_contents = match collect_adopted_draft_contents(state) {
        Ok(d) => d,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };

    if draft_contents.is_empty() {
        return RpcResponse::success(id, serde_json::json!({ "rules_learned": 0, "rules": [] }));
    }

    let agent = match state.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };

    let prompt = build_learn_rules_prompt(&draft_contents);
    let req = AgentRequest {
        prompt,
        project_root,
        ..Default::default()
    };

    let resp = match agent.execute(req).await {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    let rules = parse_rules_from_output(&resp.output);
    let count = rules.len();

    {
        let mut engine = state.rules.write().await;
        for rule in &rules {
            engine.add_rule(rule.clone());
        }
    }

    match serde_json::to_value(&rules) {
        Ok(v) => RpcResponse::success(
            id,
            serde_json::json!({ "rules_learned": count, "rules": v }),
        ),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn learn_skills(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id);

    let draft_contents = match collect_adopted_draft_contents(state) {
        Ok(d) => d,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };

    if draft_contents.is_empty() {
        return RpcResponse::success(id, serde_json::json!({ "skills_learned": 0, "skills": [] }));
    }

    let agent = match state.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };

    let prompt = build_learn_skills_prompt(&draft_contents);
    let req = AgentRequest {
        prompt,
        project_root,
        ..Default::default()
    };

    let resp = match agent.execute(req).await {
        Ok(r) => r,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };

    let skill_items = parse_skills_from_output(&resp.output);
    let mut created = Vec::new();

    {
        let mut skills = state.skills.write().await;
        for (name, content) in skill_items {
            if let Err(e) = validate_skill_name(&name) {
                tracing::warn!("skipping skill with invalid name: {e}");
                continue;
            }
            let skill = skills.create(name, content).clone();
            created.push(skill);
        }
    }

    let count = created.len();

    match serde_json::to_value(&created) {
        Ok(v) => RpcResponse::success(
            id,
            serde_json::json!({ "skills_learned": count, "skills": v }),
        ),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

/// Validate that a skill name is safe to use as a filename component.
/// Rejects empty names, path separators, and `..` sequences.
fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("skill name must not be empty".to_string());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!(
            "skill name contains path traversal characters: '{name}'"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!("skill name contains invalid characters: '{name}'"));
    }
    Ok(())
}

fn collect_adopted_draft_contents(state: &AppState) -> Result<Vec<String>, String> {
    let drafts = state.gc_agent.drafts().map_err(|e| e.to_string())?;
    let contents = drafts
        .iter()
        .filter(|d| d.status == DraftStatus::Adopted)
        .flat_map(|d| d.artifacts.iter().map(|a| a.content.clone()))
        .collect();
    Ok(contents)
}

fn build_learn_rules_prompt(draft_contents: &[String]) -> String {
    let joined = draft_contents.join("\n\n---\n\n");
    let safe_content = harness_core::prompts::wrap_external_data(&joined);
    format!(
        "Analyze the following adopted remediation drafts and extract reusable guard rules.\n\
         {safe_content}\n\n\
         For each distinct rule pattern you identify, output a block in this exact format:\n\
         ## RULE_ID: Short title\n\
         severity: high\n\
         Description of the rule and why it matters.\n\n\
         Use severity values: critical, high, medium, or low.\n\
         Use RULE_IDs like LEARN-001, LEARN-002, etc.\n\
         Output only rule blocks, no other text."
    )
}

fn build_learn_skills_prompt(draft_contents: &[String]) -> String {
    let joined = draft_contents.join("\n\n---\n\n");
    let safe_content = harness_core::prompts::wrap_external_data(&joined);
    format!(
        "Analyze the following adopted remediation drafts and extract reusable skills.\n\
         {safe_content}\n\n\
         For each reusable skill or pattern you identify, output a block in this exact format:\n\
         === skill: kebab-case-name ===\n\
         # Skill Title\n\
         Description and usage instructions.\n\n\
         Output only skill blocks, no other text."
    )
}

/// Parse `## RULE_ID: Title` blocks from agent output into Rule structs.
fn parse_rules_from_output(output: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    let normalized = format!("\n{}", output);
    for section in normalized.split("\n## ") {
        let first_line = section.lines().next().unwrap_or("");
        if let Some((id_part, title)) = first_line.split_once(':') {
            let id = id_part.trim_start_matches('#').trim().to_string();
            if id.is_empty()
                || !id
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_uppercase())
                    .unwrap_or(false)
            {
                continue;
            }
            let severity = detect_severity(section);
            let category = detect_category(&id);
            let description = section
                .lines()
                .skip(1)
                .filter(|line| !line.trim().to_lowercase().starts_with("severity:"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            rules.push(Rule {
                id: RuleId::from_str(&id),
                title: title.trim().to_string(),
                severity,
                category,
                paths: Vec::new(),
                description,
                fix_pattern: None,
            });
        }
    }
    rules
}

/// Parse `=== skill: name === ... content ...` blocks from agent output.
fn parse_skills_from_output(output: &str) -> Vec<(String, String)> {
    let mut skills = Vec::new();
    for block in output.split("=== skill:") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if let Some((name_part, content)) = block.split_once("===") {
            let name = name_part.trim().to_string();
            let content = content.trim().to_string();
            if !name.is_empty() && !content.is_empty() {
                skills.push((name, content));
            }
        }
    }
    skills
}

fn detect_severity(section: &str) -> Severity {
    section
        .lines()
        .find_map(parse_severity_from_line)
        .unwrap_or(Severity::Low)
}

fn parse_severity_from_line(line: &str) -> Option<Severity> {
    let normalized = line
        .trim_start()
        .trim_start_matches(|c| matches!(c, '-' | '*' | '>'))
        .trim_start()
        .to_ascii_lowercase();
    let value = normalized.strip_prefix("severity:")?.trim();
    let token = value
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|token| !token.is_empty())?;

    match token {
        "critical" => Some(Severity::Critical),
        "high" => Some(Severity::High),
        "medium" => Some(Severity::Medium),
        "low" => Some(Severity::Low),
        _ => None,
    }
}

fn detect_category(id: &str) -> Category {
    if id.starts_with("SEC") {
        Category::Security
    } else if id.starts_with("RS")
        || id.starts_with("GO")
        || id.starts_with("TS")
        || id.starts_with("PY")
    {
        Category::Stability
    } else {
        Category::Style
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Category, RuleId, Severity};

    #[test]
    fn parse_rules_extracts_single_rule() {
        let output =
            "## LEARN-001: No hardcoded secrets\nseverity: high\nNever put secrets in source code.";
        let rules = parse_rules_from_output(output);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, RuleId::from_str("LEARN-001"));
        assert_eq!(rules[0].title, "No hardcoded secrets");
        assert_eq!(rules[0].severity, Severity::High);
    }

    #[test]
    fn parse_rules_extracts_multiple_rules() {
        let output = "## LEARN-001: First rule\nseverity: critical\nDesc.\n\n## LEARN-002: Second rule\nseverity: medium\nDesc2.";
        let rules = parse_rules_from_output(output);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].id, RuleId::from_str("LEARN-001"));
        assert_eq!(rules[0].severity, Severity::Critical);
        assert_eq!(rules[1].id, RuleId::from_str("LEARN-002"));
        assert_eq!(rules[1].severity, Severity::Medium);
    }

    #[test]
    fn parse_rules_skips_lowercase_ids() {
        let output = "## lowercase-id: Bad rule\nseverity: high\nSomething.";
        let rules = parse_rules_from_output(output);
        assert!(rules.is_empty());
    }

    #[test]
    fn parse_rules_detects_sec_category() {
        let output = "## SEC-99: Security rule\nseverity: high\nDetails.";
        let rules = parse_rules_from_output(output);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].category, Category::Security);
    }

    #[test]
    fn detect_severity_prefers_explicit_field_over_title_keywords() {
        let section =
            "LEARN-101: Avoid high memory usage\nseverity: low\nUse streaming to stay stable.";
        assert_eq!(detect_severity(section), Severity::Low);
    }

    #[test]
    fn detect_severity_without_explicit_field_defaults_to_low() {
        let section = "LEARN-102: Critical path notes\nThis description mentions high and medium.";
        assert_eq!(detect_severity(section), Severity::Low);
    }

    #[test]
    fn detect_severity_accepts_marker_and_suffix() {
        let section = "- severity: CRITICAL (blocker)";
        assert_eq!(detect_severity(section), Severity::Critical);
    }

    #[test]
    fn parse_rules_avoids_false_positive_from_title_keyword() {
        let output =
            "## LEARN-103: Avoid high memory usage\nseverity: low\nUse bounded queues for safety.";
        let rules = parse_rules_from_output(output);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].severity, Severity::Low);
    }

    #[test]
    fn parse_skills_extracts_single_skill() {
        let output = "=== skill: my-skill ===\n# My Skill\nDoes useful things.";
        let skills = parse_skills_from_output(output);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].0, "my-skill");
        assert!(skills[0].1.contains("My Skill"));
    }

    #[test]
    fn parse_skills_extracts_multiple_skills() {
        let output =
            "=== skill: alpha ===\n# Alpha\nContent A.\n=== skill: beta ===\n# Beta\nContent B.";
        let skills = parse_skills_from_output(output);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].0, "alpha");
        assert_eq!(skills[1].0, "beta");
    }

    #[test]
    fn parse_rules_empty_output_returns_empty() {
        let rules = parse_rules_from_output("");
        assert!(rules.is_empty());
    }

    #[test]
    fn parse_skills_empty_output_returns_empty() {
        let skills = parse_skills_from_output("");
        assert!(skills.is_empty());
    }
}
