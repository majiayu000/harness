use super::*;

/// Spawn the periodic reconciliation loop as a background task.
///
/// Returns immediately without spawning when `config.enabled` is false.
pub fn start(state: Arc<AppState>, config: ReconciliationConfig) {
    if !config.enabled {
        tracing::debug!("reconciliation: periodic loop disabled");
        return;
    }

    tokio::spawn(async move {
        reconciliation_loop(state, config).await;
    });
}

async fn reconciliation_loop(state: Arc<AppState>, config: ReconciliationConfig) {
    let interval = Duration::from_secs(config.interval_secs);
    // Brief init delay so the server is fully up before the first tick.
    sleep(Duration::from_secs(15)).await;

    loop {
        let report = run_once_with_runtime_config(
            state.core.workflow_runtime_store.as_deref(),
            state.core.issue_workflow_store.as_deref(),
            &config,
            false,
            state.core.server.config.server.github_token.as_deref(),
        )
        .await;
        raise_reconciliation_alerts(&state, &report);

        tracing::info!(
            candidates = report.candidates,
            skipped_terminal = report.skipped_terminal,
            transitions = report.transitions.len(),
            workflow_transitions = report.workflow_transitions.len(),
            workflow_alerts = report.workflow_alerts.len(),
            "reconciliation: tick complete"
        );
        sleep(interval).await;
    }
}

/// External alerts for reconciliation findings (GH1582 B-019/B-020).
///
/// The once-per-TTL bound for `ready_to_merge` aging is enforced by the
/// dispatcher dedup window carrying `ready_to_merge_alert_ttl_secs`;
/// reconciliation itself stays non-destructive and stateless here.
fn raise_reconciliation_alerts(state: &Arc<AppState>, report: &ReconciliationReport) {
    let alerts = &state.observability.alerts;
    if !alerts.is_enabled() {
        return;
    }
    let ttl = Duration::from_secs(
        state
            .core
            .server
            .config
            .reconciliation
            .ready_to_merge_alert_ttl_secs,
    );
    for alert in &report.workflow_alerts {
        let pr_ref = alert.pr_url.clone().unwrap_or_else(|| {
            format!(
                "{}#{}",
                alert.repo.as_deref().unwrap_or("unknown"),
                alert.pr_number
            )
        });
        alerts.raise_with_cooldown_override(
            crate::alerting::producers::ready_to_merge_aging(
                &pr_ref,
                alert.age_secs,
                Some(&alert.workflow_id),
            ),
            Some(ttl),
        );
    }
    for transition in &report.workflow_transitions {
        if !transition.applied {
            alerts.raise(crate::alerting::producers::reconciliation_anomaly(
                &transition.workflow_id,
                &format!(
                    "transition {} -> {} failed to apply ({})",
                    transition.from, transition.to, transition.reason
                ),
            ));
        }
    }
}
