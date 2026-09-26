use crate::{handlers::error as rpc_error, http::AppState, validate_root};
use harness_protocol::methods::RpcResponse;
use harness_rules::engine::{WARN_EMPTY_SCAN_INPUT, WARN_NO_GUARDS_REGISTERED};
use std::path::PathBuf;

pub async fn rule_load(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id, &state.core.home_dir);
    let mut rules = state.engines.rules.write().await;
    match rules.load(&project_root) {
        Ok(()) => {
            let count = rules.rules().len();
            RpcResponse::success(id, serde_json::json!({ "rules_count": count }))
        }
        Err(e) => rpc_error::internal(id, e.to_string()),
    }
}

pub async fn rule_check(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
    files: Option<Vec<PathBuf>>,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id, &state.core.home_dir);
    let file_count = files.as_ref().map_or(0, |paths| paths.len());

    // Validate file paths first — pure path arithmetic, no lock needed.
    // Rejecting invalid paths before taking the snapshot avoids a full
    // deep-clone of Vec<Rule> (including description bodies) on bad requests.
    let validated_files = match files {
        Some(f) => {
            let mut validated = Vec::with_capacity(f.len());
            for file in &f {
                match crate::handlers::validate_file_in_root(file, &project_root) {
                    Ok(p) => validated.push(p),
                    Err(e) => return rpc_error::invalid_params(id, e),
                }
            }
            Some(validated)
        }
        None => None,
    };

    // Acquire the read lock only long enough to validate the request and
    // snapshot the guards/rules.  Releasing the lock before the async scan
    // prevents write starvation: concurrent rule_load() calls need a write
    // lock and would otherwise block for the entire scan duration.
    let snapshot = {
        let rules = state.engines.rules.read().await;
        if let Err(err) = rules.validate_scan_request(validated_files.as_deref()) {
            tracing::warn!(
                project_root = %project_root.display(),
                guard_count = rules.guards().len(),
                file_count,
                error = %err,
                "rule/check rejected before scan"
            );
            let message = err.to_string();
            if message.contains(WARN_EMPTY_SCAN_INPUT) {
                return rpc_error::invalid_params(id, message);
            }
            if message.contains(WARN_NO_GUARDS_REGISTERED) {
                return rpc_error::validation(id, message);
            }
            return rpc_error::internal(id, message);
        }
        rules.snapshot()
    }; // read lock released here

    // Run the async scan without holding the read lock.
    let result = match validated_files {
        Some(validated) => snapshot.scan_files(&project_root, &validated).await,
        None => snapshot.scan(&project_root).await,
    };

    match result {
        Ok(violations) => {
            let guard_count = snapshot.guard_count();
            tracing::info!(
                project_root = %project_root.display(),
                guard_count,
                file_count,
                violation_count = violations.len(),
                "rule/check scan completed"
            );
            state
                .observability
                .events
                .persist_rule_scan(&project_root, &violations)
                .await;
            match serde_json::to_value(&violations) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => rpc_error::internal(id, e.to_string()),
            }
        }
        Err(e) => {
            tracing::warn!(
                project_root = %project_root.display(),
                file_count,
                error = %e,
                "rule/check scan failed"
            );
            rpc_error::internal(id, e.to_string())
        }
    }
}

pub async fn rule_fix(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_root: PathBuf,
) -> RpcResponse {
    let project_root = validate_root!(&project_root, id, &state.core.home_dir);
    let rules = {
        let rules = state.engines.rules.read().await;
        if let Err(error) = rules.validate_scan_request(None) {
            return rpc_error::validation(id, error.to_string());
        }
        rules.clone()
    };
    match rules.scan_and_fix(&project_root, true).await {
        Ok(report) => {
            state
                .observability
                .events
                .persist_rule_scan(&project_root, &report.residual_violations)
                .await;
            match serde_json::to_value(report) {
                Ok(value) => RpcResponse::success(id, value),
                Err(error) => rpc_error::internal(id, error.to_string()),
            }
        }
        Err(error) => rpc_error::internal(id, error.to_string()),
    }
}

pub async fn exec_policy_check(
    state: &AppState,
    id: Option<serde_json::Value>,
    command: String,
) -> RpcResponse {
    if state.core.server.config.rules.exec_policy_paths.is_empty() {
        return rpc_error::validation(id, "No exec policy rules are configured".to_string());
    }
    let Some(argv) = shlex::split(&command) else {
        return rpc_error::invalid_params(id, "Command has an unmatched quote".to_string());
    };
    if argv.is_empty() {
        return rpc_error::invalid_params(id, "Command is empty".to_string());
    }
    let rules = state.engines.rules.read().await;
    let result =
        rules.check_command_policy(&argv, &harness_rules::exec_policy::MatchOptions::default());
    match serde_json::to_value(result) {
        Ok(value) => RpcResponse::success(id, value),
        Err(error) => rpc_error::internal(id, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{exec_policy_check, rule_check, rule_fix};
    use harness_core::types::{Category, EventFilters, GuardId, Language, RuleId, Severity};
    use harness_protocol::methods::{INVALID_PARAMS, VALIDATION_ERROR};
    use harness_rules::engine::{Guard, Rule, WARN_EMPTY_SCAN_INPUT, WARN_NO_GUARDS_REGISTERED};
    use std::path::PathBuf;

    use crate::test_helpers::{make_test_state, tempdir_in_home, HOME_LOCK};

    #[tokio::test]
    async fn rule_fix_requires_registered_guards() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("rule-fix-no-guard-")?;
        let state = make_test_state(dir.path()).await?;
        let response = rule_fix(&state, Some(serde_json::json!(1)), dir.path().to_path_buf()).await;
        assert_eq!(
            response.error.expect("missing guard error").code,
            VALIDATION_ERROR
        );
        Ok(())
    }

    #[tokio::test]
    async fn rule_fix_applies_rule_and_reports_residuals() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("rule-fix-applies-")?;
        let state = make_test_state(dir.path()).await?;
        let source = dir.path().join("sample.rs");
        std::fs::write(&source, "let x = foo();\n")?;
        let script = dir.path().join("detect-foo.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\nfile=\"$1/sample.rs\"\nif grep -q 'foo()' \"$file\"; then echo \"$file:1:FIX-AUTO:use bar\"; fi\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script)?.permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions)?;
        }
        {
            let mut rules = state.engines.rules.write().await;
            rules.register_guard(Guard {
                id: GuardId::from_str("FIX-GUARD"),
                script_path: script,
                language: Language::Common,
                rules: vec![],
            });
            rules.add_rule(Rule {
                id: RuleId::from_str("FIX-AUTO"),
                title: "Replace foo".to_string(),
                severity: Severity::Low,
                category: Category::Style,
                paths: vec![],
                description: String::new(),
                fix_pattern: Some("s/foo/bar/".to_string()),
            });
        }
        let response = rule_fix(&state, Some(serde_json::json!(1)), dir.path().to_path_buf()).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let report: harness_core::types::AutoFixReport =
            serde_json::from_value(response.result.expect("fix report"))?;
        assert_eq!(report.fixed_count, 1);
        assert!(report.residual_violations.is_empty());
        assert_eq!(std::fs::read_to_string(source)?, "let x = bar();\n");
        Ok(())
    }

    #[tokio::test]
    async fn exec_policy_check_reports_missing_configuration() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("exec-policy-no-rules-")?;
        let state = make_test_state(dir.path()).await?;
        let response =
            exec_policy_check(&state, Some(serde_json::json!(1)), "rm -rf .".to_string()).await;
        assert_eq!(
            response.error.expect("missing policy error").code,
            VALIDATION_ERROR
        );
        Ok(())
    }

    #[tokio::test]
    async fn rule_check_returns_warning_when_no_guards_registered() -> anyhow::Result<()> {
        // Hold HOME_LOCK so a concurrent persisted_skills_survive_restart cannot
        // change HOME between tempdir_in_home() and validate_project_root().
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("rule-check-no-guard-")?;
        let state = make_test_state(dir.path()).await?;
        let project_root = dir.path().to_path_buf();

        let response = rule_check(&state, Some(serde_json::json!(1)), project_root, None).await;

        let error = response
            .error
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected rule/check to fail without guards"))?;
        assert_eq!(error.code, VALIDATION_ERROR);
        assert!(
            error.message.contains(WARN_NO_GUARDS_REGISTERED),
            "expected warning message in error: {}",
            error.message
        );
        assert!(
            response.result.is_none(),
            "warning path must not return result"
        );

        let events = state
            .observability
            .events
            .query(&EventFilters {
                hook: Some("rule_scan".to_string()),
                ..Default::default()
            })
            .await?;
        assert!(
            events.is_empty(),
            "warning path should not persist rule_scan events"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rule_check_with_guard_returns_violations() -> anyhow::Result<()> {
        // Hold HOME_LOCK for the same reason as rule_check_returns_warning_when_no_guards_registered.
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("rule-check-violations-")?;
        let state = make_test_state(dir.path()).await?;

        // Write a guard script that always reports one violation.
        let guard_script = dir.path().join("violation-guard.sh");
        let violation_line = format!(
            "#!/usr/bin/env bash\necho '{}:1:RS-03:unwrap in production code'\n",
            dir.path().join("src/main.rs").display()
        );
        std::fs::write(&guard_script, violation_line)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&guard_script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&guard_script, perms)?;
        }

        {
            let mut rules = state.engines.rules.write().await;
            rules.register_guard(Guard {
                id: GuardId::from_str("RS-03-TEST"),
                script_path: guard_script,
                language: Language::Common,
                rules: vec![],
            });
        }

        let response = rule_check(
            &state,
            Some(serde_json::json!(1)),
            dir.path().to_path_buf(),
            None,
        )
        .await;

        assert!(
            response.error.is_none(),
            "rule/check with guard should succeed: {:?}",
            response.error
        );
        let violations: Vec<serde_json::Value> =
            serde_json::from_value(response.result.expect("expected violations result"))?;
        assert_eq!(violations.len(), 1, "expected exactly one violation");
        let rule_id = violations[0]["rule_id"].as_str().unwrap_or("");
        assert_eq!(rule_id, "RS-03", "expected RS-03 violation");
        Ok(())
    }

    #[tokio::test]
    async fn rule_check_returns_warning_for_empty_scan_input() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("rule-check-empty-input-")?;
        let state = make_test_state(dir.path()).await?;
        {
            let mut rules = state.engines.rules.write().await;
            rules.register_guard(Guard {
                id: GuardId::from_str("TEST-GUARD"),
                script_path: PathBuf::from("unused-guard.sh"),
                language: Language::Common,
                rules: vec![],
            });
        }

        let response = rule_check(
            &state,
            Some(serde_json::json!(1)),
            dir.path().to_path_buf(),
            Some(Vec::new()),
        )
        .await;

        let error = response
            .error
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("expected rule/check to fail for empty scan input"))?;
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(
            error.message.contains(WARN_EMPTY_SCAN_INPUT),
            "expected warning message in error: {}",
            error.message
        );
        assert!(
            response.result.is_none(),
            "warning path must not return result"
        );
        Ok(())
    }

    /// Verify that a concurrent write-lock attempt succeeds while `rule_check` is
    /// scanning (i.e. the read guard is not held across the async scan await).
    ///
    /// Without the snapshot fix the read guard would be held for the entire scan
    /// duration (~500 ms), causing the write to be blocked. With the fix the read
    /// guard is released before scan, so the writer acquires the lock in <50 ms.
    #[tokio::test]
    async fn rule_check_does_not_block_concurrent_writer() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("rule-check-concurrent-write-")?;
        let state = make_test_state(dir.path()).await?;

        // Guard script that sleeps long enough for the writer to race.
        let guard_script = dir.path().join("slow-guard.sh");
        std::fs::write(&guard_script, "#!/usr/bin/env bash\nsleep 0.5\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&guard_script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&guard_script, perms)?;
        }

        {
            let mut rules = state.engines.rules.write().await;
            rules.register_guard(Guard {
                id: GuardId::from_str("SLOW-GUARD"),
                script_path: guard_script,
                language: Language::Common,
                rules: vec![],
            });
        }

        // Clone the Arc so the write task owns it independently of `state`.
        let rules_arc = state.engines.rules.clone();

        // Attempt to acquire write lock 50 ms after rule_check starts.
        // With the fix: read guard released before scan → write acquired immediately.
        // Without the fix: read guard held across 500 ms scan → write blocked.
        // Allow 200 ms — well within the fix window but far before scan ends.
        let write_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tokio::time::timeout(std::time::Duration::from_millis(200), rules_arc.write())
                .await
                .is_ok()
        });

        rule_check(
            &state,
            Some(serde_json::json!(1)),
            dir.path().to_path_buf(),
            None,
        )
        .await;

        let writer_got_lock = write_task.await?;
        assert!(
            writer_got_lock,
            "concurrent writer should acquire the lock while scan is in progress \
             (lock-held-across-await regression)"
        );
        Ok(())
    }
}
