//! Ordered candidate resolution and terminal evidence mapping.

use super::authorization::TargetAuthorization;
use super::candidate::{CandidateObservation, RetainedExecutable};
use super::checkpoint::PreSpawnCheckpoint;
use super::command::{PreparedCommand, ResolvedCandidate};
use super::environment::SelectedEnvironment;
use super::executable::RetainedWorkingDirectory;
use super::{
    ConfiguredRuntimeExecutable, RuntimeEnvelopeEvidence, RuntimeFingerprintOptions,
    RuntimeFingerprintProduceError, RUNTIME_FINGERPRINT_MAX_RESOLUTION_CANDIDATES,
};
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, RuntimeCommandForm, RuntimeExecSequence,
    RuntimeExecutableIdentity, RuntimeProbeFailure, RuntimeProbeFailureDetail,
    RuntimeProbeFailureKind, RuntimeResolutionAttempt, RuntimeResolutionAttemptOutcome,
};
use harness_core::stack::AgentStackSourceScope;
use std::time::Instant;

pub(super) enum ResolutionDisposition {
    Complete(Box<AgentStackFingerprintEnvelope>),
    Selected {
        candidate_index: usize,
        candidate: ResolvedCandidate,
        executable: RetainedExecutable,
        attempts: Vec<RuntimeResolutionAttempt>,
    },
}

#[derive(Clone, Copy)]
pub(super) struct ResolutionContext<'a> {
    pub(super) configured: &'a ConfiguredRuntimeExecutable,
    pub(super) options: &'a RuntimeFingerprintOptions,
    pub(super) environment: &'a SelectedEnvironment,
    pub(super) command: &'a PreparedCommand,
    pub(super) working_directory: &'a RetainedWorkingDirectory,
    pub(super) deadline: Instant,
    pub(super) registry: &'a super::registry::OwnerRegistry,
    pub(super) stop_requested: &'a std::sync::atomic::AtomicBool,
}

pub(super) struct ResolutionCursor {
    pub(super) next_candidate_index: usize,
    pub(super) attempts: Vec<RuntimeResolutionAttempt>,
}

pub(super) fn resolve(
    context: ResolutionContext<'_>,
) -> Result<ResolutionDisposition, RuntimeFingerprintProduceError> {
    let candidate_capacity = context.command.candidate_capacity();
    resolve_from(context, 0, Vec::with_capacity(candidate_capacity), false)
}

pub(super) fn resume_after_eacces(
    context: ResolutionContext<'_>,
    cursor: ResolutionCursor,
) -> Result<ResolutionDisposition, RuntimeFingerprintProduceError> {
    resolve_from(context, cursor.next_candidate_index, cursor.attempts, true)
}

fn resolve_from(
    context: ResolutionContext<'_>,
    start_index: usize,
    mut attempts: Vec<RuntimeResolutionAttempt>,
    saw_exec_eacces: bool,
) -> Result<ResolutionDisposition, RuntimeFingerprintProduceError> {
    let ResolutionContext {
        configured,
        options,
        environment,
        command,
        working_directory,
        deadline,
        registry,
        stop_requested,
    } = context;
    let mut candidate_index = start_index;
    while candidate_index < RUNTIME_FINGERPRINT_MAX_RESOLUTION_CANDIDATES {
        let Some(candidate) = command.candidate(candidate_index)? else {
            break;
        };
        let candidate = &candidate;
        super::probe::ensure_owner_running(stop_requested)?;
        match super::candidate::observe_candidate(candidate, working_directory, deadline, registry)?
        {
            CandidateObservation::Absent => {
                attempts.push(attempt(candidate, RuntimeResolutionAttemptOutcome::Absent)?);
                if command.command_form != RuntimeCommandForm::UnixBare {
                    return complete_with(
                        configured,
                        environment,
                        command,
                        working_directory,
                        attempts,
                        RuntimeProbeFailure::new(RuntimeProbeFailureKind::PathNotFound)?,
                    );
                }
            }
            CandidateObservation::NotRegular => {
                attempts.push(attempt(
                    candidate,
                    RuntimeResolutionAttemptOutcome::NotRegular,
                )?);
                if command.command_form != RuntimeCommandForm::UnixBare {
                    return complete_with(
                        configured,
                        environment,
                        command,
                        working_directory,
                        attempts,
                        RuntimeProbeFailure::new(RuntimeProbeFailureKind::NotRegularFile)?,
                    );
                }
            }
            CandidateObservation::NotExecutable => {
                attempts.push(attempt(
                    candidate,
                    RuntimeResolutionAttemptOutcome::NotExecutable,
                )?);
                if command.command_form != RuntimeCommandForm::UnixBare {
                    return complete_with(
                        configured,
                        environment,
                        command,
                        working_directory,
                        attempts,
                        RuntimeProbeFailure::new(RuntimeProbeFailureKind::NotExecutable)?,
                    );
                }
            }
            CandidateObservation::InspectionFailed(kind) => {
                attempts.push(attempt(
                    candidate,
                    RuntimeResolutionAttemptOutcome::InspectionFailed,
                )?);
                return complete_with(
                    configured,
                    environment,
                    command,
                    working_directory,
                    attempts,
                    RuntimeProbeFailure::new(kind)?,
                );
            }
            CandidateObservation::UnsupportedFormat => {
                attempts.push(attempt(
                    candidate,
                    RuntimeResolutionAttemptOutcome::InterpreterAuthorizationUnavailable,
                )?);
                return complete_with(
                    configured,
                    environment,
                    command,
                    working_directory,
                    attempts,
                    RuntimeProbeFailure::new(
                        RuntimeProbeFailureKind::InterpreterAuthorizationUnavailable,
                    )?,
                );
            }
            CandidateObservation::Retained(executable) => {
                if configured.configured_source().source().scope()
                    == AgentStackSourceScope::Repository
                {
                    attempts.push(attempt(
                        candidate,
                        RuntimeResolutionAttemptOutcome::InspectionTarget,
                    )?);
                    return complete_inspection_with(
                        configured,
                        environment,
                        command,
                        working_directory,
                        attempts,
                        &executable,
                        RuntimeProbeFailure::with_detail(
                            RuntimeProbeFailureKind::ProbeNotAuthorized,
                            RuntimeProbeFailureDetail::ConfigurationSourceRepository,
                        )?,
                    );
                }
                let Some(boundaries) = options.repository_boundaries() else {
                    attempts.push(attempt(
                        candidate,
                        RuntimeResolutionAttemptOutcome::AuthorizationUnavailable,
                    )?);
                    return complete_with(
                        configured,
                        environment,
                        command,
                        working_directory,
                        attempts,
                        RuntimeProbeFailure::with_detail(
                            RuntimeProbeFailureKind::TargetAuthorizationUnavailable,
                            RuntimeProbeFailureDetail::BoundaryUnprovable,
                        )?,
                    );
                };
                match super::authorization::authorize_target(
                    &executable,
                    boundaries,
                    deadline,
                    registry,
                )? {
                    TargetAuthorization::Authorized => {}
                    TargetAuthorization::ResolvedTargetRepository => {
                        attempts.push(attempt(
                            candidate,
                            RuntimeResolutionAttemptOutcome::InspectionTarget,
                        )?);
                        return complete_inspection_with(
                            configured,
                            environment,
                            command,
                            working_directory,
                            attempts,
                            &executable,
                            RuntimeProbeFailure::with_detail(
                                RuntimeProbeFailureKind::ProbeNotAuthorized,
                                RuntimeProbeFailureDetail::ResolvedTargetRepository,
                            )?,
                        );
                    }
                    authorization => {
                        let detail = match authorization {
                            TargetAuthorization::BoundaryUnprovable => {
                                RuntimeProbeFailureDetail::BoundaryUnprovable
                            }
                            TargetAuthorization::LinkCountUnprovable => {
                                RuntimeProbeFailureDetail::LinkCountUnprovable
                            }
                            TargetAuthorization::UnlinkedTarget => {
                                RuntimeProbeFailureDetail::UnlinkedTarget
                            }
                            TargetAuthorization::MultipleHardLinks => {
                                RuntimeProbeFailureDetail::MultipleHardLinks
                            }
                            TargetAuthorization::Authorized
                            | TargetAuthorization::ResolvedTargetRepository => {
                                return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
                            }
                        };
                        attempts.push(attempt(
                            candidate,
                            RuntimeResolutionAttemptOutcome::AuthorizationUnavailable,
                        )?);
                        return complete_with(
                            configured,
                            environment,
                            command,
                            working_directory,
                            attempts,
                            RuntimeProbeFailure::with_detail(
                                RuntimeProbeFailureKind::TargetAuthorizationUnavailable,
                                detail,
                            )?,
                        );
                    }
                }
                match super::checkpoint::pre_spawn(
                    candidate,
                    working_directory,
                    &executable,
                    boundaries,
                    deadline,
                    registry,
                )? {
                    PreSpawnCheckpoint::Consistent => {}
                    PreSpawnCheckpoint::IdentityChanged => {
                        attempts.push(attempt(
                            candidate,
                            RuntimeResolutionAttemptOutcome::InspectionFailed,
                        )?);
                        return complete_with(
                            configured,
                            environment,
                            command,
                            working_directory,
                            attempts,
                            RuntimeProbeFailure::new(RuntimeProbeFailureKind::IdentityChanged)?,
                        );
                    }
                    PreSpawnCheckpoint::ResolvedTargetRepository => {
                        attempts.push(attempt(
                            candidate,
                            RuntimeResolutionAttemptOutcome::InspectionTarget,
                        )?);
                        return complete_inspection_with(
                            configured,
                            environment,
                            command,
                            working_directory,
                            attempts,
                            &executable,
                            RuntimeProbeFailure::with_detail(
                                RuntimeProbeFailureKind::ProbeNotAuthorized,
                                RuntimeProbeFailureDetail::ResolvedTargetRepository,
                            )?,
                        );
                    }
                    checkpoint => {
                        let detail = match checkpoint {
                            PreSpawnCheckpoint::BoundaryUnprovable => {
                                RuntimeProbeFailureDetail::BoundaryUnprovable
                            }
                            PreSpawnCheckpoint::LinkCountUnprovable => {
                                RuntimeProbeFailureDetail::LinkCountUnprovable
                            }
                            PreSpawnCheckpoint::UnlinkedTarget => {
                                RuntimeProbeFailureDetail::UnlinkedTarget
                            }
                            PreSpawnCheckpoint::MultipleHardLinks => {
                                RuntimeProbeFailureDetail::MultipleHardLinks
                            }
                            PreSpawnCheckpoint::Consistent
                            | PreSpawnCheckpoint::IdentityChanged
                            | PreSpawnCheckpoint::ResolvedTargetRepository => {
                                return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
                            }
                        };
                        attempts.push(attempt(
                            candidate,
                            RuntimeResolutionAttemptOutcome::AuthorizationUnavailable,
                        )?);
                        return complete_with(
                            configured,
                            environment,
                            command,
                            working_directory,
                            attempts,
                            RuntimeProbeFailure::with_detail(
                                RuntimeProbeFailureKind::TargetAuthorizationUnavailable,
                                detail,
                            )?,
                        );
                    }
                }
                return Ok(ResolutionDisposition::Selected {
                    candidate_index,
                    candidate: candidate.clone(),
                    executable,
                    attempts,
                });
            }
        }
        candidate_index += 1;
    }
    let failure = if command.has_candidate(RUNTIME_FINGERPRINT_MAX_RESOLUTION_CANDIDATES) {
        RuntimeProbeFailureKind::CandidateLimitExceeded
    } else if saw_exec_eacces {
        RuntimeProbeFailureKind::BareEaccesExhausted
    } else {
        RuntimeProbeFailureKind::PathNotFound
    };
    complete_with(
        configured,
        environment,
        command,
        working_directory,
        attempts,
        RuntimeProbeFailure::new(failure)?,
    )
}

fn attempt(
    candidate: &ResolvedCandidate,
    outcome: RuntimeResolutionAttemptOutcome,
) -> Result<RuntimeResolutionAttempt, RuntimeFingerprintProduceError> {
    Ok(RuntimeResolutionAttempt::new(
        candidate.candidate_digest.clone(),
        outcome,
        RuntimeExecSequence::None,
        None,
    )?)
}

fn complete_with(
    configured: &ConfiguredRuntimeExecutable,
    environment: &SelectedEnvironment,
    command: &PreparedCommand,
    working_directory: &RetainedWorkingDirectory,
    attempts: Vec<RuntimeResolutionAttempt>,
    failure: RuntimeProbeFailure,
) -> Result<ResolutionDisposition, RuntimeFingerprintProduceError> {
    complete_with_identity(
        configured,
        environment,
        command,
        working_directory,
        attempts,
        None,
        failure,
    )
}

fn complete_inspection_with(
    configured: &ConfiguredRuntimeExecutable,
    environment: &SelectedEnvironment,
    command: &PreparedCommand,
    working_directory: &RetainedWorkingDirectory,
    attempts: Vec<RuntimeResolutionAttempt>,
    retained: &RetainedExecutable,
    failure: RuntimeProbeFailure,
) -> Result<ResolutionDisposition, RuntimeFingerprintProduceError> {
    complete_with_identity(
        configured,
        environment,
        command,
        working_directory,
        attempts,
        Some(RuntimeExecutableIdentity::new(
            retained.file_size_bytes,
            Some(retained.unix_mode),
            retained.executable_sha256.clone(),
            false,
            false,
        )),
        failure,
    )
}

#[allow(clippy::too_many_arguments)]
fn complete_with_identity(
    configured: &ConfiguredRuntimeExecutable,
    environment: &SelectedEnvironment,
    command: &PreparedCommand,
    working_directory: &RetainedWorkingDirectory,
    attempts: Vec<RuntimeResolutionAttempt>,
    executable: Option<RuntimeExecutableIdentity>,
    failure: RuntimeProbeFailure,
) -> Result<ResolutionDisposition, RuntimeFingerprintProduceError> {
    Ok(ResolutionDisposition::Complete(Box::new(
        super::finish_runtime_envelope(
            configured,
            RuntimeEnvelopeEvidence {
                command_form: command.command_form,
                configured_command_digest: command.configured_command_digest.clone(),
                working_directory_digest: command.working_directory_digest.clone(),
                working_directory_identity_digest: working_directory.identity_digest.clone(),
                resolution_attempts: attempts,
                executable,
                version: None,
                environment: environment.facts.clone(),
                failures: vec![failure],
            },
        )?,
    )))
}
