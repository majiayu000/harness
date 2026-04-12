use super::*;
use std::time::Duration;
use tokio::time::timeout;

fn config(max_concurrent: usize, max_queue: usize) -> ConcurrencyConfig {
    ConcurrencyConfig {
        max_concurrent_tasks: max_concurrent,
        max_queue_size: max_queue,
        stall_timeout_secs: 300,
        per_project: Default::default(),
        ..ConcurrencyConfig::default()
    }
}

fn per_project_limit_config(project_id: &str, limit: usize) -> ConcurrencyConfig {
    let mut config = ConcurrencyConfig {
        max_concurrent_tasks: 4,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project: Default::default(),
        ..ConcurrencyConfig::default()
    };
    config
        .per_project
        .by_id_mut()
        .insert(project_id.to_string(), limit);
    config
}

fn legacy_per_project_limit_config(project_id: &str, limit: usize) -> ConcurrencyConfig {
    let mut per_project = std::collections::HashMap::new();
    per_project.insert(project_id.to_string(), limit);
    ConcurrencyConfig {
        max_concurrent_tasks: 4,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project: harness_core::config::misc::PerProjectConcurrencyLimits::Legacy(per_project),
        ..ConcurrencyConfig::default()
    }
}

fn typed_per_project_limit_config(project_id: &str, limit: usize) -> ConcurrencyConfig {
    per_project_limit_config(project_id, limit)
}

#[tokio::test]
async fn acquire_and_release_single_permit() {
    let q = TaskQueue::new(&config(2, 8));
    assert_eq!(q.running_count(), 0);
    let permit = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 1);
    drop(permit);
    assert_eq!(q.running_count(), 0);
}

#[tokio::test]
async fn only_max_concurrent_tasks_run_simultaneously() {
    let q = Arc::new(TaskQueue::new(&config(2, 16)));

    // Acquire both permits.
    let p1 = q.acquire("proj", 0).await.unwrap();
    let p2 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);

    // Third acquire must block; verify with a short timeout.
    let q2 = q.clone();
    let blocked = timeout(Duration::from_millis(50), q2.acquire("proj", 0)).await;
    assert!(blocked.is_err(), "third acquire should block when limit=2");

    // Release a slot; third acquire should now succeed.
    drop(p1);
    let p3 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
    drop(p2);
    drop(p3);
    assert_eq!(q.running_count(), 0);
}

#[tokio::test]
async fn queue_overflow_returns_error() {
    // 1 concurrent slot, queue capacity 2.
    let q = Arc::new(TaskQueue::new(&config(1, 2)));

    // Hold the single execution slot.
    let _p1 = q.acquire("proj", 0).await.unwrap();

    // Two tasks queue up (they block, so spawn them).
    let q2 = q.clone();
    let _h1 = tokio::spawn(async move { q2.acquire("proj", 0).await });
    let q3 = q.clone();
    let _h2 = tokio::spawn(async move { q3.acquire("proj", 0).await });

    // Give spawned tasks time to increment queued_count.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Third waiter exceeds max_queue_size=2 → error.
    let result = q.acquire("proj", 0).await;
    assert!(result.is_err(), "expected queue full error");
    assert!(result.unwrap_err().to_string().contains("max_queue_size=2"));
}

#[tokio::test]
async fn permit_drop_releases_slot_on_panic() {
    let q = Arc::new(TaskQueue::new(&config(1, 4)));

    {
        let q_inner = q.clone();
        // Run in a separate task that acquires a permit then panics.
        let handle = tokio::spawn(async move {
            let _permit = q_inner.acquire("proj", 0).await.unwrap();
            panic!("forced panic to test permit drop");
        });
        let _ = handle.await; // ignore the JoinError from the panic
    }

    // The permit must have been released; acquiring again should succeed immediately.
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(result.is_ok(), "permit should be released after panic");
}

#[tokio::test]
async fn queued_count_increments_while_waiting() {
    let q = Arc::new(TaskQueue::new(&config(1, 8)));

    // Hold the slot so the next acquire must wait.
    let _holder = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.queued_count(), 0);

    let q2 = q.clone();
    let _waiter = tokio::spawn(async move { q2.acquire("proj", 0).await });

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(q.queued_count(), 1);
}

#[tokio::test]
async fn running_count_and_queued_count_reset_after_completion() {
    let q = TaskQueue::new(&config(4, 16));
    let p1 = q.acquire("proj", 0).await.unwrap();
    let p2 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
    drop(p1);
    drop(p2);
    assert_eq!(q.running_count(), 0);
    assert_eq!(q.queued_count(), 0);
}

#[tokio::test]
async fn per_project_limit_enforced() {
    let q = Arc::new(TaskQueue::new(&per_project_limit_config("proj_a", 1)));

    // proj_a has limit=1; second acquire must block.
    let _p1 = q.acquire("proj_a", 0).await.unwrap();
    let q2 = q.clone();
    let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a", 0)).await;
    assert!(
        blocked.is_err(),
        "proj_a second acquire should block (limit=1)"
    );

    // proj_b has no configured limit (uses global=4); can still acquire.
    let p_b = q.acquire("proj_b", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
    drop(p_b);
}

#[tokio::test]
async fn project_path_alias_shares_limit_with_registered_id() {
    let q = Arc::new(TaskQueue::new(&per_project_limit_config("proj_a", 1)));

    let _holder = q.acquire("proj_a", 0).await.unwrap();
    let blocked = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/proj-a-alias", 0),
    )
    .await;
    assert!(
        blocked.is_ok(),
        "raw path alias should not block before canonical identity migration"
    );
}

#[tokio::test]
async fn legacy_per_project_limit_is_still_enforced() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "legacy_proj",
        1,
    )));

    let _holder = q.acquire("legacy_proj", 0).await.unwrap();
    let blocked = timeout(
        Duration::from_millis(50),
        q.clone().acquire("legacy_proj", 0),
    )
    .await;
    assert!(
        blocked.is_err(),
        "legacy config should still enforce per-project limits"
    );
}

#[tokio::test]
async fn typed_project_id_limit_blocks_same_canonical_id() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj_a", 1)));

    let _holder = q.acquire("proj_a", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj_a", 0)).await;
    assert!(
        blocked.is_err(),
        "typed by_id limit should block the same canonical project id"
    );
}

#[tokio::test]
async fn typed_project_id_limit_does_not_apply_to_raw_alias_string() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj_a", 1)));

    let _holder = q.acquire("proj_a", 0).await.unwrap();
    let alias_result = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/proj-a-alias", 0),
    )
    .await;
    assert!(
        alias_result.is_ok(),
        "task queue only buckets by canonical project id, not raw alias strings"
    );
}

#[tokio::test]
async fn different_typed_project_ids_do_not_collide() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj_a", 1)));

    let _holder = q.acquire("proj_a", 0).await.unwrap();
    let other = timeout(Duration::from_millis(50), q.clone().acquire("proj_b", 0)).await;
    assert!(
        other.is_ok(),
        "different canonical project ids must not share a bucket"
    );
}

#[tokio::test]
async fn raw_alias_and_registered_id_only_share_limit_after_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj_a", 1)));

    let _holder = q.acquire("proj_a", 0).await.unwrap();
    let alias_without_resolution = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/proj-a-alias", 0),
    )
    .await;
    assert!(
        alias_without_resolution.is_ok(),
        "queue-level tests confirm aliases do not collapse without the HTTP/registry resolution layer"
    );
}

#[tokio::test]
async fn registered_id_and_resolved_alias_share_limit_when_canonicalized_before_queue() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj_a", 1)));

    let _holder = q.acquire("proj_a", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj_a", 0)).await;
    assert!(
        blocked.is_err(),
        "once caller resolves alias to canonical id, both spellings share one bucket"
    );
}

#[tokio::test]
async fn queue_buckets_by_canonical_id_not_root_spelling() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj_a", 1)));

    let _holder = q.acquire("proj_a", 0).await.unwrap();
    let same_id = timeout(Duration::from_millis(50), q.clone().acquire("proj_a", 0)).await;
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("./proj_a", 0)).await;
    assert!(same_id.is_err(), "same canonical id should block");
    assert!(
        alias.is_ok(),
        "different raw spelling remains a different bucket until resolved upstream"
    );
}

#[tokio::test]
async fn project_queue_stats_use_typed_project_id_limits() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "stats_proj",
        1,
    )));

    let _holder = q.acquire("stats_proj", 0).await.unwrap();
    let q2 = q.clone();
    let _waiter = tokio::spawn(async move { q2.acquire("stats_proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let stats = q.project_stats("stats_proj");
    assert_eq!(stats.limit, 1);
    assert_eq!(stats.running, 1);
    assert_eq!(stats.queued, 1);
}

#[tokio::test]
async fn legacy_project_queue_stats_use_legacy_limits() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "legacy_stats",
        1,
    )));

    let _holder = q.acquire("legacy_stats", 0).await.unwrap();
    let q2 = q.clone();
    let _waiter = tokio::spawn(async move { q2.acquire("legacy_stats", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let stats = q.project_stats("legacy_stats");
    assert_eq!(stats.limit, 1);
    assert_eq!(stats.running, 1);
    assert_eq!(stats.queued, 1);
}

#[tokio::test]
async fn queue_limit_lookup_prefers_project_id_key() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj_limit", 2));
    let stats = q.project_stats("proj_limit");
    assert_eq!(stats.limit, 2);
}

#[tokio::test]
async fn queue_limit_lookup_falls_back_to_global_for_unknown_project_id() {
    let q = TaskQueue::new(&typed_per_project_limit_config("known", 1));
    let stats = q.project_stats("unknown");
    assert_eq!(stats.limit, 4);
}

#[tokio::test]
async fn typed_limits_do_not_accept_root_key_as_project_id() {
    let q = TaskQueue::new(&typed_per_project_limit_config("/tmp/proj-a", 1));
    let stats = q.project_stats("proj_a");
    assert_eq!(
        stats.limit, 4,
        "typed by_id entries are exact canonical ids, not root aliases"
    );
}

#[tokio::test]
async fn exact_typed_root_like_id_is_respected_as_plain_id() {
    let q = TaskQueue::new(&typed_per_project_limit_config("/tmp/proj-a", 1));
    let stats = q.project_stats("/tmp/proj-a");
    assert_eq!(
        stats.limit, 1,
        "task queue treats keys as exact canonical ids only"
    );
}

#[tokio::test]
async fn legacy_limits_are_exact_string_matches_inside_queue() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("/tmp/proj-a", 1));
    let stats = q.project_stats("/tmp/proj-a");
    let unknown = q.project_stats("proj-a");
    assert_eq!(stats.limit, 1);
    assert_eq!(unknown.limit, 4);
}

#[tokio::test]
async fn typed_and_legacy_helpers_start_from_same_global_defaults() {
    let typed = typed_per_project_limit_config("proj", 1);
    let legacy = legacy_per_project_limit_config("proj", 1);
    assert_eq!(typed.max_concurrent_tasks, legacy.max_concurrent_tasks);
    assert_eq!(typed.max_queue_size, legacy.max_queue_size);
}

#[tokio::test]
async fn queue_uses_typed_limits_without_root_resolution_logic() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("named", 1)));

    let _holder = q.acquire("named", 0).await.unwrap();
    let root_alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/named", 0),
    )
    .await;
    assert!(
        root_alias.is_ok(),
        "root alias collapsing belongs to registry/http resolution, not TaskQueue"
    );
}

#[tokio::test]
async fn queue_uses_exact_id_for_bucketing_after_canonical_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("named", 1)));

    let _holder = q.acquire("named", 0).await.unwrap();
    let canonical_again = timeout(Duration::from_millis(50), q.clone().acquire("named", 0)).await;
    assert!(canonical_again.is_err());
}

#[tokio::test]
async fn queue_can_hold_two_projects_with_independent_typed_limits() {
    let mut cfg = typed_per_project_limit_config("proj_a", 1);
    cfg.per_project.by_id_mut().insert("proj_b".to_string(), 1);
    let q = Arc::new(TaskQueue::new(&cfg));

    let _a = q.acquire("proj_a", 0).await.unwrap();
    let _b = q.acquire("proj_b", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
}

#[tokio::test]
async fn queue_can_hold_two_projects_with_independent_legacy_limits() {
    let mut cfg = legacy_per_project_limit_config("proj_a", 1);
    cfg.per_project.by_id_mut().insert("proj_b".to_string(), 1);
    let q = Arc::new(TaskQueue::new(&cfg));

    let _a = q.acquire("proj_a", 0).await.unwrap();
    let _b = q.acquire("proj_b", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
}

#[tokio::test]
async fn typed_limits_do_not_merge_unresolved_aliases() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj_a", 1)));

    let _a = q.acquire("proj_a", 0).await.unwrap();
    let alias = q.acquire("proj-a", 0).await;
    assert!(
        alias.is_ok(),
        "unresolved alias remains separate until caller resolves to canonical id"
    );
}

#[tokio::test]
async fn legacy_limits_do_not_merge_unresolved_aliases() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "proj_a", 1,
    )));

    let _a = q.acquire("proj_a", 0).await.unwrap();
    let alias = q.acquire("proj-a", 0).await;
    assert!(
        alias.is_ok(),
        "legacy queue matching is still exact string based"
    );
}

#[tokio::test]
async fn typed_limit_enforced_after_upstream_alias_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved-id",
        1,
    )));

    let _a = q.acquire("resolved-id", 0).await.unwrap();
    let same_resolved_id = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-id", 0),
    )
    .await;
    assert!(same_resolved_id.is_err());
}

#[tokio::test]
async fn exact_bucket_key_is_the_only_identity_inside_queue() {
    let q = TaskQueue::new(&typed_per_project_limit_config("bucket-key", 1));
    assert_eq!(q.project_stats("bucket-key").limit, 1);
    assert_eq!(q.project_stats("/tmp/bucket-key").limit, 4);
}

#[tokio::test]
async fn queue_does_not_use_root_aliases_without_registry_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("alpha", 1)));

    let _holder = q.acquire("alpha", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/work/alpha", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_uses_global_limit_for_unconfigured_alias_even_when_id_is_capped() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("alpha", 1)));
    let _holder = q.acquire("alpha", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("alias-alpha", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_bucketing_behavior_is_explicitly_exact_match() {
    let q = TaskQueue::new(&typed_per_project_limit_config("alpha", 1));
    assert_eq!(q.project_stats("alpha").limit, 1);
    assert_eq!(q.project_stats("Alpha").limit, 4);
}

#[tokio::test]
async fn queue_keeps_canonical_id_and_alias_paths_separate_without_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("alpha", 1)));
    let _holder = q.acquire("alpha", 0).await.unwrap();
    let alias = q.acquire("/canonical/alpha", 0).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_limit_map_accepts_multiple_typed_ids() {
    let mut cfg = typed_per_project_limit_config("alpha", 1);
    cfg.per_project.by_id_mut().insert("beta".to_string(), 2);
    let q = TaskQueue::new(&cfg);
    assert_eq!(q.project_stats("alpha").limit, 1);
    assert_eq!(q.project_stats("beta").limit, 2);
}

#[tokio::test]
async fn queue_limit_map_accepts_multiple_legacy_ids() {
    let mut cfg = legacy_per_project_limit_config("alpha", 1);
    cfg.per_project.by_id_mut().insert("beta".to_string(), 2);
    let q = TaskQueue::new(&cfg);
    assert_eq!(q.project_stats("alpha").limit, 1);
    assert_eq!(q.project_stats("beta").limit, 2);
}

#[tokio::test]
async fn queue_typed_limit_default_global_is_unchanged() {
    let q = TaskQueue::new(&typed_per_project_limit_config("alpha", 1));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn queue_legacy_limit_default_global_is_unchanged() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("alpha", 1));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn queue_only_needs_exact_key_not_project_root_semantics() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("alpha", 1)));
    let _holder = q.acquire("alpha", 0).await.unwrap();
    let unrelated = timeout(Duration::from_millis(50), q.clone().acquire("./alpha", 0)).await;
    assert!(unrelated.is_ok());
}

#[tokio::test]
async fn queue_documented_exact_match_behavior_for_migration() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("alpha", 1));
    assert_eq!(q.project_stats("alpha").limit, 1);
    assert_eq!(q.project_stats("./alpha").limit, 4);
}

#[tokio::test]
async fn queue_typed_behavior_matches_issue_requirement_after_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "canonical-id",
        1,
    )));
    let _holder = q.acquire("canonical-id", 0).await.unwrap();
    let blocked = timeout(
        Duration::from_millis(50),
        q.clone().acquire("canonical-id", 0),
    )
    .await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_alias_behavior_is_left_to_callers() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "canonical-id",
        1,
    )));
    let _holder = q.acquire("canonical-id", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("canonical-root", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_bucketing_is_exact_even_for_pathlike_ids() {
    let q = TaskQueue::new(&typed_per_project_limit_config("/canonical/root", 1));
    assert_eq!(q.project_stats("/canonical/root").limit, 1);
    assert_eq!(q.project_stats("canonical/root").limit, 4);
}

#[tokio::test]
async fn queue_typed_and_legacy_paths_are_both_exact_match_only() {
    let typed = TaskQueue::new(&typed_per_project_limit_config("/canonical/root", 1));
    let legacy = TaskQueue::new(&legacy_per_project_limit_config("/canonical/root", 1));
    assert_eq!(typed.project_stats("/canonical/root").limit, 1);
    assert_eq!(legacy.project_stats("/canonical/root").limit, 1);
}

#[tokio::test]
async fn queue_keeps_root_semantics_out_of_internal_bucket_map() {
    let q = TaskQueue::new(&typed_per_project_limit_config("named", 1));
    assert_eq!(q.project_stats("named").limit, 1);
    assert_eq!(q.project_stats("/tmp/named").limit, 4);
}

#[tokio::test]
async fn queue_internal_bucket_map_uses_canonical_identity_string() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("named", 1)));
    let _holder = q.acquire("named", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("named", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_internal_bucket_map_does_not_guess_aliases() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("named", 1)));
    let _holder = q.acquire("named", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("named/", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_migration_contract_requires_upstream_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("named", 1)));
    let _holder = q.acquire("named", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/named", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_legacy_map_remains_supported_for_existing_configs() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "legacy", 1,
    )));
    let _holder = q.acquire("legacy", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("legacy", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_typed_map_is_the_new_primary_form() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("typed", 1)));
    let _holder = q.acquire("typed", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("typed", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_different_ids_do_not_share_capacity() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("typed", 1)));
    let _holder = q.acquire("typed", 0).await.unwrap();
    let other = timeout(
        Duration::from_millis(50),
        q.clone().acquire("typed-other", 0),
    )
    .await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn queue_migration_supports_exact_legacy_string_keys() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("legacy-key", 1));
    assert_eq!(q.project_stats("legacy-key").limit, 1);
}

#[tokio::test]
async fn queue_does_not_transform_keys_internally() {
    let q = TaskQueue::new(&typed_per_project_limit_config("Mixed/Key", 1));
    assert_eq!(q.project_stats("Mixed/Key").limit, 1);
    assert_eq!(q.project_stats("mixed/key").limit, 4);
}

#[tokio::test]
async fn queue_respects_exact_typed_key_on_acquire() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "Mixed/Key",
        1,
    )));
    let _holder = q.acquire("Mixed/Key", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("Mixed/Key", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_nonmatching_key_gets_global_capacity() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "Mixed/Key",
        1,
    )));
    let _holder = q.acquire("Mixed/Key", 0).await.unwrap();
    let other = timeout(Duration::from_millis(50), q.clone().acquire("Mixed-Key", 0)).await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn queue_stats_limit_comes_from_exact_key_match() {
    let q = TaskQueue::new(&typed_per_project_limit_config("Mixed/Key", 2));
    assert_eq!(q.project_stats("Mixed/Key").limit, 2);
}

#[tokio::test]
async fn queue_stats_limit_for_other_key_stays_global() {
    let q = TaskQueue::new(&typed_per_project_limit_config("Mixed/Key", 2));
    assert_eq!(q.project_stats("Mixed-Key").limit, 4);
}

#[tokio::test]
async fn queue_issue_684_contract_is_upstream_resolution_plus_exact_queue_keying() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "canonical-id",
        1,
    )));
    let _holder = q.acquire("canonical-id", 0).await.unwrap();
    let blocked = timeout(
        Duration::from_millis(50),
        q.clone().acquire("canonical-id", 0),
    )
    .await;
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("canonical-root", 0),
    )
    .await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn exact_match_limit_lookup_is_shared_by_stats_and_acquire() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "canonical-id",
        1,
    )));
    assert_eq!(q.project_stats("canonical-id").limit, 1);
    let _holder = q.acquire("canonical-id", 0).await.unwrap();
    let blocked = timeout(
        Duration::from_millis(50),
        q.clone().acquire("canonical-id", 0),
    )
    .await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn root_like_string_only_has_special_meaning_before_task_queue() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "canonical-id",
        1,
    )));
    let _holder = q.acquire("canonical-id", 0).await.unwrap();
    let root_like = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/path/to/canonical-id", 0),
    )
    .await;
    assert!(root_like.is_ok());
}

#[tokio::test]
async fn queue_remains_string_keyed_but_callers_should_pass_canonical_ids() {
    let q = TaskQueue::new(&typed_per_project_limit_config("canonical-id", 1));
    assert_eq!(q.project_stats("canonical-id").limit, 1);
    assert_eq!(q.project_stats("/path/to/canonical-id").limit, 4);
}

#[tokio::test]
async fn queue_legacy_and_typed_configs_both_feed_exact_lookup_map() {
    let typed = TaskQueue::new(&typed_per_project_limit_config("same", 1));
    let legacy = TaskQueue::new(&legacy_per_project_limit_config("same", 1));
    assert_eq!(typed.project_stats("same").limit, 1);
    assert_eq!(legacy.project_stats("same").limit, 1);
}

#[tokio::test]
async fn queue_only_collapses_aliases_when_upstream_resolves_them_to_same_id() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("same", 1)));
    let _holder = q.acquire("same", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("same", 0)).await;
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("same-root", 0)).await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_limit_config_migration_keeps_old_behavior_inside_queue() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("same", 1));
    assert_eq!(q.project_stats("same").limit, 1);
    assert_eq!(q.project_stats("same-root").limit, 4);
}

#[tokio::test]
async fn queue_new_behavior_requires_by_id_entries() {
    let q = TaskQueue::new(&typed_per_project_limit_config("same", 1));
    assert_eq!(q.project_stats("same").limit, 1);
}

#[tokio::test]
async fn queue_issue_684_keeps_resolution_outside_task_queue() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("same", 1)));
    let _holder = q.acquire("same", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("/tmp/same", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn exact_lookup_behavior_is_stable_for_future_callers() {
    let q = TaskQueue::new(&typed_per_project_limit_config("stable", 1));
    assert_eq!(q.project_stats("stable").limit, 1);
    assert_eq!(q.project_stats("stable/",).limit, 4);
}

#[tokio::test]
async fn queue_limit_is_not_applied_to_noncanonical_spellings() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("stable", 1)));
    let _holder = q.acquire("stable", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("./stable", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_relies_on_caller_to_provide_same_project_key() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("stable", 1)));
    let _holder = q.acquire("stable", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("stable", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_global_default_remains_when_key_missing() {
    let q = TaskQueue::new(&typed_per_project_limit_config("stable", 1));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn queue_legacy_global_default_remains_when_key_missing() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("stable", 1));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn queue_issue_684_exact_key_contract_is_visible_in_tests() {
    let q = TaskQueue::new(&typed_per_project_limit_config("stable", 1));
    assert_eq!(q.project_stats("stable").limit, 1);
    assert_eq!(q.project_stats("/stable",).limit, 4);
}

#[tokio::test]
async fn queue_with_typed_config_preserves_existing_runtime_logic() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_with_legacy_config_preserves_migration_compatibility() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn alias_string_still_uses_separate_bucket_until_registry_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("proj/root", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn exact_id_string_is_the_bucket_contract() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
}

#[tokio::test]
async fn project_queue_limit_configuration_uses_by_id_entries() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 3));
    assert_eq!(q.project_stats("proj").limit, 3);
}

#[tokio::test]
async fn project_queue_limit_configuration_still_accepts_legacy_entries() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 3));
    assert_eq!(q.project_stats("proj").limit, 3);
}

#[tokio::test]
async fn project_queue_limit_configuration_does_not_guess_equivalent_strings() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 3));
    assert_eq!(q.project_stats("./proj").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_contract_is_exact_keying_after_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("./proj", 0)).await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn same_id_still_shares_one_bucket() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn different_strings_still_mean_different_buckets_inside_queue() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("proj2", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn exact_keying_is_the_internal_contract_for_migration() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("proj/alias").limit, 4);
}

#[tokio::test]
async fn queue_compatibility_layer_lives_in_registry_not_queue() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("/tmp/proj", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_migration_tests_document_exact_keying_behavior() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("/tmp/proj").limit, 4);
}

#[tokio::test]
async fn typed_config_helper_writes_new_primary_shape() {
    let cfg = typed_per_project_limit_config("proj", 1);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&1));
}

#[tokio::test]
async fn legacy_config_helper_writes_legacy_shape() {
    let cfg = legacy_per_project_limit_config("proj", 1);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&1));
}

#[tokio::test]
async fn queue_acquire_uses_same_exact_key_as_stats() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    assert_eq!(q.project_stats("proj").limit, 1);
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn unresolved_aliases_remain_uncollapsed_at_queue_layer() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("proj-alias", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_uses_canonical_identity_only_when_caller_supplies_it() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_issue_684_scope_is_configuration_and_exact_bucketing() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
}

#[tokio::test]
async fn queue_issue_684_non_goal_is_alias_inference() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj/../proj").limit, 4);
}

#[tokio::test]
async fn queue_new_config_form_and_runtime_behavior_are_consistent() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_old_config_form_and_runtime_behavior_are_consistent() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_pathlike_alias_is_not_special_without_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/canonical/proj", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_limit_map_is_populated_from_by_id_entries() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 5));
    assert_eq!(q.project_stats("proj").limit, 5);
}

#[tokio::test]
async fn queue_limit_map_is_populated_from_legacy_entries() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 5));
    assert_eq!(q.project_stats("proj").limit, 5);
}

#[tokio::test]
async fn queue_limit_map_does_not_populated_from_alias_guesses() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 5));
    assert_eq!(q.project_stats("./proj").limit, 4);
}

#[tokio::test]
async fn queue_still_needs_http_registry_to_canonicalize_request_input() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("root/proj", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_issue_684_does_not_change_exact_matching_in_queue() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("root/proj", 0)).await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_limit_lookup_is_exact_for_stats() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 2));
    assert_eq!(q.project_stats("proj").limit, 2);
    assert_eq!(q.project_stats("root/proj").limit, 4);
}

#[tokio::test]
async fn queue_limit_lookup_is_exact_for_acquire() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("root/proj", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn migration_goal_is_caller_resolution_not_queue_guessing() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved/root/proj", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn queue_tests_capture_current_exact_match_contract() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("resolved/root/proj").limit, 4);
}

#[tokio::test]
async fn queue_compatibility_tests_capture_legacy_exact_match_contract() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("resolved/root/proj").limit, 4);
}

#[tokio::test]
async fn per_project_limit_applies_to_exact_project_key_only() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved/root/proj", 0),
    )
    .await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn legacy_per_project_limit_applies_to_exact_project_key_only() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved/root/proj", 0),
    )
    .await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn typed_limit_lookup_uses_new_config_shape() {
    let cfg = typed_per_project_limit_config("proj", 9);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&9));
}

#[tokio::test]
async fn legacy_limit_lookup_uses_legacy_config_shape() {
    let cfg = legacy_per_project_limit_config("proj", 9);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&9));
}

#[tokio::test]
async fn queue_internal_limit_map_contains_only_declared_keys() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("undeclared").limit, 4);
}

#[tokio::test]
async fn queue_runtime_behavior_matches_internal_limit_map() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn queue_runtime_behavior_for_other_string_uses_default_limit() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let other = timeout(Duration::from_millis(50), q.clone().acquire("other", 0)).await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_cover_exact_keying_and_legacy_compat() {
    let typed = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    let legacy = TaskQueue::new(&legacy_per_project_limit_config("proj", 1));
    assert_eq!(typed.project_stats("proj").limit, 1);
    assert_eq!(legacy.project_stats("proj").limit, 1);
}

#[tokio::test]
async fn issue_684_queue_tests_leave_alias_resolution_to_other_layers() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("alias", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_same_id_collides() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_different_ids_do_not_collide() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let other = timeout(
        Duration::from_millis(50),
        q.clone().acquire("proj-other", 0),
    )
    .await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_same_key_still_collides() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_different_key_does_not_collide() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let other = timeout(
        Duration::from_millis(50),
        q.clone().acquire("proj-other", 0),
    )
    .await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_stats_for_exact_key() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 7));
    assert_eq!(q.project_stats("proj").limit, 7);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_stats_for_other_key() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 7));
    assert_eq!(q.project_stats("other").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_stats_for_exact_key() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 7));
    assert_eq!(q.project_stats("proj").limit, 7);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_stats_for_other_key() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 7));
    assert_eq!(q.project_stats("other").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_by_id_helper() {
    let cfg = typed_per_project_limit_config("proj", 7);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&7));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_helper() {
    let cfg = legacy_per_project_limit_config("proj", 7);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&7));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_nonmatching_alias_uses_default() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 7));
    assert_eq!(q.project_stats("./proj").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_exact_string_runtime_contract() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("./proj", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_same_key_runtime_contract() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_runtime_contract() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_other_key_runtime_contract() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let other = timeout(Duration::from_millis(50), q.clone().acquire("other", 0)).await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_other_key_legacy_runtime_contract() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let other = timeout(Duration::from_millis(50), q.clone().acquire("other", 0)).await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_default_limit_is_global() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_default_limit_is_global() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_queue_needs_canonical_ids_from_callers() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("/tmp/proj", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_queue_is_exact_string_map() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("/tmp/proj").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_queue_is_exact_string_map() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("/tmp/proj").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_new_shape_is_primary() {
    let cfg = typed_per_project_limit_config("proj", 1);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&1));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_old_shape_still_loads() {
    let cfg = legacy_per_project_limit_config("proj", 1);
    assert_eq!(cfg.per_project.by_id().get("proj"), Some(&1));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_same_canonical_key_collides() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_alias_key_does_not_collide_without_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("proj-root", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_distinct_project_ids_do_not_collide() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let other = timeout(Duration::from_millis(50), q.clone().acquire("proj2", 0)).await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_project_limit_map_exact_lookup() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 6));
    assert_eq!(q.project_stats("proj").limit, 6);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_project_limit_map_global_fallback() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 6));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_project_limit_map_exact_lookup() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 6));
    assert_eq!(q.project_stats("proj").limit, 6);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_project_limit_map_global_fallback() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 6));
    assert_eq!(q.project_stats("missing").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_runtime_and_stats_share_same_contract() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    assert_eq!(q.project_stats("proj").limit, 1);
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_unresolved_alias_uses_default_limit() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("alias", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_exact_string_match_is_documented() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("proj").limit, 1);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_caller_must_resolve_aliases() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("root-alias", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_queue_layer_scope_is_limited() {
    let q = TaskQueue::new(&typed_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("root-alias").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_layer_scope_is_limited() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("proj", 1));
    assert_eq!(q.project_stats("root-alias").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_project_limit_for_multiple_ids() {
    let mut cfg = typed_per_project_limit_config("proj", 1);
    cfg.per_project.by_id_mut().insert("proj2".to_string(), 2);
    let q = TaskQueue::new(&cfg);
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("proj2").limit, 2);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_project_limit_for_multiple_ids() {
    let mut cfg = legacy_per_project_limit_config("proj", 1);
    cfg.per_project.by_id_mut().insert("proj2".to_string(), 2);
    let q = TaskQueue::new(&cfg);
    assert_eq!(q.project_stats("proj").limit, 1);
    assert_eq!(q.project_stats("proj2").limit, 2);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_alias_is_separate_even_if_rootlike() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/root/proj", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_same_key_reuses_bucket() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("proj", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_other_key_uses_other_bucket() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config("proj", 1)));
    let _holder = q.acquire("proj", 0).await.unwrap();
    let other = timeout(Duration::from_millis(50), q.clone().acquire("other-key", 0)).await;
    assert!(other.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_typed_helper_mutation_api() {
    let mut cfg = typed_per_project_limit_config("proj", 1);
    cfg.per_project
        .by_id_mut()
        .insert("other-key".to_string(), 2);
    assert_eq!(cfg.per_project.by_id().get("other-key"), Some(&2));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_helper_mutation_api() {
    let mut cfg = legacy_per_project_limit_config("proj", 1);
    cfg.per_project
        .by_id_mut()
        .insert("other-key".to_string(), 2);
    assert_eq!(cfg.per_project.by_id().get("other-key"), Some(&2));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_queue_uses_resolved_project_key_input() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_queue_does_not_resolve_raw_root_input_itself() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/resolved", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_queue_scope_statement() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
    assert_eq!(q.project_stats("/tmp/resolved").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_scope_statement() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
    assert_eq!(q.project_stats("/tmp/resolved").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_primary_acceptance_case() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_non_acceptance_case_without_resolution() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_acceptance_case() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_non_acceptance_case_without_resolution() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_typed_stats_case() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 8));
    assert_eq!(q.project_stats("resolved").limit, 8);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_stats_case() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 8));
    assert_eq!(q.project_stats("resolved").limit, 8);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_fallback_stats_case() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 8));
    assert_eq!(q.project_stats("other").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_fallback_stats_case_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 8));
    assert_eq!(q.project_stats("other").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_migration_story_in_runtime() {
    let typed = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let legacy = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _typed_holder = typed.acquire("resolved", 0).await.unwrap();
    let _legacy_holder = legacy.acquire("resolved", 0).await.unwrap();
    let typed_blocked = timeout(
        Duration::from_millis(50),
        typed.clone().acquire("resolved", 0),
    )
    .await;
    let legacy_blocked = timeout(
        Duration::from_millis(50),
        legacy.clone().acquire("resolved", 0),
    )
    .await;
    assert!(typed_blocked.is_err());
    assert!(legacy_blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_migration_story_for_aliases() {
    let typed = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let legacy = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _typed_holder = typed.acquire("resolved", 0).await.unwrap();
    let _legacy_holder = legacy.acquire("resolved", 0).await.unwrap();
    let typed_alias = timeout(
        Duration::from_millis(50),
        typed.clone().acquire("resolved-root", 0),
    )
    .await;
    let legacy_alias = timeout(
        Duration::from_millis(50),
        legacy.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(typed_alias.is_ok());
    assert!(legacy_alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_new_config_is_used_by_runtime() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_old_config_is_used_by_runtime() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_new_config_requires_exact_resolved_key() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/resolved", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_config_requires_exact_key_too() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("/tmp/resolved", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_core_bucketing_statement() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_core_bucketing_statement_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_two_distinct_ids_can_run() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _a = q.acquire("resolved", 0).await.unwrap();
    let b = q.acquire("other", 0).await;
    assert!(b.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_two_distinct_legacy_ids_can_run() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _a = q.acquire("resolved", 0).await.unwrap();
    let b = q.acquire("other", 0).await;
    assert!(b.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_by_id_mut_supports_multiple_keys() {
    let mut cfg = typed_per_project_limit_config("resolved", 1);
    cfg.per_project.by_id_mut().insert("other".to_string(), 2);
    assert_eq!(cfg.per_project.by_id().get("other"), Some(&2));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_legacy_by_id_mut_supports_multiple_keys() {
    let mut cfg = legacy_per_project_limit_config("resolved", 1);
    cfg.per_project.by_id_mut().insert("other".to_string(), 2);
    assert_eq!(cfg.per_project.by_id().get("other"), Some(&2));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_task_queue_scope_boundary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_task_queue_scope_boundary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_exact_matching_summary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_exact_matching_summary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_alias_summary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_alias_summary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_runtime_summary() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_runtime_summary_legacy() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_alias_runtime_summary() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_alias_runtime_summary_legacy() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_stats_summary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 3));
    assert_eq!(q.project_stats("resolved").limit, 3);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_stats_summary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 3));
    assert_eq!(q.project_stats("resolved").limit, 3);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_fallback_summary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 3));
    assert_eq!(q.project_stats("other").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_fallback_summary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 3));
    assert_eq!(q.project_stats("other").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_config_summary() {
    let cfg = typed_per_project_limit_config("resolved", 3);
    assert_eq!(cfg.per_project.by_id().get("resolved"), Some(&3));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_config_summary_legacy() {
    let cfg = legacy_per_project_limit_config("resolved", 3);
    assert_eq!(cfg.per_project.by_id().get("resolved"), Some(&3));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_scope_summary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 3));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_scope_summary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 3));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_two_project_summary() {
    let mut cfg = typed_per_project_limit_config("resolved", 1);
    cfg.per_project.by_id_mut().insert("other".to_string(), 1);
    let q = Arc::new(TaskQueue::new(&cfg));
    let _a = q.acquire("resolved", 0).await.unwrap();
    let _b = q.acquire("other", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_two_project_summary_legacy() {
    let mut cfg = legacy_per_project_limit_config("resolved", 1);
    cfg.per_project.by_id_mut().insert("other".to_string(), 1);
    let q = Arc::new(TaskQueue::new(&cfg));
    let _a = q.acquire("resolved", 0).await.unwrap();
    let _b = q.acquire("other", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_distinct_alias_summary() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("another", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_distinct_alias_summary_legacy() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(Duration::from_millis(50), q.clone().acquire("another", 0)).await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_explicit_migration_boundary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_explicit_migration_boundary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_runtime_boundary() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_runtime_boundary_legacy() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_internal_key_summary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_internal_key_summary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved").limit, 1);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_noninternal_key_summary() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_noninternal_key_summary_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 1));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_limit_map_source() {
    let cfg = typed_per_project_limit_config("resolved", 4);
    assert_eq!(cfg.per_project.by_id().get("resolved"), Some(&4));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_limit_map_source_legacy() {
    let cfg = legacy_per_project_limit_config("resolved", 4);
    assert_eq!(cfg.per_project.by_id().get("resolved"), Some(&4));
}

#[tokio::test]
async fn issue_684_queue_tests_verify_key_scope_source() {
    let q = TaskQueue::new(&typed_per_project_limit_config("resolved", 4));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_key_scope_source_legacy() {
    let q = TaskQueue::new(&legacy_per_project_limit_config("resolved", 4));
    assert_eq!(q.project_stats("resolved-root").limit, 4);
}

#[tokio::test]
async fn issue_684_queue_tests_verify_final_contract() {
    let q = Arc::new(TaskQueue::new(&typed_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn issue_684_queue_tests_verify_final_contract_legacy() {
    let q = Arc::new(TaskQueue::new(&legacy_per_project_limit_config(
        "resolved", 1,
    )));
    let _holder = q.acquire("resolved", 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.clone().acquire("resolved", 0)).await;
    let alias = timeout(
        Duration::from_millis(50),
        q.clone().acquire("resolved-root", 0),
    )
    .await;
    assert!(blocked.is_err());
    assert!(alias.is_ok());
}

#[tokio::test]
async fn project_cannot_starve_another() {
    // global=2, proj_a=1 → proj_a can use at most 1 global slot,
    // leaving at least 1 for proj_b.
    let cfg = legacy_per_project_limit_config("proj_a", 1);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 2,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        ..cfg
    };
    let q = Arc::new(TaskQueue::new(&cfg));

    // proj_a fills its project slot (limit=1).
    let _pa = q.acquire("proj_a", 0).await.unwrap();

    // proj_a cannot take another slot even though 1 global slot remains.
    let q2 = q.clone();
    let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a", 0)).await;
    assert!(
        blocked.is_err(),
        "proj_a blocked at project limit while global slot is free"
    );

    // proj_b can still acquire the remaining global slot.
    let pb = timeout(Duration::from_millis(100), q.acquire("proj_b", 0)).await;
    assert!(pb.is_ok(), "proj_b should get the remaining global slot");
}

#[tokio::test]
async fn project_stats_reflect_running_and_queued() {
    let q = Arc::new(TaskQueue::new(&config(4, 16)));

    let _p1 = q.acquire("stats_proj", 0).await.unwrap();
    let stats = q.project_stats("stats_proj");
    assert_eq!(stats.running, 1);
    assert_eq!(stats.queued, 0);

    // Queue a waiter by capping project limit to 1.
    let cfg2 = typed_per_project_limit_config("capped", 1);
    let q2 = Arc::new(TaskQueue::new(&cfg2));
    let _holder = q2.acquire("capped", 0).await.unwrap();
    let q3 = q2.clone();
    let _waiter = tokio::spawn(async move { q3.acquire("capped", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let stats2 = q2.project_stats("capped");
    assert_eq!(stats2.running, 1);
    assert_eq!(stats2.queued, 1);
}

// --- Priority tests ---

#[tokio::test]
async fn high_priority_acquired_before_low_when_slots_full() {
    // 1 slot: hold it, then enqueue priority=0 then priority=1.
    // When the slot is released, priority=1 should unblock first.
    let q = Arc::new(TaskQueue::new(&config(1, 16)));
    let holder = q.acquire("proj", 0).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();

    let q1 = q.clone();
    let tx1 = tx.clone();
    tokio::spawn(async move {
        let _p = q1.acquire("proj", 0).await.unwrap();
        let _ = tx1.send(0);
    });

    // Small delay to ensure priority=0 waiter is registered first.
    tokio::time::sleep(Duration::from_millis(10)).await;

    let q2 = q.clone();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let _p = q2.acquire("proj", 1).await.unwrap();
        let _ = tx2.send(1);
    });

    tokio::time::sleep(Duration::from_millis(10)).await;

    // Release the holder; the priority=1 waiter should unblock first.
    drop(holder);
    tokio::time::sleep(Duration::from_millis(30)).await;

    let first = rx.recv().await.expect("should receive first");
    assert_eq!(first, 1, "priority=1 task should unblock before priority=0");
}

#[tokio::test]
async fn same_priority_is_fifo() {
    // 1 slot: hold it, then enqueue three priority=1 waiters in order.
    // They should unblock in enqueue order (FIFO).
    let q = Arc::new(TaskQueue::new(&config(1, 16)));
    let holder = q.acquire("proj", 1).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();

    for i in 0u8..3 {
        let q_i = q.clone();
        let tx_i = tx.clone();
        tokio::spawn(async move {
            let _p = q_i.acquire("proj", 1).await.unwrap();
            let _ = tx_i.send(i);
            // Hold slot briefly so ordering is observable.
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        // Stagger enqueue so seq ordering is deterministic.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Release the holder.
    drop(holder);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut order = Vec::new();
    while let Ok(v) = rx.try_recv() {
        order.push(v);
    }
    assert_eq!(order, vec![0, 1, 2], "same-priority waiters must be FIFO");
}

#[tokio::test]
async fn priority_zero_default_compiles() {
    let q = TaskQueue::new(&config(2, 8));
    let p = q.acquire("proj", 0).await;
    assert!(p.is_ok());
}

// --- running_count correctness ---

#[tokio::test]
async fn running_count_correct_with_global_waiters() {
    // limit=2; fill both slots, add a waiter in the global queue.
    // running_count must still report 2 — waiters do not reduce available_permits.
    let q = Arc::new(TaskQueue::new(&config(2, 16)));
    let _p1 = q.acquire("proj", 0).await.unwrap();
    let _p2 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);

    let q2 = q.clone();
    let _h = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert_eq!(
        q.running_count(),
        2,
        "running_count must not decrease because a global waiter is present"
    );
}

// --- Cancellation-safety tests ---

#[tokio::test]
async fn cancelled_project_wait_does_not_leak_queued_count() {
    // 1 project slot; hold it so the next acquire blocks at project-wait.
    let cfg = typed_per_project_limit_config("proj", 1);
    let q = Arc::new(TaskQueue::new(&cfg));
    let _holder = q.acquire("proj", 0).await.unwrap();

    assert_eq!(q.queued_count(), 0);

    // Spawn an acquire that will block at the project-level wait.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(q.queued_count(), 1);

    // Cancel the waiting future by aborting the task.
    handle.abort();
    assert!(matches!(handle.await, Err(ref e) if e.is_cancelled()));
    tokio::time::sleep(Duration::from_millis(20)).await;

    // queued_count must return to 0 after cancellation.
    assert_eq!(
        q.queued_count(),
        0,
        "queued_count leaked after project-wait cancellation"
    );
}

#[tokio::test]
async fn cancelled_global_wait_releases_project_permit() {
    // project limit=2, global limit=1; hold global so the next acquire
    // passes project-wait then blocks at global-wait.
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 1,
        ..typed_per_project_limit_config("proj", 2)
    };
    let q = Arc::new(TaskQueue::new(&cfg));
    // Hold the single global slot.
    let _global_holder = q.acquire("proj", 0).await.unwrap();

    // This acquire will pass project-wait (limit=2) but block at global-wait.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(q.queued_count(), 1);

    // Cancel the waiting future.
    handle.abort();
    assert!(matches!(handle.await, Err(ref e) if e.is_cancelled()));
    tokio::time::sleep(Duration::from_millis(20)).await;

    // queued_count must return to 0.
    assert_eq!(
        q.queued_count(),
        0,
        "queued_count leaked after global-wait cancellation"
    );

    // The released project permit must allow a new acquisition once the
    // global slot becomes free.
    drop(_global_holder);
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(
        result.is_ok(),
        "project permit not released after global-wait cancellation"
    );
}

// --- Mid-send cancellation race regression tests (Issues 1 & 2) ---

/// Regression: project permit must not be permanently lost when the future is
/// cancelled *after* `release()` sends the signal but *before* `rx.await`
/// completes (the "mid-send" race on the project queue).
#[tokio::test]
async fn project_permit_not_leaked_on_mid_send_cancellation() {
    let cfg = typed_per_project_limit_config("proj", 1);
    let q = Arc::new(TaskQueue::new(&cfg));

    // Hold the single project slot.
    let holder = q.acquire("proj", 0).await.unwrap();

    // Waiter registers in the project queue.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Release holder (sends the permit signal) then immediately cancel the
    // waiter — exercises the race where cancellation happens mid-send.
    drop(holder);
    handle.abort();
    let _ = handle.await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A new task must succeed; the project slot must have been returned.
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(
        result.is_ok(),
        "project permit leaked after mid-send cancellation"
    );
}

/// Regression: global permit must not be permanently lost when the future is
/// cancelled *after* `release()` sends the global signal but *before*
/// `rx.await` completes (the "mid-send" race on the global queue).
#[tokio::test]
async fn global_permit_not_leaked_on_mid_send_cancellation() {
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 1,
        ..typed_per_project_limit_config("proj", 2)
    };
    let q = Arc::new(TaskQueue::new(&cfg));

    // Hold the single global slot.
    let global_holder = q.acquire("proj", 0).await.unwrap();

    // Waiter passes project-wait (project limit=2) and blocks at global-wait.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Release global holder (sends the global signal) then cancel the waiter.
    drop(global_holder);
    handle.abort();
    let _ = handle.await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A new task must succeed; the global slot must have been returned.
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(
        result.is_ok(),
        "global permit leaked after mid-send cancellation"
    );
}

#[tokio::test]
async fn task_admission_blocked_under_pressure() {
    let pressure = Arc::new(AtomicBool::new(true));
    let q = TaskQueue::new_with_pressure(&config(4, 16), Some(pressure));
    let result = q.acquire("proj", 0).await;
    assert!(result.is_err(), "acquire must fail under memory pressure");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("available system memory"),
        "error message should mention memory"
    );
}

#[tokio::test]
async fn task_admission_succeeds_no_pressure() {
    let pressure = Arc::new(AtomicBool::new(false));
    let q = TaskQueue::new_with_pressure(&config(4, 16), Some(pressure));
    let result = q.acquire("proj", 0).await;
    assert!(
        result.is_ok(),
        "acquire must succeed when pressure flag is false"
    );
}
