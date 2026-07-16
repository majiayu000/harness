//! Registry-driven definition-id enumeration for operator-facing surfaces.
//!
//! Operator monitor, dashboard active counts, and overview previously hardcoded
//! the built-in definition ids in static lists, which hid declarative workflow
//! definitions (GH-1609) from every operator surface. These helpers read the
//! frozen definition registry instead, so declarative definitions appear
//! automatically once registered (GH-1652).
//!
//! The registry is seeded with built-ins and frozen after startup registration
//! (`server.rs`), so request-time reads are stable snapshots; no caching is
//! needed. The sort/validate/filter logic lives in pure helpers so it can be
//! unit-tested with arbitrary id sets (including the empty registry) without
//! mutating the process-global registry.

use harness_workflow::runtime::{
    known_workflow_definition_ids, PR_FEEDBACK_DEFINITION_ID, QUALITY_GATE_DEFINITION_ID,
};

/// All definition ids for operator enumeration, from the frozen registry, in
/// sorted order for deterministic payload output (B-001, B-004).
///
/// Errors when the registry reports no definitions (B-006) — impossible after a
/// healthy startup, where the built-ins are always registered.
pub fn operator_definition_ids() -> anyhow::Result<Vec<String>> {
    sorted_non_empty(known_workflow_definition_ids())
}

/// Definition ids for active-count surfaces (dashboard active counts, overview).
///
/// Built-in child definitions (`pr_feedback`, `quality_gate`) are excluded
/// because they shadow their parent workflows and would double-count active
/// work. This preserves the exclusion deliberately introduced in "Fix dashboard
/// runtime metrics" (commit 8d0fa39a), where active counts enumerated only the
/// two top-level built-ins. Declarative definitions are always included: they
/// are top-level and never shadow a built-in parent (B-002, B-003).
pub fn active_count_definition_ids() -> anyhow::Result<Vec<String>> {
    Ok(exclude_builtin_children(operator_definition_ids()?))
}

/// Sort ids and reject an empty set. Pure so it is testable without the global
/// registry (B-004, B-006).
fn sorted_non_empty(mut ids: Vec<String>) -> anyhow::Result<Vec<String>> {
    if ids.is_empty() {
        anyhow::bail!("workflow definition registry returned no definitions");
    }
    ids.sort();
    Ok(ids)
}

/// Drop the two built-in child definitions; keep everything else, including all
/// declarative ids. Order-preserving, so a sorted input stays sorted (B-002).
fn exclude_builtin_children(ids: Vec<String>) -> Vec<String> {
    ids.into_iter()
        .filter(|id| id != PR_FEEDBACK_DEFINITION_ID && id != QUALITY_GATE_DEFINITION_ID)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{GITHUB_ISSUE_PR_DEFINITION_ID, PROMPT_TASK_DEFINITION_ID};

    #[test]
    fn sorted_non_empty_sorts_and_includes_declarative(/* B-003, B-004 */) {
        let ids = sorted_non_empty(vec![
            "docs_review_flow".to_string(),
            GITHUB_ISSUE_PR_DEFINITION_ID.to_string(),
            "aardvark_flow".to_string(),
        ])
        .expect("non-empty input");
        assert_eq!(
            ids,
            vec![
                "aardvark_flow".to_string(),
                "docs_review_flow".to_string(),
                GITHUB_ISSUE_PR_DEFINITION_ID.to_string(),
            ],
            "ids must be sorted, and declarative ids must survive"
        );
    }

    #[test]
    fn sorted_non_empty_errors_on_empty_registry(/* B-006 */) {
        assert!(
            sorted_non_empty(Vec::new()).is_err(),
            "an empty registry must be an error, not a silent empty set"
        );
    }

    #[test]
    fn exclude_builtin_children_drops_children_keeps_declarative(/* B-002, B-003 */) {
        let out = exclude_builtin_children(vec![
            GITHUB_ISSUE_PR_DEFINITION_ID.to_string(),
            PR_FEEDBACK_DEFINITION_ID.to_string(),
            PROMPT_TASK_DEFINITION_ID.to_string(),
            QUALITY_GATE_DEFINITION_ID.to_string(),
            "docs_review_flow".to_string(),
        ]);
        assert_eq!(
            out,
            vec![
                GITHUB_ISSUE_PR_DEFINITION_ID.to_string(),
                PROMPT_TASK_DEFINITION_ID.to_string(),
                "docs_review_flow".to_string(),
            ],
            "built-in children excluded; top-level built-ins and declarative kept, order preserved"
        );
    }

    #[test]
    fn operator_ids_from_global_registry_include_all_builtins(/* B-001 */) {
        // The process-global registry is always seeded with the four built-ins.
        let ids = operator_definition_ids().expect("built-ins registered");
        for expected in [
            GITHUB_ISSUE_PR_DEFINITION_ID,
            PR_FEEDBACK_DEFINITION_ID,
            PROMPT_TASK_DEFINITION_ID,
            QUALITY_GATE_DEFINITION_ID,
        ] {
            assert!(
                ids.iter().any(|id| id == expected),
                "missing built-in definition id: {expected}"
            );
        }
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "operator ids must be sorted");
    }
}
