use crate::{http::AppState, validate_root};
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
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
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
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
                    Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
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
            return RpcResponse::error(id, INTERNAL_ERROR, err.to_string());
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
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => {
            tracing::warn!(
                project_root = %project_root.display(),
                file_count,
                error = %e,
                "rule/check scan failed"
            );
            RpcResponse::error(id, INTERNAL_ERROR, e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rule_check;
    use harness_core::{EventFilters, GuardId, Language};
    use harness_protocol::INTERNAL_ERROR;
    use harness_rules::engine::{Guard, WARN_EMPTY_SCAN_INPUT, WARN_NO_GUARDS_REGISTERED};
    use std::path::PathBuf;

    use crate::test_helpers::{make_test_state, tempdir_in_home, HOME_LOCK};

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
        assert_eq!(error.code, INTERNAL_ERROR);
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
        assert_eq!(error.code, INTERNAL_ERROR);
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
