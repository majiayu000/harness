//! Admission checks on the ordinary turn engine's actual execution backend.

use super::runtime_usage::RuntimeUsageContext;
use super::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::registry::AgentRegistry;
use harness_core::agent::{AgentBackend, AgentRequest, StreamItem};
use harness_core::config::workflow::{RuntimeBudgetEnforcement, RuntimeBudgetPolicy};
use harness_core::config::HarnessConfig;
use harness_core::types::{AgentId, Item, TurnStatus};
use harness_workflow::runtime::{RuntimeKind, WorkflowRuntimeStore};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::mpsc;

struct BudgetBackend {
    reports_cost: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentBackend for BudgetBackend {
    fn name(&self) -> &str {
        "budget-test"
    }

    fn reports_usage_cost(&self) -> bool {
        self.reports_cost
    }

    async fn execute_stream(
        &self,
        _request: AgentRequest,
        tx: mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tx.send(StreamItem::Done)
            .await
            .map_err(|error| harness_core::error::HarnessError::AgentExecution(error.to_string()))
    }

    async fn start_turn(
        &self,
        request: AgentRequest,
        tx: mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.execute_stream(request, tx).await
    }
}

async fn ordinary_budget_case(
    policy: RuntimeBudgetPolicy,
    force_code_agent: bool,
    oneshot_reports_cost: bool,
    adapter_reports_cost: bool,
    should_launch: bool,
) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store = Arc::new(
        WorkflowRuntimeStore::open_with_database_url(
            &root.path().join("runtime.db"),
            Some(&database_url),
        )
        .await?,
    );
    let agent_calls = Arc::new(AtomicUsize::new(0));
    let adapter_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = AgentRegistry::new("budget-test");
    registry.register(
        "budget-test",
        Arc::new(BudgetBackend {
            reports_cost: oneshot_reports_cost,
            calls: agent_calls.clone(),
        }),
    );
    let factory_calls = adapter_calls.clone();
    registry
        .register_turn_backend_factory("budget-test", move || {
            Arc::new(BudgetBackend {
                reports_cost: adapter_reports_cost,
                calls: factory_calls.clone(),
            })
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut config = HarnessConfig::default();
    config.server.project_root = root.path().to_path_buf();
    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let thread_id = server
        .thread_manager
        .start_thread(root.path().to_path_buf());
    let turn_id = server.thread_manager.start_turn(
        &thread_id,
        "bounded test".to_string(),
        AgentId::from_str("budget-test"),
    )?;
    let suffix = uuid::Uuid::new_v4();
    let context = RuntimeUsageContext {
        store,
        runtime_job_id: format!("ordinary-budget-job-{suffix}"),
        command_id: format!("ordinary-budget-command-{suffix}"),
        workflow_id: format!("ordinary-budget-workflow-{suffix}"),
        agent_run_id: None,
        runtime_kind: if force_code_agent {
            RuntimeKind::CodexExec
        } else {
            RuntimeKind::CodexJsonrpc
        },
        runtime_profile: "budget-test".to_string(),
        agent: "budget-test".to_string(),
        model: "scripted".to_string(),
        project: root.path().display().to_string(),
        task_id: None,
        candidate_group_id: None,
        candidate_id: None,
        candidate_index: None,
        candidate_count: None,
        budget_policy: policy,
    };
    let (notification_tx, _) = tokio::sync::broadcast::channel(16);
    run_turn_lifecycle_with_options(
        server.clone(),
        None,
        notification_tx,
        thread_id.clone(),
        turn_id.clone(),
        "bounded test".to_string(),
        "budget-test".to_string(),
        TurnLifecycleOptions {
            force_code_agent,
            runtime_usage: Some(context),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        agent_calls.load(Ordering::SeqCst),
        usize::from(should_launch && force_code_agent)
    );
    assert_eq!(
        adapter_calls.load(Ordering::SeqCst),
        usize::from(should_launch && !force_code_agent)
    );
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .unwrap();
    assert_eq!(
        turn.status,
        if should_launch {
            TurnStatus::Completed
        } else {
            TurnStatus::Failed
        }
    );
    if !should_launch {
        assert!(turn.items.iter().any(|item| matches!(
            item,
            Item::Error { message, .. } if message.contains("does not report USD cost")
        )));
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_budget_enforce_rejects_cost_blind_execution_backend() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    for force_code_agent in [true, false] {
        ordinary_budget_case(
            RuntimeBudgetPolicy {
                enforcement: RuntimeBudgetEnforcement::Enforce,
                ..Default::default()
            },
            force_code_agent,
            !force_code_agent,
            force_code_agent,
            false,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_budget_enforce_uses_selected_surface_cost_capability() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    for force_code_agent in [true, false] {
        ordinary_budget_case(
            RuntimeBudgetPolicy {
                enforcement: RuntimeBudgetEnforcement::Enforce,
                ..Default::default()
            },
            force_code_agent,
            force_code_agent,
            !force_code_agent,
            true,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_budget_shadow_and_unlimited_allow_cost_blind_execution() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    for policy in [
        RuntimeBudgetPolicy::default(),
        RuntimeBudgetPolicy {
            enforcement: RuntimeBudgetEnforcement::Enforce,
            unlimited: true,
            ..Default::default()
        },
    ] {
        for force_code_agent in [true, false] {
            ordinary_budget_case(policy.clone(), force_code_agent, false, false, true).await?;
        }
    }
    Ok(())
}
