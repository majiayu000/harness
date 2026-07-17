//! Declarative intake bindings: source-agnostic matching + routing registry
//! (GH-1656).
//!
//! A binding routes an incoming issue from a registered intake source to a
//! declarative workflow definition. Matching consumes only normalized
//! `IncomingIssue` fields (source name + labels), so nothing here is
//! GitHub-specific: a future Linear source is purely additive behind the
//! `IntakeSource` trait and needs no change to this module (B-003, B-010).

use super::IncomingIssue;
use harness_core::config::workflow::WorkflowDefinitionIntakePolicy;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Canonical project key used to store and look up bindings. Both the startup
/// registry build and the `poll_tick` lookup route through this single helper so
/// the keys match by construction (avoids a silent routing miss from divergent
/// canonicalization).
pub fn binding_project_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// A validated, startup-frozen intake binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeBinding {
    /// Declarative definition an issue routes to.
    pub definition_id: String,
    /// Registered intake source name the issue must originate from.
    pub source: String,
    /// The issue must carry ALL of these labels.
    pub labels: Vec<String>,
    /// The issue must carry NONE of these labels.
    pub exclude_labels: Vec<String>,
    /// Anti-runaway cap on concurrent nonterminal instances (>= 1).
    pub max_active_instances: u32,
}

impl IntakeBinding {
    /// Build a validated binding from a definition's intake policy, cross-checking
    /// the source against the deployment's enabled intake sources (B-002
    /// fail-closed). The policy's own field validation ran at parse time (T001).
    pub fn from_policy(
        definition_id: &str,
        policy: &WorkflowDefinitionIntakePolicy,
        enabled_sources: &HashSet<String>,
    ) -> anyhow::Result<Self> {
        if !enabled_sources.contains(&policy.source) {
            let mut enabled: Vec<&str> = enabled_sources.iter().map(String::as_str).collect();
            enabled.sort_unstable();
            anyhow::bail!(
                "definition `{definition_id}` intake source `{}` is not an enabled intake source (enabled: {enabled:?})",
                policy.source
            );
        }
        Ok(Self {
            definition_id: definition_id.to_string(),
            source: policy.source.clone(),
            labels: policy.filter.labels.clone(),
            exclude_labels: policy.filter.exclude_labels.clone(),
            max_active_instances: policy.max_active_instances,
        })
    }

    /// Source + all-labels + no-exclude-labels over a normalized issue. Label
    /// matching is exact and case-sensitive (B-003).
    pub fn matches(&self, issue: &IncomingIssue) -> bool {
        if issue.source != self.source {
            return false;
        }
        let present: std::collections::HashSet<&str> =
            issue.labels.iter().map(String::as_str).collect();
        self.labels
            .iter()
            .all(|label| present.contains(label.as_str()))
            && !self
                .exclude_labels
                .iter()
                .any(|label| present.contains(label.as_str()))
    }
}

/// The resolved routing target for an issue: the chosen binding plus the full
/// tie list (all matching definition ids, sorted) for audit (B-004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeMatch<'a> {
    pub binding: &'a IntakeBinding,
    /// All matching definition ids, sorted ascending. Length > 1 means a tie was
    /// broken by taking the smallest.
    pub tie_definition_ids: Vec<String>,
}

/// Project-scoped registry of intake bindings, built once at startup and
/// immutable afterwards — same freeze discipline as the definition registry.
#[derive(Debug, Default, Clone)]
pub struct IntakeBindingRegistry {
    by_project: HashMap<String, Vec<IntakeBinding>>,
}

impl IntakeBindingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(project_id, binding)` pairs. Bindings within a project are
    /// stored sorted by definition id so resolution is deterministic.
    pub fn from_bindings(pairs: impl IntoIterator<Item = (String, IntakeBinding)>) -> Self {
        let mut by_project: HashMap<String, Vec<IntakeBinding>> = HashMap::new();
        for (project_id, binding) in pairs {
            by_project.entry(project_id).or_default().push(binding);
        }
        for bindings in by_project.values_mut() {
            bindings.sort_by(|a, b| a.definition_id.cmp(&b.definition_id));
        }
        Self { by_project }
    }

    pub fn is_empty(&self) -> bool {
        self.by_project.is_empty()
    }

    /// Resolve the routing target for an issue in a project. `None` if no
    /// binding matches (the issue falls through to the default path, B-006).
    /// Multiple matches are tie-broken by smallest definition id; the tie list
    /// is returned for audit (B-004).
    pub fn resolve<'a>(
        &'a self,
        project_id: &str,
        issue: &IncomingIssue,
    ) -> Option<IntakeMatch<'a>> {
        let bindings = self.by_project.get(project_id)?;
        // Bindings are pre-sorted by definition id, so the first match is the
        // smallest and `tie_definition_ids` comes out sorted.
        let matches: Vec<&IntakeBinding> = bindings
            .iter()
            .filter(|binding| binding.matches(issue))
            .collect();
        let chosen = *matches.first()?;
        let tie_definition_ids = matches
            .iter()
            .map(|binding| binding.definition_id.clone())
            .collect();
        Some(IntakeMatch {
            binding: chosen,
            tie_definition_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::config::isolation::IsolationTrustClass;

    fn issue(source: &str, labels: &[&str]) -> IncomingIssue {
        IncomingIssue {
            source: source.to_string(),
            external_id: "1".to_string(),
            identifier: "#1".to_string(),
            title: "t".to_string(),
            description: None,
            repo: Some("owner/repo".to_string()),
            url: None,
            priority: None,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            created_at: None,
            author_trust_class: IsolationTrustClass::Trusted,
            project_root: None,
        }
    }

    fn binding(
        definition_id: &str,
        source: &str,
        labels: &[&str],
        exclude: &[&str],
    ) -> IntakeBinding {
        IntakeBinding {
            definition_id: definition_id.to_string(),
            source: source.to_string(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            exclude_labels: exclude.iter().map(|l| l.to_string()).collect(),
            max_active_instances: 8,
        }
    }

    #[test]
    fn matches_requires_source_and_all_labels() {
        let b = binding("docs_flow", "github", &["docs", "review"], &[]);
        assert!(b.matches(&issue("github", &["docs", "review", "extra"])));
        assert!(
            !b.matches(&issue("github", &["docs"])),
            "missing a required label"
        );
        assert!(
            !b.matches(&issue("feishu", &["docs", "review"])),
            "wrong source"
        );
    }

    #[test]
    fn matches_rejects_on_exclude_label() {
        let b = binding("docs_flow", "github", &["docs"], &["wip"]);
        assert!(b.matches(&issue("github", &["docs"])));
        assert!(!b.matches(&issue("github", &["docs", "wip"])));
    }

    #[test]
    fn matches_is_case_sensitive() {
        let b = binding("docs_flow", "github", &["Docs"], &[]);
        assert!(!b.matches(&issue("github", &["docs"])));
        assert!(b.matches(&issue("github", &["Docs"])));
    }

    #[test]
    fn matches_a_non_github_fixture_source() {
        // B-003: nothing is GitHub-specific; a fictional source routes the same.
        let b = binding("triage_flow", "linear", &["bug"], &[]);
        assert!(b.matches(&issue("linear", &["bug", "p1"])));
        assert!(!b.matches(&issue("github", &["bug"])));
    }

    #[test]
    fn resolve_returns_none_when_no_binding_matches() {
        let reg = IntakeBindingRegistry::from_bindings([(
            "proj".to_string(),
            binding("docs_flow", "github", &["docs"], &[]),
        )]);
        assert!(reg
            .resolve("proj", &issue("github", &["feature"]))
            .is_none());
        assert!(reg.resolve("other", &issue("github", &["docs"])).is_none());
    }

    #[test]
    fn resolve_tie_breaks_on_smallest_definition_id_with_full_tie_list() {
        // B-004: two bindings match the same issue; smallest id wins, tie audited.
        let reg = IntakeBindingRegistry::from_bindings([
            (
                "proj".to_string(),
                binding("zeta_flow", "github", &["docs"], &[]),
            ),
            (
                "proj".to_string(),
                binding("alpha_flow", "github", &["docs"], &[]),
            ),
        ]);
        let m = reg
            .resolve("proj", &issue("github", &["docs"]))
            .expect("a binding should match");
        assert_eq!(m.binding.definition_id, "alpha_flow");
        assert_eq!(m.tie_definition_ids, vec!["alpha_flow", "zeta_flow"]);
    }

    #[test]
    fn resolve_single_match_has_singleton_tie_list() {
        let reg = IntakeBindingRegistry::from_bindings([(
            "proj".to_string(),
            binding("docs_flow", "github", &["docs"], &[]),
        )]);
        let m = reg
            .resolve("proj", &issue("github", &["docs"]))
            .expect("a binding should match");
        assert_eq!(m.tie_definition_ids, vec!["docs_flow"]);
    }

    fn intake_policy(source: &str) -> WorkflowDefinitionIntakePolicy {
        use harness_core::config::workflow::IntakeFilterPolicy;
        WorkflowDefinitionIntakePolicy {
            source: source.to_string(),
            filter: IntakeFilterPolicy {
                labels: vec!["docs".to_string()],
                exclude_labels: vec!["wip".to_string()],
            },
            max_active_instances: 4,
        }
    }

    #[test]
    fn from_policy_builds_binding_when_source_is_enabled() {
        let enabled: HashSet<String> = ["github".to_string()].into_iter().collect();
        let b = IntakeBinding::from_policy("docs_flow", &intake_policy("github"), &enabled)
            .expect("enabled source should build");
        assert_eq!(b.definition_id, "docs_flow");
        assert_eq!(b.source, "github");
        assert_eq!(b.labels, ["docs"]);
        assert_eq!(b.exclude_labels, ["wip"]);
        assert_eq!(b.max_active_instances, 4);
    }

    #[test]
    fn from_policy_rejects_disabled_or_unknown_source() {
        // B-002: a binding targeting a source the deployment has not enabled
        // aborts startup fail-closed.
        let enabled: HashSet<String> = ["github".to_string()].into_iter().collect();
        assert!(
            IntakeBinding::from_policy("triage_flow", &intake_policy("feishu"), &enabled).is_err(),
            "a source not in the enabled set must be rejected"
        );
        assert!(
            IntakeBinding::from_policy("triage_flow", &intake_policy("linear"), &HashSet::new())
                .is_err(),
            "no enabled sources means every binding is rejected"
        );
    }
}
