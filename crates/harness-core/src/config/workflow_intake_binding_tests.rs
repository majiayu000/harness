//! Loader-level tests for declarative intake bindings (GH-1656).
//!
//! These exercise `load_workflow_document` end-to-end (front-matter parse +
//! atomic definition merge + `validate_identifiers`), complementing the pure
//! validation unit tests in `intake_binding.rs`.

use super::load_workflow_document;

#[test]
fn parses_definition_with_intake_binding() -> anyhow::Result<()> {
    // B-001: an `intake` block rides the atomic `definition` merge and parses
    // into the typed policy.
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
definition:
  id: docs_review_flow
  initial: review
  states:
    review:
      activity: inspect_docs
      on_success: done
      on_failure: failed
    done: {}
    failed: {}
  terminal:
    done: succeeded
    failed: failed
  recovery_targets: [review]
  intake:
    source: github
    filter:
      labels: [docs-review]
      exclude_labels: [wip]
    max_active_instances: 8
---
"#,
    )?;

    let definition = load_workflow_document(dir.path())?
        .config
        .definition
        .ok_or_else(|| anyhow::anyhow!("definition should parse"))?;
    let intake = definition
        .intake
        .ok_or_else(|| anyhow::anyhow!("intake binding should parse"))?;
    assert_eq!(intake.source, "github");
    assert_eq!(intake.filter.labels, ["docs-review"]);
    assert_eq!(intake.filter.exclude_labels, ["wip"]);
    assert_eq!(intake.max_active_instances, 8);
    Ok(())
}

#[test]
fn definition_without_intake_leaves_binding_absent() -> anyhow::Result<()> {
    // B-001: existing WORKFLOW.md files without an `intake` block are unaffected.
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
definition:
  id: docs_review_flow
  initial: review
  states:
    review: { activity: inspect_docs, on_success: done, on_failure: failed }
    done: {}
    failed: {}
  terminal: { done: succeeded, failed: failed }
  recovery_targets: [review]
---
"#,
    )?;

    let definition = load_workflow_document(dir.path())?
        .config
        .definition
        .ok_or_else(|| anyhow::anyhow!("definition should parse"))?;
    assert!(definition.intake.is_none());
    Ok(())
}

#[test]
fn rejects_intake_binding_with_empty_labels() -> anyhow::Result<()> {
    // B-002: catch-all bindings are rejected fail-closed at parse time.
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
definition:
  id: docs_review_flow
  initial: review
  states:
    review: { activity: inspect_docs, on_success: done, on_failure: failed }
    done: {}
    failed: {}
  terminal: { done: succeeded, failed: failed }
  recovery_targets: [review]
  intake:
    source: github
    filter:
      labels: []
    max_active_instances: 8
---
"#,
    )?;

    assert!(
        load_workflow_document(dir.path()).is_err(),
        "an intake binding with empty labels must be rejected at parse time"
    );
    Ok(())
}

#[test]
fn rejects_intake_binding_with_zero_cap() -> anyhow::Result<()> {
    // B-002: a zero cap is rejected fail-closed at parse time.
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
definition:
  id: docs_review_flow
  initial: review
  states:
    review: { activity: inspect_docs, on_success: done, on_failure: failed }
    done: {}
    failed: {}
  terminal: { done: succeeded, failed: failed }
  recovery_targets: [review]
  intake:
    source: github
    filter:
      labels: [docs-review]
    max_active_instances: 0
---
"#,
    )?;

    assert!(
        load_workflow_document(dir.path()).is_err(),
        "an intake binding with a zero cap must be rejected at parse time"
    );
    Ok(())
}
