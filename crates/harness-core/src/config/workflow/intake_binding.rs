//! Declarative workflow intake bindings (GH-1656).
//!
//! An optional `intake` block on a `definition` routes incoming issues from a
//! registered intake source to that declarative definition. The binding
//! consumes only normalized `IncomingIssue` fields (source name, labels), so it
//! is source-agnostic: a future Linear source is purely additive behind the
//! `IntakeSource` trait and needs no schema change here.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Optional intake binding for a declarative workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowDefinitionIntakePolicy {
    /// Registered intake source name (e.g. "github", "feishu"). Never a
    /// source-specific structure.
    pub source: String,
    pub filter: IntakeFilterPolicy,
    /// Anti-runaway cap on concurrent (nonterminal) instances of this
    /// definition. Must be >= 1.
    pub max_active_instances: u32,
}

/// Label filter for an intake binding. Matching is exact and case-sensitive,
/// consistent with `IncomingIssue.labels` as delivered by sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntakeFilterPolicy {
    /// Non-empty; an issue must carry ALL of these labels to match.
    pub labels: Vec<String>,
    /// An issue must carry NONE of these labels to match.
    #[serde(default)]
    pub exclude_labels: Vec<String>,
}

impl WorkflowDefinitionIntakePolicy {
    /// Parse-level validation (B-002). Runs when a WORKFLOW.md declares an
    /// `intake` block, before any source-registration cross-check at startup.
    pub fn validate(&self, definition_id: &str) -> anyhow::Result<()> {
        if self.source.trim().is_empty() {
            anyhow::bail!("definition `{definition_id}` intake source must not be empty");
        }
        if self.max_active_instances < 1 {
            anyhow::bail!("definition `{definition_id}` intake max_active_instances must be >= 1");
        }
        self.filter.validate(definition_id)
    }
}

impl IntakeFilterPolicy {
    fn validate(&self, definition_id: &str) -> anyhow::Result<()> {
        if self.labels.is_empty() {
            anyhow::bail!(
                "definition `{definition_id}` intake filter.labels must not be empty — no catch-all bindings"
            );
        }
        let mut seen = HashSet::new();
        for label in &self.labels {
            if label.trim().is_empty() {
                anyhow::bail!(
                    "definition `{definition_id}` intake filter.labels contains an empty label"
                );
            }
            if !seen.insert(label.as_str()) {
                anyhow::bail!(
                    "definition `{definition_id}` intake filter.labels contains a duplicate label `{label}`"
                );
            }
        }
        for label in &self.exclude_labels {
            if label.trim().is_empty() {
                anyhow::bail!(
                    "definition `{definition_id}` intake filter.exclude_labels contains an empty label"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(labels: Vec<&str>, cap: u32) -> WorkflowDefinitionIntakePolicy {
        WorkflowDefinitionIntakePolicy {
            source: "github".to_string(),
            filter: IntakeFilterPolicy {
                labels: labels.into_iter().map(str::to_string).collect(),
                exclude_labels: vec![],
            },
            max_active_instances: cap,
        }
    }

    #[test]
    fn valid_binding_passes() {
        assert!(policy(vec!["docs-review"], 8).validate("docs_flow").is_ok());
    }

    #[test]
    fn empty_source_is_rejected() {
        let mut p = policy(vec!["docs-review"], 1);
        p.source = "   ".to_string();
        assert!(p.validate("docs_flow").is_err());
    }

    #[test]
    fn empty_labels_are_rejected() {
        assert!(policy(vec![], 1).validate("docs_flow").is_err());
    }

    #[test]
    fn empty_label_string_is_rejected() {
        assert!(policy(vec!["docs-review", "  "], 1)
            .validate("docs_flow")
            .is_err());
    }

    #[test]
    fn duplicate_labels_are_rejected() {
        assert!(policy(vec!["docs-review", "docs-review"], 1)
            .validate("docs_flow")
            .is_err());
    }

    #[test]
    fn zero_cap_is_rejected() {
        assert!(policy(vec!["docs-review"], 0)
            .validate("docs_flow")
            .is_err());
    }
}
