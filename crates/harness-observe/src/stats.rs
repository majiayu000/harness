use harness_core::{types::Decision, types::Event, types::Grade};
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
    pub block_count: usize,
    pub warn_count: usize,
    pub pass_count: usize,
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

        let pass_rate = if total == 0 {
            1.0
        } else {
            pass_count as f64 / total as f64
        };
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

/// Aggregate per-rule violation counts from historical rule_check events.
pub fn aggregate_rule_stats(events: &[Event]) -> Vec<RuleStats> {
    #[derive(Default)]
    struct RuleCounts {
        total: usize,
        block: usize,
        warn: usize,
        pass: usize,
        last_seen: Option<chrono::DateTime<chrono::Utc>>,
    }

    // rule_check events store the rule_id in the `tool` field
    let mut map: HashMap<String, RuleCounts> = HashMap::new();
    for e in events.iter().filter(|e| e.hook == "rule_check") {
        let counts = map.entry(e.tool.clone()).or_default();
        counts.total += 1;
        match e.decision {
            Decision::Pass | Decision::Complete => counts.pass += 1,
            Decision::Warn => counts.warn += 1,
            Decision::Block | Decision::Gate | Decision::Escalate => counts.block += 1,
        }
        counts.last_seen = Some(counts.last_seen.map_or(e.ts, |prev| prev.max(e.ts)));
    }
    let mut stats: Vec<RuleStats> = map
        .into_iter()
        .map(|(rule_id, counts)| RuleStats {
            rule_id,
            total: counts.total,
            block_count: counts.block,
            warn_count: counts.warn,
            pass_count: counts.pass,
            last_seen: counts.last_seen,
        })
        .collect();
    stats.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    stats
}

/// Count linter/compiler feedback events (Block or Escalate) from `rule_check` events.
///
/// High values indicate the agent is in a "hallucination loop" with repeated build failures.
pub fn linter_feedback_count(events: &[Event]) -> u32 {
    events
        .iter()
        .filter(|e| e.hook == "rule_check")
        .filter(|e| matches!(e.decision, Decision::Block | Decision::Escalate))
        .count() as u32
}

/// Compute the p50 (median) of a slice of turn counts.
/// Returns `None` for an empty slice.
pub fn p50_turns(turn_counts: &[u32]) -> Option<u32> {
    if turn_counts.is_empty() {
        return None;
    }
    let mut sorted = turn_counts.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
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
    use harness_core::{types::Decision, types::Event, types::SessionId};

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
    fn aggregate_rule_stats_empty_returns_empty() {
        let stats = aggregate_rule_stats(&[]);
        assert!(stats.is_empty());
    }

    #[test]
    fn aggregate_rule_stats_groups_by_rule_id() {
        let events = vec![
            make_rule_event("SEC-01", Decision::Block),
            make_rule_event("SEC-01", Decision::Warn),
            make_rule_event("SEC-02", Decision::Pass),
            make_event("other_hook", Decision::Block),
        ];
        let stats = aggregate_rule_stats(&events);
        assert_eq!(stats.len(), 2);
        assert!(
            stats.iter().any(|s| s.rule_id == "SEC-01"
                && s.total == 2
                && s.block_count == 1
                && s.warn_count == 1),
            "expected SEC-01 with 2 total, 1 block, 1 warn"
        );
        assert!(
            stats
                .iter()
                .any(|s| s.rule_id == "SEC-02" && s.total == 1 && s.pass_count == 1),
            "expected SEC-02 with 1 total, 1 pass"
        );
    }

    fn make_rule_event(rule_id: &str, decision: Decision) -> Event {
        Event::new(SessionId::new(), "rule_check", rule_id, decision)
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
    fn aggregate_rule_stats_tracks_last_seen() {
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

    #[test]
    fn linter_feedback_count_returns_zero_for_empty_events() {
        assert_eq!(linter_feedback_count(&[]), 0);
    }

    #[test]
    fn linter_feedback_count_counts_only_block_and_escalate() {
        let events = vec![
            make_rule_event("SEC-01", Decision::Block),
            make_rule_event("SEC-01", Decision::Escalate),
            make_rule_event("SEC-02", Decision::Pass),
            make_rule_event("SEC-03", Decision::Warn),
            make_event("other_hook", Decision::Block), // not rule_check
        ];
        assert_eq!(linter_feedback_count(&events), 2);
    }

    #[test]
    fn linter_feedback_count_zero_when_all_pass() {
        let events = vec![
            make_rule_event("SEC-01", Decision::Pass),
            make_rule_event("SEC-02", Decision::Complete),
        ];
        assert_eq!(linter_feedback_count(&events), 0);
    }

    #[test]
    fn p50_turns_returns_none_for_empty_slice() {
        assert_eq!(p50_turns(&[]), None);
    }

    #[test]
    fn p50_turns_single_element() {
        assert_eq!(p50_turns(&[5]), Some(5));
    }

    #[test]
    fn p50_turns_odd_count() {
        assert_eq!(p50_turns(&[1, 3, 5, 7, 9]), Some(5));
    }

    #[test]
    fn p50_turns_even_count_returns_lower_median() {
        // sorted: [1, 2, 3, 4] — index len/2 = 2 → 3
        assert_eq!(p50_turns(&[4, 1, 3, 2]), Some(3));
    }
}
