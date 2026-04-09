use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::store::Skill;

/// Age thresholds for freshness classification (in days).
const FRESH_DAYS: i64 = 7;
const ACTIVE_DAYS: i64 = 30;
const DORMANT_DAYS: i64 = 90;
/// Minimum scored samples required to reach Dormant (rather than Stale).
const MIN_SAMPLES_DORMANT: u64 = 5;

/// Freshness classification for a skill based on recency and usage signals.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessClass {
    /// Used within the last 7 days.
    Fresh,
    /// Used within the last 30 days.
    Active,
    /// Used within 90 days with at least 5 scored samples.
    Dormant,
    /// Not used for >90 days, or >30 days with fewer than 5 scored samples,
    /// or never used.
    Stale,
}

impl Skill {
    /// Classify this skill's freshness based on `last_used` recency and
    /// `scored_samples` count.
    ///
    /// Boundary semantics: thresholds are inclusive on the recent side.
    /// - age <= 7d  → Fresh
    /// - age <= 30d → Active
    /// - age <= 90d AND scored_samples >= 5 → Dormant
    /// - otherwise → Stale
    pub fn classify_freshness(&self, now: chrono::DateTime<Utc>) -> FreshnessClass {
        let Some(last_used) = self.last_used else {
            // Never used → Stale
            return FreshnessClass::Stale;
        };
        let age = now - last_used;
        if age <= chrono::TimeDelta::days(FRESH_DAYS) {
            FreshnessClass::Fresh
        } else if age <= chrono::TimeDelta::days(ACTIVE_DAYS) {
            FreshnessClass::Active
        } else if age <= chrono::TimeDelta::days(DORMANT_DAYS)
            && self.scored_samples >= MIN_SAMPLES_DORMANT
        {
            FreshnessClass::Dormant
        } else {
            FreshnessClass::Stale
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SkillGovernanceStatus, SkillStore};
    use harness_core::types::{SkillId, SkillLocation};

    fn make_skill_with(last_used: Option<chrono::DateTime<Utc>>, scored_samples: u64) -> Skill {
        Skill {
            id: SkillId::new(),
            name: "test".to_string(),
            description: "desc".to_string(),
            content: "content".to_string(),
            trigger_patterns: vec![],
            version: "1.0.0".to_string(),
            author: "test".to_string(),
            location: SkillLocation::System,
            content_hash: "abc".to_string(),
            usage_count: 0,
            last_used,
            quality_score: 0.5,
            scored_samples,
            governance_status: SkillGovernanceStatus::Active,
            canary_ratio: 1.0,
            last_scored: None,
        }
    }

    fn days_ago(now: chrono::DateTime<Utc>, n: i64) -> chrono::DateTime<Utc> {
        now - chrono::TimeDelta::days(n)
    }

    #[test]
    fn never_used_is_stale() {
        let now = Utc::now();
        let skill = make_skill_with(None, 0);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Stale);
    }

    #[test]
    fn used_3_days_ago_is_fresh() {
        let now = Utc::now();
        let skill = make_skill_with(Some(days_ago(now, 3)), 0);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Fresh);
    }

    #[test]
    fn used_7_days_ago_is_fresh_boundary_inclusive() {
        let now = Utc::now();
        let last_used = now - chrono::TimeDelta::days(7) + chrono::TimeDelta::seconds(1);
        let skill = make_skill_with(Some(last_used), 0);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Fresh);
    }

    #[test]
    fn used_8_days_ago_is_active() {
        let now = Utc::now();
        let skill = make_skill_with(Some(days_ago(now, 8)), 0);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Active);
    }

    #[test]
    fn used_30_days_ago_is_active_boundary_inclusive() {
        let now = Utc::now();
        let last_used = now - chrono::TimeDelta::days(30) + chrono::TimeDelta::seconds(1);
        let skill = make_skill_with(Some(last_used), 0);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Active);
    }

    #[test]
    fn used_31_days_ago_high_samples_is_dormant() {
        let now = Utc::now();
        let skill = make_skill_with(Some(days_ago(now, 31)), 6);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Dormant);
    }

    #[test]
    fn used_31_days_ago_low_samples_is_stale() {
        let now = Utc::now();
        let skill = make_skill_with(Some(days_ago(now, 31)), 4);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Stale);
    }

    #[test]
    fn used_91_days_ago_high_samples_is_stale() {
        let now = Utc::now();
        let skill = make_skill_with(Some(days_ago(now, 91)), 10);
        assert_eq!(skill.classify_freshness(now), FreshnessClass::Stale);
    }

    #[test]
    fn list_stale_sort_order() {
        let now = Utc::now();
        let mut store = SkillStore::new();
        // Dormant skill — used 40 days ago, 6 samples
        store.skills_mut().push({
            let mut s = make_skill_with(Some(days_ago(now, 40)), 6);
            s.name = "dormant-40d".to_string();
            s
        });
        // Stale skill — used 100 days ago
        store.skills_mut().push({
            let mut s = make_skill_with(Some(days_ago(now, 100)), 0);
            s.name = "stale-100d".to_string();
            s
        });
        // Stale skill — never used (None last_used)
        store.skills_mut().push({
            let mut s = make_skill_with(None, 0);
            s.name = "stale-never".to_string();
            s
        });
        // Stale skill — used 50 days ago, low samples
        store.skills_mut().push({
            let mut s = make_skill_with(Some(days_ago(now, 50)), 2);
            s.name = "stale-50d".to_string();
            s
        });
        // Fresh skill — should be excluded
        store.skills_mut().push({
            let mut s = make_skill_with(Some(days_ago(now, 2)), 5);
            s.name = "fresh".to_string();
            s
        });

        let results = store.list_stale(now);
        let names: Vec<&str> = results.iter().map(|(s, _)| s.name.as_str()).collect();

        // Fresh excluded
        assert!(!names.contains(&"fresh"), "fresh skill must not appear");
        // Stale before Dormant
        let first_dormant = results
            .iter()
            .position(|(_, c)| *c == FreshnessClass::Dormant);
        let last_stale = results
            .iter()
            .rposition(|(_, c)| *c == FreshnessClass::Stale);
        if let (Some(fd), Some(ls)) = (first_dormant, last_stale) {
            assert!(ls < fd, "all Stale entries must precede Dormant entries");
        }
        // None (never used) sorts last among Stale
        let stale_entries: Vec<_> = results
            .iter()
            .filter(|(_, c)| *c == FreshnessClass::Stale)
            .collect();
        if stale_entries.len() >= 2 {
            let last = stale_entries.last().unwrap();
            assert_eq!(last.0.name, "stale-never", "never-used skill sorts last");
        }
    }

    #[test]
    fn list_stale_excludes_fresh_and_active() {
        let now = Utc::now();
        let mut store = SkillStore::new();
        store
            .skills_mut()
            .push(make_skill_with(Some(days_ago(now, 3)), 0)); // Fresh
        store
            .skills_mut()
            .push(make_skill_with(Some(days_ago(now, 20)), 0)); // Active
        store
            .skills_mut()
            .push(make_skill_with(Some(days_ago(now, 95)), 0)); // Stale

        let results = store.list_stale(now);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, FreshnessClass::Stale);
    }
}
