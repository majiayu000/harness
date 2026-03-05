use harness_core::{Decision, Event, Grade};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleStats {
    pub rule_id: String,
    pub total: usize,
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleTrend {
    pub period: String,
    pub rule_id: String,
    pub count: usize,
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

pub fn aggregate_rule_stats(events: &[Event]) -> Vec<RuleStats> {
    let mut map: HashMap<String, (usize, Option<chrono::DateTime<chrono::Utc>>)> = HashMap::new();
    for e in events.iter().filter(|e| e.hook == "rule_check") {
        let entry = map.entry(e.tool.clone()).or_insert((0, None));
        entry.0 += 1;
        entry.1 = Some(entry.1.map_or(e.ts, |prev| prev.max(e.ts)));
    }

    let mut stats: Vec<RuleStats> = map
        .into_iter()
        .map(|(rule_id, (total, last_seen))| RuleStats {
            rule_id,
            total,
            last_seen,
        })
        .collect();
    stats.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.rule_id.cmp(&b.rule_id)));
    stats
}

pub fn compute_rule_trends(events: &[Event], period_days: u32) -> Vec<RuleTrend> {
    let rule_events: Vec<&Event> = events.iter().filter(|e| e.hook == "rule_check").collect();
    if rule_events.is_empty() {
        return Vec::new();
    }

    let period = chrono::Duration::days(period_days as i64);
    // Add 1ms to ensure the period containing the latest event is always included.
    let now = chrono::Utc::now() + chrono::Duration::milliseconds(1);
    let earliest = rule_events.iter().map(|e| e.ts).min().unwrap_or(now);

    let mut trends = Vec::new();
    let mut period_start = earliest;

    while period_start < now {
        let period_end = period_start + period;
        let mut by_rule: HashMap<String, usize> = HashMap::new();
        for e in rule_events
            .iter()
            .copied()
            .filter(|e| e.ts >= period_start && e.ts < period_end)
        {
            *by_rule.entry(e.tool.clone()).or_default() += 1;
        }

        let mut rules: Vec<(String, usize)> = by_rule.into_iter().collect();
        rules.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (rule_id, count) in rules {
            trends.push(RuleTrend {
                period: period_start.format("%Y-%m-%d").to_string(),
                rule_id,
                count,
            });
        }

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

    #[test]
    fn aggregate_rule_stats_groups_by_rule_id() {
        let sid = SessionId::new();
        let mut e1 = Event::new(sid.clone(), "rule_check", "SEC-01", Decision::Block);
        e1.detail = Some("src/a.rs:1".to_string());
        let mut e2 = Event::new(sid.clone(), "rule_check", "SEC-01", Decision::Warn);
        e2.detail = Some("src/b.rs:2".to_string());
        let e3 = Event::new(sid, "rule_check", "U-05", Decision::Pass);

        let stats = aggregate_rule_stats(&[e1, e2, e3]);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].rule_id, "SEC-01");
        assert_eq!(stats[0].total, 2);
    }

    #[test]
    fn compute_rule_trends_empty_when_no_rule_events() {
        let trends = compute_rule_trends(&[make_event("h", Decision::Pass)], 7);
        assert!(trends.is_empty());
    }
}
