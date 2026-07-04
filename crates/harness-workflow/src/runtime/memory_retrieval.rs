use super::repo_memory::{
    record_from_row, RepoMemoryKind, RepoMemoryOutcome, RepoMemoryRecord, RepoMemoryRecordRow,
};
use super::store::WorkflowRuntimeStore;
use serde_json::Value;

pub const DEFAULT_REPO_MEMORY_RETRIEVAL_LIMIT: usize = 5;
pub const DEFAULT_REPO_MEMORY_TOKEN_BUDGET: usize = 800;
const DEFAULT_REPO_MEMORY_CANDIDATE_LIMIT: i64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoMemoryRetrievalOptions {
    pub limit: usize,
    pub token_budget: usize,
}

impl Default for RepoMemoryRetrievalOptions {
    fn default() -> Self {
        Self {
            limit: DEFAULT_REPO_MEMORY_RETRIEVAL_LIMIT,
            token_budget: DEFAULT_REPO_MEMORY_TOKEN_BUDGET,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetrievedRepoMemoryRecord {
    pub record: RepoMemoryRecord,
    pub estimated_tokens: usize,
}

impl WorkflowRuntimeStore {
    pub async fn retrieve_repo_memory_records(
        &self,
        repo: &str,
        activity_class: &str,
        options: RepoMemoryRetrievalOptions,
    ) -> anyhow::Result<Vec<RetrievedRepoMemoryRecord>> {
        if repo.trim().is_empty() || options.limit == 0 || options.token_budget == 0 {
            return Ok(Vec::new());
        }
        let activity_class = activity_class.trim();
        let rows = sqlx::query_as::<_, RepoMemoryRecordRow>(
            "SELECT id, repo, activity_class, outcome, kind, payload_json::text,
                evidence_ref, created_at, updated_at, last_used_at, use_count
             FROM workflow_repo_memory
             WHERE repo = $1
             ORDER BY
                CASE WHEN activity_class = $2 THEN 0 ELSE 1 END ASC,
                created_at DESC,
                id ASC
             LIMIT $3",
        )
        .bind(repo)
        .bind(activity_class)
        .bind(DEFAULT_REPO_MEMORY_CANDIDATE_LIMIT)
        .fetch_all(self.pool())
        .await?;
        let candidates = rows
            .into_iter()
            .map(record_from_row)
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(select_repo_memory_records(
            candidates,
            activity_class,
            options,
        ))
    }
}

fn select_repo_memory_records(
    mut candidates: Vec<RepoMemoryRecord>,
    activity_class: &str,
    options: RepoMemoryRetrievalOptions,
) -> Vec<RetrievedRepoMemoryRecord> {
    if options.limit == 0 || options.token_budget == 0 {
        return Vec::new();
    }
    rank_repo_memory_candidates(&mut candidates, activity_class);
    let mut selected = select_with_budget(candidates.iter(), options.limit, options.token_budget);
    ensure_failure_lesson_mix(&mut selected, &candidates, options);
    selected
}

fn rank_repo_memory_candidates(candidates: &mut [RepoMemoryRecord], activity_class: &str) {
    let activity_class = activity_class.trim();
    candidates.sort_by(|left, right| {
        let left_activity_mismatch = left.activity_class != activity_class;
        let right_activity_mismatch = right.activity_class != activity_class;
        left_activity_mismatch
            .cmp(&right_activity_mismatch)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn select_with_budget<'a>(
    candidates: impl IntoIterator<Item = &'a RepoMemoryRecord>,
    limit: usize,
    token_budget: usize,
) -> Vec<RetrievedRepoMemoryRecord> {
    let mut selected = Vec::new();
    let mut used_tokens = 0usize;
    for record in candidates {
        if selected.len() >= limit {
            break;
        }
        let estimated_tokens = estimate_repo_memory_record_tokens(record);
        let Some(next_used_tokens) = used_tokens.checked_add(estimated_tokens) else {
            continue;
        };
        if next_used_tokens > token_budget {
            continue;
        }
        selected.push(RetrievedRepoMemoryRecord {
            record: record.clone(),
            estimated_tokens,
        });
        used_tokens = next_used_tokens;
    }
    selected
}

fn ensure_failure_lesson_mix(
    selected: &mut Vec<RetrievedRepoMemoryRecord>,
    ranked_candidates: &[RepoMemoryRecord],
    options: RepoMemoryRetrievalOptions,
) {
    if selected
        .iter()
        .any(|entry| is_failure_lesson(&entry.record))
    {
        return;
    }
    let Some(failure) = ranked_candidates
        .iter()
        .find(|record| is_failure_lesson(record))
    else {
        return;
    };
    let failure_tokens = estimate_repo_memory_record_tokens(failure);
    if failure_tokens > options.token_budget {
        return;
    }
    while (selected.len() >= options.limit
        || selected_token_count(selected).saturating_add(failure_tokens) > options.token_budget)
        && selected
            .iter()
            .any(|entry| !is_failure_lesson(&entry.record))
    {
        if let Some(index) = selected
            .iter()
            .rposition(|entry| !is_failure_lesson(&entry.record))
        {
            selected.remove(index);
        }
    }
    if selected.len() < options.limit
        && selected_token_count(selected).saturating_add(failure_tokens) <= options.token_budget
    {
        selected.push(RetrievedRepoMemoryRecord {
            record: failure.clone(),
            estimated_tokens: failure_tokens,
        });
        sort_selected_by_rank(selected, ranked_candidates);
    }
}

fn selected_token_count(selected: &[RetrievedRepoMemoryRecord]) -> usize {
    selected
        .iter()
        .map(|entry| entry.estimated_tokens)
        .sum::<usize>()
}

fn sort_selected_by_rank(
    selected: &mut [RetrievedRepoMemoryRecord],
    ranked_candidates: &[RepoMemoryRecord],
) {
    selected.sort_by_key(|entry| {
        ranked_candidates
            .iter()
            .position(|record| record.id == entry.record.id)
            .unwrap_or(usize::MAX)
    });
}

fn is_failure_lesson(record: &RepoMemoryRecord) -> bool {
    record.outcome == RepoMemoryOutcome::Failed && record.kind == RepoMemoryKind::FailureLesson
}

pub fn estimate_repo_memory_record_tokens(record: &RepoMemoryRecord) -> usize {
    estimate_tokens(&repo_memory_record_budget_text(record))
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

fn repo_memory_record_budget_text(record: &RepoMemoryRecord) -> String {
    format!(
        "repo={} activity={} outcome={} kind={} evidence={} payload={}",
        record.repo,
        record.activity_class,
        record.outcome.db_value(),
        record.kind.db_value(),
        record.evidence_ref.as_deref().unwrap_or(""),
        compact_payload(&record.payload_json)
    )
}

fn compact_payload(payload: &Value) -> String {
    serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use harness_core::db::resolve_database_url;
    use serde_json::json;
    use uuid::Uuid;

    fn memory_record(
        id_seed: u128,
        activity_class: &str,
        outcome: RepoMemoryOutcome,
        kind: RepoMemoryKind,
        age_minutes: i64,
        payload: Value,
    ) -> RepoMemoryRecord {
        let created_at = Utc::now() - Duration::minutes(age_minutes);
        let mut record =
            RepoMemoryRecord::new("owner/repo", activity_class, outcome, kind, payload)
                .with_evidence_ref(format!("workflow:run-{id_seed}:event:event-{id_seed}"));
        record.id = Uuid::from_u128(id_seed);
        record.created_at = created_at;
        record.updated_at = created_at;
        record
    }

    #[test]
    fn memory_retrieval_ranks_activity_match_then_recency() {
        let same_old = memory_record(
            1,
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            30,
            json!({"validation": [{"command": "cargo test", "status": "passed"}]}),
        );
        let other_new = memory_record(
            2,
            "inspect_pr_feedback",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ReviewerPattern,
            1,
            json!({"feedback_category": "ready_to_merge"}),
        );
        let same_new = memory_record(
            3,
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            5,
            json!({"validation": [{"command": "cargo clippy", "status": "passed"}]}),
        );

        let selected = select_repo_memory_records(
            vec![same_old.clone(), other_new.clone(), same_new.clone()],
            "implement_issue",
            RepoMemoryRetrievalOptions {
                limit: 5,
                token_budget: 10_000,
            },
        );

        let ids: Vec<Uuid> = selected.iter().map(|entry| entry.record.id).collect();
        assert_eq!(ids, vec![same_new.id, same_old.id, other_new.id]);
    }

    #[test]
    fn memory_retrieval_surfaces_failure_lesson_when_successes_dominate() {
        let mut records = (1..=6)
            .map(|index| {
                memory_record(
                    index,
                    "implement_issue",
                    RepoMemoryOutcome::Done,
                    RepoMemoryKind::ValidationCommand,
                    index as i64,
                    json!({"validation": [{"command": format!("cargo test {index}"), "status": "passed"}]}),
                )
            })
            .collect::<Vec<_>>();
        let failure = memory_record(
            20,
            "implement_issue",
            RepoMemoryOutcome::Failed,
            RepoMemoryKind::FailureLesson,
            60,
            json!({"failure_class": "timeout"}),
        );
        records.push(failure.clone());

        let selected = select_repo_memory_records(
            records,
            "implement_issue",
            RepoMemoryRetrievalOptions {
                limit: 5,
                token_budget: 10_000,
            },
        );

        assert_eq!(selected.len(), 5);
        assert!(selected
            .iter()
            .any(|entry| entry.record.id == failure.id && is_failure_lesson(&entry.record)));
    }

    #[test]
    fn memory_retrieval_enforces_limit_and_whole_record_token_budget() {
        let first = memory_record(
            1,
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            1,
            json!({"validation": [{"command": "cargo test", "status": "passed"}]}),
        );
        let oversized = memory_record(
            2,
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            2,
            json!({"validation": [{"command": "x".repeat(2_000), "status": "passed"}]}),
        );
        let third = memory_record(
            3,
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            3,
            json!({"validation": [{"command": "cargo fmt", "status": "passed"}]}),
        );
        let token_budget =
            estimate_repo_memory_record_tokens(&first) + estimate_repo_memory_record_tokens(&third);

        let selected = select_repo_memory_records(
            vec![first.clone(), oversized.clone(), third.clone()],
            "implement_issue",
            RepoMemoryRetrievalOptions {
                limit: 5,
                token_budget,
            },
        );

        let ids: Vec<Uuid> = selected.iter().map(|entry| entry.record.id).collect();
        assert_eq!(ids, vec![first.id, third.id]);
        assert!(!ids.contains(&oversized.id));
        assert!(selected_token_count(&selected) <= token_budget);
    }

    #[test]
    fn memory_retrieval_returns_empty_when_budget_cannot_fit_one_record() {
        let record = memory_record(
            1,
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            1,
            json!({"validation": [{"command": "cargo test", "status": "passed"}]}),
        );

        let selected = select_repo_memory_records(
            vec![record],
            "implement_issue",
            RepoMemoryRetrievalOptions {
                limit: 5,
                token_budget: 1,
            },
        );

        assert!(selected.is_empty());
    }

    async fn open_memory_retrieval_test_store() -> anyhow::Result<Option<WorkflowRuntimeStore>> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(None);
        };
        let schema = format!("memory_retrieval_{}", Uuid::new_v4().simple());
        let store =
            WorkflowRuntimeStore::open_with_database_url_and_schema(Some(&database_url), &schema)
                .await?;
        Ok(Some(store))
    }

    #[tokio::test]
    async fn memory_retrieval_store_query_applies_ranking_and_mix() -> anyhow::Result<()> {
        let Some(store) = open_memory_retrieval_test_store().await? else {
            return Ok(());
        };
        let success = memory_record(
            1,
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            1,
            json!({"validation": [{"command": "cargo test", "status": "passed"}]}),
        );
        let other_activity = memory_record(
            2,
            "inspect_pr_feedback",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ReviewerPattern,
            0,
            json!({"feedback_category": "ready_to_merge"}),
        );
        let failure = memory_record(
            3,
            "implement_issue",
            RepoMemoryOutcome::Failed,
            RepoMemoryKind::FailureLesson,
            90,
            json!({"failure_class": "configuration"}),
        );
        for record in [&success, &other_activity, &failure] {
            store.insert_repo_memory_record(record).await?;
        }

        let selected = store
            .retrieve_repo_memory_records(
                "owner/repo",
                "implement_issue",
                RepoMemoryRetrievalOptions {
                    limit: 2,
                    token_budget: 10_000,
                },
            )
            .await?;

        let ids: Vec<Uuid> = selected.iter().map(|entry| entry.record.id).collect();
        assert_eq!(ids, vec![success.id, failure.id]);
        Ok(())
    }
}
