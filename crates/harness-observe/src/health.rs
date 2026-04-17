use crate::quality::{QualityGrader, QualityReport};
use harness_core::{
    types::Decision, types::Event, types::Grade, types::RuleId, types::Severity, types::Violation,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViolationSummary {
    pub rule_id: RuleId,
    pub count: usize,
    pub severity: Severity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalSummary {
    pub signal_type: String,
    pub count: usize,
    pub last_detected: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventSummary {
    pub total: usize,
    pub pass_count: usize,
    pub warn_count: usize,
    pub block_count: usize,
    pub escalate_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub quality: QualityReport,
    pub violation_summary: Vec<ViolationSummary>,
    pub signal_summary: Vec<SignalSummary>,
    pub event_summary: EventSummary,
    pub recommendations: Vec<String>,
}

pub fn generate_health_report(events: &[Event], violations: &[Violation]) -> HealthReport {
    let violation_summary = aggregate_violations(violations);
    let signal_summary = derive_signals(events);
    let event_summary = count_events(events);
    let quality = QualityGrader::grade(events, violations.len());
    let recommendations = build_recommendations(&quality, &violation_summary, &event_summary);

    HealthReport {
        quality,
        violation_summary,
        signal_summary,
        event_summary,
        recommendations,
    }
}

fn aggregate_violations(violations: &[Violation]) -> Vec<ViolationSummary> {
    let mut map: HashMap<RuleId, (usize, Severity)> = HashMap::new();
    for v in violations {
        let entry = map.entry(v.rule_id.clone()).or_insert((0, v.severity));
        entry.0 += 1;
    }
    let mut summaries: Vec<ViolationSummary> = map
        .into_iter()
        .map(|(rule_id, (count, severity))| ViolationSummary {
            rule_id,
            count,
            severity,
        })
        .collect();
    summaries.sort_by_key(|b| std::cmp::Reverse(b.count));
    summaries
}

fn derive_signals(events: &[Event]) -> Vec<SignalSummary> {
    let mut map: HashMap<String, (usize, Option<chrono::DateTime<chrono::Utc>>)> = HashMap::new();
    for e in events
        .iter()
        .filter(|e| matches!(e.decision, Decision::Escalate))
    {
        let entry = map.entry(e.hook.clone()).or_insert((0, None));
        entry.0 += 1;
        entry.1 = Some(match entry.1 {
            Some(t) if t > e.ts => t,
            _ => e.ts,
        });
    }
    let mut summaries: Vec<SignalSummary> = map
        .into_iter()
        .map(|(signal_type, (count, last_detected))| SignalSummary {
            signal_type,
            count,
            last_detected,
        })
        .collect();
    summaries.sort_by_key(|b| std::cmp::Reverse(b.count));
    summaries
}

fn count_events(events: &[Event]) -> EventSummary {
    let total = events.len();
    let pass_count = events
        .iter()
        .filter(|e| matches!(e.decision, Decision::Pass | Decision::Complete))
        .count();
    let warn_count = events
        .iter()
        .filter(|e| matches!(e.decision, Decision::Warn))
        .count();
    let block_count = events
        .iter()
        .filter(|e| matches!(e.decision, Decision::Block | Decision::Gate))
        .count();
    let escalate_count = events
        .iter()
        .filter(|e| matches!(e.decision, Decision::Escalate))
        .count();
    EventSummary {
        total,
        pass_count,
        warn_count,
        block_count,
        escalate_count,
    }
}

fn build_recommendations(
    quality: &QualityReport,
    violations: &[ViolationSummary],
    summary: &EventSummary,
) -> Vec<String> {
    let mut recs = Vec::new();

    if quality.dimensions.security < 90.0 {
        recs.push(
            "Review and fix security-related violations to improve security score.".to_string(),
        );
    }
    if quality.dimensions.stability < 80.0 {
        recs.push("Reduce block rate to improve stability score.".to_string());
    }
    if summary.escalate_count > 0 {
        recs.push(format!(
            "Investigate {} escalated events to prevent recurring issues.",
            summary.escalate_count
        ));
    }
    if let Some(top) = violations.first() {
        recs.push(format!(
            "Address high-frequency rule violations: {} ({} occurrences).",
            top.rule_id, top.count
        ));
    }
    if quality.grade == Grade::D {
        recs.push(
            "Consider running garbage collection immediately to clean up accumulated issues."
                .to_string(),
        );
    }

    if recs.is_empty() {
        recs.push("System health is good. Maintain current practices.".to_string());
    }

    recs
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{types::RuleId, types::SessionId, types::Severity};
    use std::path::PathBuf;

    fn pass_event() -> Event {
        Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Pass)
    }

    fn block_event() -> Event {
        Event::new(SessionId::new(), "security_check", "Edit", Decision::Block)
    }

    fn escalate_event(hook: &str) -> Event {
        Event::new(SessionId::new(), hook, "Edit", Decision::Escalate)
    }

    fn make_violation(rule_id: &str, severity: Severity) -> Violation {
        Violation {
            rule_id: RuleId::from_str(rule_id),
            file: PathBuf::from("src/main.rs"),
            line: None,
            message: "test violation".to_string(),
            severity,
        }
    }

    #[test]
    fn zero_events_zero_violations_grade_a() {
        let report = generate_health_report(&[], &[]);
        assert_eq!(report.quality.grade, Grade::A);
        assert!(report.violation_summary.is_empty());
        assert!(report.signal_summary.is_empty());
        assert_eq!(report.event_summary.total, 0);
        assert_eq!(report.recommendations.len(), 1);
        assert!(report.recommendations[0].contains("good"));
    }

    #[test]
    fn violations_grouped_by_rule_id() {
        let violations = vec![
            make_violation("SEC-01", Severity::High),
            make_violation("SEC-01", Severity::High),
            make_violation("SEC-02", Severity::Medium),
        ];
        let report = generate_health_report(&[], &violations);
        assert_eq!(report.violation_summary.len(), 2);
        // highest count first
        assert_eq!(report.violation_summary[0].rule_id.as_str(), "SEC-01");
        assert_eq!(report.violation_summary[0].count, 2);
        assert_eq!(report.violation_summary[1].rule_id.as_str(), "SEC-02");
        assert_eq!(report.violation_summary[1].count, 1);
    }

    #[test]
    fn event_summary_counts_correctly() {
        let events = vec![
            pass_event(),
            pass_event(),
            block_event(),
            escalate_event("sig"),
        ];
        let report = generate_health_report(&events, &[]);
        assert_eq!(report.event_summary.total, 4);
        assert_eq!(report.event_summary.pass_count, 2);
        assert_eq!(report.event_summary.block_count, 1);
        assert_eq!(report.event_summary.escalate_count, 1);
    }

    #[test]
    fn signals_derived_from_escalated_events() {
        let events = vec![escalate_event("signal_hook"), escalate_event("signal_hook")];
        let report = generate_health_report(&events, &[]);
        assert_eq!(report.signal_summary.len(), 1);
        assert_eq!(report.signal_summary[0].signal_type, "signal_hook");
        assert_eq!(report.signal_summary[0].count, 2);
    }

    #[test]
    fn recommendations_include_violation_when_present() {
        let violations = vec![make_violation("SEC-01", Severity::Critical)];
        let report = generate_health_report(&[], &violations);
        let has_violation_rec = report.recommendations.iter().any(|r| r.contains("SEC-01"));
        assert!(has_violation_rec);
    }

    #[test]
    fn quality_degrades_with_violations() {
        let violations: Vec<Violation> = (0..50)
            .map(|_| make_violation("SEC-01", Severity::High))
            .collect();
        let report = generate_health_report(&[], &violations);
        assert!(report.quality.dimensions.coverage < 100.0);
    }
}
