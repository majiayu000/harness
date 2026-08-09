//! Ordered candidate resolution and terminal evidence mapping.

use super::authorization::TargetAuthorization;
use super::candidate::{CandidateObservation, RetainedExecutable};
use super::checkpoint::PreSpawnCheckpoint;
use super::environment::SelectedEnvironment;
use super::executable::{PreparedCommand, ResolvedCandidate, RetainedWorkingDirectory};
use super::{
    ConfiguredRuntimeExecutable, RuntimeEnvelopeEvidence, RuntimeFingerprintOptions,
    RuntimeFingerprintProduceError,
};
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, RuntimeCommandForm, RuntimeExecSequence, RuntimeProbeFailure,
    RuntimeProbeFailureDetail, RuntimeProbeFailureKind, RuntimeResolutionAttempt,
    RuntimeResolutionAttemptOutcome,
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
    let candidate_capacity = context.command.candidates.len();
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
    for (candidate_index, candidate) in command.candidates.iter().enumerate().skip(start_index) {
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
                    return complete_with(
                        configured,
                        environment,
                        command,
                        working_directory,
                        attempts,
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
                        return complete_with(
                            configured,
                            environment,
                            command,
                            working_directory,
                            attempts,
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
                        return complete_with(
                            configured,
                            environment,
                            command,
                            working_directory,
                            attempts,
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
    }
    let failure = if command.candidate_limit_exceeded {
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
    Ok(ResolutionDisposition::Complete(Box::new(
        super::finish_runtime_envelope(
            configured,
            RuntimeEnvelopeEvidence {
                command_form: command.command_form,
                configured_command_digest: command.configured_command_digest.clone(),
                working_directory_digest: command.working_directory_digest.clone(),
                working_directory_identity_digest: working_directory.identity_digest.clone(),
                resolution_attempts: attempts,
                executable: None,
                version: None,
                environment: environment.facts.clone(),
                failures: vec![failure],
            },
        )?,
    )))
}
