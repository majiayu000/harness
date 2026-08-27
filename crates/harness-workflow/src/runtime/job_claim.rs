use super::store::runtime_job_leases::{
    append_runtime_event_tx, delete_all_runtime_job_lease_receipts_tx,
    mark_remote_lease_proof_v1_tx, postgres_timestamp_floor,
};
use super::store::{
    enum_str, fence_terminal_transition_tx, terminal_state_for_instance_tx, to_jsonb_string,
};
use super::{
    RuntimeJob, RuntimeKind, WorkflowDefinitionRegistry, WorkflowInstance, WorkflowRuntimeStore,
};
use chrono::{DateTime, Utc};

impl WorkflowRuntimeStore {
    pub async fn claim_next_runtime_job_for_runtime_kind(
        &self,
        runtime_kind: RuntimeKind,
        owner: &str,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        self.claim_next_runtime_job_matching(
            Some(runtime_kind),
            None,
            owner,
            expires_at,
            true,
            true,
        )
        .await
    }

    pub async fn claim_next_remote_host_runtime_job(
        &self,
        owner: &str,
        expires_at: DateTime<Utc>,
        supports_eval_resource_limits: bool,
        supports_trusted_eval_verifier: bool,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        self.claim_next_runtime_job_matching(
            Some(RuntimeKind::RemoteHost),
            None,
            owner,
            expires_at,
            supports_eval_resource_limits,
            supports_trusted_eval_verifier,
        )
        .await
    }

    pub async fn claim_next_runtime_job_excluding_runtime_kind(
        &self,
        runtime_kind: RuntimeKind,
        owner: &str,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        if runtime_kind == RuntimeKind::RemoteHost {
            self.reroute_legacy_remote_server_owned_jobs().await?;
        }
        self.claim_next_runtime_job_matching(
            None,
            Some(runtime_kind),
            owner,
            expires_at,
            true,
            true,
        )
        .await
    }

    async fn reroute_legacy_remote_server_owned_jobs(&self) -> anyhow::Result<usize> {
        let mut inspected_ids = Vec::<String>::new();
        let mut rerouted = 0_usize;
        loop {
            let mut tx = self.pool.begin().await?;
            let candidate: Option<(String, String)> = sqlx::query_as(
                "SELECT job.id, workflow.data::text
                 FROM runtime_jobs AS job
                 JOIN workflow_commands AS command ON command.id = job.command_id
                 JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
                 WHERE job.runtime_kind = 'remote_host'
                   AND job.status IN ('pending', 'running')
                   AND command.status = 'dispatched'
                   AND command.superseded_by_command_id IS NULL
                   AND NOT (job.id = ANY($1::text[]))
                 ORDER BY job.created_at ASC
                 LIMIT 1
                 FOR UPDATE OF workflow SKIP LOCKED",
            )
            .bind(&inspected_ids)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((id, workflow_data)) = candidate else {
                tx.commit().await?;
                return Ok(rerouted);
            };
            let workflow = super::store::workflow_instance_from_persisted_json(&workflow_data)?;
            let row: Option<(String,)> = sqlx::query_as(
                "SELECT job.data::text
                 FROM runtime_jobs AS job
                 JOIN workflow_commands AS command ON command.id = job.command_id
                 WHERE job.id = $1
                   AND job.runtime_kind = 'remote_host'
                   AND job.status IN ('pending', 'running')
                   AND command.status = 'dispatched'
                   AND command.superseded_by_command_id IS NULL
                 FOR UPDATE OF command, job",
            )
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((data,)) = row else {
                tx.commit().await?;
                continue;
            };
            let mut job: RuntimeJob = serde_json::from_str(&data)?;
            let kind = server_owned_job_kind(&self.definition_registry, &workflow, &job).map_err(
                |error| {
                    anyhow::anyhow!(
                        "workflow {} has an invalid declarative definition pin: {error:?}",
                        workflow.id
                    )
                },
            )?;
            let Some(kind) = kind else {
                tx.commit().await?;
                inspected_ids.push(id);
                continue;
            };
            reroute_legacy_remote_server_owned_job(&mut tx, &mut job, kind).await?;
            tx.commit().await?;
            rerouted += 1;
        }
    }

    pub(in crate::runtime) async fn claim_next_runtime_job_matching(
        &self,
        only_runtime_kind: Option<RuntimeKind>,
        excluded_runtime_kind: Option<RuntimeKind>,
        owner: &str,
        expires_at: DateTime<Utc>,
        supports_eval_resource_limits: bool,
        supports_trusted_eval_verifier: bool,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let records_remote_host_audit = only_runtime_kind == Some(RuntimeKind::RemoteHost);
        let only_runtime_kind = only_runtime_kind
            .map(|runtime_kind| enum_str(&runtime_kind))
            .transpose()?;
        let excluded_runtime_kind = excluded_runtime_kind
            .map(|runtime_kind| enum_str(&runtime_kind))
            .transpose()?;
        loop {
            let mut tx = self.pool.begin().await?;
            // Claimers participate in the terminal-transition lock order by
            // locking the workflow before its runtime job. Locking only the
            // job would let a pre-fence legacy row cross terminalization.
            let candidate: Option<(String, String)> = sqlx::query_as(
                "SELECT job.id, workflow.data::text
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE (
                 (
                     job.status = 'pending'
                     AND (job.not_before IS NULL OR job.not_before <= CURRENT_TIMESTAMP)
                 ) OR (
                     job.status = 'running'
                     AND job.data ? 'lease'
                     AND (job.data->'lease' ? 'expires_at')
                     AND (job.data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
                 )
             )
             AND job.data #> '{input,cancellation_requested}' IS NULL
             AND command.status = 'dispatched'
             AND command.superseded_by_command_id IS NULL
             AND ($1::text IS NULL OR job.runtime_kind = $1)
             AND ($2::text IS NULL OR job.runtime_kind <> $2)
             AND (
                 $3::boolean
                 OR (
                     job.data #> '{input,eval}' IS NULL
                     AND job.data #> '{input,command,eval}' IS NULL
                 )
             )
             AND (
                 $4::boolean
                 OR NOT (
                     COALESCE(
                         (job.data #> '{input,eval,required_runtime_host_capabilities}')
                             ? 'trusted_eval_verifier_v1',
                         false
                     )
                     OR COALESCE(
                         (job.data #> '{input,command,eval,required_runtime_host_capabilities}')
                             ? 'trusted_eval_verifier_v1',
                         false
                     )
                 )
             )
             ORDER BY
                 CASE
                     WHEN COALESCE(job.data #>> '{input,activity}', '') IN (
                         'implement_issue',
                         'implement_prompt',
                         'inspect_pr_feedback',
                         'address_pr_feedback'
                     ) THEN 0
                     ELSE 1
                 END ASC,
                 job.created_at ASC
             LIMIT 1
             FOR UPDATE OF workflow SKIP LOCKED",
            )
            .bind(only_runtime_kind.as_deref())
            .bind(excluded_runtime_kind.as_deref())
            .bind(supports_eval_resource_limits)
            .bind(supports_trusted_eval_verifier)
            .fetch_optional(&mut *tx)
            .await?;

            let Some((id, workflow_data)) = candidate else {
                tx.commit().await?;
                return Ok(None);
            };
            let workflow = super::store::workflow_instance_from_persisted_json(&workflow_data)?;
            if terminal_state_for_instance_tx(&mut tx, &self.definition_registry, &workflow)
                .await?
                .is_some()
            {
                // Old databases can contain unfinished jobs created before
                // terminal admission was enforced. Repair the invariant and
                // continue looking for work in a fresh transaction so locks
                // never span multiple workflows.
                fence_terminal_transition_tx(&mut tx, &self.definition_registry, &workflow).await?;
                tx.commit().await?;
                continue;
            }

            // The workflow lock serializes all current claim paths. Recheck
            // eligibility while taking the child-row lock because another
            // legacy writer could still have changed the candidate.
            let row: Option<(String,)> = sqlx::query_as(
                "SELECT job.data::text FROM runtime_jobs AS job
                 JOIN workflow_commands AS command ON command.id = job.command_id
                 WHERE job.id = $1
                   AND command.status = 'dispatched'
                   AND command.superseded_by_command_id IS NULL
                   AND (
                       (
                           job.status = 'pending'
                           AND (job.not_before IS NULL OR job.not_before <= CURRENT_TIMESTAMP)
                       ) OR (
                           job.status = 'running'
                           AND job.data ? 'lease'
                           AND (job.data->'lease' ? 'expires_at')
                           AND (job.data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
                       )
                   )
                   AND job.data #> '{input,cancellation_requested}' IS NULL
                 FOR UPDATE OF command, job",
            )
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((data,)) = row else {
                tx.commit().await?;
                continue;
            };

            let mut job: RuntimeJob = serde_json::from_str(&data)?;
            if records_remote_host_audit {
                if let Some(server_owned_kind) =
                    server_owned_job_kind(&self.definition_registry, &workflow, &job).map_err(
                        |error| {
                            anyhow::anyhow!(
                                "workflow {} has an invalid declarative definition pin: {error:?}",
                                workflow.id
                            )
                        },
                    )?
                {
                    reroute_legacy_remote_server_owned_job(&mut tx, &mut job, server_owned_kind)
                        .await?;
                    tx.commit().await?;
                    continue;
                }
            }
            let expires_at = if records_remote_host_audit {
                postgres_timestamp_floor(expires_at)
            } else {
                expires_at
            };
            let reclaimed = job.status == super::RuntimeJobStatus::Running;
            job.claim(owner, expires_at);
            let updated = to_jsonb_string(&job)?;
            let status = enum_str(&job.status)?;
            if records_remote_host_audit {
                mark_remote_lease_proof_v1_tx(&mut tx).await?;
            }
            sqlx::query(
                "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
            )
            .bind(&status)
            .bind(job.not_before)
            .bind(&updated)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            if records_remote_host_audit {
                delete_all_runtime_job_lease_receipts_tx(&mut tx, &id).await?;
                append_runtime_event_tx(
                    &mut tx,
                    &id,
                    if reclaimed {
                        "RuntimeJobReclaimed"
                    } else {
                        "RuntimeJobClaimed"
                    },
                    serde_json::json!({
                        "owner": owner,
                        "lease_generation": job.lease_generation,
                        "lease_expires_at": expires_at,
                        "claim_api": "runtime_host",
                    }),
                )
                .await?;
            }
            tx.commit().await?;
            return Ok(Some(job));
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ServerOwnedJobKind {
    Merge,
    Classifier,
}

fn server_owned_job_kind(
    registry: &WorkflowDefinitionRegistry,
    workflow: &WorkflowInstance,
    job: &RuntimeJob,
) -> Result<Option<ServerOwnedJobKind>, super::DeclarativeDefinitionPinError> {
    let Some(activity) = job.input.get("activity").and_then(|value| value.as_str()) else {
        return Ok(None);
    };
    if activity == "merge_pr" && super::scope_review::workflow_uses_server_merge(workflow) {
        return Ok(Some(ServerOwnedJobKind::Merge));
    }
    Ok(
        super::scope_review::runtime_job_requires_local_server(registry, workflow, job)?
            .then_some(ServerOwnedJobKind::Classifier),
    )
}

async fn reroute_legacy_remote_server_owned_job(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    job: &mut RuntimeJob,
    kind: ServerOwnedJobKind,
) -> anyhow::Result<()> {
    let (profile, reason) = match kind {
        ServerOwnedJobKind::Merge => (
            "server-owned-merge",
            "legacy remote merge rerouted to the in-process server worker",
        ),
        ServerOwnedJobKind::Classifier => {
            let input = job
                .input
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("runtime job input must be an object"))?;
            input.insert(
                "server_owned_remote_rejection".to_string(),
                serde_json::Value::Bool(true),
            );
            (
                "server-owned-classifier-rejected",
                "legacy remote classifier rejected; retry with a local agent runtime",
            )
        }
    };
    let prior_lease_generation = job.lease_generation;
    job.runtime_kind = RuntimeKind::CodexExec;
    job.runtime_profile = profile.to_string();
    job.status = super::RuntimeJobStatus::Pending;
    job.lease = None;
    job.lease_generation = job.lease_generation.saturating_add(1);
    job.not_before = None;
    job.updated_at = Utc::now();
    let data = to_jsonb_string(job)?;
    sqlx::query(
        "UPDATE runtime_jobs
         SET runtime_kind = $1, runtime_profile = $2, status = 'pending',
             not_before = NULL, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
         WHERE id = $4",
    )
    .bind(RuntimeKind::CodexExec.as_str())
    .bind(profile)
    .bind(data)
    .bind(&job.id)
    .execute(&mut **tx)
    .await?;
    delete_all_runtime_job_lease_receipts_tx(tx, &job.id).await?;
    append_runtime_event_tx(
        tx,
        &job.id,
        "RuntimeJobReroutedToServer",
        serde_json::json!({
            "reason": reason,
            "prior_runtime_kind": "remote_host",
            "prior_lease_generation": prior_lease_generation,
            "lease_generation": job.lease_generation,
        }),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifier_identity_does_not_depend_on_mutable_workflow_state() {
        let registry = WorkflowDefinitionRegistry::with_builtins();
        let workflow = WorkflowInstance::new(
            super::super::GITHUB_ISSUE_PR_DEFINITION_ID,
            super::super::GITHUB_ISSUE_PR_DEFINITION_VERSION,
            "pr_open",
            super::super::WorkflowSubject::new("issue", "issue:77"),
        )
        .with_server_data(json!({
            "definition_hash": super::super::github_issue_pr_definition_hash()
        }));
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity": super::super::CHANGE_SCOPE_REVIEW_ACTIVITY}),
        );

        assert!(matches!(
            server_owned_job_kind(&registry, &workflow, &job),
            Ok(Some(ServerOwnedJobKind::Classifier))
        ));
    }

    #[test]
    fn merge_rerouting_honors_the_persisted_execution_mode() {
        let registry = WorkflowDefinitionRegistry::with_builtins();
        let job = RuntimeJob::pending(
            "command-merge",
            RuntimeKind::RemoteHost,
            "remote",
            json!({"activity": "merge_pr"}),
        );
        let workflow = |execution: &str| {
            WorkflowInstance::new(
                super::super::GITHUB_ISSUE_PR_DEFINITION_ID,
                super::super::GITHUB_ISSUE_PR_DEFINITION_VERSION,
                "merging",
                super::super::WorkflowSubject::new("issue", "issue:78"),
            )
            .with_server_data(json!({
                "definition_hash": super::super::github_issue_pr_definition_hash(),
                "merge_execution": execution,
            }))
        };

        assert!(matches!(
            server_owned_job_kind(&registry, &workflow("server"), &job),
            Ok(Some(ServerOwnedJobKind::Merge))
        ));
        assert!(matches!(
            server_owned_job_kind(&registry, &workflow("agent"), &job),
            Ok(None)
        ));
    }
}
