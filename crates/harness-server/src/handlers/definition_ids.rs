//! Registry-driven workflow definition id enumeration for operator-facing
//! surfaces (GH-1652).
//!
//! Operator monitor, monitor sampling, dashboard active counts, and overview
//! active counts must never maintain their own static definition id lists.
//! They enumerate the frozen workflow definition registry through this module
//! so declarative definitions (GH-1609) are as visible as built-ins.

use harness_workflow::runtime::{PR_FEEDBACK_DEFINITION_ID, QUALITY_GATE_DEFINITION_ID};

/// Built-in child workflow definitions excluded from active-count surfaces.
///
/// `pr_feedback` and `quality_gate` instances are spawned as children of a
/// parent workflow (`github_issue_pr` / `prompt_task`), so counting them in
/// dashboard/overview "active" totals would double-count the parent's
/// activity. The active-count surfaces have excluded them since they were
/// introduced (see `specs/GH1652/tech.md`); this predicate preserves that
/// exclusion exactly. Declarative definitions are top-level submissions and
/// are never excluded.
const ACTIVE_COUNT_EXCLUDED_CHILD_DEFINITION_IDS: &[&str] =
    &[PR_FEEDBACK_DEFINITION_ID, QUALITY_GATE_DEFINITION_ID];

/// Definition ids for operator-facing enumeration (operator monitor and its
/// sampling), read from the frozen registry in sorted order (B-001, B-004).
///
/// Errors if the registry reports no definitions (B-006) — impossible after
/// a healthy startup, which seeds the built-ins.
pub(crate) fn operator_definition_ids() -> anyhow::Result<Vec<String>> {
    sorted_definition_ids(harness_workflow::runtime::known_workflow_definition_ids())
}

/// Definition ids for dashboard/overview active-count enumeration: the
/// operator set minus built-in child workflow definitions (B-002, B-003).
pub(crate) fn active_count_definition_ids() -> anyhow::Result<Vec<String>> {
    Ok(without_child_definitions(operator_definition_ids()?))
}

fn sorted_definition_ids(mut ids: Vec<String>) -> anyhow::Result<Vec<String>> {
    if ids.is_empty() {
        anyhow::bail!("workflow definition registry returned no definitions");
    }
    ids.sort();
    Ok(ids)
}

fn without_child_definitions(ids: Vec<String>) -> Vec<String> {
    ids.into_iter()
        .filter(|id| !ACTIVE_COUNT_EXCLUDED_CHILD_DEFINITION_IDS.contains(&id.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{GITHUB_ISSUE_PR_DEFINITION_ID, PROMPT_TASK_DEFINITION_ID};

    #[test]
    fn sorted_definition_ids_errors_on_empty_registry_snapshot() {
        let error = sorted_definition_ids(Vec::new()).expect_err("empty registry must error");
        assert!(error.to_string().contains("no definitions"));
    }

    #[test]
    fn sorted_definition_ids_sorts_for_deterministic_payload_order() {
        let ids = sorted_definition_ids(vec![
            "quality_gate".to_string(),
            "custom_release".to_string(),
            "github_issue_pr".to_string(),
        ])
        .expect("non-empty ids");
        assert_eq!(ids, ["custom_release", "github_issue_pr", "quality_gate"]);
    }

    #[test]
    fn without_child_definitions_drops_builtin_children_and_keeps_declarative_ids() {
        let ids = without_child_definitions(vec![
            "custom_release".to_string(),
            GITHUB_ISSUE_PR_DEFINITION_ID.to_string(),
            PR_FEEDBACK_DEFINITION_ID.to_string(),
            PROMPT_TASK_DEFINITION_ID.to_string(),
            QUALITY_GATE_DEFINITION_ID.to_string(),
        ]);
        assert_eq!(
            ids,
            [
                "custom_release",
                GITHUB_ISSUE_PR_DEFINITION_ID,
                PROMPT_TASK_DEFINITION_ID,
            ]
        );
    }

    #[test]
    fn operator_definition_ids_include_all_builtins_in_sorted_order() {
        let ids = operator_definition_ids().expect("registry seeds built-ins");
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "operator ids must be sorted");
        for builtin in [
            GITHUB_ISSUE_PR_DEFINITION_ID,
            PR_FEEDBACK_DEFINITION_ID,
            PROMPT_TASK_DEFINITION_ID,
            QUALITY_GATE_DEFINITION_ID,
        ] {
            assert!(ids.iter().any(|id| id == builtin), "missing {builtin}");
        }
    }

    #[test]
    fn active_count_definition_ids_exclude_builtin_child_workflows() {
        let ids = active_count_definition_ids().expect("registry seeds built-ins");
        assert!(ids.iter().any(|id| id == GITHUB_ISSUE_PR_DEFINITION_ID));
        assert!(ids.iter().any(|id| id == PROMPT_TASK_DEFINITION_ID));
        assert!(!ids.iter().any(|id| id == PR_FEEDBACK_DEFINITION_ID));
        assert!(!ids.iter().any(|id| id == QUALITY_GATE_DEFINITION_ID));
    }
}
