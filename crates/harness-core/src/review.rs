use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewProviderKind {
    LocalCli,
    LocalAgent,
    ExternalBot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Failed,
    TimedOut,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCategory {
    Security,
    Correctness,
    DataIntegrity,
    Concurrency,
    Performance,
    TestGap,
    Maintainability,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewReportFinding {
    pub severity: ReviewSeverity,
    pub category: ReviewCategory,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub message: String,
    pub evidence: Option<String>,
    pub recommendation: Option<String>,
    pub blocking: bool,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub provider_id: String,
    pub provider_kind: ReviewProviderKind,
    pub decision: ReviewDecision,
    pub summary: String,
    pub findings: Vec<ReviewReportFinding>,
    pub raw_output: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewProviderRole {
    Required,
    Advisory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewGateProviderReport {
    pub role: ReviewProviderRole,
    pub report: ReviewReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewGateDecision {
    Approved,
    ChangesRequested,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewGateResult {
    pub decision: ReviewGateDecision,
    pub blocking_provider_id: Option<String>,
    pub summary: String,
}

#[derive(Debug, Deserialize)]
struct ReviewReportPayload {
    decision: ReviewDecision,
    summary: String,
    #[serde(default)]
    findings: Vec<ReviewReportFinding>,
}

pub fn parse_review_report(
    provider_id: &str,
    provider_kind: ReviewProviderKind,
    raw_output: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> ReviewReport {
    if let Some(payload) = fenced_report_payload(raw_output)
        .and_then(parse_payload)
        .or_else(|| parse_raw_payload(raw_output))
    {
        return build_report(
            provider_id,
            provider_kind,
            raw_output,
            started_at,
            completed_at,
            payload,
        );
    }

    let legacy_findings = legacy_issue_findings(raw_output);
    if !legacy_findings.is_empty() {
        return ReviewReport {
            provider_id: provider_id.to_string(),
            provider_kind,
            decision: ReviewDecision::ChangesRequested,
            summary: format!(
                "{} blocking issue(s) reported by legacy review output.",
                legacy_findings.len()
            ),
            findings: legacy_findings,
            raw_output: Some(raw_output.to_string()),
            started_at,
            completed_at,
            elapsed_ms: elapsed_ms(started_at, completed_at),
        };
    }

    if last_non_empty_line(raw_output).is_some_and(|line| line.eq_ignore_ascii_case("APPROVED")) {
        return ReviewReport {
            provider_id: provider_id.to_string(),
            provider_kind,
            decision: ReviewDecision::Approved,
            summary: "Review provider approved the change.".to_string(),
            findings: Vec::new(),
            raw_output: Some(raw_output.to_string()),
            started_at,
            completed_at,
            elapsed_ms: elapsed_ms(started_at, completed_at),
        };
    }

    ReviewReport {
        provider_id: provider_id.to_string(),
        provider_kind,
        decision: ReviewDecision::Failed,
        summary: "Review provider output did not include a parseable decision.".to_string(),
        findings: Vec::new(),
        raw_output: Some(raw_output.to_string()),
        started_at,
        completed_at,
        elapsed_ms: elapsed_ms(started_at, completed_at),
    }
}

pub fn evaluate_review_gate(
    reports: &[ReviewGateProviderReport],
    external_required: bool,
) -> ReviewGateResult {
    let mut saw_required = false;
    for input in reports {
        if input.role == ReviewProviderRole::Required {
            saw_required = true;
            match input.report.decision {
                ReviewDecision::ChangesRequested => {
                    return gate_result(
                        ReviewGateDecision::ChangesRequested,
                        &input.report,
                        "Required review provider requested changes.",
                    );
                }
                ReviewDecision::Failed | ReviewDecision::TimedOut | ReviewDecision::Skipped => {
                    return gate_result(
                        ReviewGateDecision::Blocked,
                        &input.report,
                        "Required review provider did not complete successfully.",
                    );
                }
                ReviewDecision::Approved => {}
            }
        }
    }
    if !saw_required {
        return ReviewGateResult {
            decision: ReviewGateDecision::Blocked,
            blocking_provider_id: None,
            summary: "No required review provider report was available.".to_string(),
        };
    }

    if external_required {
        let mut saw_advisory = false;
        for input in reports {
            if input.role == ReviewProviderRole::Advisory {
                saw_advisory = true;
                match input.report.decision {
                    ReviewDecision::ChangesRequested => {
                        return gate_result(
                            ReviewGateDecision::ChangesRequested,
                            &input.report,
                            "External review provider requested changes.",
                        );
                    }
                    ReviewDecision::Failed | ReviewDecision::TimedOut | ReviewDecision::Skipped => {
                        return gate_result(
                            ReviewGateDecision::Blocked,
                            &input.report,
                            "External review provider is required and did not approve.",
                        );
                    }
                    ReviewDecision::Approved => {}
                }
            }
        }
        if !saw_advisory {
            return ReviewGateResult {
                decision: ReviewGateDecision::Blocked,
                blocking_provider_id: None,
                summary: "No external review provider report was available.".to_string(),
            };
        }
    }

    ReviewGateResult {
        decision: ReviewGateDecision::Approved,
        blocking_provider_id: None,
        summary: "All required review providers approved.".to_string(),
    }
}

fn build_report(
    provider_id: &str,
    provider_kind: ReviewProviderKind,
    raw_output: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    payload: ReviewReportPayload,
) -> ReviewReport {
    ReviewReport {
        provider_id: provider_id.to_string(),
        provider_kind,
        decision: payload.decision,
        summary: payload.summary,
        findings: payload.findings,
        raw_output: Some(raw_output.to_string()),
        started_at,
        completed_at,
        elapsed_ms: elapsed_ms(started_at, completed_at),
    }
}

fn gate_result(
    decision: ReviewGateDecision,
    report: &ReviewReport,
    summary: &str,
) -> ReviewGateResult {
    ReviewGateResult {
        decision,
        blocking_provider_id: Some(report.provider_id.clone()),
        summary: summary.to_string(),
    }
}

fn fenced_report_payload(raw_output: &str) -> Option<&str> {
    let marker = "```harness-review-report";
    let start = raw_output.find(marker)?;
    let after_marker = &raw_output[start + marker.len()..];
    let after_newline = after_marker.strip_prefix('\n').unwrap_or(after_marker);
    let end = after_newline.match_indices("```").find_map(|(index, _)| {
        (index == 0 || after_newline[..index].ends_with('\n')).then_some(index)
    })?;
    Some(after_newline[..end].trim())
}

fn parse_raw_payload(raw_output: &str) -> Option<ReviewReportPayload> {
    let trimmed = raw_output.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        parse_payload(trimmed)
    } else {
        None
    }
}

fn parse_payload(raw_json: &str) -> Option<ReviewReportPayload> {
    serde_json::from_str(raw_json).ok()
}

fn legacy_issue_findings(raw_output: &str) -> Vec<ReviewReportFinding> {
    raw_output
        .lines()
        .filter_map(legacy_issue_message)
        .map(|message| ReviewReportFinding {
            severity: ReviewSeverity::High,
            category: ReviewCategory::Other,
            path: None,
            line: None,
            message: message.trim().to_string(),
            evidence: None,
            recommendation: None,
            blocking: true,
            confidence: None,
        })
        .collect()
}

fn legacy_issue_message(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let prefix = trimmed.get(..6)?;
    if prefix.eq_ignore_ascii_case("ISSUE:") {
        trimmed.get(6..).map(str::trim)
    } else {
        None
    }
}

fn last_non_empty_line(raw_output: &str) -> Option<&str> {
    raw_output.lines().rev().find_map(|line| {
        let trimmed = line.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}

fn elapsed_ms(started_at: DateTime<Utc>, completed_at: DateTime<Utc>) -> u64 {
    completed_at
        .signed_duration_since(started_at)
        .num_milliseconds()
        .max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn started() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 24, 8, 0, 0).unwrap()
    }

    fn completed() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 24, 8, 0, 2).unwrap()
    }

    fn approved_report(provider_id: &str, role: ReviewProviderRole) -> ReviewGateProviderReport {
        ReviewGateProviderReport {
            role,
            report: ReviewReport {
                provider_id: provider_id.to_string(),
                provider_kind: ReviewProviderKind::LocalCli,
                decision: ReviewDecision::Approved,
                summary: "ok".to_string(),
                findings: Vec::new(),
                raw_output: None,
                started_at: started(),
                completed_at: completed(),
                elapsed_ms: 2_000,
            },
        }
    }

    #[test]
    fn parses_fenced_review_report_json() {
        let report = parse_review_report(
            "codex_cli_review",
            ReviewProviderKind::LocalCli,
            r#"```harness-review-report
{"decision":"changes_requested","summary":"One issue.","findings":[{"severity":"high","category":"correctness","path":"src/lib.rs","line":7,"message":"Wrong result","evidence":"The branch is not handled.","recommendation":"Handle the branch.","blocking":true,"confidence":0.9}]}
```"#,
            started(),
            completed(),
        );

        assert_eq!(report.decision, ReviewDecision::ChangesRequested);
        assert_eq!(report.elapsed_ms, 2_000);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].category, ReviewCategory::Correctness);
    }

    #[test]
    fn parses_fenced_review_report_with_backticks_in_string_field() {
        let report = parse_review_report(
            "codex_cli_review",
            ReviewProviderKind::LocalCli,
            r#"```harness-review-report
{"decision":"changes_requested","summary":"One issue.","findings":[{"severity":"medium","category":"maintainability","path":"src/lib.rs","line":9,"message":"Keep example parseable","evidence":"The output included ```rust\nlet value = 1;\n``` inside the report.","recommendation":"Preserve the full JSON payload.","blocking":true,"confidence":0.8}]}
```"#,
            started(),
            completed(),
        );

        assert_eq!(report.decision, ReviewDecision::ChangesRequested);
        assert_eq!(report.findings.len(), 1);
        assert!(report.findings[0]
            .evidence
            .as_deref()
            .is_some_and(|evidence| evidence.contains("```rust")));
    }

    #[test]
    fn parses_raw_review_report_json() {
        let report = parse_review_report(
            "codex_cli_review",
            ReviewProviderKind::LocalCli,
            r#"{"decision":"approved","summary":"No blocking issues.","findings":[]}"#,
            started(),
            completed(),
        );

        assert_eq!(report.decision, ReviewDecision::Approved);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn parses_legacy_approved_line() {
        let report = parse_review_report(
            "codex_agent_review",
            ReviewProviderKind::LocalAgent,
            "Looks good.\nAPPROVED",
            started(),
            completed(),
        );

        assert_eq!(report.decision, ReviewDecision::Approved);
    }

    #[test]
    fn parses_legacy_issue_lines_as_blocking_findings() {
        let report = parse_review_report(
            "codex_agent_review",
            ReviewProviderKind::LocalAgent,
            "ISSUE: Missing retry coverage\nIssue: Wrong timeout",
            started(),
            completed(),
        );

        assert_eq!(report.decision, ReviewDecision::ChangesRequested);
        assert_eq!(report.findings.len(), 2);
        assert!(report.findings.iter().all(|finding| finding.blocking));
    }

    #[test]
    fn malformed_output_returns_failed_report() {
        let report = parse_review_report(
            "codex_cli_review",
            ReviewProviderKind::LocalCli,
            "review text without a decision",
            started(),
            completed(),
        );

        assert_eq!(report.decision, ReviewDecision::Failed);
    }

    #[test]
    fn gate_approves_when_required_providers_approve() {
        let result = evaluate_review_gate(
            &[approved_report(
                "codex_cli_review",
                ReviewProviderRole::Required,
            )],
            false,
        );

        assert_eq!(result.decision, ReviewGateDecision::Approved);
    }

    #[test]
    fn gate_blocks_on_required_failure() {
        let mut failed = approved_report("codex_cli_review", ReviewProviderRole::Required);
        failed.report.decision = ReviewDecision::Failed;

        let result = evaluate_review_gate(&[failed], false);

        assert_eq!(result.decision, ReviewGateDecision::Blocked);
        assert_eq!(
            result.blocking_provider_id.as_deref(),
            Some("codex_cli_review")
        );
    }

    #[test]
    fn gate_blocks_when_no_required_provider_report_exists() {
        let result = evaluate_review_gate(
            &[approved_report(
                "gemini_github_bot",
                ReviewProviderRole::Advisory,
            )],
            false,
        );

        assert_eq!(result.decision, ReviewGateDecision::Blocked);
        assert_eq!(result.blocking_provider_id, None);
    }

    #[test]
    fn gate_ignores_advisory_failure_when_external_not_required() {
        let mut failed = approved_report("gemini_github_bot", ReviewProviderRole::Advisory);
        failed.report.decision = ReviewDecision::Failed;

        let result = evaluate_review_gate(
            &[
                approved_report("codex_cli_review", ReviewProviderRole::Required),
                failed,
            ],
            false,
        );

        assert_eq!(result.decision, ReviewGateDecision::Approved);
    }

    #[test]
    fn gate_blocks_when_external_required_without_advisory_report() {
        let result = evaluate_review_gate(
            &[approved_report(
                "codex_cli_review",
                ReviewProviderRole::Required,
            )],
            true,
        );

        assert_eq!(result.decision, ReviewGateDecision::Blocked);
        assert_eq!(result.blocking_provider_id, None);
    }

    #[test]
    fn gate_blocks_on_advisory_failure_when_external_required() {
        let mut failed = approved_report("gemini_github_bot", ReviewProviderRole::Advisory);
        failed.report.decision = ReviewDecision::TimedOut;

        let result = evaluate_review_gate(
            &[
                approved_report("codex_cli_review", ReviewProviderRole::Required),
                failed,
            ],
            true,
        );

        assert_eq!(result.decision, ReviewGateDecision::Blocked);
        assert_eq!(
            result.blocking_provider_id.as_deref(),
            Some("gemini_github_bot")
        );
    }
}
