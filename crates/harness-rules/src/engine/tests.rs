use super::*;

fn make_engine_with_content(content: &str) -> anyhow::Result<RuleEngine> {
    let mut engine = RuleEngine::new();
    engine.parse_rule_file(Path::new("test.md"), content)?;
    Ok(engine)
}

#[test]
fn parse_rule_file_extracts_security_rule() -> anyhow::Result<()> {
    // Leading newline required: parser splits on "\n## "
    let md = "\n## SEC-01: SQL injection\n\n严重 — use params.\n";
    let engine = make_engine_with_content(md)?;
    assert_eq!(engine.rules().len(), 1);
    assert_eq!(engine.rules()[0].id, RuleId::from_str("SEC-01"));
    assert_eq!(engine.rules()[0].title, "SQL injection");
    assert_eq!(engine.rules()[0].severity, Severity::Critical);
    assert_eq!(engine.rules()[0].category, Category::Security);
    Ok(())
}

#[test]
fn parse_rule_file_extracts_multiple_rules() -> anyhow::Result<()> {
    let md = "\n## SEC-01: First rule\n\n严重\n\n## SEC-02: Second rule\n\nhigh severity\n";
    let engine = make_engine_with_content(md)?;
    assert_eq!(engine.rules().len(), 2);
    Ok(())
}

#[test]
fn parse_rule_file_skips_lowercase_ids() -> anyhow::Result<()> {
    let md = "\n## lowercase: should be skipped\n\nsome content\n";
    let engine = make_engine_with_content(md)?;
    assert_eq!(engine.rules().len(), 0);
    Ok(())
}

#[test]
fn parse_rule_file_detects_category_from_prefix() -> anyhow::Result<()> {
    let md = "\n## RS-01: Rust stability rule\n\nhigh\n";
    let engine = make_engine_with_content(md)?;
    let rules = engine.rules();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].category, Category::Stability);
    Ok(())
}

#[test]
fn empty_rule_engine_has_no_rules() {
    let engine = RuleEngine::new();
    assert!(engine.rules().is_empty());
    assert!(engine.guards().is_empty());
}

#[test]
fn load_builtin_returns_at_least_40_rules() -> anyhow::Result<()> {
    let mut engine = RuleEngine::new();
    engine.load_builtin()?;
    assert!(
        engine.rules().len() >= 40,
        "expected >= 40 builtin rules, got {}",
        engine.rules().len()
    );
    Ok(())
}

#[test]
fn load_builtin_parses_gp_and_sec_ids() -> anyhow::Result<()> {
    let mut engine = RuleEngine::new();
    engine.load_builtin()?;
    assert!(
        engine.rules().iter().any(|r| r.id.as_str() == "GP-01"),
        "GP-01 rule missing"
    );
    assert!(
        engine.rules().iter().any(|r| r.id.as_str() == "SEC-01"),
        "SEC-01 rule missing"
    );
    Ok(())
}

#[test]
fn parse_frontmatter_paths_inline_array() -> anyhow::Result<()> {
    let frontmatter = "paths: [\"*.rs\", \"src/**\"]\n";
    let paths = RuleEngine::parse_frontmatter_paths(frontmatter)?;
    assert_eq!(paths, vec!["*.rs", "src/**"]);
    Ok(())
}

#[test]
fn parse_frontmatter_paths_empty() -> anyhow::Result<()> {
    let paths = RuleEngine::parse_frontmatter_paths("")?;
    assert!(paths.is_empty());
    Ok(())
}

#[test]
fn parse_frontmatter_paths_quoted_string() -> anyhow::Result<()> {
    let frontmatter = "paths: \"**/*.go\"\n";
    let paths = RuleEngine::parse_frontmatter_paths(frontmatter)?;
    assert_eq!(paths, vec!["**/*.go"]);
    Ok(())
}

#[test]
fn parse_frontmatter_paths_block_sequence() -> anyhow::Result<()> {
    let frontmatter = "paths:\n  - \"*.rs\"\n  - \"src/**\"\n";
    let paths = RuleEngine::parse_frontmatter_paths(frontmatter)?;
    assert_eq!(paths, vec!["*.rs", "src/**"]);
    Ok(())
}

#[test]
fn add_rule_inserts_new_rule() {
    let mut engine = RuleEngine::new();
    let rule = Rule {
        id: RuleId::from_str("LEARN-001"),
        title: "Test rule".to_string(),
        severity: Severity::High,
        category: Category::Style,
        paths: Vec::new(),
        description: "desc".to_string(),
        fix_pattern: None,
    };
    engine.add_rule(rule);
    assert_eq!(engine.rules().len(), 1);
    assert_eq!(engine.rules()[0].id, RuleId::from_str("LEARN-001"));
}

#[test]
fn add_rule_deduplicates_by_id() {
    let mut engine = RuleEngine::new();
    let make = |title: &str| Rule {
        id: RuleId::from_str("LEARN-001"),
        title: title.to_string(),
        severity: Severity::High,
        category: Category::Style,
        paths: Vec::new(),
        description: "desc".to_string(),
        fix_pattern: None,
    };
    engine.add_rule(make("first"));
    engine.add_rule(make("duplicate"));
    assert_eq!(engine.rules().len(), 1, "duplicate rule_id must be skipped");
    assert_eq!(engine.rules()[0].title, "first");
}

#[test]
fn auto_register_builtin_guards_registers_guard_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut engine = RuleEngine::new();
    let first = engine.auto_register_builtin_guards(dir.path())?;
    let second = engine.auto_register_builtin_guards(dir.path())?;

    assert_eq!(first, 1);
    assert_eq!(second, 0);
    assert_eq!(engine.guards().len(), 1);
    assert_eq!(engine.guards()[0].id.as_str(), BUILTIN_BASELINE_GUARD_ID);
    assert!(
        engine.guards()[0].script_path.is_file(),
        "builtin guard script should be materialized on disk"
    );
    Ok(())
}

#[test]
fn validate_scan_request_rejects_missing_guards() {
    let engine = RuleEngine::new();
    let err = engine
        .validate_scan_request(None)
        .expect_err("missing guards should be rejected");
    assert!(
        err.to_string().contains(WARN_NO_GUARDS_REGISTERED),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_scan_request_rejects_empty_file_input() {
    let mut engine = RuleEngine::new();
    engine.register_guard(Guard {
        id: GuardId::from_str("TEST-GUARD"),
        script_path: PathBuf::from("unused.sh"),
        language: Language::Common,
        rules: vec![],
    });
    let err = engine
        .validate_scan_request(Some(&[]))
        .expect_err("empty scan input should be rejected");
    assert!(
        err.to_string().contains(WARN_EMPTY_SCAN_INPUT),
        "unexpected error: {err}"
    );
}

#[test]
fn load_exec_policy_files_adds_command_policy() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let policy_path = sandbox.path().join("policy.star");
    std::fs::write(
        &policy_path,
        r#"
prefix_rule(pattern = ["git", "push"], decision = "prompt")
"#,
    )?;
    let mut engine = RuleEngine::new();
    engine.load_exec_policy_files(&[policy_path])?;

    let result = engine.check_command_policy(
        &["git".to_string(), "push".to_string()],
        &MatchOptions::default(),
    );
    assert_eq!(
        result.decision,
        Some(crate::exec_policy::ExecDecision::Prompt)
    );
    Ok(())
}

#[test]
fn load_exec_policy_files_merges_host_executable_paths_for_same_name() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let first_policy_path = sandbox.path().join("first.star");
    let second_policy_path = sandbox.path().join("second.star");
    std::fs::write(
        &first_policy_path,
        r#"
prefix_rule(pattern = ["git", "status"], decision = "allow")
host_executable(name = "git", paths = ["/usr/bin/git"])
"#,
    )?;
    std::fs::write(
        &second_policy_path,
        r#"
prefix_rule(pattern = ["git", "status"], decision = "allow")
host_executable(name = "git", paths = ["/opt/homebrew/bin/git"])
"#,
    )?;

    let mut engine = RuleEngine::new();
    engine.load_exec_policy_files(&[first_policy_path, second_policy_path])?;
    let options = MatchOptions {
        resolve_host_executables: true,
    };

    let usr_bin_result = engine.check_command_policy(
        &["/usr/bin/git".to_string(), "status".to_string()],
        &options,
    );
    assert_eq!(
        usr_bin_result.decision,
        Some(crate::exec_policy::ExecDecision::Allow)
    );

    let homebrew_result = engine.check_command_policy(
        &["/opt/homebrew/bin/git".to_string(), "status".to_string()],
        &options,
    );
    assert_eq!(
        homebrew_result.decision,
        Some(crate::exec_policy::ExecDecision::Allow)
    );
    Ok(())
}

#[test]
fn parse_frontmatter_fix_pattern_returns_none_when_absent() -> anyhow::Result<()> {
    let frontmatter = "paths: [\"*.rs\"]\n";
    let result = RuleEngine::parse_frontmatter_fix_pattern(frontmatter)?;
    assert!(result.is_none());
    Ok(())
}

#[test]
fn parse_frontmatter_fix_pattern_returns_none_for_empty_frontmatter() -> anyhow::Result<()> {
    let result = RuleEngine::parse_frontmatter_fix_pattern("")?;
    assert!(result.is_none());
    Ok(())
}

#[test]
fn parse_frontmatter_fix_pattern_extracts_value() -> anyhow::Result<()> {
    let frontmatter = "paths: [\"*.rs\"]\nfix_pattern: 's/foo/bar/'\n";
    let result = RuleEngine::parse_frontmatter_fix_pattern(frontmatter)?;
    assert_eq!(result.as_deref(), Some("s/foo/bar/"));
    Ok(())
}

#[test]
fn parse_fix_pattern_parses_slash_delimited_sed_string() {
    let (re, replacement) = RuleEngine::parse_fix_pattern("s/foo/bar/").expect("should parse");
    assert_eq!(re.as_str(), "foo");
    assert_eq!(replacement, "bar");
}

#[test]
fn parse_fix_pattern_accepts_pipe_delimiter() {
    let (re, replacement) = RuleEngine::parse_fix_pattern("s|foo|bar|").expect("should parse");
    assert_eq!(re.as_str(), "foo");
    assert_eq!(replacement, "bar");
}

#[test]
fn parse_fix_pattern_returns_none_for_invalid_format() {
    assert!(RuleEngine::parse_fix_pattern("not-a-pattern").is_none());
    assert!(RuleEngine::parse_fix_pattern("s/only_pattern").is_none());
    assert!(RuleEngine::parse_fix_pattern("").is_none());
}

#[test]
fn apply_fix_rewrites_file_when_pattern_matches() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("sample.rs");
    std::fs::write(&file_path, "let x = foo();\nlet y = foo();\n")?;

    let mut engine = RuleEngine::new();
    engine.add_rule(Rule {
        id: RuleId::from_str("FIX-01"),
        title: "Replace foo with bar".to_string(),
        severity: Severity::Low,
        category: Category::Style,
        paths: vec![],
        description: String::new(),
        fix_pattern: Some("s/foo/bar/".to_string()),
    });

    let violation = Violation {
        rule_id: RuleId::from_str("FIX-01"),
        file: file_path.clone(),
        line: Some(1),
        message: "use bar".to_string(),
        severity: Severity::Low,
    };

    let modified = engine.apply_fix(&violation, dir.path())?;
    assert!(modified);
    let content = std::fs::read_to_string(&file_path)?;
    assert_eq!(content, "let x = bar();\nlet y = bar();\n");
    Ok(())
}

#[test]
fn apply_fix_returns_false_when_no_fix_pattern() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("sample.rs");
    std::fs::write(&file_path, "let x = foo();\n")?;

    let mut engine = RuleEngine::new();
    engine.add_rule(Rule {
        id: RuleId::from_str("FIX-02"),
        title: "No fix".to_string(),
        severity: Severity::Low,
        category: Category::Style,
        paths: vec![],
        description: String::new(),
        fix_pattern: None,
    });

    let violation = Violation {
        rule_id: RuleId::from_str("FIX-02"),
        file: file_path,
        line: Some(1),
        message: "some issue".to_string(),
        severity: Severity::Low,
    };

    let modified = engine.apply_fix(&violation, dir.path())?;
    assert!(!modified);
    Ok(())
}

#[test]
fn apply_fix_returns_false_when_pattern_does_not_match() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("sample.rs");
    std::fs::write(&file_path, "let x = baz();\n")?;

    let mut engine = RuleEngine::new();
    engine.add_rule(Rule {
        id: RuleId::from_str("FIX-03"),
        title: "Replace foo".to_string(),
        severity: Severity::Low,
        category: Category::Style,
        paths: vec![],
        description: String::new(),
        fix_pattern: Some("s/foo/bar/".to_string()),
    });

    let violation = Violation {
        rule_id: RuleId::from_str("FIX-03"),
        file: file_path,
        line: Some(1),
        message: "use bar".to_string(),
        severity: Severity::Low,
    };

    let modified = engine.apply_fix(&violation, dir.path())?;
    assert!(!modified);
    Ok(())
}

#[test]
fn apply_fixes_returns_count_of_modified_files() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file1 = dir.path().join("a.rs");
    let file2 = dir.path().join("b.rs");
    std::fs::write(&file1, "foo()\n")?;
    std::fs::write(&file2, "foo()\n")?;

    let mut engine = RuleEngine::new();
    engine.add_rule(Rule {
        id: RuleId::from_str("FIX-04"),
        title: "Replace foo".to_string(),
        severity: Severity::Low,
        category: Category::Style,
        paths: vec![],
        description: String::new(),
        fix_pattern: Some("s/foo/bar/".to_string()),
    });

    let violations = vec![
        Violation {
            rule_id: RuleId::from_str("FIX-04"),
            file: file1,
            line: Some(1),
            message: "use bar".to_string(),
            severity: Severity::Low,
        },
        Violation {
            rule_id: RuleId::from_str("FIX-04"),
            file: file2,
            line: Some(1),
            message: "use bar".to_string(),
            severity: Severity::Low,
        },
    ];

    let fixed = engine.apply_fixes(&violations, dir.path())?;
    assert_eq!(fixed, 2);
    Ok(())
}

#[test]
fn parse_rule_file_sets_fix_pattern_from_frontmatter() -> anyhow::Result<()> {
    let md = "---\npaths: [\"*.rs\"]\nfix_pattern: \"s/foo/bar/\"\n---\n\n## FIX-10: Replace foo\n\nlow severity\n";
    let engine = make_engine_with_content(md)?;
    assert_eq!(engine.rules().len(), 1);
    assert_eq!(engine.rules()[0].fix_pattern.as_deref(), Some("s/foo/bar/"));
    Ok(())
}

#[test]
fn load_configured_requirements_missing_path_errors() {
    let mut engine = RuleEngine::new();
    engine.configure_sources(
        Vec::new(),
        None,
        Some(PathBuf::from("/tmp/does-not-exist-requirements.toml")),
    );
    let error = engine
        .load_configured_requirements()
        .expect_err("missing configured requirements path must error");
    assert!(error.to_string().contains("rules.requirements_path"));
}

#[tokio::test]
async fn scan_and_fix_with_auto_fix_disabled_returns_empty_report() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project)?;

    // Guard that always emits no violations.
    let script = dir.path().join("noop.sh");
    std::fs::write(&script, "#!/usr/bin/env bash\nexit 0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms)?;
    }

    let mut engine = RuleEngine::new();
    engine.register_guard(Guard {
        id: GuardId::from_str("NOOP"),
        script_path: script,
        language: Language::Common,
        rules: vec![],
    });

    let report = engine.scan_and_fix(&project, false).await?;
    assert!(
        report.attempts.is_empty(),
        "auto_fix=false must not attempt any fix"
    );
    assert_eq!(report.fixed_count, 0);
    Ok(())
}

#[tokio::test]
async fn scan_and_fix_with_no_fixable_rules_returns_empty_attempts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project)?;

    // Guard that emits one violation for a rule with no fix_pattern.
    let script = dir.path().join("guard.sh");
    std::fs::write(
        &script,
        "#!/usr/bin/env bash\necho \"src/lib.rs:1:FIX-NO-PATTERN:some issue\"\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms)?;
    }

    let mut engine = RuleEngine::new();
    engine.register_guard(Guard {
        id: GuardId::from_str("TEST"),
        script_path: script,
        language: Language::Common,
        rules: vec![],
    });
    engine.add_rule(Rule {
        id: RuleId::from_str("FIX-NO-PATTERN"),
        title: "No pattern rule".to_string(),
        severity: Severity::Low,
        category: Category::Style,
        paths: vec![],
        description: String::new(),
        fix_pattern: None,
    });

    let report = engine.scan_and_fix(&project, true).await?;
    assert!(
        report.attempts.is_empty(),
        "no fix_pattern rules should produce no attempts"
    );
    assert_eq!(report.fixed_count, 0);
    // The residual violations still include the unfixable one.
    assert_eq!(report.residual_violations.len(), 1);
    Ok(())
}

#[tokio::test]
async fn scan_and_fix_resolves_violation_when_auto_fix_enabled() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project)?;

    // File that triggers a violation when it contains "foo()".
    let src_file = project.join("sample.rs");
    std::fs::write(&src_file, "let x = foo();\n")?;

    // Guard: emit a violation for sample.rs when "foo()" is present.
    let script = dir.path().join("detect-foo.sh");
    std::fs::write(
        &script,
        "#!/usr/bin/env bash\n\
             file=\"$1/sample.rs\"\n\
             if grep -q 'foo()' \"$file\" 2>/dev/null; then\n\
               echo \"$file:1:FIX-AUTO:use bar instead of foo\"\n\
             fi\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms)?;
    }

    let mut engine = RuleEngine::new();
    engine.register_guard(Guard {
        id: GuardId::from_str("FOO-GUARD"),
        script_path: script,
        language: Language::Common,
        rules: vec![],
    });
    engine.add_rule(Rule {
        id: RuleId::from_str("FIX-AUTO"),
        title: "Replace foo".to_string(),
        severity: Severity::Low,
        category: Category::Style,
        paths: vec![],
        description: String::new(),
        fix_pattern: Some("s/foo/bar/".to_string()),
    });

    let report = engine.scan_and_fix(&project, true).await?;

    assert_eq!(report.fixed_count, 1, "one file should be fixed");
    assert_eq!(report.attempts.len(), 1);
    assert!(report.attempts[0].applied, "fix should be applied");
    assert!(
        report.attempts[0].resolved,
        "violation should be resolved after re-scan"
    );
    assert!(
        report.residual_violations.is_empty(),
        "no violations should remain after fix"
    );

    let content = std::fs::read_to_string(&src_file)?;
    assert_eq!(content, "let x = bar();\n");
    Ok(())
}
