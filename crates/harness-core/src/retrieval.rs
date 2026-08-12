use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RetrievalSurface {
    Skill,
    RepoMemory,
}

impl RetrievalSurface {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::RepoMemory => "repo_memory",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RetrievalQuery<'a> {
    pub surface: RetrievalSurface,
    pub text: &'a str,
    pub activity_class: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub limit: usize,
}

impl<'a> RetrievalQuery<'a> {
    pub const fn new(surface: RetrievalSurface, text: &'a str, limit: usize) -> Self {
        Self {
            surface,
            text,
            activity_class: None,
            repo: None,
            limit,
        }
    }

    pub const fn with_activity_class(mut self, activity_class: &'a str) -> Self {
        self.activity_class = Some(activity_class);
        self
    }

    pub const fn with_repo(mut self, repo: &'a str) -> Self {
        self.repo = Some(repo);
        self
    }
}

#[derive(Debug, Clone)]
pub struct RetrievalCandidate<'a> {
    pub id: &'a str,
    pub fields: Vec<RetrievalField<'a>>,
    pub native_repo: bool,
}

impl<'a> RetrievalCandidate<'a> {
    pub fn new(id: &'a str, fields: Vec<RetrievalField<'a>>) -> Self {
        Self {
            id,
            fields,
            native_repo: true,
        }
    }

    pub const fn with_native_repo(mut self, native_repo: bool) -> Self {
        self.native_repo = native_repo;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoredCandidate {
    pub id: String,
    pub score: f64,
    pub native_repo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalError {
    message: String,
}

impl RetrievalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RetrievalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RetrievalError {}

pub trait KnowledgeRetriever: Send + Sync {
    fn name(&self) -> &'static str;

    fn rank(
        &self,
        query: &RetrievalQuery<'_>,
        candidates: &[RetrievalCandidate<'_>],
    ) -> Result<Vec<ScoredCandidate>, RetrievalError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LexicalKnowledgeRetriever;

impl KnowledgeRetriever for LexicalKnowledgeRetriever {
    fn name(&self) -> &'static str {
        "lexical"
    }

    fn rank(
        &self,
        query: &RetrievalQuery<'_>,
        candidates: &[RetrievalCandidate<'_>],
    ) -> Result<Vec<ScoredCandidate>, RetrievalError> {
        if query.limit == 0 || candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut scored = candidates
            .iter()
            .map(|candidate| ScoredCandidate {
                id: candidate.id.to_string(),
                score: score_lexical_relevance(query.text, &candidate.fields).score,
                native_repo: candidate.native_repo,
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| right.native_repo.cmp(&left.native_repo))
                .then_with(|| left.id.cmp(&right.id))
        });
        scored.truncate(query.limit);
        Ok(scored)
    }
}

pub fn score_retrieval_candidate(
    retriever: &dyn KnowledgeRetriever,
    query: &RetrievalQuery<'_>,
    candidate: RetrievalCandidate<'_>,
) -> Result<f64, RetrievalError> {
    let candidate_id = candidate.id.to_string();
    Ok(retriever
        .rank(query, std::slice::from_ref(&candidate))?
        .into_iter()
        .find(|scored| scored.id == candidate_id)
        .map(|scored| scored.score)
        .unwrap_or(0.0))
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalExecution {
    pub selected: Vec<ScoredCandidate>,
    pub comparison: Option<RetrievalComparison>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalComparison {
    pub surface: RetrievalSurface,
    pub primary_implementation: &'static str,
    pub shadow_implementation: &'static str,
    pub primary_ranked: Vec<ScoredCandidate>,
    pub shadow_ranked: Vec<ScoredCandidate>,
    pub overlap_count: usize,
    pub rank_divergence: f64,
    pub shadow_error: Option<String>,
}

pub struct RetrievalExecutor<'a> {
    primary: &'a dyn KnowledgeRetriever,
    shadow: Option<&'a dyn KnowledgeRetriever>,
}

impl<'a> RetrievalExecutor<'a> {
    pub fn new(primary: &'a dyn KnowledgeRetriever) -> Self {
        Self {
            primary,
            shadow: None,
        }
    }

    pub fn with_shadow(mut self, shadow: &'a dyn KnowledgeRetriever) -> Self {
        self.shadow = Some(shadow);
        self
    }

    pub fn retrieve(
        &self,
        query: &RetrievalQuery<'_>,
        candidates: &[RetrievalCandidate<'_>],
    ) -> Result<RetrievalExecution, RetrievalError> {
        let selected = self.primary.rank(query, candidates)?;
        let Some(shadow) = self.shadow else {
            return Ok(RetrievalExecution {
                selected,
                comparison: None,
            });
        };
        let (shadow_ranked, shadow_error) = match shadow.rank(query, candidates) {
            Ok(ranked) => (ranked, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let comparison = RetrievalComparison {
            surface: query.surface,
            primary_implementation: self.primary.name(),
            shadow_implementation: shadow.name(),
            overlap_count: overlap_count(&selected, &shadow_ranked),
            rank_divergence: rank_divergence(&selected, &shadow_ranked),
            primary_ranked: selected.clone(),
            shadow_ranked,
            shadow_error,
        };
        Ok(RetrievalExecution {
            selected,
            comparison: Some(comparison),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RetrievalField<'a> {
    pub text: &'a str,
    pub weight: f64,
}

impl<'a> RetrievalField<'a> {
    pub fn new(text: &'a str, weight: f64) -> Self {
        Self { text, weight }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LexicalRelevanceScore {
    pub score: f64,
    pub matched_terms: usize,
    pub query_terms: usize,
}

impl LexicalRelevanceScore {
    pub const fn none(query_terms: usize) -> Self {
        Self {
            score: 0.0,
            matched_terms: 0,
            query_terms,
        }
    }
}

pub fn score_lexical_relevance(
    query: &str,
    fields: &[RetrievalField<'_>],
) -> LexicalRelevanceScore {
    let query_terms = retrieval_terms(query);
    if query_terms.is_empty() || fields.is_empty() {
        return LexicalRelevanceScore::none(query_terms.len());
    }

    let mut document_counts: BTreeMap<String, f64> = BTreeMap::new();
    let mut document_weight = 0.0f64;
    let normalized_query = normalize_phrase(query);
    let mut phrase_bonus = 0.0f64;

    for field in fields {
        let weight = field.weight.max(0.0);
        if weight == 0.0 || field.text.trim().is_empty() {
            continue;
        }
        let field_terms = retrieval_terms(field.text);
        document_weight += weight * field_terms.len() as f64;
        for term in field_terms {
            *document_counts.entry(term).or_insert(0.0) += weight;
        }

        let normalized_field = normalize_phrase(field.text);
        let field_term_count = normalized_field.split_whitespace().count();
        if (1..=8).contains(&field_term_count)
            && !normalized_field.is_empty()
            && contains_token_phrase(&normalized_query, &normalized_field)
        {
            phrase_bonus = phrase_bonus.max(0.20 * weight.min(2.0));
        }
    }

    if document_counts.is_empty() {
        return LexicalRelevanceScore::none(query_terms.len());
    }

    let mut matched = BTreeSet::new();
    let mut weighted_hits = 0.0f64;
    for term in &query_terms {
        if let Some(weight) = document_counts.get(term) {
            matched.insert(term.clone());
            weighted_hits += *weight;
        }
    }
    if matched.is_empty() {
        return LexicalRelevanceScore::none(query_terms.len());
    }

    let denominator = query_terms.len().min(document_counts.len()).max(1) as f64;
    let coverage = matched.len() as f64 / denominator;
    let density = (weighted_hits / document_weight.max(1.0)).min(1.0);
    let score = (coverage * 0.78 + density * 0.22 + phrase_bonus).min(1.0);

    LexicalRelevanceScore {
        score,
        matched_terms: matched.len(),
        query_terms: query_terms.len(),
    }
}

fn retrieval_terms(text: &str) -> BTreeSet<String> {
    tokenize(text)
        .into_iter()
        .filter(|term| !is_stopword(term))
        .collect()
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            push_normalized_token(&mut tokens, &current);
            current.clear();
        }
    }
    if !current.is_empty() {
        push_normalized_token(&mut tokens, &current);
    }
    tokens
}

fn push_normalized_token(tokens: &mut Vec<String>, token: &str) {
    if token.len() <= 1 {
        return;
    }
    tokens.push(normalize_token(token));
}

fn normalize_token(token: &str) -> String {
    for suffix in ["ing", "ed", "es", "s"] {
        if token.len() > suffix.len() + 3 && token.ends_with(suffix) {
            return token[..token.len() - suffix.len()].to_string();
        }
    }
    token.to_string()
}

fn normalize_phrase(text: &str) -> String {
    tokenize(text).join(" ")
}

fn contains_token_phrase(normalized_text: &str, normalized_phrase: &str) -> bool {
    let text_terms = normalized_text.split_whitespace().collect::<Vec<_>>();
    let phrase_terms = normalized_phrase.split_whitespace().collect::<Vec<_>>();
    !phrase_terms.is_empty()
        && phrase_terms.len() <= text_terms.len()
        && text_terms
            .windows(phrase_terms.len())
            .any(|window| window == phrase_terms.as_slice())
}

fn is_stopword(term: &str) -> bool {
    matches!(
        term,
        "about"
            | "after"
            | "again"
            | "against"
            | "also"
            | "and"
            | "any"
            | "are"
            | "because"
            | "before"
            | "but"
            | "can"
            | "could"
            | "did"
            | "does"
            | "for"
            | "from"
            | "had"
            | "has"
            | "have"
            | "how"
            | "into"
            | "its"
            | "make"
            | "must"
            | "not"
            | "our"
            | "out"
            | "over"
            | "please"
            | "should"
            | "that"
            | "the"
            | "their"
            | "then"
            | "there"
            | "this"
            | "through"
            | "with"
            | "would"
            | "you"
            | "your"
    )
}

fn overlap_count(primary: &[ScoredCandidate], shadow: &[ScoredCandidate]) -> usize {
    let primary_ids = primary
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect::<BTreeSet<_>>();
    shadow
        .iter()
        .filter(|candidate| primary_ids.contains(candidate.id.as_str()))
        .count()
}

fn rank_divergence(primary: &[ScoredCandidate], shadow: &[ScoredCandidate]) -> f64 {
    if primary.is_empty() && shadow.is_empty() {
        return 0.0;
    }
    let max_rank = primary.len().max(shadow.len()).max(1);
    let mut ids = primary
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect::<BTreeSet<_>>();
    ids.extend(shadow.iter().map(|candidate| candidate.id.as_str()));
    let rank_delta_sum = ids
        .iter()
        .map(|id| {
            let primary_rank = primary
                .iter()
                .position(|candidate| candidate.id.as_str() == *id)
                .unwrap_or(max_rank);
            let shadow_rank = shadow
                .iter()
                .position(|candidate| candidate.id.as_str() == *id)
                .unwrap_or(max_rank);
            primary_rank.abs_diff(shadow_rank)
        })
        .sum::<usize>();
    let denominator = (ids.len() * max_rank).max(1) as f64;
    (rank_delta_sum as f64 / denominator).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_relevance_matches_terms_regardless_of_order() {
        let score = score_lexical_relevance(
            "please review the code changes",
            &[RetrievalField::new("code review", 2.0)],
        );

        assert!(score.score > 0.7, "score was {}", score.score);
        assert_eq!(score.matched_terms, 2);
    }

    #[test]
    fn lexical_relevance_returns_zero_without_content_overlap() {
        let score = score_lexical_relevance(
            "fix a postgres migration",
            &[RetrievalField::new("frontend visual polish", 1.0)],
        );

        assert_eq!(score, LexicalRelevanceScore::none(3));
    }

    #[test]
    fn lexical_relevance_does_not_phrase_match_inside_tokens() {
        let score = score_lexical_relevance(
            "cargo build failed",
            &[RetrievalField::new("go build", 2.0)],
        );

        assert!(
            score.score < 0.7,
            "score should not receive a phrase bonus from 'cargo', was {}",
            score.score
        );
        assert_eq!(score.matched_terms, 1);
    }

    #[test]
    fn lexical_retriever_ranks_candidates_by_weighted_score() {
        let retriever = LexicalKnowledgeRetriever;
        let query = RetrievalQuery::new(RetrievalSurface::Skill, "review code changes", 10);
        let ranked = retriever
            .rank(
                &query,
                &[
                    RetrievalCandidate::new(
                        "skill:build-fix",
                        vec![RetrievalField::new("build error", 2.0)],
                    ),
                    RetrievalCandidate::new(
                        "skill:review",
                        vec![RetrievalField::new("code review", 2.0)],
                    ),
                ],
            )
            .expect("lexical ranking succeeds");

        assert_eq!(ranked[0].id, "skill:review");
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn lexical_retriever_prefers_native_repo_on_score_ties() {
        let retriever = LexicalKnowledgeRetriever;
        let query = RetrievalQuery::new(RetrievalSurface::RepoMemory, "postgres timeout", 10);
        let ranked = retriever
            .rank(
                &query,
                &[
                    RetrievalCandidate::new(
                        "foreign",
                        vec![RetrievalField::new("postgres timeout", 1.0)],
                    )
                    .with_native_repo(false),
                    RetrievalCandidate::new(
                        "native",
                        vec![RetrievalField::new("postgres timeout", 1.0)],
                    ),
                ],
            )
            .expect("lexical ranking succeeds");

        assert_eq!(ranked[0].id, "native");
    }

    #[test]
    fn retrieval_executor_keeps_primary_selection_with_shadow_comparison() {
        struct ReverseRetriever;
        impl KnowledgeRetriever for ReverseRetriever {
            fn name(&self) -> &'static str {
                "reverse"
            }

            fn rank(
                &self,
                _query: &RetrievalQuery<'_>,
                candidates: &[RetrievalCandidate<'_>],
            ) -> Result<Vec<ScoredCandidate>, RetrievalError> {
                Ok(candidates
                    .iter()
                    .rev()
                    .map(|candidate| ScoredCandidate {
                        id: candidate.id.to_string(),
                        score: 1.0,
                        native_repo: candidate.native_repo,
                    })
                    .collect())
            }
        }

        let primary = LexicalKnowledgeRetriever;
        let shadow = ReverseRetriever;
        let query = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 2);
        let result = RetrievalExecutor::new(&primary)
            .with_shadow(&shadow)
            .retrieve(
                &query,
                &[
                    RetrievalCandidate::new("a", vec![RetrievalField::new("code review", 1.0)]),
                    RetrievalCandidate::new("b", vec![RetrievalField::new("build error", 1.0)]),
                ],
            )
            .expect("primary retrieval succeeds");

        assert_eq!(result.selected[0].id, "a");
        let comparison = result.comparison.expect("comparison is recorded");
        assert_eq!(comparison.primary_implementation, "lexical");
        assert_eq!(comparison.shadow_implementation, "reverse");
        assert_eq!(comparison.overlap_count, 2);
        assert!(comparison.rank_divergence > 0.0);
        assert!(comparison.shadow_error.is_none());
    }

    #[test]
    fn retrieval_executor_isolates_shadow_failure() {
        struct FailingRetriever;
        impl KnowledgeRetriever for FailingRetriever {
            fn name(&self) -> &'static str {
                "failing"
            }

            fn rank(
                &self,
                _query: &RetrievalQuery<'_>,
                _candidates: &[RetrievalCandidate<'_>],
            ) -> Result<Vec<ScoredCandidate>, RetrievalError> {
                Err(RetrievalError::new("embedding backend unavailable"))
            }
        }

        let primary = LexicalKnowledgeRetriever;
        let shadow = FailingRetriever;
        let query = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 1);
        let result = RetrievalExecutor::new(&primary)
            .with_shadow(&shadow)
            .retrieve(
                &query,
                &[RetrievalCandidate::new(
                    "skill:review",
                    vec![RetrievalField::new("code review", 1.0)],
                )],
            )
            .expect("primary retrieval succeeds");

        assert_eq!(result.selected.len(), 1);
        let comparison = result.comparison.expect("comparison is recorded");
        assert_eq!(
            comparison.shadow_error.as_deref(),
            Some("embedding backend unavailable")
        );
        assert!(comparison.shadow_ranked.is_empty());
    }
}
