//! Integration tests for ReviewStore orphan-cleanup methods:
//! `list_assigned_task_ids` and `reset_task_ids_for_terminal`.

use harness_server::review_store::{ReviewFinding, ReviewStore};

fn make_finding(id: &str, rule_id: &str, file: &str, priority: &str) -> ReviewFinding {
    ReviewFinding {
        id: id.into(),
        rule_id: rule_id.into(),
        priority: priority.into(),
        impact: 3,
        confidence: 5,
        effort: 2,
        file: file.into(),
        line: 10,
        title: format!("finding {id}"),
        description: "desc".into(),
        action: "fix it".into(),
        task_id: None,
    }
}

#[tokio::test]
async fn list_assigned_task_ids_excludes_null_and_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = ReviewStore::open(&dir.path().join("review.db")).await?;
    let findings = vec![
        make_finding("F1", "R1", "a.rs", "P1"),
        make_finding("F2", "R2", "b.rs", "P1"),
        make_finding("F3", "R3", "c.rs", "P1"),
    ];
    store.persist_findings("rev-1", "proj", &findings).await?;

    // F1: leave task_id NULL
    // F2: claim only → task_id = 'pending'
    store.try_claim_finding("proj", "R2", "b.rs").await?;
    // F3: claim + confirm → real task_id
    store.try_claim_finding("proj", "R3", "c.rs").await?;
    store
        .confirm_task_spawned("proj", "R3", "c.rs", "task-real")
        .await?;

    let assigned = store.list_assigned_task_ids("proj").await?;
    assert_eq!(assigned.len(), 1);
    assert_eq!(assigned[0].0, "R3");
    assert_eq!(assigned[0].1, "c.rs");
    assert_eq!(assigned[0].2, "task-real");
    Ok(())
}

#[tokio::test]
async fn reset_task_ids_for_terminal_frees_finding() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = ReviewStore::open(&dir.path().join("review.db")).await?;
    let findings = vec![
        make_finding("F1", "R1", "a.rs", "P1"),
        make_finding("F2", "R2", "b.rs", "P1"),
    ];
    store.persist_findings("rev-1", "proj", &findings).await?;

    store.try_claim_finding("proj", "R1", "a.rs").await?;
    store
        .confirm_task_spawned("proj", "R1", "a.rs", "task-111")
        .await?;
    store.try_claim_finding("proj", "R2", "b.rs").await?;
    store
        .confirm_task_spawned("proj", "R2", "b.rs", "task-222")
        .await?;

    // Only task-111 reached terminal state.
    let reset = store.reset_task_ids_for_terminal(&["task-111"]).await?;
    assert_eq!(reset, 1);

    // F1 freed for re-spawn; F2 still blocked.
    let spawnable = store.list_spawnable_findings("rev-1", &["P1"]).await?;
    assert_eq!(spawnable.len(), 1);
    assert_eq!(spawnable[0].rule_id, "R1");
    Ok(())
}

#[tokio::test]
async fn reset_task_ids_for_terminal_empty_input_is_noop() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = ReviewStore::open(&dir.path().join("review.db")).await?;
    let reset = store.reset_task_ids_for_terminal(&[]).await?;
    assert_eq!(reset, 0);
    Ok(())
}
