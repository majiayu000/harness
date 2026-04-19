use super::*;
use chrono::Utc;

async fn open_test_store() -> anyhow::Result<Option<ReviewStore>> {
    if std::env::var("DATABASE_URL").is_err() {
        return Ok(None);
    }
    let dir = tempfile::tempdir()?;
    let store = ReviewStore::open(&dir.path().join("review.db")).await?;
    Ok(Some(store))
}

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

async fn get_cooldown(
    store: &ReviewStore,
    project_root: &str,
    rule_id: &str,
    file: &str,
) -> (i64, Option<chrono::DateTime<Utc>>) {
    sqlx::query_as::<_, (i64, Option<chrono::DateTime<Utc>>)>(
        "SELECT failure_count, cooldown_until \
         FROM review_findings \
         WHERE project_root = $1 AND rule_id = $2 AND file = $3 AND status = 'open'",
    )
    .bind(project_root)
    .bind(rule_id)
    .bind(file)
    .fetch_one(store.pool())
    .await
    .unwrap()
}

async fn set_pending(store: &ReviewStore, project_root: &str, rule_id: &str, file: &str) {
    sqlx::query(
        "UPDATE review_findings SET task_id = 'pending', claimed_at = CURRENT_TIMESTAMP \
         WHERE project_root = $1 AND rule_id = $2 AND file = $3 AND status = 'open'",
    )
    .bind(project_root)
    .bind(rule_id)
    .bind(file)
    .execute(store.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_persist_no_duplicates() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let store = std::sync::Arc::new(store);

    let finding = make_finding("F001", "RS-03", "src/lib.rs", "P2");
    let findings = vec![finding];

    let store1 = store.clone();
    let store2 = store.clone();
    let f1 = findings.clone();
    let f2 = findings.clone();

    let (r1, r2) = tokio::join!(
        store1.persist_findings("", "rev-1", &f1),
        store2.persist_findings("", "rev-2", &f2),
    );

    r1?;
    r2?;

    let open = store.list_open().await?;
    assert_eq!(open.len(), 1);
    Ok(())
}

#[tokio::test]
async fn test_claim_confirm_roundtrip() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let findings = vec![make_finding("F001", "RS-03", "src/lib.rs", "P1")];
    store.persist_findings("", "rev-1", &findings).await?;

    assert!(
        store.try_claim_finding("", "RS-03", "src/lib.rs").await?,
        "first claim must succeed"
    );
    assert!(
        !store.try_claim_finding("", "RS-03", "src/lib.rs").await?,
        "duplicate claim must return false"
    );
    store
        .confirm_task_spawned("", "RS-03", "src/lib.rs", "task-abc")
        .await?;

    let spawnable = store
        .list_spawnable_findings("rev-1", &["P1", "P2"])
        .await?;
    assert!(
        spawnable.is_empty(),
        "confirmed finding must not be returned"
    );
    Ok(())
}

#[tokio::test]
async fn test_release_claim_allows_retry() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let findings = vec![make_finding("F001", "RS-03", "src/lib.rs", "P1")];
    store.persist_findings("", "rev-1", &findings).await?;

    assert!(store.try_claim_finding("", "RS-03", "src/lib.rs").await?);
    store.release_claim("", "RS-03", "src/lib.rs").await?;

    assert!(
        store.try_claim_finding("", "RS-03", "src/lib.rs").await?,
        "finding must be claimable after release"
    );
    Ok(())
}

#[tokio::test]
async fn test_list_spawnable_findings_filters_priority() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let findings = vec![
        make_finding("F0", "R0", "a.rs", "P0"),
        make_finding("F1", "R1", "b.rs", "P1"),
        make_finding("F2", "R2", "c.rs", "P2"),
        make_finding("F3", "R3", "d.rs", "P3"),
    ];
    store.persist_findings("", "rev-1", &findings).await?;

    let spawnable = store
        .list_spawnable_findings("rev-1", &["P1", "P2"])
        .await?;
    let ids: Vec<&str> = spawnable.iter().map(|f| f.id.as_str()).collect();
    assert!(ids.contains(&"F1"), "P1 must be returned");
    assert!(ids.contains(&"F2"), "P2 must be returned");
    assert!(!ids.contains(&"F0"), "P0 must be excluded");
    assert!(!ids.contains(&"F3"), "P3 must be excluded");
    Ok(())
}

#[tokio::test]
async fn test_list_spawnable_findings_skips_already_spawned() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let findings = vec![
        make_finding("F1", "R1", "a.rs", "P1"),
        make_finding("F2", "R2", "b.rs", "P2"),
    ];
    store.persist_findings("", "rev-1", &findings).await?;
    store.try_claim_finding("", "R1", "a.rs").await?;
    store
        .confirm_task_spawned("", "R1", "a.rs", "task-111")
        .await?;

    let spawnable = store
        .list_spawnable_findings("rev-1", &["P1", "P2"])
        .await?;
    assert_eq!(spawnable.len(), 1);
    assert_eq!(spawnable[0].id, "F2");
    Ok(())
}

#[tokio::test]
async fn test_list_spawnable_findings_skips_closed() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let findings = vec![make_finding("F1", "R1", "a.rs", "P1")];
    store.persist_findings("", "rev-1", &findings).await?;
    sqlx::query("UPDATE review_findings SET status = 'resolved' WHERE id = 'F1'")
        .execute(store.pool())
        .await?;

    let spawnable = store
        .list_spawnable_findings("rev-1", &["P1", "P2"])
        .await?;
    assert!(
        spawnable.is_empty(),
        "resolved findings must not be returned"
    );
    Ok(())
}

#[tokio::test]
async fn test_dedup_across_reviews_preserves_task_id() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let finding = make_finding("F1", "RS-03", "src/lib.rs", "P1");

    store
        .persist_findings("", "rev-1", std::slice::from_ref(&finding))
        .await?;
    store.try_claim_finding("", "RS-03", "src/lib.rs").await?;
    store
        .confirm_task_spawned("", "RS-03", "src/lib.rs", "task-orig")
        .await?;

    let finding2 = make_finding("F1", "RS-03", "src/lib.rs", "P1");
    store.persist_findings("", "rev-2", &[finding2]).await?;

    let spawnable = store
        .list_spawnable_findings("rev-2", &["P1", "P2"])
        .await?;
    assert!(
        spawnable.is_empty(),
        "recurring finding with task_id must not be re-spawned"
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_survives_review_id_change() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let finding = make_finding("F1", "RS-03", "src/lib.rs", "P1");

    store
        .persist_findings("", "rev-1", std::slice::from_ref(&finding))
        .await?;

    let finding2 = make_finding("F1", "RS-03", "src/lib.rs", "P1");
    store.persist_findings("", "rev-2", &[finding2]).await?;

    let claimed = store.try_claim_finding("", "RS-03", "src/lib.rs").await?;
    assert!(claimed, "must succeed even after review_id changed");
    store
        .confirm_task_spawned("", "RS-03", "src/lib.rs", "task-from-rev1-poller")
        .await?;

    let spawnable = store
        .list_spawnable_findings("rev-2", &["P1", "P2"])
        .await?;
    assert!(
        spawnable.is_empty(),
        "finding with task_id must not be re-spawned after review_id change"
    );
    Ok(())
}

#[tokio::test]
async fn store_persist_and_list() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };

    let findings = vec![ReviewFinding {
        id: "F001".into(),
        rule_id: "RS-03".into(),
        priority: "P2".into(),
        impact: 3,
        confidence: 5,
        effort: 2,
        file: "src/lib.rs".into(),
        line: 10,
        title: "unwrap in prod".into(),
        description: "panic risk".into(),
        action: "use ?".into(),
        task_id: None,
    }];

    let inserted = store.persist_findings("", "rev-1", &findings).await?;
    assert_eq!(inserted, 1);

    let inserted = store.persist_findings("", "rev-2", &findings).await?;
    assert_eq!(inserted, 0, "same rule_id+file = dedup (recurring)");

    let open = store.list_open().await?;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].rule_id, "RS-03");
    Ok(())
}

#[tokio::test]
async fn test_mark_finding_failed_increments_count() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    for expected in 1u32..=3 {
        set_pending(&store, "", "R1", "a.rs").await;
        store.mark_finding_failed("", "R1", "a.rs").await?;
        let (count, _) = get_cooldown(&store, "", "R1", "a.rs").await;
        assert_eq!(
            count as u32, expected,
            "failure_count after {} calls",
            expected
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_cooldown_until_is_future() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    set_pending(&store, "", "R1", "a.rs").await;
    store.mark_finding_failed("", "R1", "a.rs").await?;

    let (_, cooldown_until) = get_cooldown(&store, "", "R1", "a.rs").await;
    let dt = cooldown_until.expect("cooldown_until must be set after failure");
    let now = Utc::now();
    assert!(dt > now, "cooldown_until must be in the future, got {dt}");
    Ok(())
}

#[tokio::test]
async fn test_exponential_backoff_schedule() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    let expected_secs: &[i64] = &[3600, 7200, 14400, 28800, 57600, 86400];
    for &expected in expected_secs {
        set_pending(&store, "", "R1", "a.rs").await;
        let before = Utc::now();
        store.mark_finding_failed("", "R1", "a.rs").await?;
        let after = Utc::now();

        let (_, cooldown_until) = get_cooldown(&store, "", "R1", "a.rs").await;
        let dt = cooldown_until.unwrap();

        let lower = before + chrono::Duration::seconds(expected) - chrono::Duration::seconds(2);
        let upper = after + chrono::Duration::seconds(expected) + chrono::Duration::seconds(2);
        assert!(
            dt >= lower && dt <= upper,
            "cooldown_until {dt} not in expected window [{lower}, {upper}] for delay {expected}s"
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_list_spawnable_excludes_cooldown() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    sqlx::query(
        "UPDATE review_findings \
         SET cooldown_until = CURRENT_TIMESTAMP + INTERVAL '3600 seconds' \
         WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
    )
    .execute(store.pool())
    .await?;

    let spawnable = store
        .list_spawnable_findings("rev-1", &["P1", "P2"])
        .await?;
    assert!(
        spawnable.is_empty(),
        "finding with active cooldown must not appear in spawnable list"
    );
    Ok(())
}

#[tokio::test]
async fn test_list_spawnable_includes_expired_cooldown() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    sqlx::query(
        "UPDATE review_findings \
         SET cooldown_until = CURRENT_TIMESTAMP - INTERVAL '1 second' \
         WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
    )
    .execute(store.pool())
    .await?;

    let spawnable = store
        .list_spawnable_findings("rev-1", &["P1", "P2"])
        .await?;
    assert_eq!(
        spawnable.len(),
        1,
        "finding with expired cooldown must appear in spawnable list"
    );
    Ok(())
}

#[tokio::test]
async fn test_reset_clears_cooldown() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    set_pending(&store, "", "R1", "a.rs").await;
    store.mark_finding_failed("", "R1", "a.rs").await?;
    let (count_before, cooldown_before) = get_cooldown(&store, "", "R1", "a.rs").await;
    assert_eq!(count_before, 1);
    assert!(cooldown_before.is_some());

    store.reset_finding_cooldown("", "R1", "a.rs").await?;

    let (count_after, cooldown_after) = get_cooldown(&store, "", "R1", "a.rs").await;
    assert_eq!(count_after, 0, "failure_count must be 0 after reset");
    assert!(
        cooldown_after.is_none(),
        "cooldown_until must be NULL after reset"
    );
    Ok(())
}

#[tokio::test]
async fn test_strategy1_recovery_triggers_backoff() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    sqlx::query(
        "UPDATE review_findings \
         SET task_id = 'pending', claimed_at = CURRENT_TIMESTAMP, real_task_id = 'task-xyz' \
         WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
    )
    .execute(store.pool())
    .await?;

    let recovered = store
        .recover_stale_pending_claims("", 3900, |_tid| Some(true))
        .await?;
    assert_eq!(recovered, 1, "one row must be recovered");

    let (count, cooldown) = get_cooldown(&store, "", "R1", "a.rs").await;
    assert_eq!(
        count, 1,
        "failure_count must be 1 after strategy-1 recovery"
    );
    assert!(
        cooldown.is_some(),
        "cooldown_until must be set after strategy-1 recovery"
    );
    Ok(())
}

#[tokio::test]
async fn test_strategy2_timeout_no_backoff() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    sqlx::query(
        "UPDATE review_findings \
         SET task_id = 'pending', \
             claimed_at = CURRENT_TIMESTAMP - INTERVAL '4000 seconds', \
             real_task_id = NULL \
         WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
    )
    .execute(store.pool())
    .await?;

    let recovered = store
        .recover_stale_pending_claims("", 3900, |_tid| Some(false))
        .await?;
    assert_eq!(recovered, 1, "one row must be recovered via timeout");

    let (count, cooldown) = get_cooldown(&store, "", "R1", "a.rs").await;
    assert_eq!(
        count, 0,
        "failure_count must remain 0 for strategy-2 recovery"
    );
    assert!(
        cooldown.is_none(),
        "cooldown_until must stay NULL for strategy-2 recovery"
    );
    Ok(())
}

#[tokio::test]
async fn test_resolution_clears_cooldown_state() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    store
        .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
        .await?;

    set_pending(&store, "", "R1", "a.rs").await;
    store.mark_finding_failed("", "R1", "a.rs").await?;
    set_pending(&store, "", "R1", "a.rs").await;
    store.mark_finding_failed("", "R1", "a.rs").await?;

    let (count_before, cooldown_before) = get_cooldown(&store, "", "R1", "a.rs").await;
    assert_eq!(count_before, 2);
    assert!(cooldown_before.is_some());

    let cleared = store.reset_cooldowns_for_resolved("", "rev-2").await?;
    assert_eq!(cleared, 1, "one finding must have its cooldown cleared");

    let (count_after, cooldown_after) = get_cooldown(&store, "", "R1", "a.rs").await;
    assert_eq!(count_after, 0, "failure_count must be 0 after resolution");
    assert!(
        cooldown_after.is_none(),
        "cooldown_until must be NULL after resolution"
    );
    Ok(())
}
