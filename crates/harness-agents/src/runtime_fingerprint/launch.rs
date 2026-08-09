//! Initial and retry target launch outcome mapping.

use super::authorization::TargetAuthorization;
use super::checkpoint::PreSpawnCheckpoint;
use super::completion::InitialCompletion;
use super::RuntimeFingerprintProduceError;
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, RuntimeCommandForm, RuntimeExecSequence, RuntimeProbeFailure,
    RuntimeProbeFailureDetail, RuntimeProbeFailureKind, RuntimeResolutionAttempt,
    RuntimeResolutionAttemptOutcome,
};
use std::time::Instant;

pub(super) enum InitialLaunch {
    Complete(Box<AgentStackFingerprintEnvelope>),
    ContinueAfterEacces(Vec<RuntimeResolutionAttempt>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitialExecFailure {
    ContinueAfterEacces,
    RetryAfterEtxtbsy,
    Terminal {
        outcome: RuntimeResolutionAttemptOutcome,
        kind: RuntimeProbeFailureKind,
    },
}

pub(super) fn launch_initial(
    context: InitialCompletion<'_>,
    mut attempts: Vec<RuntimeResolutionAttempt>,
    deadline: Instant,
) -> Result<InitialLaunch, RuntimeFingerprintProduceError> {
    super::probe::ensure_owner_running(context.stop_requested)?;
    let target = super::target::start_initial(
        context.configured,
        context.environment,
        context.working_directory,
        context.executable,
        deadline,
        context.registry,
    )?;
    match target {
        super::target::TargetStart::ExecStopped(stopped) => Ok(InitialLaunch::Complete(Box::new(
            super::completion::complete_initial(context, attempts, stopped, deadline)?,
        ))),
        super::target::TargetStart::SetupFailed(detail) => {
            attempts.push(super::completion::attempt(
                context.candidate,
                RuntimeResolutionAttemptOutcome::SupervisionSetupFailed,
                RuntimeExecSequence::None,
            )?);
            Ok(InitialLaunch::Complete(Box::new(
                super::completion::finish(
                    &context,
                    attempts,
                    None,
                    None,
                    vec![RuntimeProbeFailure::with_detail(
                        RuntimeProbeFailureKind::SupervisionSetupFailed,
                        detail,
                    )?],
                )?,
            )))
        }
        super::target::TargetStart::ExecFailed(errno) => {
            match classify_initial_exec_failure(errno, context.command.command_form) {
                InitialExecFailure::ContinueAfterEacces => {
                    attempts.push(super::completion::attempt(
                        context.candidate,
                        RuntimeResolutionAttemptOutcome::ExecEacces,
                        RuntimeExecSequence::Single,
                    )?);
                    Ok(InitialLaunch::ContinueAfterEacces(attempts))
                }
                InitialExecFailure::RetryAfterEtxtbsy => {
                    retry_after_etxtbsy(context, attempts, deadline)
                }
                InitialExecFailure::Terminal { outcome, kind } => {
                    attempts.push(super::completion::attempt(
                        context.candidate,
                        outcome,
                        RuntimeExecSequence::Single,
                    )?);
                    Ok(InitialLaunch::Complete(Box::new(
                        super::completion::finish(
                            &context,
                            attempts,
                            None,
                            None,
                            vec![RuntimeProbeFailure::new(kind)?],
                        )?,
                    )))
                }
            }
        }
    }
}

fn retry_after_etxtbsy(
    context: InitialCompletion<'_>,
    attempts: Vec<RuntimeResolutionAttempt>,
    deadline: Instant,
) -> Result<InitialLaunch, RuntimeFingerprintProduceError> {
    super::probe::ensure_owner_running(context.stop_requested)?;
    let retry_at = Instant::now() + super::RUNTIME_FINGERPRINT_ETXTBSY_RETRY_DELAY;
    if retry_at >= deadline {
        return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
    }
    std::thread::sleep(super::RUNTIME_FINGERPRINT_ETXTBSY_RETRY_DELAY);
    super::probe::ensure_owner_running(context.stop_requested)?;
    if Instant::now() >= deadline {
        return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
    }
    let boundaries = context
        .options
        .repository_boundaries()
        .ok_or(RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    match super::authorization::authorize_target(
        context.executable,
        boundaries,
        deadline,
        context.registry,
    )? {
        TargetAuthorization::Authorized => {}
        TargetAuthorization::ResolvedTargetRepository => {
            return retry_failure(
                &context,
                attempts,
                RuntimeResolutionAttemptOutcome::RetryNotAuthorized,
                RuntimeProbeFailure::with_detail(
                    RuntimeProbeFailureKind::ProbeNotAuthorized,
                    RuntimeProbeFailureDetail::ResolvedTargetRepository,
                )?,
            );
        }
        authorization => {
            return retry_failure(
                &context,
                attempts,
                RuntimeResolutionAttemptOutcome::RetryAuthorizationUnavailable,
                RuntimeProbeFailure::with_detail(
                    RuntimeProbeFailureKind::TargetAuthorizationUnavailable,
                    authorization_detail(authorization)?,
                )?,
            );
        }
    }
    match super::checkpoint::pre_spawn(
        context.candidate,
        context.working_directory,
        context.executable,
        boundaries,
        deadline,
        context.registry,
    )? {
        PreSpawnCheckpoint::Consistent => {}
        PreSpawnCheckpoint::IdentityChanged => {
            return retry_failure(
                &context,
                attempts,
                RuntimeResolutionAttemptOutcome::InspectionFailed,
                RuntimeProbeFailure::new(RuntimeProbeFailureKind::IdentityChanged)?,
            );
        }
        PreSpawnCheckpoint::ResolvedTargetRepository => {
            return retry_failure(
                &context,
                attempts,
                RuntimeResolutionAttemptOutcome::RetryNotAuthorized,
                RuntimeProbeFailure::with_detail(
                    RuntimeProbeFailureKind::ProbeNotAuthorized,
                    RuntimeProbeFailureDetail::ResolvedTargetRepository,
                )?,
            );
        }
        checkpoint => {
            return retry_failure(
                &context,
                attempts,
                RuntimeResolutionAttemptOutcome::RetryAuthorizationUnavailable,
                RuntimeProbeFailure::with_detail(
                    RuntimeProbeFailureKind::TargetAuthorizationUnavailable,
                    checkpoint_detail(checkpoint)?,
                )?,
            );
        }
    }

    let target = super::target::start_retry(
        context.configured,
        context.environment,
        context.working_directory,
        context.executable,
        deadline,
        context.registry,
    )?;
    let sequence = RuntimeExecSequence::EtxtbsyThenCheckpointAfter150Ms;
    match target {
        super::target::TargetStart::ExecStopped(stopped) => Ok(InitialLaunch::Complete(Box::new(
            super::completion::complete_retry(context, attempts, stopped, deadline)?,
        ))),
        super::target::TargetStart::SetupFailed(detail) => retry_failure(
            &context,
            attempts,
            RuntimeResolutionAttemptOutcome::SupervisionSetupFailed,
            RuntimeProbeFailure::with_detail(
                RuntimeProbeFailureKind::SupervisionSetupFailed,
                detail,
            )?,
        ),
        super::target::TargetStart::ExecFailed(errno) => {
            let (outcome, failure) = terminal_exec_failure(errno)?;
            let mut attempts = attempts;
            attempts.push(super::completion::attempt(
                context.candidate,
                outcome,
                sequence,
            )?);
            Ok(InitialLaunch::Complete(Box::new(
                super::completion::finish(&context, attempts, None, None, vec![failure])?,
            )))
        }
    }
}

fn retry_failure(
    context: &InitialCompletion<'_>,
    mut attempts: Vec<RuntimeResolutionAttempt>,
    outcome: RuntimeResolutionAttemptOutcome,
    failure: RuntimeProbeFailure,
) -> Result<InitialLaunch, RuntimeFingerprintProduceError> {
    attempts.push(super::completion::attempt(
        context.candidate,
        outcome,
        RuntimeExecSequence::EtxtbsyThenCheckpointAfter150Ms,
    )?);
    Ok(InitialLaunch::Complete(Box::new(
        super::completion::finish(context, attempts, None, None, vec![failure])?,
    )))
}

fn authorization_detail(
    authorization: TargetAuthorization,
) -> Result<RuntimeProbeFailureDetail, RuntimeFingerprintProduceError> {
    match authorization {
        TargetAuthorization::BoundaryUnprovable => {
            Ok(RuntimeProbeFailureDetail::BoundaryUnprovable)
        }
        TargetAuthorization::LinkCountUnprovable => {
            Ok(RuntimeProbeFailureDetail::LinkCountUnprovable)
        }
        TargetAuthorization::UnlinkedTarget => Ok(RuntimeProbeFailureDetail::UnlinkedTarget),
        TargetAuthorization::MultipleHardLinks => Ok(RuntimeProbeFailureDetail::MultipleHardLinks),
        TargetAuthorization::Authorized | TargetAuthorization::ResolvedTargetRepository => {
            Err(RuntimeFingerprintProduceError::InvalidLaunchContext)
        }
    }
}

fn checkpoint_detail(
    checkpoint: PreSpawnCheckpoint,
) -> Result<RuntimeProbeFailureDetail, RuntimeFingerprintProduceError> {
    match checkpoint {
        PreSpawnCheckpoint::BoundaryUnprovable => Ok(RuntimeProbeFailureDetail::BoundaryUnprovable),
        PreSpawnCheckpoint::LinkCountUnprovable => {
            Ok(RuntimeProbeFailureDetail::LinkCountUnprovable)
        }
        PreSpawnCheckpoint::UnlinkedTarget => Ok(RuntimeProbeFailureDetail::UnlinkedTarget),
        PreSpawnCheckpoint::MultipleHardLinks => Ok(RuntimeProbeFailureDetail::MultipleHardLinks),
        PreSpawnCheckpoint::Consistent
        | PreSpawnCheckpoint::IdentityChanged
        | PreSpawnCheckpoint::ResolvedTargetRepository => {
            Err(RuntimeFingerprintProduceError::InvalidLaunchContext)
        }
    }
}

fn terminal_exec_failure(
    errno: libc::c_int,
) -> Result<(RuntimeResolutionAttemptOutcome, RuntimeProbeFailure), RuntimeFingerprintProduceError>
{
    let (outcome, kind) = terminal_exec_failure_kind(errno);
    Ok((outcome, RuntimeProbeFailure::new(kind)?))
}

fn classify_initial_exec_failure(
    errno: libc::c_int,
    form: RuntimeCommandForm,
) -> InitialExecFailure {
    if errno == libc::EACCES && form == RuntimeCommandForm::UnixBare {
        InitialExecFailure::ContinueAfterEacces
    } else if errno == libc::ETXTBSY {
        InitialExecFailure::RetryAfterEtxtbsy
    } else {
        let (outcome, kind) = terminal_exec_failure_kind(errno);
        InitialExecFailure::Terminal { outcome, kind }
    }
}

fn terminal_exec_failure_kind(
    errno: libc::c_int,
) -> (RuntimeResolutionAttemptOutcome, RuntimeProbeFailureKind) {
    if matches!(errno, libc::ENOENT | libc::ENOTDIR) {
        (
            RuntimeResolutionAttemptOutcome::InterpreterAuthorizationUnavailable,
            RuntimeProbeFailureKind::InterpreterAuthorizationUnavailable,
        )
    } else if matches!(errno, libc::ENOSYS | libc::EPERM | libc::EINVAL) {
        (
            RuntimeResolutionAttemptOutcome::HandleExecutionUnavailable,
            RuntimeProbeFailureKind::HandleExecutionUnavailable,
        )
    } else {
        (
            RuntimeResolutionAttemptOutcome::ExecFailed,
            RuntimeProbeFailureKind::SpawnFailed,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_and_retry_exec_errno_policies_are_closed() {
        assert_eq!(
            classify_initial_exec_failure(libc::EACCES, RuntimeCommandForm::UnixBare),
            InitialExecFailure::ContinueAfterEacces
        );
        assert_eq!(
            classify_initial_exec_failure(libc::ETXTBSY, RuntimeCommandForm::UnixAbsolute),
            InitialExecFailure::RetryAfterEtxtbsy
        );
        assert_eq!(
            classify_initial_exec_failure(libc::EACCES, RuntimeCommandForm::UnixQualified),
            InitialExecFailure::Terminal {
                outcome: RuntimeResolutionAttemptOutcome::ExecFailed,
                kind: RuntimeProbeFailureKind::SpawnFailed,
            }
        );
        assert_eq!(
            terminal_exec_failure_kind(libc::ETXTBSY),
            (
                RuntimeResolutionAttemptOutcome::ExecFailed,
                RuntimeProbeFailureKind::SpawnFailed,
            )
        );
        assert_eq!(
            terminal_exec_failure_kind(libc::ENOENT),
            (
                RuntimeResolutionAttemptOutcome::InterpreterAuthorizationUnavailable,
                RuntimeProbeFailureKind::InterpreterAuthorizationUnavailable,
            )
        );
        assert_eq!(
            terminal_exec_failure_kind(libc::ENOSYS),
            (
                RuntimeResolutionAttemptOutcome::HandleExecutionUnavailable,
                RuntimeProbeFailureKind::HandleExecutionUnavailable,
            )
        );
    }
}
