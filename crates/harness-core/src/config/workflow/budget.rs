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
}

impl Default for RuntimeBudgetPolicy {
    fn default() -> Self {
        Self {
            default_workflow_budget_usd: default_workflow_budget_usd(),
            enforcement: RuntimeBudgetEnforcement::default(),
            unlimited: false,
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
        Ok(())
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
