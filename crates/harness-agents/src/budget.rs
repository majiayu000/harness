use harness_core::BudgetTier;
use serde::{Deserialize, Serialize};

/// Reasoning budget sandwich: planning (xhigh) → execution (high) → validation (xhigh).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningBudget {
    pub planning: BudgetTier,
    pub execution: BudgetTier,
    pub validation: BudgetTier,
}

impl Default for ReasoningBudget {
    fn default() -> Self {
        Self {
            planning: BudgetTier::XHigh,
            execution: BudgetTier::High,
            validation: BudgetTier::XHigh,
        }
    }
}

impl ReasoningBudget {
    pub fn model_for_tier(tier: BudgetTier) -> &'static str {
        match tier {
            BudgetTier::XHigh => "opus",
            BudgetTier::High => "sonnet",
            BudgetTier::Medium => "haiku",
        }
    }

    pub fn planning_model(&self) -> &'static str {
        Self::model_for_tier(self.planning)
    }

    pub fn execution_model(&self) -> &'static str {
        Self::model_for_tier(self.execution)
    }

    pub fn validation_model(&self) -> &'static str {
        Self::model_for_tier(self.validation)
    }
}
