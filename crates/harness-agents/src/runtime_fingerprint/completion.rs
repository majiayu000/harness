//! Checked completion of a target that reached the verified exec stop.

use super::candidate::RetainedExecutable;
use super::checkpoint::PreSpawnCheckpoint;
use super::environment::{RuntimeOutputClassification, SelectedEnvironment};
use super::executable::{PreparedCommand, ResolvedCandidate, RetainedWorkingDirectory};
use super::supervision::SupervisionOutcome;
use super::target::StoppedTarget;
use super::{
    ConfiguredRuntimeExecutable, RuntimeEnvelopeEvidence, RuntimeFingerprintOptions,
    RuntimeFingerprintProduceError,
};
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, RuntimeExecSequence, RuntimeExecutableIdentity,
    RuntimeExecutionContext, RuntimeProbeFailure, RuntimeProbeFailureKind,
    RuntimeResolutionAttempt, RuntimeResolutionAttemptOutcome, RuntimeVersionFacts,
};
use std::time::Instant;

pub(super) struct InitialCompletion<'a> {
    pub(super) configured: &'a ConfiguredRuntimeExecutable,
    pub(super) options: &'a RuntimeFingerprintOptions,
    pub(super) environment: &'a SelectedEnvironment,
    pub(super) command: &'a PreparedCommand,
    pub(super) working_directory: &'a RetainedWorkingDirectory,
    pub(super) candidate: &'a ResolvedCandidate,
    pub(super) executable: &'a RetainedExecutable,
    pub(super) registry: &'a super::registry::OwnerRegistry,
    pub(super) stop_requested: &'a std::sync::atomic::AtomicBool,
}

pub(super) fn complete_initial(
    context: InitialCompletion<'_>,
    attempts: Vec<RuntimeResolutionAttempt>,
    stopped: StoppedTarget,
    deadline: Instant,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    complete(
        context,
        attempts,
        stopped,
        deadline,
        RuntimeExecSequence::Single,
    )
}

pub(super) fn complete_retry(
    context: InitialCompletion<'_>,
    attempts: Vec<RuntimeResolutionAttempt>,
    stopped: StoppedTarget,
    deadline: Instant,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    complete(
        context,
        attempts,
        stopped,
        deadline,
        RuntimeExecSequence::EtxtbsyThenCheckpointAfter150Ms,
    )
}

fn complete(
    context: InitialCompletion<'_>,
    mut attempts: Vec<RuntimeResolutionAttempt>,
    stopped: StoppedTarget,
    deadline: Instant,
    sequence: RuntimeExecSequence,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    let cleanup_deadline = Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE;
    let exec_stop = match super::exec_stop::verify(
        stopped.pid(),
        context.executable,
        deadline,
        context.registry,
    ) {
        Ok(checkpoint) => checkpoint,
        Err(error) => {
            stopped.terminate_without_resume(cleanup_deadline)?;
            return Err(error);
        }
    };
    if exec_stop == super::exec_stop::ExecStopCheckpoint::IdentityChanged {
        stopped.terminate_without_resume(cleanup_deadline)?;
        attempts.push(attempt(
            context.candidate,
            RuntimeResolutionAttemptOutcome::ExecVerificationFailed,
            sequence,
        )?);
        return finish(
            &context,
            attempts,
            None,
            None,
            vec![RuntimeProbeFailure::new(
                RuntimeProbeFailureKind::IdentityChanged,
            )?],
        );
    }

    attempts.push(attempt(
        context.candidate,
        RuntimeResolutionAttemptOutcome::ExecStarted,
        sequence,
    )?);
    let outcome = super::supervision::run(
        stopped,
        context.options.max_output_bytes(),
        deadline,
        context.stop_requested,
    )?;
    let (stdout, stderr, termination) = match outcome {
        SupervisionOutcome::Captured {
            stdout,
            stderr,
            termination,
        } => (stdout, stderr, termination),
        SupervisionOutcome::Failed(failure) => {
            return finish(&context, attempts, None, None, vec![failure]);
        }
    };

    let boundaries = context
        .options
        .repository_boundaries()
        .ok_or(RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    match super::checkpoint::post_reap(
        context.candidate,
        context.working_directory,
        context.executable,
        boundaries,
        deadline,
        context.registry,
    )? {
        PreSpawnCheckpoint::Consistent => {}
        PreSpawnCheckpoint::BoundaryUnprovable | PreSpawnCheckpoint::LinkCountUnprovable => {
            return finish(
                &context,
                attempts,
                None,
                None,
                vec![RuntimeProbeFailure::new(
                    RuntimeProbeFailureKind::MetadataUnavailable,
                )?],
            );
        }
        PreSpawnCheckpoint::IdentityChanged
        | PreSpawnCheckpoint::ResolvedTargetRepository
        | PreSpawnCheckpoint::UnlinkedTarget
        | PreSpawnCheckpoint::MultipleHardLinks => {
            return finish(
                &context,
                attempts,
                None,
                None,
                vec![RuntimeProbeFailure::new(
                    RuntimeProbeFailureKind::IdentityChanged,
                )?],
            );
        }
    }

    let identity = RuntimeExecutableIdentity::new(
        context.executable.file_size_bytes,
        Some(context.executable.unix_mode),
        context.executable.executable_sha256.clone(),
        true,
        true,
    );
    match super::environment::classify_completed_runtime_output(
        context.configured.runtime_kind(),
        &stdout,
        &stderr,
        termination,
    )? {
        RuntimeOutputClassification::Version(version) => finish(
            &context,
            attempts,
            Some(identity),
            Some(version),
            Vec::new(),
        ),
        RuntimeOutputClassification::Failure(failure) => {
            finish(&context, attempts, Some(identity), None, vec![failure])
        }
    }
}

pub(super) fn attempt(
    candidate: &ResolvedCandidate,
    outcome: RuntimeResolutionAttemptOutcome,
    sequence: RuntimeExecSequence,
) -> Result<RuntimeResolutionAttempt, RuntimeFingerprintProduceError> {
    Ok(RuntimeResolutionAttempt::new(
        candidate.candidate_digest.clone(),
        outcome,
        sequence,
        (sequence != RuntimeExecSequence::None)
            .then_some(RuntimeExecutionContext::LinuxFdCloexecExecveatEmptyPathFd10),
    )?)
}

pub(super) fn finish(
    context: &InitialCompletion<'_>,
    attempts: Vec<RuntimeResolutionAttempt>,
    executable: Option<RuntimeExecutableIdentity>,
    version: Option<RuntimeVersionFacts>,
    failures: Vec<RuntimeProbeFailure>,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    super::finish_runtime_envelope(
        context.configured,
        RuntimeEnvelopeEvidence {
            command_form: context.command.command_form,
            configured_command_digest: context.command.configured_command_digest.clone(),
            working_directory_digest: context.command.working_directory_digest.clone(),
            working_directory_identity_digest: context.working_directory.identity_digest.clone(),
            resolution_attempts: attempts,
            executable,
            version,
            environment: context.environment.facts.clone(),
            failures,
        },
    )
}
