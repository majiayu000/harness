use super::*;

impl TaskStore {
    pub async fn open(db_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        Self::open_with_database_url(db_path, None).await
    }

    pub async fn open_with_database_url(
        db_path: &std::path::Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open_with_database_url(db_path, configured_database_url).await?;
        Self::from_task_db(db, db_path).await
    }

    pub async fn open_with_context(
        db_path: &std::path::Path,
        context: &PgStoreContext,
        setup_pool: &sqlx::postgres::PgPool,
    ) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open_with_context(context, setup_pool).await?;
        Self::from_task_db(db, db_path).await
    }

    pub async fn open_shared_with_data_dir(
        db_path: &std::path::Path,
        context: &PgStoreContext,
        setup_pool: &sqlx::postgres::PgPool,
        data_dir: &std::path::Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open_shared_with_data_dir(context, setup_pool, data_dir).await?;
        crate::task_db::migrate_legacy_task_db_if_needed(db_path, configured_database_url, &db)
            .await?;
        Self::from_task_db(db, db_path).await
    }

    pub async fn open_shared_for_reconciliation_with_data_dir(
        db_path: &std::path::Path,
        context: &PgStoreContext,
        setup_pool: &sqlx::postgres::PgPool,
        data_dir: &std::path::Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open_shared_with_data_dir(context, setup_pool, data_dir).await?;
        crate::task_db::migrate_legacy_task_db_if_needed(db_path, configured_database_url, &db)
            .await?;
        Self::from_task_db_without_startup_recovery(db, db_path).await
    }

    async fn from_task_db(db: TaskDb, db_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        Self::from_task_db_with_startup_recovery(db, db_path, true).await
    }

    async fn from_task_db_without_startup_recovery(
        db: TaskDb,
        db_path: &std::path::Path,
    ) -> anyhow::Result<Arc<Self>> {
        Self::from_task_db_with_startup_recovery(db, db_path, false).await
    }

    async fn from_task_db_with_startup_recovery(
        db: TaskDb,
        db_path: &std::path::Path,
        run_startup_recovery: bool,
    ) -> anyhow::Result<Arc<Self>> {
        // 1. Event replay: runs BEFORE recover_in_progress so event-sourced
        //    data (pr_url, terminal status) wins over checkpoint data.
        let event_log_path = db_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("task-events.jsonl");
        if run_startup_recovery {
            if let Err(e) = crate::event_replay::replay_and_recover(&db, &event_log_path).await {
                if e.downcast_ref::<crate::task_db::TaskRecoveryConflict>()
                    .is_some()
                {
                    return Err(e);
                }
                tracing::warn!("startup: event replay failed (non-fatal): {e}");
            }

            // 2. Legacy checkpoint-based recovery as fallback.
            let recovery = db.recover_in_progress().await?;
            if recovery.resumed > 0 {
                tracing::info!(
                    "startup recovery: resumed {} task(s) from checkpoint",
                    recovery.resumed
                );
            }
            if recovery.failed > 0 {
                tracing::warn!(
                    "startup recovery: marked {} interrupted task(s) as failed (no fresh checkpoint)",
                    recovery.failed
                );
            }
        }

        // 3. Open the event log for appending during this server session.
        let event_log = match crate::event_replay::TaskEventLog::open(&event_log_path) {
            Ok(log) => {
                tracing::debug!("task event log: {}", event_log_path.display());
                Some(Arc::new(log))
            }
            Err(e) => {
                tracing::warn!(
                    "failed to open task event log at {}: {e}",
                    event_log_path.display()
                );
                None
            }
        };

        let cache = DashMap::new();
        let persist_locks = DashMap::new();
        // Only load active (non-terminal) tasks into the in-memory cache to prevent
        // unbounded memory growth from historical completed tasks.
        let active_statuses = &[
            "pending",
            "awaiting_deps",
            "triaging",
            "planning",
            "implementing",
            "review_generating",
            "review_waiting",
            "planner_generating",
            "planner_waiting",
            "agent_review",
            "waiting",
            "reviewing",
        ];
        for task in db.list_by_status(active_statuses).await? {
            persist_locks.insert(task.id.clone(), Arc::new(Mutex::new(())));
            cache.insert(task.id.clone(), task);
        }
        let store = Arc::new(Self {
            cache,
            db,
            persist_locks,
            stream_txs: DashMap::new(),
            abort_handles: DashMap::new(),
            rate_limit_until: RwLock::new(None),
            event_log,
        });
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn task_db_schema_for_test(&self) -> &str {
        self.db.schema()
    }

    #[cfg(test)]
    pub(crate) fn task_db_store_key_for_test(&self) -> &str {
        self.db.store_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::{TaskState, TaskStatus};
    use harness_core::types::TaskId;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Record};
    use tracing::subscriber::Interest;
    use tracing::{Event, Id, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct CapturedStartupLogs(Arc<Mutex<Vec<String>>>);

    #[derive(Default)]
    struct MessageVisitor(String);

    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0.push_str(&format!("{value:?}"));
            }
        }
    }

    impl Subscriber for CapturedStartupLogs {
        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            Interest::always()
        }
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _attributes: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _span: &Id, _values: &Record<'_>) {}
        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(visitor.0);
        }
        fn enter(&self, _span: &Id) {}
        fn exit(&self, _span: &Id) {}
    }

    impl CapturedStartupLogs {
        fn output(&self) -> String {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .join("\n")
        }
    }

    #[tokio::test]
    async fn checkpoint_conflict_fails_store_startup_before_aggregate_logs() -> anyhow::Result<()> {
        if std::env::var("HARNESS_DATABASE_URL").is_err() {
            return Ok(());
        }
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("tasks.db");
        let db = TaskDb::open(&db_path).await?;
        let task_id = "startup-checkpoint-conflict";
        let mut task = TaskState::new(TaskId(task_id.to_string()));
        task.status = TaskStatus::Implementing;
        db.insert(&task).await?;
        db.write_checkpoint(
            task_id,
            None,
            Some("plan"),
            Some("https://github.com/owner/repo/pull/1716"),
            "pr_created",
        )
        .await?;

        let interleave = TaskDb::install_recovery_write_interleave_for_test(task_id);
        let pool = db.postgres_pool();
        let store_key = db.store_key().to_string();
        let captured = CapturedStartupLogs::default();
        let startup = async {
            let _subscriber_guard = tracing::subscriber::set_default(captured.clone());
            TaskStore::open(&db_path).await
        };
        let actor = async {
            interleave.wait_until_selected().await;
            sqlx::query("UPDATE tasks SET version = version + 1 WHERE store_key = $1 AND id = $2")
                .bind(&store_key)
                .bind(task_id)
                .execute(&pool)
                .await?;
            let fields: (String, Option<String>, Option<String>, String, i32) = sqlx::query_as(
                "SELECT status, pr_url, error, scheduler_state, version \
                 FROM tasks WHERE store_key = $1 AND id = $2",
            )
            .bind(&store_key)
            .bind(task_id)
            .fetch_one(&pool)
            .await?;
            interleave.release();
            anyhow::Ok(fields)
        };
        let (startup_result, actor_fields) = tokio::join!(startup, actor);
        let error = match startup_result {
            Err(error) => error,
            Ok(_) => panic!("checkpoint conflict must fail task-store startup"),
        };
        assert!(error
            .downcast_ref::<crate::task_db::TaskRecoveryConflict>()
            .is_some());
        let durable_fields: (String, Option<String>, Option<String>, String, i32) = sqlx::query_as(
            "SELECT status, pr_url, error, scheduler_state, version \
                 FROM tasks WHERE store_key = $1 AND id = $2",
        )
        .bind(&store_key)
        .bind(task_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            durable_fields, actor_fields?,
            "failed startup recovery must not rewrite the actor-owned durable row"
        );
        let output = captured.output();
        for success_wording in [
            "startup recovery: resumed task",
            "wrote back pr_url",
            "resumed 1 task(s) from checkpoint",
            "marked 1 interrupted task(s) as failed",
        ] {
            assert!(
                !output.contains(success_wording),
                "startup conflict must exclude aggregate success wording {success_wording:?}: {output}"
            );
        }
        Ok(())
    }
}
