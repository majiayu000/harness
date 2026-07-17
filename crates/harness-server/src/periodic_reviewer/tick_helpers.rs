use crate::http::AppState;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

pub(super) fn ensure_review_queue_limit(state: &Arc<AppState>, project_root: &std::path::Path) {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    state
        .concurrency
        .review_task_queue
        .set_project_limit(&canonical, 1);
}

/// Maximum number of violations inlined into the prompt.
///
/// The full list may contain hundreds of entries; inlining all of them can
/// exceed OS ARG_MAX or the model's context window before the agent even
/// starts.  Only the first N are embedded; the total count is always reported
/// so the agent knows additional findings exist.
pub(super) const MAX_INLINE_VIOLATIONS: usize = 20;

pub(super) fn format_violations_for_prompt(
    violations: &[harness_core::types::Violation],
) -> String {
    if violations.is_empty() {
        return "No violations found.".to_string();
    }
    let total = violations.len();
    let shown = violations.len().min(MAX_INLINE_VIOLATIONS);
    let lines: Vec<String> = violations[..shown]
        .iter()
        .map(|v| {
            let loc = match v.line {
                Some(l) => format!("{}:{l}", v.file.display()),
                None => v.file.display().to_string(),
            };
            format!("[{:?}] {}: {} ({})", v.severity, v.rule_id, v.message, loc)
        })
        .collect();
    let mut out = format!(
        "{total} violation(s) (showing {shown}):\n{}",
        lines.join("\n")
    );
    if total > shown {
        out.push_str(&format!(
            "\n... and {} more violation(s) not shown. Run guard scripts locally for the full list.",
            total - shown
        ));
    }
    out
}

pub(super) fn pick_secondary_review_agent<F>(
    primary_agent: &str,
    candidates: &[String],
    mut is_available: F,
) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    candidates
        .iter()
        .find(|agent| agent.as_str() != primary_agent && is_available(agent.as_str()))
        .cloned()
}

/// Poll a task until it reaches a terminal state, then extract its output.
pub(super) async fn poll_task_output(
    store: &crate::task_runner::TaskStore,
    task_id: &harness_core::types::TaskId,
    timeout_secs: u64,
) -> Option<String> {
    let poll_interval = Duration::from_secs(15);
    let max_wait = if timeout_secs == 0 {
        Duration::from_secs(999_999)
    } else {
        Duration::from_secs(timeout_secs + 120)
    };
    let start = tokio::time::Instant::now();
    loop {
        sleep(poll_interval).await;
        if start.elapsed() > max_wait {
            tracing::warn!(task_id = %task_id, "poll_task_output: timed out");
            return None;
        }
        let Some(task) = store.get(task_id) else {
            continue;
        };
        if !task.status.is_terminal() {
            continue;
        }
        if task.status.is_cancelled() {
            return None;
        }
        if task.status.is_failure() {
            tracing::error!(
                task_id = %task_id,
                error = ?task.error,
                status = task.status.as_ref(),
                "poll_task_output: task failed"
            );
            return None;
        }
        let output: String = task
            .rounds
            .iter()
            .filter_map(|r| r.detail.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        if output.is_empty() {
            tracing::warn!(task_id = %task_id, "poll_task_output: completed but no output");
            return None;
        }
        return Some(output);
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewOutput {
    pub(super) findings: Vec<ReviewFinding>,
    pub(super) summary: ReviewSummary,
}

/// Validation-only representation of the finding schema required by the
/// periodic-review prompt. The fields stay private because the persistence and
/// auto-fix consumers were removed with the review store.
#[derive(Debug, Deserialize)]
pub(super) struct ReviewFinding {
    #[serde(rename = "id")]
    _id: String,
    #[serde(rename = "rule_id")]
    _rule_id: String,
    #[serde(rename = "priority")]
    _priority: String,
    #[serde(rename = "impact")]
    _impact: i32,
    #[serde(rename = "confidence")]
    _confidence: i32,
    #[serde(rename = "effort")]
    _effort: i32,
    #[serde(rename = "file")]
    _file: String,
    #[serde(rename = "line")]
    _line: i64,
    #[serde(rename = "title")]
    _title: String,
    #[serde(rename = "description")]
    _description: String,
    #[serde(rename = "action")]
    _action: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReviewSummary {
    #[serde(rename = "p0_count")]
    _p0_count: i32,
    #[serde(rename = "p1_count")]
    _p1_count: i32,
    #[serde(rename = "p2_count")]
    _p2_count: i32,
    #[serde(rename = "p3_count")]
    _p3_count: i32,
    pub(super) health_score: i32,
}

/// Parse the structured result emitted by a periodic review agent.
///
/// Agents may wrap the JSON in prose, Markdown fences, or explicit review
/// markers. Parsing remains a prerequisite for advancing the review watermark.
pub(super) fn parse_review_output(raw: &str) -> anyhow::Result<ReviewOutput> {
    let trimmed = raw.trim();

    const START_MARKER: &str = "REVIEW_JSON_START";
    const END_MARKER: &str = "REVIEW_JSON_END";
    if let Some(start) = trimmed.find(START_MARKER) {
        let after_marker = &trimmed[start + START_MARKER.len()..];
        if let Some(end) = after_marker.find(END_MARKER) {
            let json_slice = after_marker[..end].trim();
            return serde_json::from_str(json_slice)
                .map_err(|error| anyhow::anyhow!("failed to parse marked review JSON: {error}"));
        }
    }

    let start = trimmed
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in review output"))?;
    let end = trimmed
        .rfind('}')
        .map(|index| index + 1)
        .ok_or_else(|| anyhow::anyhow!("no closing brace found in review output"))?;
    serde_json::from_str(&trimmed[start..end])
        .map_err(|error| anyhow::anyhow!("failed to parse review JSON: {error}"))
}

#[cfg(test)]
mod review_output_tests {
    use super::parse_review_output;

    const EMPTY_REVIEW: &str = r#"{"findings":[],"summary":{"p0_count":0,"p1_count":0,"p2_count":0,"p3_count":0,"health_score":100}}"#;

    #[test]
    fn parses_clean_json() -> anyhow::Result<()> {
        let output = parse_review_output(EMPTY_REVIEW)?;
        assert!(output.findings.is_empty());
        assert_eq!(output.summary.health_score, 100);
        Ok(())
    }

    #[test]
    fn parses_markdown_fenced_json() -> anyhow::Result<()> {
        let output = parse_review_output(&format!("```json\n{EMPTY_REVIEW}\n```"))?;
        assert!(output.findings.is_empty());
        Ok(())
    }

    #[test]
    fn parses_marked_json_before_surrounding_prose() -> anyhow::Result<()> {
        let raw =
            format!("Created issues.\nREVIEW_JSON_START\n{EMPTY_REVIEW}\nREVIEW_JSON_END\nDone.");
        let output = parse_review_output(&raw)?;
        assert_eq!(output.summary.health_score, 100);
        Ok(())
    }

    #[test]
    fn rejects_output_without_json() {
        assert!(parse_review_output("not json at all").is_err());
    }

    #[test]
    fn rejects_findings_that_do_not_match_the_prompt_schema() {
        let malformed = r#"{"findings":[{}],"summary":{"p0_count":0,"p1_count":0,"p2_count":0,"p3_count":0,"health_score":100}}"#;
        assert!(parse_review_output(malformed).is_err());
    }
}
