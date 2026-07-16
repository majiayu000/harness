use super::*;
use harness_core::types::{
    Artifact, ArtifactType, Draft, DraftStatus, ProjectId, RemediationType, Signal, SignalType,
};

fn make_test_gc_agent(dir: &std::path::Path) -> GcAgent {
    let signal_detector = SignalDetector::new(
        crate::signal_detector::SignalThresholds::default(),
        ProjectId::new(),
    );
    let draft_store = DraftStore::new(dir).unwrap();
    GcAgent::new(
        GcConfig::default(),
        signal_detector,
        draft_store,
        dir.to_path_buf(),
    )
}

fn test_signal() -> Signal {
    Signal::new(
        SignalType::RepeatedWarn,
        ProjectId::new(),
        serde_json::json!({"test": true}),
        RemediationType::Guard,
    )
}

fn test_draft(artifacts: Vec<Artifact>) -> Draft {
    Draft {
        id: DraftId::new(),
        status: DraftStatus::Pending,
        signal: test_signal(),
        artifacts,
        rationale: "test".into(),
        validation: "test".into(),
        generated_at: Utc::now(),
        agent_model: "test".into(),
    }
}

#[test]
fn artifact_path_under_prefix_accepts_paths_inside_prefix() {
    assert!(artifact_path_under_prefix(
        Path::new(".harness/generated/rules/new.md"),
        ".harness/generated/"
    ));
    assert!(artifact_path_under_prefix(
        Path::new(".harness/generated/rules/./new.md"),
        ".harness/generated/"
    ));
}

#[test]
fn artifact_path_under_prefix_rejects_paths_outside_prefix() {
    assert!(!artifact_path_under_prefix(
        Path::new("src/main.rs"),
        ".harness/generated/"
    ));
    assert!(!artifact_path_under_prefix(
        Path::new("/etc/passwd"),
        ".harness/generated/"
    ));
    assert!(!artifact_path_under_prefix(
        Path::new(".harness/generated/../../etc/passwd"),
        ".harness/generated/"
    ));
}

#[test]
fn auto_adopt_matching_off_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let gc = make_test_gc_agent(dir.path());
    let draft = test_draft(vec![]);
    gc.draft_store.save(&draft).unwrap();
    let adopted = gc.auto_adopt_matching(
        std::slice::from_ref(&draft.id),
        AutoAdoptPolicy::Off,
        ".harness/generated/",
    );
    assert!(adopted.is_empty());
}

#[test]
fn auto_adopt_matching_rules_only_adopts_rule_drafts_under_prefix() {
    let sandbox = tempfile::tempdir().unwrap();
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let signal_detector = SignalDetector::new(
        crate::signal_detector::SignalThresholds::default(),
        ProjectId::new(),
    );
    let draft_store = DraftStore::new(sandbox.path()).unwrap();
    let gc = GcAgent::new(
        GcConfig::default(),
        signal_detector,
        draft_store,
        project_root.clone(),
    );

    // Eligible: Rule remediation + artifact under prefix.
    let eligible = Draft {
        signal: Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({}),
            RemediationType::Rule,
        ),
        ..test_draft(vec![Artifact {
            artifact_type: ArtifactType::Rule,
            target_path: PathBuf::from(".harness/generated/rules/new.md"),
            content: "rule".into(),
        }])
    };
    gc.draft_store.save(&eligible).unwrap();

    // Ineligible — remediation is Guard.
    let guard_draft = test_draft(vec![Artifact {
        artifact_type: ArtifactType::Guard,
        target_path: PathBuf::from(".harness/generated/guards/g.sh"),
        content: "guard".into(),
    }]);
    gc.draft_store.save(&guard_draft).unwrap();

    // Ineligible — artifact outside prefix.
    let outside = Draft {
        signal: Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({}),
            RemediationType::Rule,
        ),
        ..test_draft(vec![Artifact {
            artifact_type: ArtifactType::Rule,
            target_path: PathBuf::from("src/main.rs"),
            content: "rule".into(),
        }])
    };
    gc.draft_store.save(&outside).unwrap();

    let adopted = gc.auto_adopt_matching(
        &[
            eligible.id.clone(),
            guard_draft.id.clone(),
            outside.id.clone(),
        ],
        AutoAdoptPolicy::RulesOnly,
        ".harness/generated/",
    );
    assert_eq!(adopted, vec![eligible.id.clone()]);

    let reloaded = gc.draft_store.get(&eligible.id).unwrap().unwrap();
    assert_eq!(reloaded.status, DraftStatus::Adopted);
    let written =
        std::fs::read_to_string(project_root.join(".harness/generated/rules/new.md")).unwrap();
    assert_eq!(written, "rule");

    let guard_reloaded = gc.draft_store.get(&guard_draft.id).unwrap().unwrap();
    assert_eq!(guard_reloaded.status, DraftStatus::Pending);
    let outside_reloaded = gc.draft_store.get(&outside.id).unwrap().unwrap();
    assert_eq!(outside_reloaded.status, DraftStatus::Pending);
}

#[test]
fn adopt_rejects_absolute_path_outside_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let gc = make_test_gc_agent(dir.path());

    let draft = test_draft(vec![Artifact {
        artifact_type: ArtifactType::Guard,
        target_path: std::path::PathBuf::from("/etc/passwd"),
        content: "malicious".into(),
    }]);
    gc.draft_store.save(&draft).unwrap();

    let err = gc.adopt(&draft.id).unwrap_err();
    assert!(
        err.to_string().contains("outside project root"),
        "unexpected error: {err}"
    );
}

#[test]
fn adopt_rejects_parent_dir_traversal() {
    let sandbox = tempfile::tempdir().unwrap();
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let signal_detector = SignalDetector::new(
        crate::signal_detector::SignalThresholds::default(),
        ProjectId::new(),
    );
    let draft_store = DraftStore::new(&project_root).unwrap();
    let gc = GcAgent::new(
        GcConfig::default(),
        signal_detector,
        draft_store,
        project_root.clone(),
    );

    let draft = test_draft(vec![Artifact {
        artifact_type: ArtifactType::Skill,
        target_path: std::path::PathBuf::from("../../etc/shadow"),
        content: "traversal".into(),
    }]);
    gc.draft_store.save(&draft).unwrap();

    let err = gc.adopt(&draft.id).unwrap_err();
    assert!(
        err.to_string().contains("outside project root"),
        "unexpected error: {err}"
    );
    // Verify no file was written outside the project root
    let escaped = sandbox.path().join("etc/shadow");
    assert!(!escaped.exists());
}

#[test]
fn adopt_accepts_valid_relative_path() {
    let dir = tempfile::tempdir().unwrap();
    let gc = make_test_gc_agent(dir.path());

    let draft = test_draft(vec![Artifact {
        artifact_type: ArtifactType::Guard,
        target_path: std::path::PathBuf::from("subdir/test.md"),
        content: "valid content".into(),
    }]);
    gc.draft_store.save(&draft).unwrap();

    gc.adopt(&draft.id).unwrap();
    assert!(dir.path().join("subdir/test.md").exists());
}

// --- parse_artifacts tests ---

fn make_signal(remediation: RemediationType) -> Signal {
    Signal::new(
        SignalType::RepeatedWarn,
        ProjectId::new(),
        serde_json::json!({}),
        remediation,
    )
}

#[test]
fn parse_artifacts_falls_back_to_single_blob_when_no_structure() {
    let signal = make_signal(RemediationType::Guard);
    let output = "Some plain text output with no code blocks or diffs.";
    let artifacts = parse_artifacts(output, &signal);
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].content, output);
    assert!(artifacts[0].target_path.to_string_lossy().ends_with(".md"));
}

#[test]
fn parse_artifacts_extracts_code_blocks_with_paths() {
    let signal = make_signal(RemediationType::Guard);
    let output = "Here are the changes:\n\
        ```rust src/main.rs\n\
        fn main() {\n    println!(\"hello\");\n}\n\
        ```\n\
        And also:\n\
        ```python tests/test_foo.py\n\
        def test_foo():\n    pass\n\
        ```\n";
    let artifacts = parse_artifacts(output, &signal);
    assert_eq!(artifacts.len(), 2);

    let paths: Vec<_> = artifacts
        .iter()
        .map(|a| a.target_path.to_string_lossy().into_owned())
        .collect();
    assert!(paths.contains(&"src/main.rs".to_string()));
    assert!(paths.contains(&"tests/test_foo.py".to_string()));

    let main_rs = artifacts
        .iter()
        .find(|a| a.target_path.to_string_lossy() == "src/main.rs")
        .unwrap();
    assert!(main_rs.content.contains("fn main()"));
}

#[test]
fn parse_artifacts_skips_code_blocks_without_path() {
    let signal = make_signal(RemediationType::Rule);
    let output = "```rust\nfn foo() {}\n```\n";
    let artifacts = parse_artifacts(output, &signal);
    // No path in the block header → fall back to single blob
    assert_eq!(artifacts.len(), 1);
}

#[test]
fn parse_artifacts_extracts_unified_diff() {
    let signal = make_signal(RemediationType::Guard);
    let output = "Apply this patch:\n\
        --- a/src/lib.rs\n\
        +++ b/src/lib.rs\n\
        @@ -1,3 +1,4 @@\n\
         fn foo() {}\n\
        +fn bar() {}\n";
    let artifacts = parse_artifacts(output, &signal);
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0]
        .target_path
        .to_string_lossy()
        .starts_with(".harness/drafts/"));
    assert!(artifacts[0]
        .target_path
        .to_string_lossy()
        .ends_with(".patch"));
    assert!(artifacts[0].content.contains("+++ b/src/lib.rs"));
}

#[test]
fn parse_artifacts_multiple_diff_hunks() {
    let signal = make_signal(RemediationType::Hook);
    let output = "\
        --- a/a.rs\n\
        +++ b/a.rs\n\
        @@ -1 +1 @@\n\
        -old\n\
        +new\n\
        --- a/b.rs\n\
        +++ b/b.rs\n\
        @@ -1 +1 @@\n\
        -foo\n\
        +bar\n";
    let artifacts = parse_artifacts(output, &signal);
    assert_eq!(artifacts.len(), 2);
    let paths: Vec<_> = artifacts
        .iter()
        .map(|a| a.target_path.to_string_lossy().into_owned())
        .collect();
    assert!(paths.iter().all(|p| p.starts_with(".harness/drafts/")));
    assert!(paths.iter().all(|p| p.ends_with(".patch")));
}

#[test]
fn parse_artifacts_code_block_takes_precedence_over_diff_same_path() {
    let signal = make_signal(RemediationType::Skill);
    let output = "\
        ```rust src/lib.rs\n\
        fn full_content() {}\n\
        ```\n\
        --- a/src/lib.rs\n\
        +++ b/src/lib.rs\n\
        @@ -1 +1 @@\n\
        +fn full_content() {}\n";
    let artifacts = parse_artifacts(output, &signal);
    // Deduplication: same path, code block wins
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].content.contains("fn full_content()"));
    // Content should be from the code block, not the diff header
    assert!(!artifacts[0].content.contains("+++ b/src/lib.rs"));
}

#[test]
fn adopt_rejected_draft_errors() {
    let dir = tempfile::tempdir().unwrap();
    let gc = make_test_gc_agent(dir.path());

    let mut draft = test_draft(vec![]);
    draft.status = DraftStatus::Rejected;
    gc.draft_store.save(&draft).unwrap();

    let err = gc.adopt(&draft.id).unwrap_err();
    assert!(
        err.to_string().contains("Rejected"),
        "unexpected error: {err}"
    );
}

#[test]
fn auto_adopt_skips_rejected() {
    let sandbox = tempfile::tempdir().unwrap();
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let signal_detector = SignalDetector::new(
        crate::signal_detector::SignalThresholds::default(),
        ProjectId::new(),
    );
    let draft_store = DraftStore::new(sandbox.path()).unwrap();
    let gc = GcAgent::new(
        GcConfig::default(),
        signal_detector,
        draft_store,
        project_root.clone(),
    );

    let mut rejected = Draft {
        signal: Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({}),
            RemediationType::Rule,
        ),
        ..test_draft(vec![Artifact {
            artifact_type: ArtifactType::Rule,
            target_path: PathBuf::from(".harness/generated/rules/rejected.md"),
            content: "should not be written".into(),
        }])
    };
    rejected.status = DraftStatus::Rejected;
    gc.draft_store.save(&rejected).unwrap();

    let adopted = gc.auto_adopt_matching(
        std::slice::from_ref(&rejected.id),
        AutoAdoptPolicy::RulesOnly,
        ".harness/generated/",
    );
    assert!(adopted.is_empty(), "rejected draft should not be adopted");

    let reloaded = gc.draft_store.get(&rejected.id).unwrap().unwrap();
    assert_eq!(reloaded.status, DraftStatus::Rejected);
    assert!(!project_root
        .join(".harness/generated/rules/rejected.md")
        .exists());
}
