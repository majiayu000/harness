//! PR conflict-size assessment policy.
//!
//! Harness no longer queries `gh` or runs local git commands to inspect PRs.
//! Repository mutation and GitHub state inspection belong in the agent prompt,
//! so this module only preserves the conflict-size type used by the executor
//! and reports that automatic host-side assessment is unavailable.

use std::path::Path;

/// Classification of how large a PR's merge conflict is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrConflictSize {
    /// Host-side assessment is unavailable; the agent must inspect GitHub/git state.
    Unknown(String),
}

/// Return an unknown conflict size so the executor continues without invoking
/// host-side git/GitHub commands.
pub(crate) async fn assess_pr_conflict(pr_num: u64, _project: &Path) -> PrConflictSize {
    PrConflictSize::Unknown(format!(
        "PR #{pr_num} conflict assessment is delegated to the agent prompt"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conflict_assessment_is_delegated() {
        let result = assess_pr_conflict(42, std::path::Path::new(".")).await;
        assert!(matches!(result, PrConflictSize::Unknown(_)));
    }
}
