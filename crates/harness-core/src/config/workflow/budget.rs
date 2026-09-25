use serde::{Deserialize, Serialize};

/// Pre-dispatch workflow budget gate policy (GH-1770).
///
/// Spend is aggregated from adapter-reported `runtime_usage.cost_usd_micros`
/// for the workflow instance. The gate runs in the runtime command dispatcher
/// before a command is enqueued as a runtime job.
///
/// The default enforcement is [`RuntimeBudgetEnforcement::Shadow`]: the
/// dispatcher records the would-block decision as a `BudgetShadowDecision`
/// runtime event but still dispatches. `Enforce` defers the command with the
/// `workflow_budget_exhausted` dispatch barrier reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeBudgetPolicy {
    /// Per-workflow-instance spend ceiling in USD.
    #[serde(default = "default_workflow_budget_usd")]
    pub default_workflow_budget_usd: f64,
    #[serde(default)]
    pub enforcement: RuntimeBudgetEnforcement,
    /// Explicit opt-out: no gate at all. An absent policy no longer means
    /// unlimited; only this flag does.
    #[serde(default)]
    pub unlimited: bool,
    /// Per-runtime-profile spend cap in USD over the current UTC day.
    /// `None` = no daily cap; daily caps roll out last per the GH-1770
    /// spec, so unlike the workflow ceiling there is no built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_profile_cap_usd: Option<f64>,
    /// Fraction of `daily_profile_cap_usd` at which a profile enters the
    /// throttle band. Inside the band the profile still runs, but its commands
    /// yield the dispatch slot to profiles that are under their own threshold.
    /// Ignored when no daily cap is configured.
    #[serde(
        default = "default_daily_throttle_ratio",
        skip_serializing_if = "is_default_daily_throttle_ratio"
    )]
    pub daily_throttle_ratio: f64,
}

impl Default for RuntimeBudgetPolicy {
    fn default() -> Self {
        Self {
            default_workflow_budget_usd: default_workflow_budget_usd(),
            enforcement: RuntimeBudgetEnforcement::default(),
            unlimited: false,
            daily_profile_cap_usd: None,
            daily_throttle_ratio: default_daily_throttle_ratio(),
        }
    }
}

impl RuntimeBudgetPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.unlimited {
            return Ok(());
        }
        if !self.default_workflow_budget_usd.is_finite() || self.default_workflow_budget_usd <= 0.0
        {
            anyhow::bail!(
                "runtime_budget_policy.default_workflow_budget_usd must be a positive finite number"
            );
        }
        if let Some(cap) = self.daily_profile_cap_usd {
            if !cap.is_finite() || cap <= 0.0 {
                anyhow::bail!(
                    "runtime_budget_policy.daily_profile_cap_usd must be a positive finite number when set"
                );
            }
            if !self.daily_throttle_ratio.is_finite()
                || self.daily_throttle_ratio <= 0.0
                || self.daily_throttle_ratio > 1.0
            {
                anyhow::bail!(
                    "runtime_budget_policy.daily_throttle_ratio must be within (0, 1] when a daily cap is set"
                );
            }
        }
        Ok(())
    }

    /// USD spend at which a profile enters the throttle band, or `None` when
    /// no daily cap is configured.
    pub fn daily_throttle_threshold_usd(&self) -> Option<f64> {
        self.daily_profile_cap_usd
            .map(|cap| cap * self.daily_throttle_ratio)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeBudgetEnforcement {
    /// Record the decision, never block.
    #[default]
    Shadow,
    /// Defer dispatch once the workflow spend reaches the budget.
    Enforce,
}

impl RuntimeBudgetEnforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }
}

fn default_workflow_budget_usd() -> f64 {
    15.0
}

fn default_daily_throttle_ratio() -> f64 {
    0.8
}

/// The built-in ratio is not rendered back into config, so an unchanged policy
/// serializes exactly as it did before the throttle band existed.
fn is_default_daily_throttle_ratio(ratio: &f64) -> bool {
    *ratio == default_daily_throttle_ratio()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_shadow_with_builtin_budget() {
        let policy = RuntimeBudgetPolicy::default();
        assert_eq!(policy.default_workflow_budget_usd, 15.0);
        assert_eq!(policy.enforcement, RuntimeBudgetEnforcement::Shadow);
        assert!(!policy.unlimited);
        policy.validate().expect("default policy is valid");
    }

    #[test]
    fn policy_deserializes_from_partial_front_matter() {
        let policy: RuntimeBudgetPolicy =
            serde_yaml::from_str("enforcement: enforce").expect("partial policy parses");
        assert_eq!(policy.default_workflow_budget_usd, 15.0);
        assert_eq!(policy.enforcement, RuntimeBudgetEnforcement::Enforce);
        assert!(!policy.unlimited);
    }

    #[test]
    fn enforcement_serializes_as_snake_case() {
        assert_eq!(
            serde_yaml::to_string(&RuntimeBudgetEnforcement::Shadow).unwrap(),
            "shadow\n"
        );
        assert_eq!(
            serde_yaml::to_string(&RuntimeBudgetEnforcement::Enforce).unwrap(),
            "enforce\n"
        );
    }

    #[test]
    fn daily_cap_defaults_off_and_is_not_serialized() {
        let policy = RuntimeBudgetPolicy::default();
        assert_eq!(policy.daily_profile_cap_usd, None);
        let rendered = serde_yaml::to_string(&policy).expect("serialize");
        assert!(
            !rendered.contains("daily_profile_cap_usd"),
            "absent cap must not appear in rendered config: {rendered}"
        );
        assert!(
            !rendered.contains("daily_throttle_ratio"),
            "the built-in throttle ratio must not appear in rendered config: {rendered}"
        );
    }

    #[test]
    fn validate_rejects_non_positive_daily_cap() {
        for cap in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            let policy = RuntimeBudgetPolicy {
                daily_profile_cap_usd: Some(cap),
                ..RuntimeBudgetPolicy::default()
            };
            assert!(policy.validate().is_err(), "cap {cap} must be rejected");
        }
        let policy = RuntimeBudgetPolicy {
            daily_profile_cap_usd: Some(200.0),
            ..RuntimeBudgetPolicy::default()
        };
        policy.validate().expect("positive cap validates");
    }

    #[test]
    fn validate_rejects_out_of_range_throttle_ratio() {
        for ratio in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
            let policy = RuntimeBudgetPolicy {
                daily_profile_cap_usd: Some(200.0),
                daily_throttle_ratio: ratio,
                ..RuntimeBudgetPolicy::default()
            };
            assert!(policy.validate().is_err(), "ratio {ratio} must be rejected");
        }
        // Without a daily cap the ratio is inert and must not fail validation.
        let policy = RuntimeBudgetPolicy {
            daily_profile_cap_usd: None,
            daily_throttle_ratio: 5.0,
            ..RuntimeBudgetPolicy::default()
        };
        policy.validate().expect("ratio is ignored without a cap");
    }

    #[test]
    fn throttle_threshold_defaults_to_eighty_percent_of_the_cap() {
        let policy = RuntimeBudgetPolicy {
            daily_profile_cap_usd: Some(200.0),
            ..RuntimeBudgetPolicy::default()
        };
        assert_eq!(policy.daily_throttle_ratio, 0.8);
        assert_eq!(policy.daily_throttle_threshold_usd(), Some(160.0));
        assert_eq!(
            RuntimeBudgetPolicy::default().daily_throttle_threshold_usd(),
            None
        );
    }

    #[test]
    fn validate_rejects_non_positive_budget() {
        for budget in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let policy = RuntimeBudgetPolicy {
                default_workflow_budget_usd: budget,
                ..RuntimeBudgetPolicy::default()
            };
            assert!(
                policy.validate().is_err(),
                "budget {budget} must be rejected"
            );
        }
    }

    #[test]
    fn unlimited_skips_budget_validation() {
        let policy = RuntimeBudgetPolicy {
            default_workflow_budget_usd: 0.0,
            unlimited: true,
            ..RuntimeBudgetPolicy::default()
        };
        policy.validate().expect("unlimited policy ignores budget");
    }
}
