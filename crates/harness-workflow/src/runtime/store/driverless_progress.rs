use super::{instance_helpers::progress_state_selector_rows, WorkflowRuntimeStore};
use crate::runtime::{WorkflowProgressMode, GITHUB_ISSUE_PR_DEFINITION_ID};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverlessProgressProvenanceStatus {
    Established,
    MissingStateEntryProvenance,
    AmbiguousStateEntryProvenance,
}

impl DriverlessProgressProvenanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Established => "established",
            Self::MissingStateEntryProvenance => "missing_state_entry_provenance",
            Self::AmbiguousStateEntryProvenance => "ambiguous_state_entry_provenance",
        }
    }
}

impl TryFrom<&str> for DriverlessProgressProvenanceStatus {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "established" => Ok(Self::Established),
            "missing_state_entry_provenance" => Ok(Self::MissingStateEntryProvenance),
            "ambiguous_state_entry_provenance" => Ok(Self::AmbiguousStateEntryProvenance),
            other => anyhow::bail!("unknown driverless progress provenance status: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverlessProgressInstance {
    pub workflow_id: String,
    pub definition_id: String,
    pub state: String,
    pub age_secs: u64,
    pub provenance_status: DriverlessProgressProvenanceStatus,
}

impl WorkflowRuntimeStore {
    pub async fn list_driverless_progress_instances(
        &self,
        limit: i64,
    ) -> anyhow::Result<Vec<DriverlessProgressInstance>> {
        let mut selectors = progress_state_selector_rows(
            &self.definition_registry,
            WorkflowProgressMode::CommandDriven,
        );
        for definition in self.list_persisted_declarative_definitions().await? {
            for state in
                definition.registered().states.iter().filter(|state| {
                    state.progress_mode == Some(WorkflowProgressMode::CommandDriven)
                })
            {
                let unpinned_legacy_builtin = definition.registered().id
                    == GITHUB_ISSUE_PR_DEFINITION_ID
                    && definition.definition_version() == 1;
                selectors.insert(
                    definition.registered().id.clone(),
                    Some(i64::from(definition.definition_version())),
                    (!unpinned_legacy_builtin).then(|| definition.definition_hash().to_string()),
                    state.key.state.to_string(),
                );
            }
        }
        if selectors.definition_ids.is_empty() {
            return Ok(Vec::new());
        }

        let limit = limit.clamp(1, 500);
        let rows: Vec<(String, String, String, i64, String)> = sqlx::query_as(
            "WITH command_driven_states(
                     definition_id, definition_version, definition_hash, state
                 ) AS (
                 SELECT * FROM unnest($1::text[], $2::bigint[], $3::text[], $4::text[])
             ),
             candidates AS (
                 SELECT instance.id,
                        instance.definition_id,
                        instance.state,
                        instance.updated_at
                 FROM workflow_instances AS instance
                 JOIN command_driven_states AS registered
                   ON registered.definition_id = instance.definition_id
                  AND registered.state = instance.state
                  AND (
                      (registered.definition_version IS NULL
                       AND registered.definition_hash IS NULL)
                      OR (
                          registered.definition_version =
                              (instance.data->>'definition_version')::bigint
                          AND registered.definition_hash IS NULL
                      )
                      OR (
                          registered.definition_version =
                              (instance.data->>'definition_version')::bigint
                          AND registered.definition_hash =
                              instance.data->'data'->>'definition_hash'
                      )
                  )
             ),
             accepted_with_sequence AS (
                 SELECT decision.id AS decision_id,
                        decision.workflow_id,
                        event.sequence,
                        decision.data->'decision'->>'next_state' AS next_state,
                        dense_rank() OVER (
                            PARTITION BY decision.workflow_id
                            ORDER BY event.sequence DESC
                        ) AS sequence_rank
                 FROM workflow_decisions AS decision
                 JOIN workflow_events AS event
                   ON event.id = decision.event_id
                  AND event.workflow_id = decision.workflow_id
                 JOIN candidates AS candidate
                   ON candidate.id = decision.workflow_id
                 WHERE decision.accepted = TRUE
             ),
             accepted_without_sequence AS (
                 SELECT decision.workflow_id, COUNT(*) AS count
                 FROM workflow_decisions AS decision
                 JOIN candidates AS candidate
                   ON candidate.id = decision.workflow_id
                 LEFT JOIN workflow_events AS event
                   ON event.id = decision.event_id
                  AND event.workflow_id = decision.workflow_id
                 WHERE decision.accepted = TRUE
                   AND event.id IS NULL
                 GROUP BY decision.workflow_id
             ),
             newest AS (
                 SELECT workflow_id,
                        COUNT(*) AS decision_count,
                        CASE WHEN COUNT(*) = 1
                            THEN (ARRAY_AGG(decision_id))[1]
                        END AS decision_id,
                        CASE WHEN COUNT(*) = 1
                            THEN (ARRAY_AGG(next_state))[1]
                        END AS next_state
                 FROM accepted_with_sequence
                 WHERE sequence_rank = 1
                 GROUP BY workflow_id
             ),
             classified AS (
                 SELECT candidate.*,
                        newest.decision_id,
                        CASE
                            WHEN COALESCE(unsequenced.count, 0) > 0
                              OR COALESCE(newest.decision_count, 0) > 1
                                THEN 'ambiguous_state_entry_provenance'
                            WHEN newest.decision_id IS NULL
                              OR newest.next_state IS DISTINCT FROM candidate.state
                                THEN 'missing_state_entry_provenance'
                            ELSE 'established'
                        END AS provenance_status
                 FROM candidates AS candidate
                 LEFT JOIN newest ON newest.workflow_id = candidate.id
                 LEFT JOIN accepted_without_sequence AS unsequenced
                   ON unsequenced.workflow_id = candidate.id
             )
             SELECT classified.id,
                    classified.definition_id,
                    classified.state,
                    GREATEST(
                        0,
                        FLOOR(EXTRACT(EPOCH FROM (CURRENT_TIMESTAMP - classified.updated_at)))
                    )::bigint AS age_secs,
                    classified.provenance_status
             FROM classified
             WHERE classified.provenance_status <> 'established'
                OR NOT EXISTS (
                    SELECT 1
                    FROM workflow_commands AS command
                    WHERE command.workflow_id = classified.id
                      AND command.decision_id = classified.decision_id
                      AND command.command_type IN ('enqueue_activity', 'start_child_workflow')
                      AND (
                          command.status IN ('pending', 'dispatching', 'deferred', 'dispatched')
                          OR EXISTS (
                              SELECT 1
                              FROM runtime_jobs AS job
                              WHERE job.command_id = command.id
                                AND job.status IN ('pending', 'running')
                          )
                      )
                )
             ORDER BY classified.updated_at ASC, classified.id ASC
             LIMIT $5",
        )
        .bind(&selectors.definition_ids)
        .bind(&selectors.definition_versions)
        .bind(&selectors.definition_hashes)
        .bind(&selectors.states)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(
                |(workflow_id, definition_id, state, age_secs, provenance_status)| {
                    Ok(DriverlessProgressInstance {
                        workflow_id,
                        definition_id,
                        state,
                        age_secs: u64::try_from(age_secs).map_err(|_| {
                            anyhow::anyhow!("driverless progress age must be non-negative")
                        })?,
                        provenance_status: provenance_status.as_str().try_into()?,
                    })
                },
            )
            .collect()
    }
}
