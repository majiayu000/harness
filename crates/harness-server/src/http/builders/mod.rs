pub(crate) mod engines;
pub(crate) mod intake;
pub(crate) mod registry;
pub(crate) mod registry_failures;
pub(crate) mod registry_migration;
pub(crate) mod services;
pub(crate) mod storage;
pub(crate) mod workspace_pool_config;

pub(crate) fn ensure_startup_context_not_path_derived(
    opener: &'static str,
    context: &harness_core::db::PgStoreContext,
) -> anyhow::Result<()> {
    if matches!(
        context.ownership(),
        Some(ownership) if ownership.retention_class == "path_derived"
    ) {
        anyhow::bail!(
            "startup opener '{opener}' attempted to use path-derived Postgres schema '{}'; \
             normal production startup must use fixed shared schemas. Path-derived schemas \
             are only allowed for legacy migration, backfill, and test compatibility.",
            context.schema()
        );
    }
    Ok(())
}

#[cfg(test)]
tokio::task_local! {
    static FORCED_STARTUP_FAILURES: std::collections::HashMap<&'static str, String>;
}

#[cfg(test)]
pub(crate) async fn with_forced_startup_failures<F, T>(
    failures: &[(&'static str, &str)],
    future: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let mut forced = std::collections::HashMap::new();
    for (name, error) in failures {
        forced.insert(*name, (*error).to_string());
    }
    FORCED_STARTUP_FAILURES.scope(forced, future).await
}

pub(crate) fn forced_startup_error(name: &'static str) -> Option<String> {
    #[cfg(test)]
    {
        FORCED_STARTUP_FAILURES
            .try_with(|failures| failures.get(name).cloned())
            .ok()
            .flatten()
    }

    #[cfg(not(test))]
    {
        let _ = name;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::PgStoreContext;
    use std::path::Path;

    const TEST_DATABASE_URL: &str = "postgres://user:pass@localhost:5432/harness";

    #[test]
    fn storage_path_schema_guard_allows_shared_startup_contexts() -> anyhow::Result<()> {
        let contexts = [
            (
                "tasks",
                crate::task_db::TaskDb::shared_schema_context(Some(TEST_DATABASE_URL))?,
            ),
            (
                "eval_store",
                crate::eval_store::EvalStore::shared_schema_context(Some(TEST_DATABASE_URL))?,
            ),
            (
                "thread_db",
                crate::thread_db::ThreadDb::shared_schema_context(Some(TEST_DATABASE_URL))?,
            ),
            (
                "plan_db",
                crate::plan_db::PlanDb::shared_schema_context(Some(TEST_DATABASE_URL))?,
            ),
            (
                "issue_workflow_store",
                PgStoreContext::new(TEST_DATABASE_URL, "workflow_issue")?,
            ),
            (
                "project_workflow_store",
                PgStoreContext::new(TEST_DATABASE_URL, "workflow_project")?,
            ),
            (
                "workflow_runtime_store",
                PgStoreContext::new(TEST_DATABASE_URL, "workflow_runtime")?,
            ),
            (
                "project_registry",
                crate::project_registry::ProjectRegistry::shared_schema_context(Some(
                    TEST_DATABASE_URL,
                ))?,
            ),
            (
                "workspace_lease_store",
                crate::task_db::TaskDb::shared_schema_context(Some(TEST_DATABASE_URL))?,
            ),
            (
                "runtime_state_store",
                crate::runtime_state_store::RuntimeStateStore::shared_schema_context(Some(
                    TEST_DATABASE_URL,
                ))?,
            ),
            (
                "review_store",
                crate::review_store::ReviewStore::shared_schema_context(Some(TEST_DATABASE_URL))?,
            ),
            (
                "event_store",
                harness_observe::event_store::EventStore::shared_schema_context(Some(
                    TEST_DATABASE_URL,
                ))?,
            ),
        ];

        for (opener, context) in contexts {
            ensure_startup_context_not_path_derived(opener, &context)?;
        }
        Ok(())
    }

    #[test]
    fn storage_path_schema_guard_rejects_path_derived_startup_context() -> anyhow::Result<()> {
        let context = PgStoreContext::from_legacy_path_schema(
            Path::new("/tmp/harness/path-derived-startup.db"),
            Some(TEST_DATABASE_URL),
        )?;

        let err = ensure_startup_context_not_path_derived("thread_db", &context)
            .expect_err("path-derived startup context should be rejected");
        let message = err.to_string();
        assert!(
            message.contains("thread_db"),
            "error should name the startup opener: {message}"
        );
        assert!(
            message.contains("path-derived Postgres schema"),
            "error should describe the rejected schema class: {message}"
        );
        Ok(())
    }
}
