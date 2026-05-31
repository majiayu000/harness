use super::*;
use std::collections::HashSet;

fn create_normalized_workspace_root(parent: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let raw_workspace_root = parent.join(name);
    std::fs::create_dir(&raw_workspace_root)?;
    Ok(normalize_path_for_comparison(&raw_workspace_root))
}

#[test]
fn owner_path_is_orphaned_detects_missing_parent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let live = dir.path().join("threads");
    assert!(
        !owner_path_is_orphaned(PATH_DERIVED_OWNER_KIND, &live),
        "schema whose parent dir exists must never be reaped"
    );

    let dead = dir.path().join("removed-workspace/threads");
    assert!(
        owner_path_is_orphaned(PATH_DERIVED_OWNER_KIND, &dead),
        "schema whose parent dir is gone must be reaped"
    );
    Ok(())
}

#[test]
fn owner_path_is_orphaned_detects_removed_directory_owner() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let workspace = parent.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    assert!(
        !owner_path_is_orphaned(PATH_DERIVED_DIRECTORY_OWNER_KIND, &workspace),
        "live directory owner path must be kept"
    );

    std::fs::remove_dir(&workspace)?;
    assert!(
        owner_path_is_orphaned(PATH_DERIVED_DIRECTORY_OWNER_KIND, &workspace),
        "deleted directory owner path must be reaped even when its parent remains"
    );
    Ok(())
}

#[test]
fn owner_path_is_orphaned_detects_legacy_workspace_directory_owner() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let workspace_root = create_normalized_workspace_root(parent.path(), "workspaces")?;
    let workspace = workspace_root.join("550e8400-e29b-41d4-a716-446655440000");
    let workspace_roots = vec![normalize_path_for_comparison(&workspace_root)];
    std::fs::create_dir(&workspace)?;
    assert!(
        !owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "live legacy workspace directory owner path must be kept"
    );

    std::fs::remove_dir(&workspace)?;
    assert!(
        owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "deleted legacy workspace directory owner path must be reaped"
    );
    Ok(())
}

#[test]
fn owner_path_is_orphaned_detects_suffixed_uuid_legacy_workspace_directory_owner(
) -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let workspace_root = create_normalized_workspace_root(parent.path(), "workspaces")?;
    let workspace = workspace_root.join("550e8400-e29b-41d4-a716-446655440000-seq");
    let workspace_roots = vec![normalize_path_for_comparison(&workspace_root)];
    std::fs::create_dir(&workspace)?;
    assert!(
        !owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "live suffixed UUID workspace directory owner path must be kept"
    );

    std::fs::remove_dir(&workspace)?;
    assert!(
        owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "deleted suffixed UUID workspace directory owner path must be reaped"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn owner_path_is_orphaned_matches_symlinked_legacy_workspace_root() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let workspace_root = create_normalized_workspace_root(parent.path(), "workspaces")?;
    let symlink_root = parent.path().join("linked-workspaces");
    std::os::unix::fs::symlink(&workspace_root, &symlink_root)?;
    let workspace = symlink_root.join("550e8400-e29b-41d4-a716-446655440000-p0");
    let workspace_roots = vec![workspace_root];
    std::fs::create_dir(&workspace)?;
    assert!(
        !owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "live legacy workspace directory under a symlinked root must be kept"
    );

    std::fs::remove_dir(&workspace)?;
    assert!(
        owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "deleted legacy workspace directory under a symlinked root must be reaped"
    );
    Ok(())
}

#[test]
fn owner_path_is_orphaned_detects_dotted_legacy_workspace_directory_owner() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let workspace_root = create_normalized_workspace_root(parent.path(), "workspaces")?;
    let workspace = workspace_root.join("abcd1234__my.org_repo__issue_42");
    let workspace_roots = vec![normalize_path_for_comparison(&workspace_root)];
    std::fs::create_dir(&workspace)?;
    assert!(
        !owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "live dotted legacy workspace directory owner path must be kept"
    );

    std::fs::remove_dir(&workspace)?;
    assert!(
        owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "deleted dotted legacy workspace directory owner path must be reaped"
    );
    Ok(())
}

#[test]
fn owner_path_is_orphaned_detects_custom_legacy_workspace_directory_owner() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let workspace_root = create_normalized_workspace_root(parent.path(), "ws")?;
    let workspace = workspace_root.join("runtime-job-123");
    let workspace_roots = vec![normalize_path_for_comparison(&workspace_root)];
    std::fs::create_dir(&workspace)?;
    assert!(
        !owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "live custom-root legacy workspace directory owner path must be kept"
    );

    std::fs::remove_dir(&workspace)?;
    assert!(
        !owner_path_is_orphaned(PATH_DERIVED_OWNER_KIND, &workspace),
        "custom-root legacy workspace directory owner path must be kept without configured roots"
    );
    assert!(
        owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &workspace,
            &workspace_roots
        ),
        "deleted custom-root legacy workspace directory owner path must be reaped"
    );
    Ok(())
}

#[test]
fn legacy_owner_path_has_workspace_key_accepts_parallel_uuid_suffixes() {
    let root = PathBuf::from("/workspaces");
    assert!(legacy_owner_path_has_workspace_key(
        &root.join("550e8400-e29b-41d4-a716-446655440000-seq")
    ));
    assert!(legacy_owner_path_has_workspace_key(
        &root.join("550e8400-e29b-41d4-a716-446655440000-p0")
    ));
    assert!(!legacy_owner_path_has_workspace_key(
        &root.join("550e8400-e29b-41d4-a716-446655440000-step")
    ));
}

#[test]
fn owner_path_is_orphaned_keeps_ambiguous_legacy_missing_child() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let owner_path = parent.path().join("harness-workspaces/tasks");
    let owner_parent = owner_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("owner path should have a parent"))?;
    std::fs::create_dir(owner_parent)?;
    assert!(
        !owner_path_is_orphaned(PATH_DERIVED_OWNER_KIND, &owner_path),
        "legacy path-derived store under an unconfigured workspace-like root must be kept"
    );
    Ok(())
}

#[test]
fn owner_path_is_orphaned_keeps_configured_root_fixed_store_path() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let workspace_root = parent.path().join("workspaces");
    std::fs::create_dir(&workspace_root)?;
    let owner_path = crate::config::dirs::default_db_path(&workspace_root, "tasks");
    let workspace_roots = vec![normalize_path_for_comparison(&workspace_root)];

    assert!(
        !owner_path_is_orphaned_with_workspace_roots(
            PATH_DERIVED_OWNER_KIND,
            &owner_path,
            &workspace_roots
        ),
        "fixed store paths under a configured root must not be reaped as legacy workspace directories"
    );
    Ok(())
}

#[test]
fn normalize_path_lexically_preserves_root_and_leading_parent() {
    assert_eq!(
        normalize_path_lexically(Path::new("/../workspace")),
        PathBuf::from("/workspace")
    );
    assert_eq!(
        normalize_path_lexically(Path::new("../workspace")),
        PathBuf::from("../workspace")
    );
    assert_eq!(
        normalize_path_lexically(Path::new("a/../../workspace")),
        PathBuf::from("../workspace")
    );
}

#[test]
fn drop_schema_cascade_statement_can_tolerate_missing_schema() -> anyhow::Result<()> {
    assert_eq!(
        drop_schema_cascade_statement("h1234567890abcdef", true)?,
        "DROP SCHEMA IF EXISTS \"h1234567890abcdef\" CASCADE"
    );
    assert_eq!(
        drop_schema_cascade_statement("h1234567890abcdef", false)?,
        "DROP SCHEMA \"h1234567890abcdef\" CASCADE"
    );
    Ok(())
}

#[test]
fn path_derived_ownership_records_canonical_path() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tasks.db");
    let ownership = PgSchemaOwnership::path_derived("h1234567890abcdef".to_string(), path.clone())?;

    assert_eq!(ownership.schema_name, "h1234567890abcdef");
    assert_eq!(ownership.owner_kind, "path_derived_store");
    assert_eq!(ownership.owner_key, path.to_string_lossy());
    assert_eq!(
        ownership.owner_path.as_deref(),
        Some(path.to_string_lossy().as_ref())
    );
    assert_eq!(ownership.retention_class, "path_derived");
    Ok(())
}

#[test]
fn path_derived_ownership_records_directory_owner_kind() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let ownership =
        PgSchemaOwnership::path_derived("h1234567890abcdef".to_string(), dir.path().to_path_buf())?;

    assert_eq!(ownership.schema_name, "h1234567890abcdef");
    assert_eq!(ownership.owner_kind, "path_derived_directory_store");
    assert_eq!(ownership.owner_key, dir.path().to_string_lossy());
    assert_eq!(
        ownership.owner_path.as_deref(),
        Some(dir.path().to_string_lossy().as_ref())
    );
    assert_eq!(ownership.retention_class, "path_derived");
    Ok(())
}

#[test]
fn cleanup_keeps_directory_path_derived_schema_in_manual_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let row = PgSchemaInventoryRow {
        schema_name: "h5555555555555555".to_string(),
        owner_kind: Some("path_derived_directory_store".to_string()),
        owner_key: Some(dir.path().to_string_lossy().to_string()),
        owner_path: Some(dir.path().to_string_lossy().to_string()),
        retention_class: Some("path_derived".to_string()),
        table_count: 2,
        estimated_row_count: 5,
    };

    let candidate = classify_schema_cleanup_candidate(row);

    assert!(candidate.registered);
    assert_eq!(candidate.action, PgSchemaCleanupAction::Keep);
    assert_eq!(
        candidate.reason,
        "registered path-derived schema; owner_path is an identity key, not a liveness check"
    );
    Ok(())
}

#[test]
fn cleanup_keeps_path_derived_schema_when_owner_path_exists() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let row = PgSchemaInventoryRow {
        schema_name: "h1111111111111111".to_string(),
        owner_kind: Some("path_derived_store".to_string()),
        owner_key: Some(dir.path().to_string_lossy().to_string()),
        owner_path: Some(dir.path().to_string_lossy().to_string()),
        retention_class: Some("path_derived".to_string()),
        table_count: 2,
        estimated_row_count: 5,
    };

    let candidate = classify_schema_cleanup_candidate(row);

    assert!(candidate.registered);
    assert_eq!(candidate.action, PgSchemaCleanupAction::Keep);
    assert_eq!(
        candidate.reason,
        "registered path-derived schema; owner_path is an identity key, not a liveness check"
    );
    Ok(())
}

#[test]
fn cleanup_keeps_path_derived_schema_when_owner_path_is_missing() {
    let row = PgSchemaInventoryRow {
        schema_name: "h2222222222222222".to_string(),
        owner_kind: Some("path_derived_store".to_string()),
        owner_key: Some("/definitely/missing/harness/schema-owner".to_string()),
        owner_path: Some("/definitely/missing/harness/schema-owner".to_string()),
        retention_class: Some("path_derived".to_string()),
        table_count: 1,
        estimated_row_count: 0,
    };

    let candidate = classify_schema_cleanup_candidate(row);

    assert!(candidate.registered);
    assert_eq!(candidate.action, PgSchemaCleanupAction::Keep);
    assert_eq!(
        candidate.reason,
        "registered path-derived schema; owner_path is an identity key, not a liveness check"
    );
}

#[test]
fn cleanup_marks_unknown_registered_schema_with_missing_path_as_drop_candidate() {
    let row = PgSchemaInventoryRow {
        schema_name: "h4444444444444444".to_string(),
        owner_kind: Some("external_owner".to_string()),
        owner_key: Some("/definitely/missing/harness/schema-owner".to_string()),
        owner_path: Some("/definitely/missing/harness/schema-owner".to_string()),
        retention_class: Some("path_derived".to_string()),
        table_count: 1,
        estimated_row_count: 0,
    };

    let candidate = classify_schema_cleanup_candidate(row);

    assert!(candidate.registered);
    assert_eq!(candidate.action, PgSchemaCleanupAction::DropCandidate);
    assert_eq!(candidate.reason, "registered owner path is missing");
}

#[test]
fn cleanup_blocks_unregistered_schema_by_default() {
    let row = PgSchemaInventoryRow {
        schema_name: "h3333333333333333".to_string(),
        owner_kind: None,
        owner_key: None,
        owner_path: None,
        retention_class: None,
        table_count: 1,
        estimated_row_count: 0,
    };

    let candidate = classify_schema_cleanup_candidate(row);

    assert!(!candidate.registered);
    assert_eq!(candidate.action, PgSchemaCleanupAction::Blocked);
    assert!(candidate.reason.contains("explicit allowlist"));
}

#[test]
fn drop_request_validation_rejects_registered_keep_candidate() {
    let plan = PgSchemaCleanupPlan {
        candidates: vec![PgSchemaCleanupCandidate {
            schema_name: "h1111111111111111".to_string(),
            registered: true,
            owner_kind: Some("path_derived_store".to_string()),
            owner_key: Some("h1111111111111111".to_string()),
            owner_path: None,
            retention_class: Some("path_derived".to_string()),
            table_count: 1,
            estimated_row_count: 0,
            action: PgSchemaCleanupAction::Keep,
            reason: "registered path-derived schema".to_string(),
        }],
    };
    let requested_registered = HashSet::from(["h1111111111111111".to_string()]);
    let allowlist = HashSet::new();

    let err = validate_pg_schema_drop_request(plan, &requested_registered, &allowlist)
        .expect_err("keep schema should be rejected");

    assert!(err
        .to_string()
        .contains("registered schema 'h1111111111111111' is not a cleanup drop candidate"));
}

#[test]
fn drop_request_validation_fails_before_selecting_partial_targets() {
    let plan = PgSchemaCleanupPlan {
        candidates: vec![PgSchemaCleanupCandidate {
            schema_name: "h1111111111111111".to_string(),
            registered: true,
            owner_kind: Some("external_owner".to_string()),
            owner_key: Some("/definitely/missing/harness/schema-owner".to_string()),
            owner_path: Some("/definitely/missing/harness/schema-owner".to_string()),
            retention_class: Some("path_derived".to_string()),
            table_count: 1,
            estimated_row_count: 0,
            action: PgSchemaCleanupAction::DropCandidate,
            reason: "registered owner path is missing".to_string(),
        }],
    };
    let requested_registered = HashSet::from([
        "h1111111111111111".to_string(),
        "h9999999999999999".to_string(),
    ]);
    let allowlist = HashSet::new();

    let err = validate_pg_schema_drop_request(plan, &requested_registered, &allowlist)
        .expect_err("missing schema should reject the whole cleanup request");

    assert!(err
        .to_string()
        .contains("registered schema 'h9999999999999999' was not found"));
}

#[test]
fn legacy_path_schema_name_detection_is_exact() {
    assert!(is_legacy_path_schema_name("h0123456789abcdef"));
    assert!(!is_legacy_path_schema_name("workflow_runtime"));
    assert!(!is_legacy_path_schema_name("h0123456789abcdeg"));
    assert!(!is_legacy_path_schema_name("h0123456789abcde"));
}
