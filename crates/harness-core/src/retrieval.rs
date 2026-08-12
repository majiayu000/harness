use std::collections::{BTreeMap, BTreeSet};

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
}
