use harness_core::{TaskClassification, TaskComplexity};

/// Count file paths referenced in a prompt by looking for tokens that contain
/// a dot followed by a known source-file extension.
fn count_file_references(prompt: &str) -> usize {
    const EXTENSIONS: &[&str] = &[
        "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "swift", "cpp", "c", "h", "toml",
        "yaml", "yml", "json", "sh", "md",
    ];

    prompt
        .split_whitespace()
        .filter_map(|token| {
            // Strip surrounding punctuation common in prose (e.g., backticks, parens)
            let token = token.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '_' && c != '-' && c != '/'
            });
            if let Some(dot_pos) = token.rfind('.') {
                let ext = &token[dot_pos + 1..];
                if EXTENSIONS.contains(&ext) {
                    Some(token)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Classify a task request by complexity.
///
/// Heuristic:
/// - Count distinct file-path tokens (tokens with source-file extensions).
/// - 0–2 files → Simple
/// - 3–5 files → Medium
/// - 6+  files → Complex
/// - If `issue` or `pr` is present, bump to at least Medium.
pub fn classify(prompt: &str, issue: Option<u64>, pr: Option<u64>) -> TaskClassification {
    let file_count = count_file_references(prompt);

    let complexity = match file_count {
        0..=2 => TaskComplexity::Simple,
        3..=5 => TaskComplexity::Medium,
        _ => TaskComplexity::Complex,
    };

    // Issue or PR presence bumps to at least Medium.
    let complexity = if (issue.is_some() || pr.is_some()) && complexity == TaskComplexity::Simple {
        TaskComplexity::Medium
    } else {
        complexity
    };

    TaskClassification {
        complexity,
        language: None,
        requires_write: false,
        requires_network: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::TaskComplexity;

    #[test]
    fn simple_prompt_no_files() {
        let c = classify("Fix the login bug", None, None);
        assert_eq!(c.complexity, TaskComplexity::Simple);
    }

    #[test]
    fn simple_prompt_two_files() {
        let c = classify("Update src/main.rs and README.md", None, None);
        assert_eq!(c.complexity, TaskComplexity::Simple);
    }

    #[test]
    fn medium_prompt_four_files() {
        let c = classify(
            "Edit src/auth.rs, src/models.rs, src/routes.rs, and tests/auth.rs",
            None,
            None,
        );
        assert_eq!(c.complexity, TaskComplexity::Medium);
    }

    #[test]
    fn complex_prompt_six_files() {
        let c = classify(
            "Refactor src/a.rs src/b.rs src/c.rs src/d.rs src/e.rs src/f.rs",
            None,
            None,
        );
        assert_eq!(c.complexity, TaskComplexity::Complex);
    }

    #[test]
    fn issue_bumps_simple_to_medium() {
        let c = classify("Fix bug", Some(42), None);
        assert_eq!(c.complexity, TaskComplexity::Medium);
    }

    #[test]
    fn pr_bumps_simple_to_medium() {
        let c = classify("Review changes", None, Some(7));
        assert_eq!(c.complexity, TaskComplexity::Medium);
    }

    #[test]
    fn complex_stays_complex_with_pr() {
        let c = classify("Rewrite a.rs b.rs c.rs d.rs e.rs f.rs g.rs", None, Some(1));
        assert_eq!(c.complexity, TaskComplexity::Complex);
    }
}
