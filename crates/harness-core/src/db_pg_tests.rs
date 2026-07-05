use super::{
    configure_pg_pool_from_server, pg_create_schema_if_not_exists, pg_open_pool, pg_pool_settings,
    pg_schema_for_path, resolve_database_url, validate_schema_name, PgStoreContext,
    DEFAULT_PG_ACQUIRE_TIMEOUT_SECS, DEFAULT_PG_MAX_CONNECTIONS,
};
use crate::config::server::ServerConfig;
use crate::db_pg_schema_registry::{PG_SCHEMA_REGISTRY_SCHEMA, PG_SCHEMA_REGISTRY_TABLE};
use crate::test_support::process_env_lock;
use std::path::Path;

struct CurrentDirGuard {
    original: std::path::PathBuf,
}

impl CurrentDirGuard {
    fn enter(path: &std::path::Path) -> Self {
        let original = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(path).expect("set current dir");
        Self { original }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original).expect("restore current dir");
    }
}

fn with_isolated_pool_config<F>(f: F)
where
    F: FnOnce(),
{
    let _lock = process_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::enter(dir.path());
    let xdg = dir.path().join("xdg");
    let server = ServerConfig {
        database_pool_max_connections: None,
        database_pool_acquire_timeout_secs: None,
        ..Default::default()
    };
    configure_pg_pool_from_server(&server);
    temp_env::with_vars(
        [
            ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
            ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
            ("HARNESS_DATABASE_POOL_MAX_CONNECTIONS", None::<&str>),
            ("HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS", None::<&str>),
        ],
        || {
            f();
            configure_pg_pool_from_server(&ServerConfig::default());
        },
    );
}

#[test]
fn valid_schema_names() {
    for name in ["public", "_priv", "h1a2b3c4d5e6f7a8", "schema_1", "A"] {
        assert!(
            validate_schema_name(name).is_ok(),
            "expected ok for {name:?}"
        );
    }
}

#[test]
fn empty_schema_rejected() {
    assert!(validate_schema_name("").is_err());
}

#[test]
fn digit_first_rejected() {
    assert!(validate_schema_name("1schema").is_err());
}

#[test]
fn double_quote_rejected() {
    assert!(validate_schema_name("sch\"ema").is_err());
}

#[test]
fn semicolon_rejected() {
    assert!(validate_schema_name("s;DROP TABLE").is_err());
}

#[test]
fn hyphen_rejected() {
    assert!(validate_schema_name("my-schema").is_err());
}

#[test]
fn exactly_63_bytes_accepted() {
    let name = "a".repeat(63);
    assert!(
        validate_schema_name(&name).is_ok(),
        "63-byte name should be valid"
    );
}

#[test]
fn over_63_bytes_rejected() {
    let name = "a".repeat(64);
    assert!(
        validate_schema_name(&name).is_err(),
        "64-byte name should be rejected"
    );
}

#[test]
fn pg_schema_for_path_preserves_legacy_hash_format() {
    let schema =
        pg_schema_for_path(Path::new("/tmp/harness/tasks.db")).expect("path schema should resolve");

    assert_eq!(schema, "h1b76aa87802f7705");
}

#[test]
fn pg_schema_for_path_normalizes_relative_aliases() {
    let _lock = process_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let app_dir = dir.path().join("app");
    std::fs::create_dir(&app_dir).expect("create app dir");
    let _cwd = CurrentDirGuard::enter(&app_dir);

    let canonical_path = app_dir
        .canonicalize()
        .expect("canonical app dir")
        .join("tasks.db");
    let canonical_schema =
        pg_schema_for_path(&canonical_path).expect("canonical path schema should resolve");

    assert_eq!(
        pg_schema_for_path(Path::new("./tasks.db")).expect("dot path schema should resolve"),
        canonical_schema
    );
    assert_eq!(
        pg_schema_for_path(Path::new("../app/tasks.db"))
            .expect("parent alias schema should resolve"),
        canonical_schema
    );
}

#[test]
fn pg_schema_for_path_normalizes_absolute_aliases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app_dir = dir.path().join("app");
    std::fs::create_dir(&app_dir).expect("create app dir");

    let canonical_path = app_dir
        .canonicalize()
        .expect("canonical app dir")
        .join("tasks.db");
    let absolute_alias = app_dir.join("..").join("app").join("tasks.db");

    assert_eq!(
        pg_schema_for_path(&absolute_alias).expect("absolute alias schema should resolve"),
        pg_schema_for_path(&canonical_path).expect("canonical path schema should resolve")
    );
}

#[test]
fn pg_store_context_from_legacy_path_schema_uses_configured_url_and_path_schema(
) -> anyhow::Result<()> {
    let context = PgStoreContext::from_legacy_path_schema(
        Path::new("/tmp/harness/tasks.db"),
        Some(" postgres://user:pass@localhost:5432/harness "),
    )
    .expect("store context should resolve");

    assert_eq!(
        context.database_url(),
        "postgres://user:pass@localhost:5432/harness"
    );
    assert_eq!(context.schema(), "h1b76aa87802f7705");
    let ownership = context
        .ownership()
        .ok_or_else(|| anyhow::anyhow!("path-derived context should carry ownership metadata"))?;
    assert_eq!(ownership.owner_kind, "path_derived_store");
    assert_eq!(ownership.retention_class, "path_derived");
    assert_eq!(
        ownership.owner_path.as_deref(),
        Some("/tmp/harness/tasks.db")
    );
    Ok(())
}

#[test]
fn pg_store_context_debug_redacts_database_url() {
    let context = PgStoreContext::new(
        "postgres://user:secret@localhost:5432/harness",
        "h1b76aa87802f7705",
    )
    .expect("store context should resolve");
    let debug = format!("{context:?}");

    assert!(debug.contains("database_url: \"[REDACTED]\""));
    assert!(debug.contains("schema: \"h1b76aa87802f7705\""));
    assert!(
        !debug.contains("secret"),
        "debug output must not expose database credentials: {debug}"
    );
}

#[test]
fn pg_store_context_from_schema_leaves_legacy_hash_schemas_unowned() -> anyhow::Result<()> {
    let context = PgStoreContext::from_schema(
        "h0123456789abcdef",
        Some("postgres://user:pass@localhost:5432/harness"),
    )?;

    assert!(context.ownership().is_none());
    Ok(())
}

#[test]
fn pg_store_context_from_schema_leaves_shared_schema_canonical() -> anyhow::Result<()> {
    let context = PgStoreContext::from_schema(
        "workflow_runtime",
        Some("postgres://user:pass@localhost:5432/harness"),
    )?;

    assert!(context.ownership().is_none());
    Ok(())
}

#[test]
fn pg_store_context_rejects_invalid_schema() {
    let err = PgStoreContext::new("postgres://user:pass@localhost:5432/harness", "bad-schema")
        .expect_err("invalid schema should fail");

    assert!(
        err.to_string().contains("ASCII letters"),
        "error should explain schema validation, got: {err}"
    );
}

#[tokio::test]
async fn pg_store_context_runtime_pool_errors_name_the_step() {
    let context = PgStoreContext::new("not-a-valid-postgres-url", "h1b76aa87802f7705")
        .expect("context should accept opaque URL strings");
    let err = context
        .open_runtime_pool()
        .await
        .expect_err("invalid URL should fail before connecting");
    assert!(
        err.to_string().contains("runtime pool"),
        "error should identify the runtime-pool step: {err}"
    );
}

#[tokio::test]
async fn concurrent_schema_create_if_not_exists_is_idempotent() -> anyhow::Result<()> {
    let database_url = {
        let _lock = process_env_lock();
        let Ok(database_url) = crate::db_test_safety::resolve_test_database_url(None) else {
            return Ok(());
        };
        database_url
    };
    let setup_pool = match pg_open_pool(&database_url).await {
        Ok(pool) => pool,
        Err(_) => return Ok(()),
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let schema = format!("schema_race_test_{}_{}", std::process::id(), nanos);

    let mut handles = Vec::new();
    for _ in 0..16 {
        let pool = setup_pool.clone();
        let schema = schema.clone();
        handles.push(tokio::spawn(async move {
            pg_create_schema_if_not_exists(&pool, &schema).await
        }));
    }
    for handle in handles {
        handle.await??;
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_namespace WHERE nspname = $1")
        .bind(&schema)
        .fetch_one(&setup_pool)
        .await?;
    assert_eq!(count, 1);

    sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", schema))
        .execute(&setup_pool)
        .await?;
    setup_pool.close().await;
    Ok(())
}

#[test]
fn pg_store_context_can_reuse_shared_setup_pool() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _lock = process_env_lock();
    let Ok(database_url) = crate::db_test_safety::resolve_test_database_url(None) else {
        return;
    };
    runtime.block_on(async {
        let setup_pool = match pg_open_pool(&database_url).await {
            Ok(pool) => pool,
            Err(_) => return,
        };
        let context = PgStoreContext::from_legacy_path_schema(
            Path::new("/tmp/harness/shared.db"),
            Some(&database_url),
        )
        .expect("context should resolve");
        let pool = context
            .open_pool_with_setup_pool(&setup_pool)
            .await
            .expect("shared setup pool should initialize the store");
        pool.close().await;
        setup_pool.close().await;
    });
}

#[tokio::test]
async fn pg_store_context_registers_path_schema_with_shared_setup_pool() -> anyhow::Result<()> {
    let database_url = {
        let _lock = process_env_lock();
        let Ok(database_url) = crate::db_test_safety::resolve_test_database_url(None) else {
            return Ok(());
        };
        database_url
    };
    let setup_pool = match pg_open_pool(&database_url).await {
        Ok(pool) => pool,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let store_path = dir.path().join(format!(
        "registered-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let context = PgStoreContext::from_legacy_path_schema(&store_path, Some(&database_url))?;
    let schema = context.schema().to_string();
    let expected_owner_path = context
        .ownership()
        .and_then(|ownership| ownership.owner_path.clone())
        .ok_or_else(|| anyhow::anyhow!("path-derived context should carry owner_path"))?;
    let pool = context.open_pool_with_setup_pool(&setup_pool).await?;

    let row: (String, String, Option<String>, String) = sqlx::query_as(&format!(
        "SELECT owner_kind, owner_key, owner_path, retention_class
         FROM \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\"
         WHERE schema_name = $1"
    ))
    .bind(&schema)
    .fetch_one(&setup_pool)
    .await?;

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", schema))
        .execute(&setup_pool)
        .await?;
    sqlx::query(&format!(
        "DELETE FROM \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\"
         WHERE schema_name = $1"
    ))
    .bind(&schema)
    .execute(&setup_pool)
    .await?;
    setup_pool.close().await;

    assert_eq!(row.0, "path_derived_store");
    assert_eq!(row.1, expected_owner_path);
    assert_eq!(row.2.as_deref(), Some(expected_owner_path.as_str()));
    assert_eq!(row.3, "path_derived");
    Ok(())
}

#[test]
fn supabase_pooler_urls_use_single_connection() {
    with_isolated_pool_config(|| {
        let url = "postgresql://user:pass@aws-1-ap-northeast-1.pooler.supabase.com:5432/postgres";
        let settings = pg_pool_settings(url).expect("pool settings should resolve");
        assert_eq!(settings.max_connections, 1);
    });
}

#[test]
fn direct_postgres_urls_keep_default_connection_budget() {
    with_isolated_pool_config(|| {
        let url = "postgresql://user:pass@db.example.internal:5432/postgres";
        let settings = pg_pool_settings(url).expect("pool settings should resolve");
        assert_eq!(settings.max_connections, DEFAULT_PG_MAX_CONNECTIONS);
        assert_eq!(
            settings.acquire_timeout,
            std::time::Duration::from_secs(DEFAULT_PG_ACQUIRE_TIMEOUT_SECS)
        );
    });
}

#[test]
fn server_pool_config_overrides_default_connection_budget() {
    with_isolated_pool_config(|| {
        let server = ServerConfig {
            database_pool_max_connections: Some(8),
            database_pool_acquire_timeout_secs: Some(30),
            ..Default::default()
        };
        configure_pg_pool_from_server(&server);

        let settings = pg_pool_settings("postgres://user:pass@localhost:5432/harness")
            .expect("pool settings should resolve");
        assert_eq!(settings.max_connections, 8);
        assert_eq!(settings.acquire_timeout, std::time::Duration::from_secs(30));
    });
}

#[test]
fn env_pool_config_overrides_server_config() {
    with_isolated_pool_config(|| {
        let server = ServerConfig {
            database_pool_max_connections: Some(4),
            database_pool_acquire_timeout_secs: Some(20),
            ..Default::default()
        };
        configure_pg_pool_from_server(&server);

        temp_env::with_vars(
            [
                ("HARNESS_DATABASE_POOL_MAX_CONNECTIONS", Some("12")),
                ("HARNESS_DATABASE_POOL_ACQUIRE_TIMEOUT_SECS", Some("45")),
            ],
            || {
                let settings = pg_pool_settings("postgres://user:pass@localhost:5432/harness")
                    .expect("pool settings should resolve");
                assert_eq!(settings.max_connections, 12);
                assert_eq!(settings.acquire_timeout, std::time::Duration::from_secs(45));
            },
        );
    });
}

#[test]
fn zero_pool_config_is_rejected() {
    with_isolated_pool_config(|| {
        let server = ServerConfig {
            database_pool_max_connections: Some(0),
            ..Default::default()
        };
        configure_pg_pool_from_server(&server);

        let err = pg_pool_settings("postgres://user:pass@localhost:5432/harness")
            .expect_err("zero max connections should fail");
        assert!(
            err.to_string().contains("greater than 0"),
            "error should explain invalid pool size: {err}"
        );
    });
}

#[test]
fn configured_database_url_wins_over_config_sources() {
    let _lock = process_env_lock();
    temp_env::with_vars(
        [
            (
                "HARNESS_DATABASE_URL",
                Some("postgres://env-user:env-pass@env-host:5432/envdb"),
            ),
            (
                "DATABASE_URL",
                Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
            ),
        ],
        || {
            let resolved =
                resolve_database_url(Some("postgres://cfg-user:cfg-pass@cfg-host:5432/cfgdb"))
                    .expect("configured database URL should resolve");
            assert_eq!(resolved, "postgres://cfg-user:cfg-pass@cfg-host:5432/cfgdb");
        },
    );
}

#[test]
fn harness_database_url_used_as_config_override() {
    let _lock = process_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::enter(dir.path());
    let xdg = dir.path().join("xdg");
    temp_env::with_vars(
        [
            ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
            ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
            (
                "HARNESS_DATABASE_URL",
                Some("postgres://env-user:env-pass@env-host:5432/envdb"),
            ),
            (
                "DATABASE_URL",
                Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
            ),
        ],
        || {
            let resolved = resolve_database_url(None).expect("HARNESS_DATABASE_URL should resolve");
            assert_eq!(resolved, "postgres://env-user:env-pass@env-host:5432/envdb");
        },
    );
}

#[test]
fn discovered_config_database_url_used_as_fallback() {
    let _lock = process_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::enter(dir.path());
    let xdg = dir.path().join("xdg");
    let harness_dir = xdg.join("harness");
    std::fs::create_dir_all(&harness_dir).expect("create config dir");

    let mut config = crate::config::HarnessConfig::default();
    config.server.database_url =
        Some("postgres://file-user:file-pass@file-host:5432/filedb".to_string());
    std::fs::write(
        harness_dir.join("config.toml"),
        toml::to_string(&config).expect("serialize config"),
    )
    .expect("write config");

    temp_env::with_vars(
        [
            ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
            ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
            ("HARNESS_DATABASE_URL", None::<&str>),
            (
                "DATABASE_URL",
                Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
            ),
        ],
        || {
            let resolved = resolve_database_url(None).expect("config database URL should resolve");
            assert_eq!(
                resolved,
                "postgres://file-user:file-pass@file-host:5432/filedb"
            );
        },
    );
}

#[test]
fn repository_default_config_database_url_used_as_fallback() {
    let _lock = process_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let xdg = dir.path().join("xdg");
    let repo_config = dir.path().join("config");
    let nested_cwd = dir.path().join("crates/harness-core");
    std::fs::create_dir_all(&repo_config).expect("create repo config dir");
    std::fs::create_dir_all(&nested_cwd).expect("create nested cwd");
    std::fs::write(
        repo_config.join("default.toml"),
        r#"
            [server]
            database_url = "postgres://repo-user:repo-pass@repo-host:5432/repodb"
        "#,
    )
    .expect("write repo config");

    let _cwd = CurrentDirGuard::enter(&nested_cwd);
    temp_env::with_vars(
        [
            ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
            ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
            ("HARNESS_DATABASE_URL", None::<&str>),
            (
                "DATABASE_URL",
                Some("postgres://ignored-user:ignored-pass@ignored-host:5432/ignoreddb"),
            ),
        ],
        || {
            let resolved =
                resolve_database_url(None).expect("repo config database URL should resolve");
            assert_eq!(
                resolved,
                "postgres://repo-user:repo-pass@repo-host:5432/repodb"
            );
        },
    );
}

#[test]
fn discovered_config_survives_deleted_current_dir() {
    let _lock = process_env_lock();
    let home = tempfile::tempdir().expect("home tempdir");
    let cwd = tempfile::tempdir().expect("cwd tempdir");
    let xdg = home.path().join("xdg");
    let harness_dir = xdg.join("harness");
    std::fs::create_dir_all(&harness_dir).expect("create config dir");

    let mut config = crate::config::HarnessConfig::default();
    config.server.database_url =
        Some("postgres://file-user:file-pass@file-host:5432/filedb".to_string());
    std::fs::write(
        harness_dir.join("config.toml"),
        toml::to_string(&config).expect("serialize config"),
    )
    .expect("write config");

    let cwd_path = cwd.path().to_path_buf();
    let _cwd = CurrentDirGuard::enter(&cwd_path);
    drop(cwd);

    temp_env::with_vars(
        [
            ("HOME", Some(home.path().to_str().expect("utf8 home"))),
            ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
            ("HARNESS_DATABASE_URL", None::<&str>),
            ("DATABASE_URL", None::<&str>),
        ],
        || {
            let resolved = resolve_database_url(None).expect("config database URL should resolve");
            assert_eq!(
                resolved,
                "postgres://file-user:file-pass@file-host:5432/filedb"
            );
        },
    );
}

#[test]
fn bare_database_url_is_ignored_without_config() {
    let _lock = process_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::enter(dir.path());
    let xdg = dir.path().join("xdg");
    temp_env::with_vars(
        [
            ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
            ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
            ("HARNESS_DATABASE_URL", None::<&str>),
            (
                "DATABASE_URL",
                Some("postgres://env-user:env-pass@env-host:5432/envdb"),
            ),
        ],
        || {
            let err = resolve_database_url(None).expect_err("bare DATABASE_URL should not resolve");
            assert!(
                err.to_string().contains("server.database_url"),
                "error should mention the TOML config path, got: {err}"
            );
        },
    );
}

#[test]
fn missing_database_url_returns_error() {
    let _lock = process_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::enter(dir.path());
    let xdg = dir.path().join("xdg");
    temp_env::with_vars(
        [
            ("HOME", Some(dir.path().to_str().expect("utf8 tempdir"))),
            ("XDG_CONFIG_HOME", Some(xdg.to_str().expect("utf8 xdg"))),
            ("HARNESS_DATABASE_URL", None::<&str>),
            ("DATABASE_URL", None::<&str>),
        ],
        || {
            let err = resolve_database_url(None).expect_err("missing database URL should fail");
            assert!(
                err.to_string().contains("server.database_url"),
                "error should mention the TOML config path, got: {err}"
            );
        },
    );
}
