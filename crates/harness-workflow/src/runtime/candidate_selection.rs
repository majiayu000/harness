use super::model::ActivityArtifact;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub const CANDIDATE_SELECTION_RECORD_TYPE: &str = "candidate_selection";
pub const CANDIDATE_SELECTION_SCHEMA: &str = "harness.runtime.candidate_selection.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOutcome {
    Succeeded,
    Failed,
    Stalled,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateCheckConclusion {
    Passed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDiffScope {
    Sane,
    TooLarge,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEvidence {
    pub candidate_id: String,
    pub runtime_job_id: Option<String>,
    pub branch: Option<String>,
    pub outcome: CandidateOutcome,
    pub validation: CandidateCheckConclusion,
    pub ci: CandidateCheckConclusion,
    pub quality_score: Option<i64>,
    pub diff_scope: CandidateDiffScope,
    pub files_changed: Option<u32>,
    pub lines_changed: Option<u32>,
    pub completed_at_epoch_ms: Option<i64>,
}

impl CandidateEvidence {
    pub fn succeeded(candidate_id: impl Into<String>) -> Self {
        Self {
            candidate_id: candidate_id.into(),
            runtime_job_id: None,
            branch: None,
            outcome: CandidateOutcome::Succeeded,
            validation: CandidateCheckConclusion::Unknown,
            ci: CandidateCheckConclusion::Unknown,
            quality_score: None,
            diff_scope: CandidateDiffScope::Unknown,
            files_changed: None,
            lines_changed: None,
            completed_at_epoch_ms: None,
        }
    }

    pub fn failed(candidate_id: impl Into<String>) -> Self {
        Self {
            outcome: CandidateOutcome::Failed,
            ..Self::succeeded(candidate_id)
        }
    }

    pub fn with_validation(mut self, validation: CandidateCheckConclusion) -> Self {
        self.validation = validation;
        self
    }

    pub fn with_ci(mut self, ci: CandidateCheckConclusion) -> Self {
        self.ci = ci;
        self
    }

    pub fn with_quality_score(mut self, quality_score: i64) -> Self {
        self.quality_score = Some(quality_score);
        self
    }

    pub fn with_diff(
        mut self,
        diff_scope: CandidateDiffScope,
        files_changed: u32,
        lines_changed: u32,
    ) -> Self {
        self.diff_scope = diff_scope;
        self.files_changed = Some(files_changed);
        self.lines_changed = Some(lines_changed);
        self
    }

    pub fn with_completed_at_epoch_ms(mut self, completed_at_epoch_ms: i64) -> Self {
        self.completed_at_epoch_ms = Some(completed_at_epoch_ms);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePromotionRecord {
    pub from_candidate_id: String,
    pub to_candidate_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateSelectionInput {
    pub candidate_group_id: String,
    pub candidates: Vec<CandidateEvidence>,
    #[serde(default)]
    pub promotion_chain: Vec<CandidatePromotionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateRankingRecord {
    pub rank: usize,
    pub candidate_id: String,
    pub selected: bool,
    pub outcome: CandidateOutcome,
    pub validation: CandidateCheckConclusion,
    pub ci: CandidateCheckConclusion,
    pub quality_score: Option<i64>,
    pub diff_scope: CandidateDiffScope,
    pub files_changed: Option<u32>,
    pub lines_changed: Option<u32>,
    pub completed_at_epoch_ms: Option<i64>,
    pub rank_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateSelectionRecord {
    pub schema: String,
    pub record_type: String,
    pub candidate_group_id: String,
    pub selected_candidate_id: Option<String>,
    pub ranking: Vec<CandidateRankingRecord>,
    pub promotion_chain: Vec<CandidatePromotionRecord>,
}

impl CandidateSelectionRecord {
    pub fn to_activity_artifact(&self) -> Result<ActivityArtifact, serde_json::Error> {
        Ok(ActivityArtifact::new(
            CANDIDATE_SELECTION_RECORD_TYPE,
            serde_json::to_value(self)?,
        ))
    }
}

pub fn select_candidate(input: CandidateSelectionInput) -> CandidateSelectionRecord {
    let mut candidates = input.candidates;
    candidates.sort_by(compare_candidates);
    let selected_candidate_id = candidates
        .iter()
        .find(|candidate| candidate.outcome == CandidateOutcome::Succeeded)
        .map(|candidate| candidate.candidate_id.clone());
    let ranking = candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected =
                selected_candidate_id.as_deref() == Some(candidate.candidate_id.as_str());
            CandidateRankingRecord {
                rank: index + 1,
                candidate_id: candidate.candidate_id,
                selected,
                outcome: candidate.outcome,
                validation: candidate.validation,
                ci: candidate.ci,
                quality_score: candidate.quality_score,
                diff_scope: candidate.diff_scope,
                files_changed: candidate.files_changed,
                lines_changed: candidate.lines_changed,
                completed_at_epoch_ms: candidate.completed_at_epoch_ms,
                rank_reason: rank_reason(candidate.outcome, selected),
            }
        })
        .collect();

    CandidateSelectionRecord {
        schema: CANDIDATE_SELECTION_SCHEMA.to_string(),
        record_type: CANDIDATE_SELECTION_RECORD_TYPE.to_string(),
        candidate_group_id: input.candidate_group_id,
        selected_candidate_id,
        ranking,
        promotion_chain: input.promotion_chain,
    }
}

fn compare_candidates(left: &CandidateEvidence, right: &CandidateEvidence) -> Ordering {
    outcome_rank(right.outcome)
        .cmp(&outcome_rank(left.outcome))
        .then_with(|| check_rank(right.validation).cmp(&check_rank(left.validation)))
        .then_with(|| check_rank(right.ci).cmp(&check_rank(left.ci)))
        .then_with(|| quality_rank(right).cmp(&quality_rank(left)))
        .then_with(|| diff_scope_rank(right.diff_scope).cmp(&diff_scope_rank(left.diff_scope)))
        .then_with(|| diff_size(left).cmp(&diff_size(right)))
        .then_with(|| completed_at_rank(left).cmp(&completed_at_rank(right)))
        .then_with(|| left.candidate_id.cmp(&right.candidate_id))
}

fn outcome_rank(outcome: CandidateOutcome) -> u8 {
    match outcome {
        CandidateOutcome::Succeeded => 1,
        CandidateOutcome::Failed | CandidateOutcome::Stalled | CandidateOutcome::Cancelled => 0,
    }
}

fn check_rank(conclusion: CandidateCheckConclusion) -> u8 {
    match conclusion {
        CandidateCheckConclusion::Passed => 2,
        CandidateCheckConclusion::Unknown => 1,
        CandidateCheckConclusion::Failed => 0,
    }
}

fn quality_rank(candidate: &CandidateEvidence) -> i64 {
    candidate.quality_score.unwrap_or(i64::MIN)
}

fn diff_scope_rank(scope: CandidateDiffScope) -> u8 {
    match scope {
        CandidateDiffScope::Sane => 2,
        CandidateDiffScope::Unknown => 1,
        CandidateDiffScope::TooLarge => 0,
    }
}

fn diff_size(candidate: &CandidateEvidence) -> u64 {
    let files_changed = candidate
        .files_changed
        .map(u64::from)
        .unwrap_or(u64::MAX / 2);
    let lines_changed = candidate
        .lines_changed
        .map(u64::from)
        .unwrap_or(u64::MAX / 2);
    files_changed.saturating_add(lines_changed)
}

fn completed_at_rank(candidate: &CandidateEvidence) -> i64 {
    candidate.completed_at_epoch_ms.unwrap_or(i64::MAX)
}

fn rank_reason(outcome: CandidateOutcome, selected: bool) -> String {
    if selected {
        return "selected_highest_ranked_succeeded_candidate".to_string();
    }
    match outcome {
        CandidateOutcome::Succeeded => {
            "ranked_below_selected_candidate_by_deterministic_evidence".to_string()
        }
        CandidateOutcome::Failed | CandidateOutcome::Stalled | CandidateOutcome::Cancelled => {
            "ineligible_terminal_outcome".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(candidates: Vec<CandidateEvidence>) -> CandidateSelectionRecord {
        select_candidate(CandidateSelectionInput {
            candidate_group_id: "group-1".to_string(),
            candidates,
            promotion_chain: Vec::new(),
        })
    }

    #[test]
    fn candidate_selection_prefers_green_validation_before_ci_and_quality() {
        let record = selection(vec![
            CandidateEvidence::succeeded("candidate-a")
                .with_validation(CandidateCheckConclusion::Passed)
                .with_ci(CandidateCheckConclusion::Unknown)
                .with_quality_score(10),
            CandidateEvidence::succeeded("candidate-b")
                .with_validation(CandidateCheckConclusion::Unknown)
                .with_ci(CandidateCheckConclusion::Passed)
                .with_quality_score(100),
        ]);

        assert_eq!(record.selected_candidate_id.as_deref(), Some("candidate-a"));
        assert_eq!(record.ranking[0].candidate_id, "candidate-a");
        assert!(record.ranking[0].selected);
        assert_eq!(
            record.ranking[0].rank_reason,
            "selected_highest_ranked_succeeded_candidate"
        );
    }

    #[test]
    fn candidate_selection_uses_diff_size_then_completion_time_as_tiebreaks() {
        let record = selection(vec![
            CandidateEvidence::succeeded("candidate-later-smaller")
                .with_validation(CandidateCheckConclusion::Passed)
                .with_ci(CandidateCheckConclusion::Passed)
                .with_quality_score(90)
                .with_diff(CandidateDiffScope::Sane, 2, 20)
                .with_completed_at_epoch_ms(300),
            CandidateEvidence::succeeded("candidate-earlier-larger")
                .with_validation(CandidateCheckConclusion::Passed)
                .with_ci(CandidateCheckConclusion::Passed)
                .with_quality_score(90)
                .with_diff(CandidateDiffScope::Sane, 4, 30)
                .with_completed_at_epoch_ms(100),
        ]);

        assert_eq!(
            record.selected_candidate_id.as_deref(),
            Some("candidate-later-smaller")
        );

        let record = selection(vec![
            CandidateEvidence::succeeded("candidate-later")
                .with_validation(CandidateCheckConclusion::Passed)
                .with_ci(CandidateCheckConclusion::Passed)
                .with_quality_score(90)
                .with_diff(CandidateDiffScope::Sane, 2, 20)
                .with_completed_at_epoch_ms(300),
            CandidateEvidence::succeeded("candidate-earlier")
                .with_validation(CandidateCheckConclusion::Passed)
                .with_ci(CandidateCheckConclusion::Passed)
                .with_quality_score(90)
                .with_diff(CandidateDiffScope::Sane, 2, 20)
                .with_completed_at_epoch_ms(100),
        ]);

        assert_eq!(
            record.selected_candidate_id.as_deref(),
            Some("candidate-earlier")
        );
    }

    #[test]
    fn candidate_selection_never_selects_failed_terminal_candidates() {
        let record = selection(vec![
            CandidateEvidence::failed("candidate-a")
                .with_validation(CandidateCheckConclusion::Passed)
                .with_ci(CandidateCheckConclusion::Passed)
                .with_quality_score(100),
            CandidateEvidence::failed("candidate-b")
                .with_validation(CandidateCheckConclusion::Unknown)
                .with_ci(CandidateCheckConclusion::Passed)
                .with_quality_score(90),
        ]);

        assert_eq!(record.selected_candidate_id, None);
        assert!(record.ranking.iter().all(|ranking| !ranking.selected));
        assert!(record
            .ranking
            .iter()
            .all(|ranking| ranking.rank_reason == "ineligible_terminal_outcome"));
    }

    #[test]
    fn candidate_selection_record_preserves_promotion_chain() {
        let record = select_candidate(CandidateSelectionInput {
            candidate_group_id: "group-1".to_string(),
            candidates: vec![CandidateEvidence::succeeded("candidate-a")],
            promotion_chain: vec![CandidatePromotionRecord {
                from_candidate_id: "candidate-b".to_string(),
                to_candidate_id: "candidate-a".to_string(),
                reason: "winner PR creation failed".to_string(),
            }],
        });

        assert_eq!(record.schema, CANDIDATE_SELECTION_SCHEMA);
        assert_eq!(record.record_type, CANDIDATE_SELECTION_RECORD_TYPE);
        assert_eq!(record.promotion_chain.len(), 1);
        assert_eq!(record.promotion_chain[0].to_candidate_id, "candidate-a");
    }

    #[test]
    fn candidate_selection_record_converts_to_persistable_activity_artifact(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let record = selection(vec![CandidateEvidence::succeeded("candidate-a")]);

        let artifact = record.to_activity_artifact()?;

        assert_eq!(artifact.artifact_type, CANDIDATE_SELECTION_RECORD_TYPE);
        assert_eq!(artifact.artifact["schema"], CANDIDATE_SELECTION_SCHEMA);
        assert_eq!(
            artifact.artifact["record_type"],
            CANDIDATE_SELECTION_RECORD_TYPE
        );

        let round_tripped: CandidateSelectionRecord = serde_json::from_value(artifact.artifact)?;
        assert_eq!(
            round_tripped.selected_candidate_id.as_deref(),
            Some("candidate-a")
        );
        Ok(())
    }
}
