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
}
