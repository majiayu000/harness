use super::ValidationContext;
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;

impl ValidationContext {
    pub fn new(actor: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self {
            actor: actor.into(),
            now,
            resource_budget_available: true,
            replan_available: true,
            wait_available: true,
            allow_terminal_reopen: false,
            allow_missing_pinned_cancel: false,
            active_dedupe_keys: BTreeSet::new(),
        }
    }

    pub fn without_resource_budget(mut self) -> Self {
        self.resource_budget_available = false;
        self
    }

    pub fn with_replan_exhausted(mut self) -> Self {
        self.replan_available = false;
        self
    }

    pub fn with_wait_exhausted(mut self) -> Self {
        self.wait_available = false;
        self
    }

    pub fn with_active_dedupe_key(mut self, dedupe_key: impl Into<String>) -> Self {
        self.active_dedupe_keys.insert(dedupe_key.into());
        self
    }

    pub fn allow_terminal_reopen(mut self) -> Self {
        self.allow_terminal_reopen = true;
        self
    }

    pub fn allow_missing_pinned_cancel(mut self) -> Self {
        self.allow_missing_pinned_cancel = true;
        self
    }
}
