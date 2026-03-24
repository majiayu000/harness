use chrono::{DateTime, Utc};
use harness_core::RuleId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Stage in the rule graduation pipeline.
///
/// ```text
/// EXPERIMENTAL (7 days, off by default)
///   ↓ precision ≥ 70% AND ≥ 20 samples
/// WARN (emit warnings, collect triage)
///   ↓ precision ≥ 90% AND ≥ 50 samples AND 30 days no FP
/// ERROR_BLOCK (block commits)
///   ↓ precision < 80% at any stage
/// DEMOTED → DISABLED
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleLifecycle {
    /// New rule; runs in observe-only mode (no output to user).
    Experimental,
    /// Emits warnings; triaged by user feedback.
    Warn,
    /// Blocks commits; high-confidence rule.
    ErrorBlock,
    /// Precision fell below threshold; demoted from active enforcement.
    Demoted,
    /// Disabled; no scanning performed.
    Disabled,
}

impl RuleLifecycle {
    /// Returns `true` when the lifecycle stage produces visible output.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Warn | Self::ErrorBlock)
    }

    /// Returns `true` when violations from this rule should block commits.
    pub fn is_blocking(self) -> bool {
        matches!(self, Self::ErrorBlock)
    }
}

impl std::fmt::Display for RuleLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Experimental => "experimental",
            Self::Warn => "warn",
            Self::ErrorBlock => "error_block",
            Self::Demoted => "demoted",
            Self::Disabled => "disabled",
        };
        write!(f, "{s}")
    }
}

/// Per-rule precision tracking entry stored in `rule-scorecard.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorecardEntry {
    pub lifecycle: RuleLifecycle,
    /// True-positive count (user confirmed real violation).
    pub tp: u64,
    /// False-positive count (user marked as spurious).
    pub fp: u64,
    /// Acceptable count (intentional pattern, not a bug).
    pub acceptable: u64,
    /// Total triage samples recorded.
    pub samples: u64,
    /// Computed precision `tp / (tp + fp)`, `None` when no samples yet.
    pub precision: Option<f64>,
    /// ISO-8601 timestamp when the rule reached its current lifecycle stage.
    pub promoted_at: Option<DateTime<Utc>>,
    /// ISO-8601 timestamp of the last recorded false positive.
    pub last_fp_at: Option<DateTime<Utc>>,
    /// Free-form notes about the rule.
    #[serde(default)]
    pub notes: String,
}

impl ScorecardEntry {
    pub fn new_experimental() -> Self {
        Self {
            lifecycle: RuleLifecycle::Experimental,
            tp: 0,
            fp: 0,
            acceptable: 0,
            samples: 0,
            precision: None,
            promoted_at: None,
            last_fp_at: None,
            notes: String::new(),
        }
    }

    /// Recompute precision from raw counts.
    pub fn recompute_precision(&mut self) {
        let denominator = self.tp + self.fp;
        self.precision = if denominator == 0 {
            None
        } else {
            Some(self.tp as f64 / denominator as f64)
        };
    }

    /// Evaluate whether a lifecycle transition should occur and apply it.
    ///
    /// Returns `true` when the lifecycle stage changed.
    pub fn evaluate_transition(&mut self, now: DateTime<Utc>) -> bool {
        let precision = match self.precision {
            Some(p) => p,
            None => return false,
        };

        match self.lifecycle {
            RuleLifecycle::Experimental => {
                if precision >= 0.70 && self.samples >= 20 {
                    self.lifecycle = RuleLifecycle::Warn;
                    self.promoted_at = Some(now);
                    return true;
                }
            }
            RuleLifecycle::Warn => {
                if precision < 0.80 {
                    self.lifecycle = RuleLifecycle::Demoted;
                    self.promoted_at = Some(now);
                    return true;
                }
                if precision >= 0.90 && self.samples >= 50 {
                    let thirty_days = chrono::Duration::days(30);
                    let fp_ok = match self.last_fp_at {
                        None => true,
                        Some(last_fp) => now - last_fp >= thirty_days,
                    };
                    if fp_ok {
                        self.lifecycle = RuleLifecycle::ErrorBlock;
                        self.promoted_at = Some(now);
                        return true;
                    }
                }
            }
            RuleLifecycle::ErrorBlock => {
                if precision < 0.80 {
                    self.lifecycle = RuleLifecycle::Demoted;
                    self.promoted_at = Some(now);
                    return true;
                }
            }
            RuleLifecycle::Demoted => {
                self.lifecycle = RuleLifecycle::Disabled;
                self.promoted_at = Some(now);
                return true;
            }
            RuleLifecycle::Disabled => {}
        }
        false
    }
}

/// Top-level scorecard document written to `.harness/rule-scorecard.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scorecard {
    pub updated_at: DateTime<Utc>,
    pub rules: HashMap<String, ScorecardEntry>,
}

impl Scorecard {
    pub fn new() -> Self {
        Self {
            updated_at: Utc::now(),
            rules: HashMap::new(),
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    }

    pub fn save(&mut self, path: &Path) -> anyhow::Result<()> {
        self.updated_at = Utc::now();
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Return the lifecycle stage for a rule, defaulting to `Warn` when not
    /// present in the scorecard (backwards-compatible with pre-existing guards).
    pub fn lifecycle_for(&self, rule_id: &RuleId) -> RuleLifecycle {
        self.rules
            .get(rule_id.as_str())
            .map(|e| e.lifecycle)
            .unwrap_or(RuleLifecycle::Warn)
    }

    /// Ensure every rule in `rule_ids` has an entry, initialising absent ones
    /// as `Experimental`.
    pub fn ensure_entries<'a>(&mut self, rule_ids: impl Iterator<Item = &'a str>) {
        for id in rule_ids {
            self.rules
                .entry(id.to_string())
                .or_insert_with(ScorecardEntry::new_experimental);
        }
    }
}

impl Default for Scorecard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experimental_promotes_to_warn_at_threshold() {
        let mut entry = ScorecardEntry::new_experimental();
        entry.tp = 15;
        entry.fp = 5;
        entry.samples = 20;
        entry.recompute_precision(); // 0.75
        let changed = entry.evaluate_transition(Utc::now());
        assert!(changed);
        assert_eq!(entry.lifecycle, RuleLifecycle::Warn);
    }

    #[test]
    fn experimental_stays_when_samples_too_low() {
        let mut entry = ScorecardEntry::new_experimental();
        entry.tp = 14;
        entry.fp = 1;
        entry.samples = 15;
        entry.recompute_precision();
        let changed = entry.evaluate_transition(Utc::now());
        assert!(!changed);
        assert_eq!(entry.lifecycle, RuleLifecycle::Experimental);
    }

    #[test]
    fn warn_demotes_on_low_precision() {
        let mut entry = ScorecardEntry {
            lifecycle: RuleLifecycle::Warn,
            tp: 7,
            fp: 3,
            acceptable: 0,
            samples: 10,
            precision: None,
            promoted_at: None,
            last_fp_at: None,
            notes: String::new(),
        };
        entry.recompute_precision(); // 0.70 < 0.80 threshold
        let changed = entry.evaluate_transition(Utc::now());
        assert!(changed);
        assert_eq!(entry.lifecycle, RuleLifecycle::Demoted);
    }

    #[test]
    fn warn_promotes_to_error_block_after_30_day_fp_free() {
        let mut entry = ScorecardEntry {
            lifecycle: RuleLifecycle::Warn,
            tp: 50,
            fp: 0,
            acceptable: 0,
            samples: 50,
            precision: None,
            promoted_at: None,
            last_fp_at: None, // no FP ever recorded
            notes: String::new(),
        };
        entry.recompute_precision(); // 1.0
        let changed = entry.evaluate_transition(Utc::now());
        assert!(changed);
        assert_eq!(entry.lifecycle, RuleLifecycle::ErrorBlock);
    }

    #[test]
    fn warn_does_not_promote_when_recent_fp() {
        let recent_fp = Utc::now() - chrono::Duration::days(10);
        let mut entry = ScorecardEntry {
            lifecycle: RuleLifecycle::Warn,
            tp: 50,
            fp: 0,
            acceptable: 0,
            samples: 50,
            precision: None,
            promoted_at: None,
            last_fp_at: Some(recent_fp),
            notes: String::new(),
        };
        entry.recompute_precision();
        let changed = entry.evaluate_transition(Utc::now());
        assert!(!changed);
        assert_eq!(entry.lifecycle, RuleLifecycle::Warn);
    }

    #[test]
    fn scorecard_lifecycle_for_unknown_rule_defaults_to_warn() {
        let sc = Scorecard::new();
        let id = RuleId::from_str("UNKNOWN-99");
        assert_eq!(sc.lifecycle_for(&id), RuleLifecycle::Warn);
    }

    #[test]
    fn scorecard_serde_roundtrip() -> anyhow::Result<()> {
        let mut sc = Scorecard::new();
        sc.rules
            .insert("RS-03".to_string(), ScorecardEntry::new_experimental());
        let json = serde_json::to_string(&sc)?;
        let back: Scorecard = serde_json::from_str(&json)?;
        assert!(back.rules.contains_key("RS-03"));
        Ok(())
    }
}
