use super::*;
use crate::runtime::model::WorkflowSubject;
use harness_core::db::resolve_database_url;

async fn test_store() -> anyhow::Result<Option<WorkflowRuntimeStore>> {
    if resolve_database_url(None).is_err() {
        return Ok(None);
    }
    let dir = tempfile::tempdir()?;
    Ok(Some(
        WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?,
    ))
}

fn discovered_instance(id: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        "discovered",
        WorkflowSubject::new("issue", "issue:1784"),
    )
    .with_id(id)
}

#[tokio::test]
async fn public_upsert_rejects_unrecorded_state_change() -> anyhow::Result<()> {
    let Some(store) = test_store().await? else {
        return Ok(());
    };
    let initial = discovered_instance("public-upsert-state-change");
    store.upsert_instance(&initial).await?;

    let mut target = initial.clone();
    target.state = "implementing".to_string();
    target.version = 1;
    let error = store
        .upsert_instance(&target)
        .await
        .expect_err("state change must require a matching decision");

    assert!(error
        .to_string()
        .contains("public workflow instance upsert cannot change protected fields: state"));
    assert_eq!(
        store
            .get_instance(&initial.id)
            .await?
            .expect("workflow")
            .state,
        "discovered"
    );
    Ok(())
}

#[tokio::test]
async fn public_upsert_rejects_unknown_initial_state() -> anyhow::Result<()> {
    let Some(store) = test_store().await? else {
        return Ok(());
    };
    let invalid = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "invented_state",
        WorkflowSubject::new("issue", "issue:1784"),
    )
    .with_id("public-upsert-invalid-initial-state");

    let error = store
        .upsert_instance(&invalid)
        .await
        .expect_err("an unknown state must not be inserted through the public boundary");

    assert!(error.to_string().contains("invented_state"));
    assert!(store.get_instance(&invalid.id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn public_upsert_rejects_registered_non_initial_state() -> anyhow::Result<()> {
    let Some(store) = test_store().await? else {
        return Ok(());
    };
    let invalid = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "done",
        WorkflowSubject::new("issue", "issue:1784"),
    )
    .with_id("public-upsert-registered-non-initial-state");

    let error = store
        .upsert_instance(&invalid)
        .await
        .expect_err("a registered non-initial state must not cross the public insert boundary");

    assert!(error.to_string().contains("canonical initial state"));
    assert!(store.get_instance(&invalid.id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn public_upsert_rejects_nonzero_initial_version() -> anyhow::Result<()> {
    let Some(store) = test_store().await? else {
        return Ok(());
    };
    let mut invalid = discovered_instance("public-upsert-nonzero-initial-version");
    invalid.version = 1;

    let error = store
        .upsert_instance(&invalid)
        .await
        .expect_err("an initial instance must start at version zero");

    assert!(error.to_string().contains("must start at version 0"));
    assert!(store.get_instance(&invalid.id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn public_upsert_rejects_same_version_data_overwrite() -> anyhow::Result<()> {
    let Some(store) = test_store().await? else {
        return Ok(());
    };
    let initial = discovered_instance("public-upsert-same-version-data")
        .with_server_data(serde_json::json!({"generation": 1}));
    store.upsert_instance(&initial).await?;

    let target = initial
        .clone()
        .with_server_data(serde_json::json!({"generation": 2}));
    let error = store
        .upsert_instance(&target)
        .await
        .expect_err("same-version data replacement must not overwrite concurrent state");

    assert!(error.to_string().contains("same version"));
    assert_eq!(
        store
            .get_instance(&initial.id)
            .await?
            .expect("workflow")
            .data["generation"],
        1
    );
    Ok(())
}

#[tokio::test]
async fn public_upsert_rejects_version_jump() -> anyhow::Result<()> {
    let Some(store) = test_store().await? else {
        return Ok(());
    };
    let initial = discovered_instance("public-upsert-version-jump");
    store.upsert_instance(&initial).await?;

    let mut target = initial.clone();
    target.version = 2;
    let error = store
        .upsert_instance(&target)
        .await
        .expect_err("public upsert must not skip workflow versions");

    assert!(error.to_string().contains("version"));
    assert_eq!(
        store
            .get_instance(&initial.id)
            .await?
            .expect("workflow")
            .version,
        0
    );
    Ok(())
}

/// No workflow row may appear with no record of how it came to exist
/// (GH-1864). Decision-driven creation carries its own event and decision;
/// creation through the public insert APIs records a `WorkflowInstanceCreated`
/// event in the same transaction.
#[tokio::test]
async fn public_creation_records_an_initial_event_atomically() -> anyhow::Result<()> {
    let Some(store) = test_store().await? else {
        return Ok(());
    };

    let instance = discovered_instance("gh1864-eventless-creation");
    assert!(store.insert_instance_if_absent(&instance).await?);

    let events = store.events_for(&instance.id).await?;
    assert_eq!(
        events.len(),
        1,
        "creation must record exactly one provenance event"
    );
    assert_eq!(events[0].event_type, WORKFLOW_INSTANCE_CREATED_EVENT);
    assert_eq!(events[0].event["definition_id"], instance.definition_id);
    assert_eq!(events[0].event["state"], instance.state);

    // A losing concurrent insert must not append a second creation event.
    assert!(!store.insert_instance_if_absent(&instance).await?);
    assert_eq!(store.events_for(&instance.id).await?.len(), 1);

    // The other public creation entry point is held to the same rule.
    let upserted = discovered_instance("gh1864-eventless-creation-upsert");
    store.upsert_instance(&upserted).await?;
    let events = store.events_for(&upserted.id).await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, WORKFLOW_INSTANCE_CREATED_EVENT);
    Ok(())
}
