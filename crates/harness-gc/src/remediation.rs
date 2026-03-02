use harness_core::{RemediationType, SignalType};

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
