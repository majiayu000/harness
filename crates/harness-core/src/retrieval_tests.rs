use super::*;

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

struct DuplicateRetriever;

impl KnowledgeRetriever for DuplicateRetriever {
    fn name(&self) -> &'static str {
        "duplicate"
    }

    fn rank(
        &self,
        _query: &RetrievalQuery<'_>,
        _candidates: &[RetrievalCandidate<'_>],
    ) -> Result<Vec<ScoredCandidate>, RetrievalError> {
        Ok(["a", "a", "b"]
            .into_iter()
            .map(|id| ScoredCandidate {
                id: id.to_string(),
                score: 1.0,
                native_repo: true,
            })
            .collect())
    }
}

struct EmptyRetriever;

impl KnowledgeRetriever for EmptyRetriever {
    fn name(&self) -> &'static str {
        "empty"
    }

    fn rank(
        &self,
        _query: &RetrievalQuery<'_>,
        _candidates: &[RetrievalCandidate<'_>],
    ) -> Result<Vec<ScoredCandidate>, RetrievalError> {
        Ok(Vec::new())
    }
}

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
fn lexical_retriever_deduplicates_before_query_limit() {
    let retriever = LexicalKnowledgeRetriever;
    let query = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 2);
    let candidates = [
        RetrievalCandidate::new("a", vec![RetrievalField::new("code review", 1.0)]),
        RetrievalCandidate::new("a", vec![RetrievalField::new("code review", 1.0)]),
        RetrievalCandidate::new("b", vec![RetrievalField::new("build error", 1.0)]),
    ];

    let ranked = retriever
        .rank(&query, &candidates)
        .expect("lexical ranking succeeds");
    assert_eq!(
        ranked
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );

    let result = RetrievalExecutor::new(&retriever)
        .with_shadow(&retriever)
        .retrieve(&query, &candidates)
        .expect("retrieval succeeds");
    assert_eq!(result.selected, ranked);
    let comparison = result.comparison.expect("comparison is recorded");
    assert_eq!(comparison.shadow_ranked, ranked);
    assert_eq!(comparison.overlap_count, Some(2));
    assert_eq!(comparison.rank_divergence, Some(0.0));
}

#[test]
fn retrieval_executor_keeps_primary_selection_with_shadow_comparison() {
    let primary = LexicalKnowledgeRetriever;
    let shadow = ReverseRetriever;
    let query = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 3);
    let result = RetrievalExecutor::new(&primary)
        .with_shadow(&shadow)
        .retrieve(
            &query,
            &[
                RetrievalCandidate::new("a", vec![RetrievalField::new("code review", 1.0)]),
                RetrievalCandidate::new("b", vec![RetrievalField::new("build error", 1.0)]),
                RetrievalCandidate::new("c", vec![RetrievalField::new("deploy", 1.0)]),
            ],
        )
        .expect("primary retrieval succeeds");

    assert_eq!(result.selected[0].id, "a");
    let comparison = result.comparison.expect("comparison is recorded");
    assert_eq!(comparison.primary_implementation, "lexical");
    assert_eq!(comparison.shadow_implementation, "reverse");
    assert_eq!(comparison.overlap_count, Some(3));
    assert_eq!(comparison.rank_divergence, Some(1.0));
    assert!(comparison.shadow_error.is_none());
}

#[test]
fn rank_divergence_uses_kendall_pair_inversions() {
    let ranked = |ids: &[&str]| {
        ids.iter()
            .map(|id| ScoredCandidate {
                id: (*id).to_owned(),
                score: 1.0,
                native_repo: true,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rank_divergence(&ranked(&["a", "b", "c"]), &ranked(&["b", "a", "c"])),
        1.0 / 3.0
    );
    assert_eq!(rank_divergence(&ranked(&[]), &ranked(&["a", "b"])), 0.5);
    assert_eq!(
        rank_divergence(&ranked(&["a", "b"]), &ranked(&["b", "c"])),
        2.0 / 3.0
    );
    assert_eq!(
        rank_divergence(&ranked(&["a", "b"]), &ranked(&["c", "d"])),
        5.0 / 6.0
    );
}

#[test]
fn retrieval_executor_clamps_primary_and_shadow_results_to_query_limit() {
    let primary = ReverseRetriever;
    let shadow = ReverseRetriever;
    let query = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 1);
    let result = RetrievalExecutor::new(&primary)
        .with_shadow(&shadow)
        .retrieve(
            &query,
            &[
                RetrievalCandidate::new("a", vec![RetrievalField::new("code review", 1.0)]),
                RetrievalCandidate::new("b", vec![RetrievalField::new("build error", 1.0)]),
            ],
        )
        .expect("retrieval succeeds");

    assert_eq!(
        result
            .selected
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect::<Vec<_>>(),
        vec!["b"]
    );
    let comparison = result.comparison.expect("comparison is recorded");
    assert_eq!(comparison.primary_ranked, result.selected);
    assert_eq!(comparison.shadow_ranked.len(), 1);
    assert_eq!(comparison.shadow_ranked[0].id, "b");
    assert_eq!(comparison.overlap_count, Some(1));
}

#[test]
fn retrieval_executor_deduplicates_rankings_before_applying_query_limit() {
    let retriever = DuplicateRetriever;
    let query = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 2);
    let candidates = [
        RetrievalCandidate::new("a", vec![RetrievalField::new("code review", 1.0)]),
        RetrievalCandidate::new("b", vec![RetrievalField::new("build error", 1.0)]),
    ];
    let result = RetrievalExecutor::new(&retriever)
        .with_shadow(&retriever)
        .retrieve(&query, &candidates)
        .expect("retrieval succeeds");

    assert_eq!(
        result
            .selected
            .iter()
            .map(|candidate| candidate.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    let comparison = result.comparison.expect("comparison is recorded");
    assert_eq!(comparison.shadow_ranked, result.selected);
    assert_eq!(comparison.overlap_count, Some(2));
    assert_eq!(comparison.rank_divergence, Some(0.0));

    let zero_limit = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 0);
    let zero_result = RetrievalExecutor::new(&retriever)
        .retrieve(&zero_limit, &candidates)
        .expect("zero-limit retrieval succeeds");
    assert!(zero_result.selected.is_empty());
}

#[test]
fn retrieval_executor_isolates_shadow_failure() {
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
    assert_eq!(comparison.overlap_count, None);
    assert_eq!(comparison.rank_divergence, None);
}

#[test]
fn retrieval_executor_omits_metrics_for_shadow_failure_with_empty_primary() {
    let query = RetrievalQuery::new(RetrievalSurface::Skill, "code review", 1);
    let result = RetrievalExecutor::new(&EmptyRetriever)
        .with_shadow(&FailingRetriever)
        .retrieve(&query, &[])
        .expect("primary retrieval succeeds");
    let comparison = result.comparison.expect("comparison is recorded");

    assert!(result.selected.is_empty());
    assert!(comparison.shadow_error.is_some());
    assert_eq!(comparison.overlap_count, None);
    assert_eq!(comparison.rank_divergence, None);
}
