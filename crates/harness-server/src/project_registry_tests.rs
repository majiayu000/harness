use super::*;
use futures::FutureExt;
use harness_core::db::{pg_open_pool, resolve_database_url, PgStoreContext};

fn unique_test_schema(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("{prefix}_{nanos}_{count}")
}

fn project(id: &str, root: &str) -> Project {
    Project {
        id: id.to_string(),
        root: PathBuf::from(root),
        name: Some(id.to_string()),
        max_concurrent: None,
        default_agent: None,
        active: true,
        created_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

async fn open_test_registry(name: &str) -> anyhow::Result<Option<Arc<ProjectRegistry>>> {
    if resolve_database_url(None).is_err() {
        return Ok(None);
    }
    let dir = tempfile::tempdir()?;
    let registry = ProjectRegistry::open(&dir.path().join(name)).await?;
    Ok(Some(registry))
}

#[test]
fn shared_schema_context_uses_fixed_project_registry_schema() -> anyhow::Result<()> {
    let context = ProjectRegistry::shared_schema_context(Some(
        "postgres://user:pass@localhost:5432/harness",
    ))?;
    assert_eq!(context.schema(), PROJECT_REGISTRY_SCHEMA);
    assert!(
        context.ownership().is_none(),
        "shared project_registry schema must not register path-derived ownership"
    );
    Ok(())
}

#[test]
fn store_key_for_missing_relative_data_dir_is_absolute() -> anyhow::Result<()> {
    let relative = PathBuf::from(format!(
        "target/project_registry_missing_data_dir_{}",
        unique_test_schema("store_key")
    ));
    assert!(
        !relative.exists(),
        "test path should stay missing so canonicalize fails"
    );
    let expected = std::env::current_dir()?.join(&relative);
    assert_eq!(
        ProjectRegistry::store_key_for_data_dir(&relative),
        expected.to_string_lossy()
    );
    Ok(())
}

#[tokio::test]
async fn shared_schema_project_registry_keeps_store_rows_isolated() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let shared_schema = unique_test_schema("project_registry_scope_test");
    let shared_context = PgStoreContext::from_schema(&shared_schema, Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    let registry_a =
        ProjectRegistry::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir)
            .await?;
    let registry_b =
        ProjectRegistry::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir)
            .await?;

    let result = std::panic::AssertUnwindSafe(async {
        assert_ne!(registry_a.store_key(), registry_b.store_key());

        registry_a
            .register(project("same-project-id", "/project-a"))
            .await?;
        registry_b
            .register(project("same-project-id", "/project-b"))
            .await?;

        assert_eq!(registry_a.list().await?.len(), 1);
        assert_eq!(registry_b.list().await?.len(), 1);
        assert_eq!(
            registry_a
                .get("same-project-id")
                .await?
                .expect("store a project should exist")
                .root,
            PathBuf::from("/project-a")
        );
        assert_eq!(
            registry_b
                .get("same-project-id")
                .await?
                .expect("store b project should exist")
                .root,
            PathBuf::from("/project-b")
        );

        assert!(registry_b.remove("same-project-id").await?);
        assert!(
            registry_a.get("same-project-id").await?.is_some(),
            "deletes must not cross store keys"
        );
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    registry_a.pool().close().await;
    registry_b.pool().close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{shared_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn legacy_project_registry_migration_backfills_once() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    let other_data_dir = dir.path().join("other-data");
    let legacy_path = target_data_dir.join("projects.db");
    let legacy_schema = PgStoreContext::from_path(&legacy_path, Some(&database_url))?
        .schema()
        .to_owned();
    let target_schema = unique_test_schema("project_registry_migration_test");
    let setup_pool = pg_open_pool(&database_url).await?;
    let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
    let target_registry =
        ProjectRegistry::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir)
            .await?;
    let other_registry =
        ProjectRegistry::open_shared_with_data_dir(&target_context, &setup_pool, &other_data_dir)
            .await?;
    let legacy_registry =
        ProjectRegistry::open_with_database_url(&legacy_path, Some(&database_url)).await?;

    let result = std::panic::AssertUnwindSafe(async {
        legacy_registry
            .register(project("legacy-project", "/legacy/project"))
            .await?;

        let copied = migrate_legacy_project_registry_if_needed(
            &legacy_path,
            Some(&database_url),
            target_registry.as_ref(),
        )
        .await?;
        assert_eq!(copied, 1, "one legacy project should be copied");

        let copied_again = migrate_legacy_project_registry_if_needed(
            &legacy_path,
            Some(&database_url),
            target_registry.as_ref(),
        )
        .await?;
        assert_eq!(copied_again, 0, "migration must be idempotent");

        let loaded = target_registry
            .get("legacy-project")
            .await?
            .expect("legacy project should be present in the shared schema");
        assert_eq!(loaded.root, PathBuf::from("/legacy/project"));
        assert!(
            other_registry.get("legacy-project").await?.is_none(),
            "other data_dir scopes must not hydrate legacy projects"
        );

        target_registry
            .register(project("legacy-project", "/shared/project"))
            .await?;
        let copied_after_shared_update = migrate_legacy_project_registry_if_needed(
            &legacy_path,
            Some(&database_url),
            target_registry.as_ref(),
        )
        .await?;
        assert_eq!(
            copied_after_shared_update, 0,
            "completed migration must not overwrite updated shared project rows"
        );
        let updated = target_registry
            .get("legacy-project")
            .await?
            .expect("updated shared project should remain");
        assert_eq!(updated.root, PathBuf::from("/shared/project"));

        assert!(target_registry.remove("legacy-project").await?);
        let copied_after_shared_delete = migrate_legacy_project_registry_if_needed(
            &legacy_path,
            Some(&database_url),
            target_registry.as_ref(),
        )
        .await?;
        assert_eq!(
            copied_after_shared_delete, 0,
            "completed migration must not resurrect deleted shared projects"
        );
        assert!(
            target_registry.get("legacy-project").await?.is_none(),
            "deleted shared project must stay deleted"
        );
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    legacy_registry.pool().close().await;
    target_registry.pool().close().await;
    other_registry.pool().close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{legacy_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn register_and_get_roundtrip() -> anyhow::Result<()> {
    let Some(registry) = open_test_registry("projects.db").await? else {
        return Ok(());
    };

    let project = Project {
        id: "my-project".to_string(),
        root: PathBuf::from("/tmp/my-project"),
        name: None,
        max_concurrent: None,
        default_agent: None,
        active: true,
        created_at: "2026-01-01T00:00:00Z".to_string(),
    };
    registry.register(project.clone()).await?;

    let loaded = registry
        .get("my-project")
        .await?
        .expect("project should exist");
    assert_eq!(loaded.id, "my-project");
    assert_eq!(loaded.root, PathBuf::from("/tmp/my-project"));
    assert!(loaded.active);
    Ok(())
}

#[tokio::test]
async fn list_returns_all_projects() -> anyhow::Result<()> {
    let Some(registry) = open_test_registry("projects.db").await? else {
        return Ok(());
    };

    for i in 0..3u32 {
        registry
            .register(Project {
                id: format!("p{i}"),
                root: PathBuf::from(format!("/tmp/p{i}")),
                name: None,
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
    }

    let all = registry.list().await?;
    assert_eq!(all.len(), 3);
    Ok(())
}

#[tokio::test]
async fn remove_returns_true_when_found() -> anyhow::Result<()> {
    let Some(registry) = open_test_registry("projects.db").await? else {
        return Ok(());
    };

    registry
        .register(Project {
            id: "to-delete".to_string(),
            root: PathBuf::from("/tmp/x"),
            name: None,
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await?;

    assert!(registry.remove("to-delete").await?);
    assert!(registry.get("to-delete").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn remove_returns_false_when_missing() -> anyhow::Result<()> {
    let Some(registry) = open_test_registry("projects.db").await? else {
        return Ok(());
    };
    assert!(!registry.remove("nonexistent").await?);
    Ok(())
}

#[tokio::test]
async fn resolve_path_returns_root() -> anyhow::Result<()> {
    let Some(registry) = open_test_registry("projects.db").await? else {
        return Ok(());
    };

    registry
        .register(Project {
            id: "harness".to_string(),
            root: PathBuf::from("/home/user/harness"),
            name: None,
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await?;

    let path = registry.resolve_path("harness").await?;
    assert_eq!(path, Some(PathBuf::from("/home/user/harness")));
    assert!(registry.resolve_path("unknown").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn survives_reopen() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("projects.db");

    {
        let registry = ProjectRegistry::open(&db_path).await?;
        registry
            .register(Project {
                id: "persistent".to_string(),
                root: PathBuf::from("/tmp/persistent"),
                name: None,
                max_concurrent: Some(2),
                default_agent: Some("claude".to_string()),
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
    }

    let registry = ProjectRegistry::open(&db_path).await?;
    let loaded = registry
        .get("persistent")
        .await?
        .expect("should survive reopen");
    assert_eq!(loaded.max_concurrent, Some(2));
    assert_eq!(loaded.default_agent.as_deref(), Some("claude"));
    Ok(())
}

#[tokio::test]
async fn get_by_root_returns_canonical_match() -> anyhow::Result<()> {
    let Some(registry) = open_test_registry("projects.db").await? else {
        return Ok(());
    };

    let dir = tempfile::tempdir()?;
    let canonical_root = dir.path().canonicalize()?;
    registry
        .register(Project {
            id: "root-keyed".to_string(),
            root: canonical_root.clone(),
            name: Some("root-keyed".to_string()),
            max_concurrent: Some(2),
            default_agent: Some("codex".to_string()),
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await?;

    let loaded = registry
        .get_by_root(dir.path())
        .await?
        .expect("project should resolve by canonical root");
    assert_eq!(loaded.id, "root-keyed");
    assert_eq!(loaded.root, canonical_root);
    assert_eq!(loaded.default_agent.as_deref(), Some("codex"));
    Ok(())
}
