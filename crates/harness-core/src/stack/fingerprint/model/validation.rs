use super::*;
use serde::de::Error as _;

pub(super) fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum EnvironmentStateWire {
    Unset,
    Redacted,
    SetDigest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentFactWire {
    key: RuntimeEnvironmentKey,
    state: EnvironmentStateWire,
    value_sha256: Option<String>,
}

impl<'de> Deserialize<'de> for RuntimeEnvironmentFact {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = EnvironmentFactWire::deserialize(deserializer)?;
        let value = match (wire.state, wire.value_sha256) {
            (EnvironmentStateWire::Unset, None) => RuntimeEnvironmentValue::Unset,
            (EnvironmentStateWire::Redacted, None) => RuntimeEnvironmentValue::Redacted,
            (EnvironmentStateWire::SetDigest, Some(value)) => RuntimeEnvironmentValue::SetDigest {
                value_sha256: Sha256Digest::parse(&value).map_err(D::Error::custom)?,
            },
            _ => return Err(D::Error::custom("invalid runtime environment fact state")),
        };
        Ok(Self {
            key: wire.key,
            value,
        })
    }
}

pub(super) fn payload_is_valid(payload: &RuntimeExecutableFingerprintPayload) -> bool {
    payload.runtime_kind == payload.role_binding.runtime_kind()
        && attempts_are_valid(payload.command_form, &payload.resolution_attempts)
        && failures_are_valid(&payload.failures)
        && valid_environment(payload.runtime_kind, &payload.environment)
        && payload
            .version
            .as_ref()
            .is_none_or(|version| normalized_version_is_valid(&version.normalized_version))
        && payload.executable.as_ref().is_none_or(|identity| {
            identity.file_size_bytes > 0 && identity.unix_mode.is_some_and(|mode| mode & 0o111 != 0)
        })
        && observation_state_is_valid(payload)
}

fn attempts_are_valid(form: RuntimeCommandForm, attempts: &[RuntimeResolutionAttempt]) -> bool {
    if attempts.len() > 64
        || (matches!(
            form,
            RuntimeCommandForm::UnixAbsolute | RuntimeCommandForm::UnixQualified
        ) && attempts.len() != 1)
    {
        return false;
    }

    attempts.iter().enumerate().all(|(index, attempt)| {
        let terminal_is_last = !outcome_is_terminal(attempt.outcome) || index + 1 == attempts.len();
        let absolute_or_qualified_does_not_fallback = form == RuntimeCommandForm::UnixBare
            || attempt.outcome != RuntimeResolutionAttemptOutcome::ExecEacces;
        attempt_is_valid(attempt.exec_sequence, attempt.exec_context, attempt.outcome)
            && terminal_is_last
            && absolute_or_qualified_does_not_fallback
    })
}

pub(super) fn attempt_is_valid(
    sequence: RuntimeExecSequence,
    context: Option<RuntimeExecutionContext>,
    outcome: RuntimeResolutionAttemptOutcome,
) -> bool {
    let context_matches_sequence = (sequence == RuntimeExecSequence::None) == context.is_none();
    let context_is_closed = context.is_none()
        || context == Some(RuntimeExecutionContext::LinuxFdCloexecExecveatEmptyPathFd10);
    context_matches_sequence && context_is_closed && sequence_allows_outcome(sequence, outcome)
}

pub(super) fn normalized_version_is_valid(value: &str) -> bool {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(left, right)| (left, Some(right)));
    if build.is_some_and(|part| !identifier_list_is_valid(part, false))
        || without_build.contains('+')
    {
        return false;
    }
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(left, right)| (left, Some(right)));
    if prerelease.is_some_and(|part| !identifier_list_is_valid(part, true)) {
        return false;
    }
    let mut parts = core.split('.');
    let valid = (0..3).all(|_| parts.next().is_some_and(numeric_identifier_is_valid));
    valid && parts.next().is_none()
}

fn numeric_identifier_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn identifier_list_is_valid(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!reject_numeric_leading_zero
                    || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || numeric_identifier_is_valid(identifier))
        })
}

fn sequence_allows_outcome(
    sequence: RuntimeExecSequence,
    outcome: RuntimeResolutionAttemptOutcome,
) -> bool {
    use RuntimeExecSequence as S;
    use RuntimeResolutionAttemptOutcome as O;

    match sequence {
        S::None => matches!(
            outcome,
            O::Absent
                | O::NotRegular
                | O::NotExecutable
                | O::InspectionFailed
                | O::InspectionTarget
                | O::AuthorizationUnavailable
                | O::InterpreterAuthorizationUnavailable
                | O::SupervisionSetupFailed
        ),
        S::Single => matches!(
            outcome,
            O::InterpreterAuthorizationUnavailable
                | O::HandleExecutionUnavailable
                | O::ExecVerificationFailed
                | O::ExecEacces
                | O::ExecFailed
                | O::ExecStarted
        ),
        S::EtxtbsyThenCheckpointAfter150Ms => matches!(
            outcome,
            O::InspectionFailed
                | O::InterpreterAuthorizationUnavailable
                | O::HandleExecutionUnavailable
                | O::SupervisionSetupFailed
                | O::RetryNotAuthorized
                | O::RetryAuthorizationUnavailable
                | O::ExecVerificationFailed
                | O::ExecEacces
                | O::ExecFailed
                | O::ExecStarted
        ),
    }
}

fn outcome_is_terminal(outcome: RuntimeResolutionAttemptOutcome) -> bool {
    !matches!(
        outcome,
        RuntimeResolutionAttemptOutcome::Absent
            | RuntimeResolutionAttemptOutcome::NotRegular
            | RuntimeResolutionAttemptOutcome::NotExecutable
            | RuntimeResolutionAttemptOutcome::ExecEacces
    )
}

fn failures_are_valid(failures: &[RuntimeProbeFailure]) -> bool {
    failures.iter().all(RuntimeProbeFailure::valid)
        && failures
            .windows(2)
            .all(|pair| (pair[0].phase, pair[0].kind.rank()) < (pair[1].phase, pair[1].kind.rank()))
}

fn observation_state_is_valid(payload: &RuntimeExecutableFingerprintPayload) -> bool {
    let primary = payload
        .failures
        .iter()
        .filter(|failure| failure.phase != RuntimeProbePhase::LifecycleCleanup)
        .collect::<Vec<_>>();

    match &payload.version {
        Some(_) => {
            payload.failures.is_empty()
                && payload.executable.as_ref().is_some_and(|identity| {
                    identity.checkpoint_consistent_path && identity.exec_stop_consistent_handle
                })
                && payload.resolution_attempts.last().is_some_and(|attempt| {
                    attempt.outcome == RuntimeResolutionAttemptOutcome::ExecStarted
                })
        }
        None => {
            primary.len() == 1
                && primary_failure_matches(
                    payload.command_form,
                    &payload.resolution_attempts,
                    payload.executable.as_ref(),
                    primary[0],
                )
        }
    }
}

fn primary_failure_matches(
    form: RuntimeCommandForm,
    attempts: &[RuntimeResolutionAttempt],
    executable: Option<&RuntimeExecutableIdentity>,
    failure: &RuntimeProbeFailure,
) -> bool {
    use RuntimeProbeFailureDetail as D;
    use RuntimeProbeFailureKind as K;
    use RuntimeResolutionAttemptOutcome as O;

    let last = attempts.last();
    let final_outcome_is = |expected| last.is_some_and(|attempt| attempt.outcome == expected);
    let final_sequence_is =
        |expected| last.is_some_and(|attempt| attempt.exec_sequence == expected);
    let no_final_identity = executable.is_none();

    match failure.kind {
        K::PathNotFound => {
            no_final_identity
                && match form {
                    RuntimeCommandForm::UnixBare => {
                        !attempts.is_empty()
                            && attempts.iter().all(|attempt| {
                                matches!(
                                    attempt.outcome,
                                    O::Absent | O::NotRegular | O::NotExecutable
                                )
                            })
                    }
                    RuntimeCommandForm::UnixAbsolute | RuntimeCommandForm::UnixQualified => {
                        attempts.len() == 1 && final_outcome_is(O::Absent)
                    }
                }
        }
        K::PathUnusable => attempts.is_empty() && no_final_identity,
        K::CandidateLimitExceeded => {
            form == RuntimeCommandForm::UnixBare
                && attempts.len() == 64
                && attempts
                    .iter()
                    .all(|attempt| !outcome_is_terminal(attempt.outcome))
                && no_final_identity
        }
        K::OpenFailed | K::ExecutableTooLarge | K::ReadFailed => {
            final_outcome_is(O::InspectionFailed) && no_final_identity
        }
        K::MetadataUnavailable => {
            final_outcome_is(O::InspectionFailed) || final_outcome_is(O::ExecStarted)
        }
        K::NotRegularFile => final_outcome_is(O::NotRegular) && no_final_identity,
        K::NotExecutable => final_outcome_is(O::NotExecutable) && no_final_identity,
        K::IdentityChanged => {
            final_outcome_is(O::InspectionFailed)
                || final_outcome_is(O::ExecVerificationFailed)
                || final_outcome_is(O::ExecStarted)
        }
        K::ProbeNotAuthorized => match failure.detail {
            Some(D::ConfigurationSourceRepository) => final_outcome_is(O::InspectionTarget),
            Some(D::ResolvedTargetRepository) => {
                final_outcome_is(O::InspectionTarget)
                    || (final_outcome_is(O::RetryNotAuthorized)
                        && final_sequence_is(RuntimeExecSequence::EtxtbsyThenCheckpointAfter150Ms))
            }
            _ => false,
        },
        K::TargetAuthorizationUnavailable => {
            final_outcome_is(O::AuthorizationUnavailable)
                || (final_outcome_is(O::RetryAuthorizationUnavailable)
                    && final_sequence_is(RuntimeExecSequence::EtxtbsyThenCheckpointAfter150Ms))
        }
        K::InterpreterAuthorizationUnavailable => {
            final_outcome_is(O::InterpreterAuthorizationUnavailable)
        }
        K::HandleExecutionUnavailable => final_outcome_is(O::HandleExecutionUnavailable),
        K::SupervisionSetupFailed => final_outcome_is(O::SupervisionSetupFailed),
        K::SpawnFailed => final_outcome_is(O::ExecFailed),
        K::TransitiveExecutionDenied
        | K::Timeout
        | K::OutputLimitExceeded
        | K::OutputReadFailed
        | K::NonzeroExit
        | K::TerminatedBySignal
        | K::InvalidUtf8
        | K::EmptyOutput
        | K::UnparseableVersion
        | K::AmbiguousVersion => final_outcome_is(O::ExecStarted),
        K::BareEaccesExhausted => {
            form == RuntimeCommandForm::UnixBare
                && no_final_identity
                && attempts
                    .iter()
                    .any(|attempt| attempt.outcome == O::ExecEacces)
                && attempts
                    .iter()
                    .rev()
                    .find(|attempt| {
                        !matches!(
                            attempt.outcome,
                            O::Absent | O::NotRegular | O::NotExecutable
                        )
                    })
                    .is_some_and(|attempt| attempt.outcome == O::ExecEacces)
        }
        K::TerminationFailed | K::ReapFailed | K::OutputDrainFailed => false,
    }
}
