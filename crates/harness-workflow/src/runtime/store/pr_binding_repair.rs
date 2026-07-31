use super::{commit_same_state_instance_tx, select_instance_for_update_tx, WorkflowRuntimeStore};
use crate::runtime::{WorkflowCommand, WorkflowCommandType, WorkflowInstance};
use anyhow::Context;
use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowPrBindingRepairOutcome {
    Repaired {
        instance: Box<WorkflowInstance>,
        command_id: String,
        pr_number: u64,
        pr_url: String,
    },
    NoBindPrCommand,
    StaleInstance,
}

impl WorkflowRuntimeStore {
    pub async fn repair_pr_binding_from_latest_command(
        &self,
        workflow_id: &str,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowPrBindingRepairOutcome> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.rollback().await?;
            return Ok(WorkflowPrBindingRepairOutcome::StaleInstance);
        };
        if current.version != expected_version {
            tx.rollback().await?;
            return Ok(WorkflowPrBindingRepairOutcome::StaleInstance);
        }

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data::text
             FROM workflow_commands
             WHERE workflow_id = $1
             ORDER BY created_at DESC, id DESC
             FOR UPDATE",
        )
        .bind(workflow_id)
        .fetch_all(&mut *tx)
        .await?;
        let bind_pr = rows
            .into_iter()
            .map(|(id, data)| Ok((id, serde_json::from_str::<WorkflowCommand>(&data)?)))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .find(|(_, command)| command.command_type == WorkflowCommandType::BindPr);
        let Some((command_id, command)) = bind_pr else {
            tx.rollback().await?;
            return Ok(WorkflowPrBindingRepairOutcome::NoBindPrCommand);
        };
        let pr_number = command
            .command
            .get("pr_number")
            .and_then(serde_json::Value::as_u64)
            .context("bind_pr command is missing pr_number")?;
        let pr_url = command
            .command
            .get("pr_url")
            .and_then(serde_json::Value::as_str)
            .context("bind_pr command is missing pr_url")?
            .to_string();

        let mut target = current.clone();
        if !target.data.is_object() {
            target.data = json!({});
        }
        let data = target
            .data
            .as_object_mut()
            .context("workflow runtime instance data is not an object")?;
        data.insert("pr_number".to_string(), json!(pr_number));
        data.insert("pr_url".to_string(), json!(pr_url));
        target.version = target.version.saturating_add(1);
        commit_same_state_instance_tx(&mut tx, &current, &target).await?;
        tx.commit().await?;
        Ok(WorkflowPrBindingRepairOutcome::Repaired {
            instance: Box::new(target),
            command_id,
            pr_number,
            pr_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{WorkflowCommand, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};
    use harness_core::db::resolve_database_url;

    fn workflow(id: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "awaiting_feedback",
            WorkflowSubject::new("issue", "issue:1784"),
        )
        .with_id(id)
    }

    #[tokio::test]
    async fn pr_binding_repair_requires_command_evidence_and_exact_version() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?;
        let instance = workflow("pr-binding-repair");
        store.force_upsert_instance_for_test(&instance).await?;

        assert_eq!(
            store
                .repair_pr_binding_from_latest_command(&instance.id, 0)
                .await?,
            WorkflowPrBindingRepairOutcome::NoBindPrCommand
        );

        let bind_pr = WorkflowCommand::bind_pr(
            1845,
            "https://github.com/majiayu000/harness/pull/1845",
            "bind-pr-1845",
        );
        let command_id = store.enqueue_command(&instance.id, None, &bind_pr).await?;
        let repaired = store
            .repair_pr_binding_from_latest_command(&instance.id, 0)
            .await?;
        let WorkflowPrBindingRepairOutcome::Repaired {
            instance: repaired,
            command_id: repaired_command_id,
            pr_number,
            pr_url,
        } = repaired
        else {
            anyhow::bail!("bind_pr evidence should repair the missing binding");
        };
        assert_eq!(repaired_command_id, command_id);
        assert_eq!(pr_number, 1845);
        assert_eq!(pr_url, "https://github.com/majiayu000/harness/pull/1845");
        assert_eq!(repaired.version, 1);
        assert_eq!(repaired.data["pr_number"], 1845);

        assert_eq!(
            store
                .repair_pr_binding_from_latest_command(&instance.id, 0)
                .await?,
            WorkflowPrBindingRepairOutcome::StaleInstance
        );
        Ok(())
    }
}
