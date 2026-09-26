use harness_protocol::rest::RuntimeHostUsageEvidence;
use harness_workflow::runtime::{EvalTrustedVerifier, RuntimeJob, QUALITY_GATE_ACTIVITY};
use serde_json::{json, Value};

pub(super) fn validate_eval_usage(
    job: &RuntimeJob,
    usage: &RuntimeHostUsageEvidence,
) -> Result<(), Value> {
    // Revision-bound native quality gates execute without a model. Keep unknown
    // cost unknown; trusted verifier commands must still match their embedded contract.
    let native_gate_usage = usage.model.is_empty()
        && usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cached_input_tokens == 0
        && usage.total_tokens == 0
        && usage.cost_usd_micros.is_none_or(|cost| cost == 0)
        && is_native_quality_gate_job(job);
    let measured_total = usage.input_tokens.saturating_add(usage.output_tokens);
    if !native_gate_usage
        && (usage.model.trim().is_empty()
            || usage.cached_input_tokens > usage.input_tokens
            || usage.total_tokens < measured_total
            || usage.total_tokens == 0)
    {
        return Err(json!({
            "error": "eval host usage evidence is invalid",
            "measured_total_tokens": measured_total,
            "reported_total_tokens": usage.total_tokens,
        }));
    }
    Ok(())
}

fn is_native_quality_gate_job(job: &RuntimeJob) -> bool {
    if super::evidence::runtime_job_activity(job) != QUALITY_GATE_ACTIVITY {
        return false;
    }
    if job
        .input
        .pointer("/command/expected_head_sha")
        .and_then(Value::as_str)
        .is_none_or(|head| head.len() != 40 || !head.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return false;
    }
    let Some(commands) = job.input.pointer("/command/validation_commands_argv") else {
        return false;
    };
    let Ok(commands) = serde_json::from_value::<Vec<Vec<String>>>(commands.clone()) else {
        return false;
    };
    let trusted = commands.iter().any(|argv| {
        argv.first().map(String::as_str) == Some("harness")
            && argv.get(1).map(String::as_str) == Some("eval")
            && argv.get(2).map(String::as_str) == Some("verify-trusted")
    });
    !commands.is_empty()
        && commands.iter().all(|argv| {
            !argv.is_empty()
                && argv.iter().all(|part| !part.is_empty())
                && (!trusted
                    || argv
                        .get(3)
                        .and_then(|id| id.parse::<EvalTrustedVerifier>().ok())
                        .is_some_and(|verifier| *argv == verifier.validation_argv()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_protocol::rest::{RuntimeHostExecutionEvidence, RuntimeHostValidationEvidence};
    use harness_sandbox::{ResourceLimitReport, ResourceLimits, ResourceUsage};
    use harness_workflow::runtime::{ActivityResult, ActivityStatus, RuntimeKind};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn native_job() -> RuntimeJob {
        RuntimeJob::pending(
            "native-verifier",
            RuntimeKind::RemoteHost,
            "eval-isolated-runtime-host",
            json!({
                "activity": QUALITY_GATE_ACTIVITY,
                "command": {
                    "eval": {"case_id": "gh1454-scoped-ci-jobs"},
                    "expected_head_sha": HEAD,
                    "validation_commands_argv": [EvalTrustedVerifier::Gh1454CiContractV1.validation_argv()],
                },
            }),
        )
    }

    fn native_usage() -> RuntimeHostUsageEvidence {
        RuntimeHostUsageEvidence {
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            total_tokens: 0,
            cost_usd_micros: None,
        }
    }

    #[test]
    fn native_verifier_completion_preserves_zero_usage_and_validation_outcome() {
        for (exit_code, cost) in [(0, None), (1, Some(0))] {
            let usage = RuntimeHostUsageEvidence {
                cost_usd_micros: cost,
                ..native_usage()
            };
            let evidence = RuntimeHostExecutionEvidence {
                checked_out_commit: HEAD.to_string(),
                resource_limit_report: json!(ResourceLimitReport {
                    limits: ResourceLimits::evaluation_defaults(45)
                        .cap_by(ResourceLimits::operator_default_maxima())
                        .expect("valid limits"),
                    usage: ResourceUsage::default(),
                    termination: None,
                    reason: "native verification completed".to_string(),
                }),
                usage,
                isolation_cleanup_status: "cleaned".to_string(),
                validation: vec![RuntimeHostValidationEvidence {
                    argv: EvalTrustedVerifier::Gh1454CiContractV1.validation_argv(),
                    exit_code,
                    output_sha256: "b".repeat(64),
                    duration_ms: 10,
                }],
            };
            let result = super::super::evidence::attach_eval_checkout_evidence(
                &native_job(),
                ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "verified"),
                Some(evidence),
            )
            .expect("native verifier completion accepts honest zero model usage");
            assert_eq!(
                result.status,
                if exit_code == 0 {
                    ActivityStatus::Succeeded
                } else {
                    ActivityStatus::Failed
                }
            );
            assert_eq!(
                harness_workflow::runtime::completion_evidence::server_validation_digest_passed(
                    &result
                ),
                exit_code == 0,
            );
            let usage = &result
                .artifacts
                .iter()
                .find(|artifact| {
                    artifact.artifact_type
                        == harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_HOST_USAGE
                })
                .expect("host usage retained")
                .artifact;
            assert_eq!(usage["model"], "");
            assert_eq!(usage["total_tokens"], 0);
            assert_eq!(usage["cost_usd_micros"], json!(cost));
        }
    }

    #[test]
    fn native_quality_gate_zero_usage_requires_revision_and_exact_trusted_command() {
        let original = native_job();
        let argv = EvalTrustedVerifier::Gh1454CiContractV1.validation_argv();
        let mut wrong_digest = argv.clone();
        *wrong_digest.last_mut().expect("digest argument") = "0".repeat(64);
        let mut extra_arg = argv.clone();
        extra_arg.push("extra".to_string());
        let mut unknown_verifier = argv.clone();
        unknown_verifier[3] = "unknown".to_string();
        for commands in [
            Value::Null,
            json!([]),
            json!([argv, ["cargo", "check"]]),
            json!([wrong_digest]),
            json!([extra_arg]),
            json!([unknown_verifier]),
        ] {
            let mut job = original.clone();
            job.input["command"]["validation_commands_argv"] = commands;
            assert!(validate_eval_usage(&job, &native_usage()).is_err());
        }
        let mut implementation = original;
        implementation.input["activity"] = json!("implement_issue");
        assert!(validate_eval_usage(&implementation, &native_usage()).is_err());

        let mut ordinary_gate = native_job();
        ordinary_gate.input["command"]["validation_commands_argv"] = json!([["cargo", "check"]]);
        assert!(validate_eval_usage(&ordinary_gate, &native_usage()).is_ok());
        ordinary_gate.input["command"]["expected_head_sha"] = Value::Null;
        assert!(validate_eval_usage(&ordinary_gate, &native_usage()).is_err());
    }

    #[test]
    fn native_verifier_zero_usage_rejects_contradictory_model_measurements() {
        let mut usages = Vec::new();
        for field in 0..6 {
            let mut usage = native_usage();
            match field {
                0 => usage.model = "model".to_string(),
                1 => usage.input_tokens = 1,
                2 => usage.output_tokens = 1,
                3 => usage.cached_input_tokens = 1,
                4 => usage.total_tokens = 1,
                _ => usage.cost_usd_micros = Some(1),
            }
            usages.push(usage);
        }
        for usage in usages {
            assert!(validate_eval_usage(&native_job(), &usage).is_err());
        }
    }

    #[test]
    fn eval_agent_usage_retains_nonzero_model_contract() {
        let mut job = native_job();
        job.input["activity"] = json!("implement_issue");
        let valid = RuntimeHostUsageEvidence {
            model: "test-model".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 4,
            total_tokens: 15,
            cost_usd_micros: None,
        };
        assert!(validate_eval_usage(&job, &valid).is_ok());
        for field in 0..3 {
            let mut usage = valid.clone();
            match field {
                0 => usage.model.clear(),
                1 => usage.cached_input_tokens = 11,
                _ => usage.total_tokens = 14,
            }
            assert!(validate_eval_usage(&job, &usage).is_err());
        }
    }
}
