use harness_core::{types::RemediationType, types::SignalType};

/// Map signal types to their recommended remediation artifact types.
pub fn recommended_remediation(signal_type: SignalType) -> RemediationType {
    match signal_type {
        SignalType::RepeatedWarn => RemediationType::Guard,
        SignalType::ChronicBlock => RemediationType::Rule,
        SignalType::HotFiles => RemediationType::Skill,
        SignalType::SlowSessions => RemediationType::Skill,
        SignalType::WarnEscalation => RemediationType::Rule,
        SignalType::LinterViolations => RemediationType::Guard,
    }
}

/// Priority for signal processing (lower = higher priority).
pub fn signal_priority(signal_type: SignalType) -> u8 {
    match signal_type {
        SignalType::ChronicBlock => 0,
        SignalType::WarnEscalation => 1,
        SignalType::RepeatedWarn => 2,
        SignalType::LinterViolations => 3,
        SignalType::HotFiles => 4,
        SignalType::SlowSessions => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_priority_mapping_is_stable() {
        let ordered = [
            SignalType::ChronicBlock,
            SignalType::WarnEscalation,
            SignalType::RepeatedWarn,
            SignalType::LinterViolations,
            SignalType::HotFiles,
            SignalType::SlowSessions,
        ];
        let priorities: Vec<u8> = ordered.into_iter().map(signal_priority).collect();

        assert_eq!(priorities, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn sorting_uses_priority_boundaries() {
        let mut signals = vec![
            SignalType::SlowSessions,
            SignalType::HotFiles,
            SignalType::LinterViolations,
            SignalType::RepeatedWarn,
            SignalType::WarnEscalation,
            SignalType::ChronicBlock,
        ];

        signals.sort_by_key(|signal| signal_priority(*signal));

        assert_eq!(
            signals,
            vec![
                SignalType::ChronicBlock,
                SignalType::WarnEscalation,
                SignalType::RepeatedWarn,
                SignalType::LinterViolations,
                SignalType::HotFiles,
                SignalType::SlowSessions,
            ]
        );
    }
}
