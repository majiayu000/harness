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
    // A blocked example applies to every activity without inventing successful
    // work. The activity contract describes the evidence needed for success.
    schema.insert(
        "wire_format_example".into(),
        json!({
            "activity":activity,"status":"blocked",
            "summary":"Explain the actual external input needed to continue.",
            "artifacts":[],"signals":[],"validation":[],
            "error":"Describe the missing access or input.","error_kind":"external_dependency"
        }),
    );
    schema.insert("array_item_fields".into(), json!({
        "artifacts": {"artifact_type":"non-empty artifact name", "artifact":"payload value, encoded only when the transport schema requires it"},
        "signals": {"signal_type":"accepted signal name", "signal":"payload value, encoded only when the transport schema requires it"},
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
