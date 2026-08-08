use super::*;

const LET_CONTRACT: EntryContract = EntryContract {
    relative_path: "fixture.rs",
    function: "run",
    builder: REGISTRY_BUILDER,
    expected_use: ExpectedUse::LetBinding("agent_registry"),
};
const TAIL_CONTRACT: EntryContract = EntryContract {
    relative_path: "fixture.rs",
    function: "create_agent",
    builder: CLAUDE_BUILDER,
    expected_use: ExpectedUse::TailExpression,
};

#[test]
fn direct_ungated_binding_with_later_use_passes() {
    expect_pass(
        r#"
        fn run() {
            let agent_registry =
                harness_agents::builder::registry_from_config()?;
            consume(&agent_registry);
        }
        "#,
        LET_CONTRACT,
    );
}

#[test]
fn function_and_statement_test_gates_fail_closed() {
    for source in [
        gated_function("#[test]"),
        gated_function("#[cfg(unix)]"),
        gated_function("#[cfg_attr(test, test)]"),
        r#"
        fn run() {
            #[cfg(unix)]
            let agent_registry =
                harness_agents::builder::registry_from_config()?;
            consume(agent_registry);
        }
        "#
        .to_string(),
    ] {
        expect_reject(&source, LET_CONTRACT);
    }
}

#[test]
fn nested_const_closure_and_local_function_bait_do_not_pass() {
    for source in [
        r#"
        fn run() {
            {
                let agent_registry =
                    harness_agents::builder::registry_from_config()?;
                consume(agent_registry);
            }
        }
        "#,
        r#"
        fn run() {
            let _bait = || harness_agents::builder::registry_from_config();
        }
        "#,
        r#"
        fn run() {
            const BAIT: () = {
                harness_agents::builder::registry_from_config();
            };
        }
        "#,
        r#"
        fn run() {
            fn bait() {
                harness_agents::builder::registry_from_config();
            }
        }
        "#,
    ] {
        expect_reject(source, LET_CONTRACT);
    }
}

#[test]
fn unreachable_shadowed_and_unused_bindings_are_rejected() {
    for source in [
        r#"
        fn run() {
            return;
            let agent_registry =
                harness_agents::builder::registry_from_config()?;
            consume(agent_registry);
        }
        "#,
        r#"
        fn run() {
            let agent_registry =
                harness_agents::builder::registry_from_config()?;
            let agent_registry = replacement();
            consume(agent_registry);
        }
        "#,
        r#"
        fn run() {
            let agent_registry =
                harness_agents::builder::registry_from_config()?;
            consume_something_else();
        }
        "#,
    ] {
        expect_reject(source, LET_CONTRACT);
    }
}

#[test]
fn builder_alias_is_not_treated_as_the_canonical_call() {
    expect_reject(
        r#"
        use harness_agents::builder::registry_from_config as build_registry;
        fn run() {
            let agent_registry = build_registry()?;
            consume(agent_registry);
        }
        "#,
        LET_CONTRACT,
    );
}

#[test]
fn direct_ungated_reachable_tail_call_passes() {
    expect_pass(
        r#"
        fn create_agent() {
            harness_agents::builder::claude_agent_from_config()
        }
        "#,
        TAIL_CONTRACT,
    );
}

#[test]
fn tail_call_bait_and_unreachable_tail_are_rejected() {
    for source in [
        r#"
        fn create_agent() {
            || harness_agents::builder::claude_agent_from_config()
        }
        "#,
        r#"
        fn create_agent() {
            {
                harness_agents::builder::claude_agent_from_config()
            }
        }
        "#,
        r#"
        fn create_agent() {
            return fallback();
            harness_agents::builder::claude_agent_from_config()
        }
        "#,
    ] {
        expect_reject(source, TAIL_CONTRACT);
    }
}

#[test]
fn review_exception_accepts_the_exact_runtime_boundary() {
    assert!(verify_codex_review_exception(&review_fixture(
        "SandboxMode::ReadOnlyWithNetwork",
        r#"Some("never".to_string())"#,
    ))
    .is_ok());
}

#[test]
fn review_exception_rejects_constructor_or_request_privilege_drift() {
    let constructor_drift = review_fixture_with_constructor(
        "SandboxMode::DangerFullAccess",
        "SandboxMode::ReadOnlyWithNetwork",
        r#"Some("never".to_string())"#,
    );
    let request_drift = review_fixture(
        "SandboxMode::DangerFullAccess",
        r#"Some("never".to_string())"#,
    );
    let approval_drift = review_fixture(
        "SandboxMode::ReadOnlyWithNetwork",
        r#"Some("on-request".to_string())"#,
    );

    for source in [constructor_drift, request_drift, approval_drift] {
        assert!(verify_codex_review_exception(&source).is_err());
    }
}

#[test]
fn review_exception_rejects_suppression_or_config_propagation_drift() {
    let missing_local_suppression = review_fixture(
        "SandboxMode::ReadOnlyWithNetwork",
        r#"Some("never".to_string())"#,
    )
    .replace("            #[allow(clippy::disallowed_methods)]\n", "");
    let model_drift = review_fixture(
        "SandboxMode::ReadOnlyWithNetwork",
        r#"Some("never".to_string())"#,
    )
    .replace("model: Some(review_config.model)", "model: None");
    let reasoning_drift = review_fixture(
        "SandboxMode::ReadOnlyWithNetwork",
        r#"Some("never".to_string())"#,
    )
    .replace(
        "reasoning_effort: Some(review_config.reasoning_effort)",
        "reasoning_effort: None",
    );

    for source in [missing_local_suppression, model_drift, reasoning_drift] {
        assert!(verify_codex_review_exception(&source).is_err());
    }
}

fn review_fixture(request_sandbox: &str, approval_policy: &str) -> String {
    review_fixture_with_constructor(
        "SandboxMode::ReadOnlyWithNetwork",
        request_sandbox,
        approval_policy,
    )
}

fn review_fixture_with_constructor(
    constructor_sandbox: &str,
    request_sandbox: &str,
    approval_policy: &str,
) -> String {
    format!(
        r#"
        async fn review() {{
            #[allow(clippy::disallowed_methods)]
            let agent = CodexAgent::new(
                review_config.cli_path.clone(),
                {constructor_sandbox},
            )
            .with_stream_timeout(Some(review_config.timeout_secs));
            agent.execute_review(CodexReviewRequest {{
                project_root: project,
                instructions: None,
                base_ref: None,
                model: Some(review_config.model),
                reasoning_effort: Some(review_config.reasoning_effort),
                sandbox_mode: {request_sandbox},
                approval_policy: {approval_policy},
                permission_mode: Default::default(),
                env_vars: Default::default(),
            }}).await
        }}
        "#
    )
}

fn gated_function(attribute: &str) -> String {
    format!(
        r#"
        {attribute}
        fn run() {{
            let agent_registry =
                harness_agents::builder::registry_from_config()?;
            consume(agent_registry);
        }}
        "#
    )
}

fn expect_pass(source: &str, contract: EntryContract) {
    verify_entry_contract(source, contract)
        .unwrap_or_else(|error| panic!("fixture should pass: {error}"));
}

fn expect_reject(source: &str, contract: EntryContract) {
    assert!(
        verify_entry_contract(source, contract).is_err(),
        "fixture should be rejected"
    );
}
