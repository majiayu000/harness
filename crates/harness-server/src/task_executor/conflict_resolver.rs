//! Agent-reported resumed-PR conflict gate outcomes.

const CLEAN_PR_TOKEN: &str = "CLEAN_PR";
const REBASE_PUSHED_TOKEN: &str = "REBASE_PUSHED";
const MANUAL_RESOLUTION_REQUIRED_TOKEN: &str = "MANUAL_RESOLUTION_REQUIRED";

/// Exact conflict-gate outcomes accepted from the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictCheckOutcome {
    CleanPr,
    RebasePushed,
    ManualResolutionRequired,
}

/// Parse the final conflict-gate terminal token from the agent output.
///
/// The gate is intentionally strict and fail-closed: only the exact last
/// non-empty line is accepted as a valid outcome.
pub(crate) fn parse_conflict_check_output(output: &str) -> Result<ConflictCheckOutcome, String> {
    let Some(last_line) = output.lines().rev().find(|line| !line.trim().is_empty()) else {
        return Err("missing terminal conflict gate token".to_string());
    };
    match last_line.trim() {
        CLEAN_PR_TOKEN => Ok(ConflictCheckOutcome::CleanPr),
        REBASE_PUSHED_TOKEN => Ok(ConflictCheckOutcome::RebasePushed),
        MANUAL_RESOLUTION_REQUIRED_TOKEN => Ok(ConflictCheckOutcome::ManualResolutionRequired),
        other => Err(format!("unexpected conflict gate terminal token `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_pr_token() {
        let result = parse_conflict_check_output("inspected mergeability\nCLEAN_PR");
        assert_eq!(result, Ok(ConflictCheckOutcome::CleanPr));
    }

    #[test]
    fn parses_rebase_pushed_token() {
        let result = parse_conflict_check_output("rebased and pushed\nREBASE_PUSHED");
        assert_eq!(result, Ok(ConflictCheckOutcome::RebasePushed));
    }

    #[test]
    fn rejects_malformed_last_line() {
        let result =
            parse_conflict_check_output("MANUAL_RESOLUTION_REQUIRED\nextra trailing output");
        assert_eq!(
            result,
            Err("unexpected conflict gate terminal token `extra trailing output`".to_string())
        );
    }
}
