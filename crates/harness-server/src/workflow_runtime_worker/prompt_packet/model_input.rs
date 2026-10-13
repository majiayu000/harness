use serde_json::{json, Value};

/// Simplify only the model-facing copy. The durable packet retains all audit
/// data and the transport schema; declarative workflows retain their policy.
pub(super) fn simplify(packet: &mut Value) {
    let encoded = matches!(
        packet
            .pointer("/runtime_job/runtime_kind")
            .and_then(Value::as_str),
        Some("codex_exec" | "codex_jsonrpc")
    );
    let builtin = packet.get("activity_policy").is_none()
        && matches!(
            packet
                .pointer("/workflow/definition_id")
                .and_then(Value::as_str),
            Some("github_issue_pr" | "pr_feedback" | "quality_gate" | "prompt_task")
        );
    if !builtin {
        return;
    }
    if let Some(object) = packet.as_object_mut() {
        object.remove("runtime_profile");
        object.remove("required_structured_output");
    }
    if let Some(file) = packet
        .get_mut("workflow_file")
        .and_then(Value::as_object_mut)
    {
        file.remove("config");
    }
    let Some(schema) = packet
        .get_mut("activity_result_schema")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if !encoded {
        schema.remove("json_schema");
    }
    schema.remove("optional_artifacts");
    schema.remove("workflow_decision_contract");
    schema.insert("decision_owner".into(), json!("Harness selects transitions and schedules follow-up work. Report this activity's evidence; do not emit workflow_decision artifacts or scheduler commands."));
    if let Some(artifacts) = schema
        .get_mut("activity_contract")
        .and_then(|contract| contract.get_mut("accepted_artifacts"))
        .and_then(Value::as_array_mut)
    {
        artifacts.retain(|value| value.as_str() != Some("workflow_decision"));
    }
    if let Some(contract) = schema
        .get_mut("transition_contract")
        .and_then(Value::as_object_mut)
    {
        contract.remove("structured_decision");
        if let Some(success) = contract
            .get_mut("on_succeeded")
            .and_then(Value::as_object_mut)
        {
            // Payload types already live in activity_contract; scheduling is server-owned.
            success.remove("accepted_artifacts");
            success.remove("accepted_signals");
            success.remove("reducer_next_state");
        }
    }
    let activity = schema
        .get("activity")
        .and_then(Value::as_str)
        .unwrap_or("workflow_activity")
        .to_owned();
    let stop_signal = json!({ "stop_reason_code": "ci_backend_unavailable" });
    schema.insert("stop_reason_contract".into(), json!({
        "description": "For a confirmed temporary service gap, include an explicit stop_reason_code in an existing signal payload or in the optional runtime_stop metadata signal shown below. This metadata is accepted in addition to activity_contract.accepted_signals; it does not replace a required activity outcome. A succeeded local review must still emit exactly one review outcome and may put the code directly in LocalReviewBlocked. Both blocked and succeeded + LocalReviewBlocked preserve the code.",
        "transient_stop_reason_codes": harness_workflow::runtime::reason_class::TRANSIENT_STOP_REASON_CODES,
        "gate_rule": "A blocked result without a stop code is maintainer_input_required (terminal), even with error_kind external_dependency. Human approval and unchanged acceptance gates require operator input; do not label them transient. Report only a code supported by observed evidence. Runtime-enforced stop reasons take precedence. Fatal, configuration, and unknown error kinds override transient codes; unknown codes are terminal. Failed executions retain the existing bounded retry policy.",
        "signal_example": {
            "signal_type": "runtime_stop",
            "signal": if encoded {
                json!({"encoding":"harness.runtime.json_payload.v1", "json":stop_signal.to_string()})
            } else {
                stop_signal
            }
        }
    }));
    // A blocked example applies to every activity without inventing successful
    // work. The activity contract describes the evidence needed for success.
    // plan_issue replaces it: an unmet explicit approval is configuration.
    schema.insert(
        "wire_format_example".into(),
        json!({
            "activity":activity,"status":"blocked",
            "summary":"Explain the actual external input needed to continue.",
            "artifacts":[],"signals":[],"validation":[],
            "error":"Describe the missing access or input.","error_kind":"external_dependency"
        }),
    );
    if activity == "plan_issue" {
        schema.insert(
            "wire_format_example".into(),
            json!({
                "activity": activity,
                "status": "blocked",
                "summary": "An explicit approval is unmet. Record the approval, then an operator calls POST /api/workflows/runtime/unblock. Unblock does not grant the approval.",
                "artifacts": [],
                "signals": [],
                "validation": [],
                "error": "No recorded approval authorizes this step.",
                "error_kind": "configuration"
            }),
        );
    }
    schema.insert("array_item_fields".into(), json!({
        "artifacts": {"artifact_type":"non-empty artifact name", "artifact":"payload value, encoded only when the transport schema requires it"},
        "signals": {"signal_type":"activity outcome signal name or optional runtime_stop metadata", "signal":"payload value, encoded only when the transport schema requires it"},
        "validation": {"command":"actual command run", "status":"actual result such as passed or failed", "reason":"explanation or null"},
        "requirement":"Use these exact field names. Arrays may be empty only when there is no corresponding evidence."
    }));
    if activity == "run_local_review" && !encoded {
        schema.insert("wire_format_example".into(), json!({
            "activity":activity, "status":"succeeded",
            "summary":"Explain the review and evidence for its conclusion.", "artifacts":[],
            "signals":[{"signal_type":"LocalReviewPassed","signal":{"pr_number":123,"reviewed_head_sha":"<actual reviewed SHA>","working_tree_clean":true,"findings":[]}}],
            "validation":[{"command":"<actual command run>","status":"passed","reason":null}],
            "error":null,"error_kind":null
        }));
    }
    schema.insert("example_scope".into(), json!("Example shape only, not a suggested outcome. Complete the requested work when possible. Use succeeded only with the evidence required by this activity, actual findings and actual validation results. Use native artifact/signal payloads unless the transport schema explicitly requires encoded payloads."));
}
