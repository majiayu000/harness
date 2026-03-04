use chrono::{DateTime, Utc};
use harness_core::{Decision, Event, Grade};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-rule violation summary for historical trend queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleViolationStat {
    pub rule_id: String,
    pub count: usize,
    pub last_seen: Option<DateTime<Utc>>,
}

/// Aggregate `rule_check` events by rule_id (stored in the `tool` field).
///
/// Only Block and Warn decisions are counted as violations.
pub fn rule_violation_stats(events: &[Event]) -> Vec<RuleViolationStat> {
    // (count, last_seen)
    let mut map: HashMap<String, (usize, Option<DateTime<Utc>>)> = HashMap::new();
    for e in events {
        if e.hook != "rule_check" {
            continue;
        }
        if !matches!(e.decision, Decision::Block | Decision::Warn) {
            continue;
        }
        let entry = map.entry(e.tool.clone()).or_insert((0, None));
        entry.0 += 1;
        entry.1 = Some(match entry.1 {
            Some(prev) => prev.max(e.ts),
            None => e.ts,
        });
    }
    let mut stats: Vec<RuleViolationStat> = map
        .into_iter()
        .map(|(rule_id, (count, last_seen))| RuleViolationStat {
            rule_id,
            count,
            last_seen,
        })
        .collect();
    stats.sort_by(|a, b| b.count.cmp(&a.count).then(a.rule_id.cmp(&b.rule_id)));
    stats
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookStats {
    pub hook: String,
    pub total: usize,
    pub pass_rate: f64,
    pub warn_rate: f64,
    pub block_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplianceTrend {
    pub period: String,
    pub pass_rate: f64,
    pub violation_count: usize,
    pub grade: Grade,
}

pub fn aggregate_hook_stats(events: &[Event]) -> Vec<HookStats> {
    // (total, pass, warn, block)
    let mut map: HashMap<String, (usize, usize, usize, usize)> = HashMap::new();
    for e in events {
        let entry = map.entry(e.hook.clone()).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        match e.decision {
            Decision::Pass | Decision::Complete => entry.1 += 1,
            Decision::Warn => entry.2 += 1,
            Decision::Block | Decision::Gate | Decision::Escalate => entry.3 += 1,
        }
    }
    let mut stats: Vec<HookStats> = map
        .into_iter()
        .map(|(hook, (total, pass, warn, block))| {
            let t = total as f64;
            HookStats {
                hook,
                total,
                pass_rate: if total == 0 { 1.0 } else { pass as f64 / t },
                warn_rate: if total == 0 { 0.0 } else { warn as f64 / t },
                block_rate: if total == 0 { 0.0 } else { block as f64 / t },
            }
        })
        .collect();
    stats.sort_by(|a, b| a.hook.cmp(&b.hook));
    stats
}

pub fn compute_trends(events: &[Event], period_days: u32) -> Vec<ComplianceTrend> {
    if events.is_empty() {
        return Vec::new();
    }

    let period = chrono::Duration::days(period_days as i64);
    // Add 1ms to ensure the period containing the latest event is always included.
    let now = chrono::Utc::now() + chrono::Duration::milliseconds(1);
    let earliest = events.iter().map(|e| e.ts).min().unwrap_or(now);

    let mut trends = Vec::new();
    let mut period_start = earliest;

    while period_start < now {
        let period_end = period_start + period;
        let period_events: Vec<&Event> = events
            .iter()
            .filter(|e| e.ts >= period_start && e.ts < period_end)
            .collect();

        let total = period_events.len();
        let pass_count = period_events
            .iter()
            .filter(|e| matches!(e.decision, Decision::Pass | Decision::Complete))
            .count();
        let violation_count = period_events
            .iter()
            .filter(|e| {
                matches!(
                    e.decision,
                    Decision::Block | Decision::Warn | Decision::Escalate
                )
            })
            .count();

        let pass_rate = if total == 0 { 1.0 } else { pass_count as f64 / total as f64 };
        let grade = Grade::from_score(pass_rate * 100.0);

        trends.push(ComplianceTrend {
            period: period_start.format("%Y-%m-%d").to_string(),
            pass_rate,
            violation_count,
            grade,
        });

        period_start = period_end;
    }

    trends
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, Event, SessionId};

    fn make_event(hook: &str, decision: Decision) -> Event {
        Event::new(SessionId::new(), hook, "Edit", decision)
    }

    #[test]
    fn aggregate_empty_events_returns_empty() {
        let stats = aggregate_hook_stats(&[]);
        assert!(stats.is_empty());
    }

    #[test]
    fn aggregate_groups_by_hook() {
        let events = vec![
            make_event("hook_a", Decision::Pass),
            make_event("hook_a", Decision::Block),
            make_event("hook_b", Decision::Pass),
        ];
        let stats = aggregate_hook_stats(&events);
        assert_eq!(stats.len(), 2);
        let hook_a = stats.iter().find(|s| s.hook == "hook_a").unwrap();
        assert_eq!(hook_a.total, 2);
        assert!((hook_a.pass_rate - 0.5).abs() < f64::EPSILON);
        assert!((hook_a.block_rate - 0.5).abs() < f64::EPSILON);
        let hook_b = stats.iter().find(|s| s.hook == "hook_b").unwrap();
        assert_eq!(hook_b.total, 1);
        assert!((hook_b.pass_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn aggregate_rates_sum_to_one() {
        let events = vec![
            make_event("h", Decision::Pass),
            make_event("h", Decision::Warn),
            make_event("h", Decision::Block),
            make_event("h", Decision::Escalate),
        ];
        let stats = aggregate_hook_stats(&events);
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        let sum = s.pass_rate + s.warn_rate + s.block_rate;
        assert!((sum - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_trends_empty_events_returns_empty() {
        let trends = compute_trends(&[], 7);
        assert!(trends.is_empty());
    }

    #[test]
    fn compute_trends_single_event_returns_one_period() {
        let events = vec![make_event("h", Decision::Pass)];
        let trends = compute_trends(&events, 7);
        assert!(!trends.is_empty());
        assert!((trends[0].pass_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(trends[0].grade, Grade::A);
    }

    #[test]
    fn compute_trends_all_blocks_grade_d() {
        let events = vec![
            make_event("h", Decision::Block),
            make_event("h", Decision::Block),
        ];
        let trends = compute_trends(&events, 7);
        assert!(!trends.is_empty());
        assert!((trends[0].pass_rate - 0.0).abs() < f64::EPSILON);
        assert_eq!(trends[0].grade, Grade::D);
    }

    fn rule_event(rule_id: &str, decision: Decision) -> Event {
        Event::new(SessionId::new(), "rule_check", rule_id, decision)
    }

    #[test]
    fn rule_violation_stats_empty_returns_empty() {
        let stats = rule_violation_stats(&[]);
        assert!(stats.is_empty());
    }

    #[test]
    fn rule_violation_stats_counts_block_and_warn() {
        let events = vec![
            rule_event("SEC-01", Decision::Block),
            rule_event("SEC-01", Decision::Block),
            rule_event("RS-05", Decision::Warn),
            rule_event("SEC-01", Decision::Pass), // Pass should not count
        ];
        let stats = rule_violation_stats(&events);
        assert_eq!(stats.len(), 2);
        assert!(stats.iter().any(|s| s.rule_id == "SEC-01" && s.count == 2), "SEC-01 count should be 2");
        assert!(stats.iter().any(|s| s.rule_id == "RS-05" && s.count == 1), "RS-05 count should be 1");
    }

    #[test]
    fn rule_violation_stats_ignores_non_rule_check_events() {
        let events = vec![
            make_event("other_hook", Decision::Block),
            rule_event("SEC-01", Decision::Block),
        ];
        let stats = rule_violation_stats(&events);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].rule_id, "SEC-01");
    }

    #[test]
    fn rule_violation_stats_sorted_by_count_desc() {
        let events = vec![
            rule_event("A", Decision::Block),
            rule_event("B", Decision::Block),
            rule_event("B", Decision::Block),
        ];
        let stats = rule_violation_stats(&events);
        assert_eq!(stats[0].rule_id, "B");
        assert_eq!(stats[0].count, 2);
        assert_eq!(stats[1].rule_id, "A");
        assert_eq!(stats[1].count, 1);
    }

    #[test]
    fn rule_violation_stats_last_seen_is_most_recent() {
        let mut e1 = rule_event("SEC-01", Decision::Block);
        let mut e2 = rule_event("SEC-01", Decision::Block);
        e1.ts = chrono::Utc::now() - chrono::Duration::hours(2);
        e2.ts = chrono::Utc::now();
        let stats = rule_violation_stats(&[e1, e2]);
        assert_eq!(stats.len(), 1);
        assert!(stats[0].last_seen.is_some());
    }
}
