use super::*;

pub async fn update_status(
    store: &TaskStore,
    task_id: &TaskId,
    status: TaskStatus,
    turn: u32,
) -> anyhow::Result<()> {
    // Terminal-state preservation: once a task has reached Done, Failed, or
    // Cancelled, refuse to transition it to any other status. This prevents
    // late-running execution paths (e.g. a task that was cancelled while
    // waiting for a queue permit) from resurrecting a terminal task to
    // Triaging / Implementing / Reviewing or overwriting one terminal state
    // with another. Issue #1046.
    if let Some(existing) = store.get(task_id) {
        if existing.status.is_terminal() && existing.status != status {
            tracing::warn!(
                task_id = %task_id.0,
                current = existing.status.as_ref(),
                attempted = status.as_ref(),
                "refusing to overwrite terminal task status"
            );
            return Ok(());
        }
    }
    let status_str = status.as_ref().to_string();
    // Track whether the inner closure actually applied the status change.
    // When the inner re-check refuses (race: another writer marked terminal
    // between the snapshot above and the exclusive cache lock), `mutate_and_persist`
    // still persists the row but the durable status does not change. The
    // StatusChanged event must reflect that — logging it for a status that was
    // never written would put the event log out of sync with the durable row.
    let applied = std::sync::atomic::AtomicBool::new(false);
    mutate_and_persist(store, task_id, |s| {
        if s.status.is_terminal() && s.status != status {
            return;
        }
        s.status = status.clone();
        s.turn = turn;
        match status {
            TaskStatus::Pending => s.scheduler.clear_to_queued(),
            TaskStatus::AwaitingDeps => {
                s.scheduler = crate::task_runner::TaskSchedulerState::awaiting_dependencies()
            }
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled => {
                s.scheduler.mark_terminal(&status)
            }
            _ => {
                if matches!(
                    s.scheduler.authority_state,
                    crate::task_runner::SchedulerAuthorityState::Queued
                        | crate::task_runner::SchedulerAuthorityState::Recovering
                        | crate::task_runner::SchedulerAuthorityState::RetryBackoff
                        | crate::task_runner::SchedulerAuthorityState::AwaitingDependencies
                ) {
                    s.scheduler.claim_scheduler("local-scheduler");
                }
            }
        }
        applied.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .await?;
    if applied.load(std::sync::atomic::Ordering::Relaxed) {
        store.log_event(crate::event_replay::TaskEvent::StatusChanged {
            task_id: task_id.0.clone(),
            ts: crate::event_replay::now_ts(),
            status: status_str,
            turn,
        });
    } else {
        tracing::debug!(
            task_id = %task_id.0,
            attempted = status_str,
            "update_status skipped: inner guard refused (terminal-state preservation); not logging StatusChanged"
        );
    }
    Ok(())
}

impl TerminalTransition {
    pub fn applied(&self) -> bool {
        matches!(self, Self::Applied)
    }
}

pub async fn mark_terminal_once(
    store: &TaskStore,
    id: &TaskId,
    outcome: TaskTerminalOutcome,
) -> anyhow::Result<TerminalTransition> {
    let lock = store
        .persist_locks
        .entry(id.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;

    let mut rollback_state = None;
    let mut state_to_persist = None;
    let mut event = None;
    let transition = if let Some(mut entry) = store.cache.get_mut(id) {
        if entry.status.is_terminal() {
            TerminalTransition::AlreadyTerminal(entry.status.clone())
        } else {
            let original = entry.value().clone();
            let status = outcome.status();
            let reason = outcome.reason_string();
            entry.apply_terminal_outcome_once(outcome);
            let attempted = entry.value().clone();
            state_to_persist = Some(attempted.clone());
            rollback_state = Some((original, attempted));
            event = Some(match status {
                TaskStatus::Done => crate::event_replay::TaskEvent::Completed {
                    task_id: id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                },
                TaskStatus::Failed => crate::event_replay::TaskEvent::Failed {
                    task_id: id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                    reason: reason.unwrap_or_else(|| "failed".to_string()),
                },
                TaskStatus::Cancelled => crate::event_replay::TaskEvent::Cancelled {
                    task_id: id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                    reason,
                },
                _ => unreachable!("terminal outcome produced non-terminal status"),
            });
            TerminalTransition::Applied
        }
    } else {
        TerminalTransition::MissingTask
    };

    if transition.applied() {
        let Some(state) = state_to_persist else {
            anyhow::bail!("terminal transition was applied without a persisted task snapshot");
        };
        if let Err(error) = store.db.update(&state).await {
            if let Some((original, attempted)) = rollback_state {
                if let Some(mut entry) = store.cache.get_mut(id) {
                    if task_states_match_for_rollback(entry.value(), &attempted) {
                        *entry.value_mut() = original;
                    } else {
                        tracing::warn!(
                            task_id = %id.0,
                            "mark_terminal_once rollback skipped because cache changed after failed persist"
                        );
                    }
                }
            }
            return Err(error);
        }
        if let Some(mut entry) = store.cache.get_mut(id) {
            entry.value_mut().version = entry.value().version.saturating_add(1);
        }
        if let Some(event) = event {
            store.log_event(event);
        }
    }

    Ok(transition)
}

/// Mutate a task in the cache then persist to SQLite.
pub async fn mutate_and_persist(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) -> anyhow::Result<()> {
    let mut rollback_state = None;
    if let Some(mut entry) = store.cache.get_mut(id) {
        let original = entry.value().clone();
        f(entry.value_mut());
        entry.value_mut().reconcile_scheduler_with_status();
        let attempted = entry.value().clone();
        rollback_state = Some((original, attempted));
    }
    if let Err(error) = store.persist(id).await {
        if let Some((original, attempted)) = rollback_state {
            if let Some(mut entry) = store.cache.get_mut(id) {
                if task_states_match_for_rollback(entry.value(), &attempted) {
                    *entry.value_mut() = original;
                } else {
                    tracing::warn!(
                        task_id = %id.0,
                        "mutate_and_persist rollback skipped because cache changed after failed persist"
                    );
                }
            }
        }
        return Err(error);
    }
    Ok(())
}

fn task_states_match_for_rollback(current: &TaskState, attempted: &TaskState) -> bool {
    match (
        serde_json::to_value(current),
        serde_json::to_value(attempted),
    ) {
        (Ok(current), Ok(attempted)) => current == attempted,
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!("mutate_and_persist rollback comparison failed: {error}");
            false
        }
    }
}
