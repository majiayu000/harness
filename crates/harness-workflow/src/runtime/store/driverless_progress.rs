use super::WorkflowRuntimeStore;
use crate::runtime::state_registry::{
    known_workflow_definition_ids, workflow_states_for_definition, WorkflowProgressMode,
};

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
        let (definition_ids, states) = command_driven_state_pairs();
        if definition_ids.is_empty() {
            return Ok(Vec::new());
        }

        let limit = limit.clamp(1, 500);
        let rows: Vec<(String, String, String, i64, String)> = sqlx::query_as(
            "WITH command_driven_states(definition_id, state) AS (
                 SELECT * FROM unnest($1::text[], $2::text[])
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
             LIMIT $3",
        )
        .bind(&definition_ids)
        .bind(&states)
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

fn command_driven_state_pairs() -> (Vec<String>, Vec<String>) {
    let pairs: Vec<(String, String)> = known_workflow_definition_ids()
        .into_iter()
        .flat_map(|definition_id| {
            workflow_states_for_definition(&definition_id)
                .into_iter()
                .filter(|state| state.progress_mode == Some(WorkflowProgressMode::CommandDriven))
                .map(move |state| (definition_id.clone(), state.key.state.to_string()))
        })
        .collect();
    pairs.into_iter().unzip()
}
