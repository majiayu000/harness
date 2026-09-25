use crate::runtime::{WorkflowInstance, WorkflowRuntimeStore};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub(super) enum MissingWorkflowFamilyMember {
    Reject,
    Skip,
}

pub(super) async fn workflow_family_instances(
    store: &WorkflowRuntimeStore,
    root_workflow_id: &str,
    missing_member: MissingWorkflowFamilyMember,
) -> anyhow::Result<Vec<WorkflowInstance>> {
    let mut instances = Vec::new();
    let mut pending = vec![(root_workflow_id.to_string(), None)];
    let mut visited = BTreeSet::new();
    while let Some((workflow_id, pending_instance)) = pending.pop() {
        if !visited.insert(workflow_id.clone()) {
            continue;
        }
        let instance = match pending_instance {
            Some(instance) => instance,
            None => {
                let Some(instance) = store.get_instance(&workflow_id).await? else {
                    match missing_member {
                        MissingWorkflowFamilyMember::Reject => {
                            anyhow::bail!("eval workflow family member disappeared: {workflow_id}");
                        }
                        MissingWorkflowFamilyMember::Skip => continue,
                    }
                };
                instance
            }
        };
        for child in store.list_instances_by_parent(&workflow_id, None).await? {
            pending.push((child.id.clone(), Some(child)));
        }
        instances.push(instance);
    }
    Ok(instances)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        WorkflowInstance, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
        QUALITY_GATE_DEFINITION_ID,
    };

    #[tokio::test]
    async fn missing_member_policy_is_explicit() -> anyhow::Result<()> {
        if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;

        let skipped =
            workflow_family_instances(&store, "missing-root", MissingWorkflowFamilyMember::Skip)
                .await?;
        assert!(skipped.is_empty());

        let rejected =
            workflow_family_instances(&store, "missing-root", MissingWorkflowFamilyMember::Reject)
                .await
                .expect_err("strict traversal must reject missing members");
        assert!(rejected
            .to_string()
            .contains("eval workflow family member disappeared: missing-root"));
        Ok(())
    }

    #[tokio::test]
    async fn cycles_terminate_deterministically() -> anyhow::Result<()> {
        if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let root = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:1"),
        )
        .with_id("family-cycle-root")
        .with_parent("family-cycle-child");
        let child = WorkflowInstance::new(
            QUALITY_GATE_DEFINITION_ID,
            1,
            "checking",
            WorkflowSubject::new("quality_gate", "issue:1"),
        )
        .with_id("family-cycle-child")
        .with_parent("family-cycle-root");
        store.force_upsert_lifecycle_state_for_test(&root).await?;
        store.force_upsert_lifecycle_state_for_test(&child).await?;

        let family =
            workflow_family_instances(&store, &root.id, MissingWorkflowFamilyMember::Reject)
                .await?;
        let ids = family
            .into_iter()
            .map(|instance| instance.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["family-cycle-root", "family-cycle-child"]);
        Ok(())
    }
}
