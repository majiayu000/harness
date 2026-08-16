use harness_workflow::runtime::EvalValidationCommandEvidence;

pub(in crate::commands::eval) fn render_verification_evidence(
    evidence: &[EvalValidationCommandEvidence],
) -> String {
    if evidence.is_empty() {
        return "none".to_string();
    }
    evidence
        .iter()
        .map(|entry| {
            format!(
                "verifier={} verifier_sha256={} exit_code={} output_sha256={}",
                entry.verifier_id.as_deref().unwrap_or("command"),
                entry.verifier_sha256.as_deref().unwrap_or("n/a"),
                entry
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
                entry.output_sha256.as_deref().unwrap_or("n/a")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub(in crate::commands::eval) fn render_verification_transition(
    baseline: &[EvalValidationCommandEvidence],
    candidate: &[EvalValidationCommandEvidence],
) -> String {
    format!(
        "{}->{}",
        render_verification_evidence(baseline),
        render_verification_evidence(candidate)
    )
}
