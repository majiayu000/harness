//! Checked completion of a target that reached the verified exec stop.

use super::candidate::RetainedExecutable;
use super::checkpoint::PreSpawnCheckpoint;
use super::command::{PreparedCommand, ResolvedCandidate};
use super::environment::{RuntimeOutputClassification, SelectedEnvironment};
use super::executable::RetainedWorkingDirectory;
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
    let exec_stop = match super::exec_stop::verify(
        stopped.pid(),
        context.executable,
        deadline,
        context.registry,
    ) {
        Ok(checkpoint) => checkpoint,
        Err(error) => {
            stopped.terminate_without_resume(
                Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE,
            )?;
            return Err(error);
        }
    };
    if exec_stop == super::exec_stop::ExecStopCheckpoint::IdentityChanged {
        stopped.terminate_without_resume(
            Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE,
        )?;
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
    let (captured, supervision_failure) = match outcome {
        SupervisionOutcome::Captured {
            stdout,
            stderr,
            termination,
        } => (Some((stdout, stderr, termination)), None),
        SupervisionOutcome::Failed {
            failures,
            target_reaped,
        } => (None, Some((failures, target_reaped))),
    };

    if let Some((failures, false)) = supervision_failure.as_ref() {
        return finish(&context, attempts, None, None, failures.clone());
    }

    let boundaries = context
        .options
        .repository_boundaries()
        .ok_or(RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let post_reap_deadline = if supervision_failure.is_some() {
        Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE
    } else {
        deadline
    };
    match super::checkpoint::post_reap(
        context.candidate,
        context.working_directory,
        context.executable,
        boundaries,
        post_reap_deadline,
        context.registry,
    )? {
        PreSpawnCheckpoint::Consistent => {}
        PreSpawnCheckpoint::BoundaryUnprovable | PreSpawnCheckpoint::LinkCountUnprovable => {
            return finish(
                &context,
                attempts,
                None,
                None,
                post_reap_failure(
                    supervision_failure
                        .as_ref()
                        .map(|(failures, _)| failures.as_slice()),
                    RuntimeProbeFailureKind::MetadataUnavailable,
                )?,
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
                post_reap_failure(
                    supervision_failure
                        .as_ref()
                        .map(|(failures, _)| failures.as_slice()),
                    RuntimeProbeFailureKind::IdentityChanged,
                )?,
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
    if let Some((failures, true)) = supervision_failure {
        return finish(&context, attempts, Some(identity), None, failures);
    }
    let Some((stdout, stderr, termination)) = captured else {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    };
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

fn post_reap_failure(
    prior: Option<&[RuntimeProbeFailure]>,
    kind: RuntimeProbeFailureKind,
) -> Result<Vec<RuntimeProbeFailure>, RuntimeFingerprintProduceError> {
    let mut failures = vec![RuntimeProbeFailure::new(kind)?];
    failures.extend(
        prior
            .into_iter()
            .flatten()
            .filter(|failure| {
                matches!(
                    failure.kind(),
                    RuntimeProbeFailureKind::TerminationFailed
                        | RuntimeProbeFailureKind::ReapFailed
                        | RuntimeProbeFailureKind::OutputDrainFailed
                )
            })
            .cloned(),
    );
    Ok(failures)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_reap_failure_replaces_only_the_primary_failure() {
        let prior = vec![
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::OutputDrainFailed).unwrap(),
        ];
        let failures =
            post_reap_failure(Some(&prior), RuntimeProbeFailureKind::IdentityChanged).unwrap();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].kind(), RuntimeProbeFailureKind::IdentityChanged);
        assert_eq!(
            failures[1].kind(),
            RuntimeProbeFailureKind::OutputDrainFailed
        );
    }
}
