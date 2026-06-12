use super::*;

fn make_skill(name: &str, location: SkillLocation) -> Skill {
    Skill {
        id: SkillId::new(),
        name: name.to_string(),
        description: "desc".to_string(),
        content: "content".to_string(),
        trigger_patterns: Vec::new(),
        version: "1.0.0".to_string(),
        author: "test".to_string(),
        location,
        content_hash: compute_content_hash("content"),
        usage_count: 0,
        last_used: None,
        quality_score: default_quality_score(),
        scored_samples: 0,
        governance_status: SkillGovernanceStatus::Active,
        canary_ratio: default_canary_ratio(),
        last_scored: None,
    }
}

fn make_skill_with_patterns(
    name: &str,
    location: SkillLocation,
    description: &str,
    trigger_patterns: &[&str],
    author: &str,
) -> Skill {
    Skill {
        description: description.to_string(),
        content: String::new(),
        content_hash: compute_content_hash(""),
        trigger_patterns: trigger_patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect(),
        author: author.to_string(),
        ..make_skill(name, location)
    }
}

fn make_retired_skill_with_patterns(
    name: &str,
    location: SkillLocation,
    description: &str,
    trigger_patterns: &[&str],
    author: &str,
) -> Skill {
    Skill {
        quality_score: 0.2,
        scored_samples: 100,
        governance_status: SkillGovernanceStatus::Retired,
        canary_ratio: 0.0,
        last_scored: Some(Utc::now()),
        ..make_skill_with_patterns(name, location, description, trigger_patterns, author)
    }
}

#[test]
fn deduplicate_keeps_higher_priority() {
    let mut store = SkillStore::new();
    store
        .skills
        .push(make_skill("deploy", SkillLocation::System));
    store.skills.push(make_skill("deploy", SkillLocation::Repo));
    store.deduplicate();
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].location, SkillLocation::Repo);
}

#[test]
fn deduplicate_removes_lower_priority_duplicate() {
    let mut store = SkillStore::new();
    store.skills.push(make_skill("lint", SkillLocation::User));
    store.skills.push(make_skill("lint", SkillLocation::Admin));
    store.deduplicate();
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].location, SkillLocation::User);
}

#[test]
fn deduplicate_keeps_unique_skills() {
    let mut store = SkillStore::new();
    store.skills.push(make_skill("alpha", SkillLocation::Repo));
    store.skills.push(make_skill("beta", SkillLocation::User));
    store.deduplicate();
    assert_eq!(store.list().len(), 2);
}

#[test]
fn create_adds_skill_to_store() {
    let mut store = SkillStore::new();
    store.create(
        "my-skill".to_string(),
        "# My Skill\nDoes stuff.".to_string(),
    );
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].name, "my-skill");
}

#[test]
fn delete_removes_skill() {
    let mut store = SkillStore::new();
    store.create("removable".to_string(), "content".to_string());
    let id = store.list()[0].id.clone();
    assert!(store.delete(&id));
    assert!(store.list().is_empty());
}

#[test]
fn create_persists_file_to_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let persist_path = dir.path().to_path_buf();
    let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
    store.create(
        "my-skill".to_string(),
        "# My Skill\nDoes stuff.".to_string(),
    );
    let file = persist_path.join("my-skill.md");
    assert!(file.exists(), "skill file should be written to disk");
    let contents = std::fs::read_to_string(&file).expect("read file");
    assert!(contents.contains("Does stuff."));
}

#[test]
fn delete_removes_file_from_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let persist_path = dir.path().to_path_buf();
    let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
    store.create("removable".to_string(), "content".to_string());
    let file = persist_path.join("removable.md");
    assert!(file.exists(), "file should exist after create");
    let id = store.list()[0].id.clone();
    assert!(store.delete(&id));
    assert!(!file.exists(), "file should be removed after delete");
}

#[test]
fn search_finds_by_name() {
    let mut store = SkillStore::new();
    store.create("rust-lint".to_string(), "# Lint tool".to_string());
    store.create("python-format".to_string(), "# Format tool".to_string());
    let results = store.search("rust");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "rust-lint");
}

#[test]
fn load_builtin_adds_all_registered_skills() {
    let mut store = SkillStore::new();
    store.load_builtin();
    assert_eq!(store.list().len(), crate::builtin::BUILTIN_SKILLS.len());
}

#[test]
fn load_builtin_all_system_location() {
    let mut store = SkillStore::new();
    store.load_builtin();
    for skill in store.list() {
        assert_eq!(
            skill.location,
            SkillLocation::System,
            "builtin skill '{}' should have System location",
            skill.name
        );
    }
}

#[test]
fn load_builtin_user_skill_overrides_builtin() {
    let mut store = SkillStore::new();
    store.skills.push(make_skill("review", SkillLocation::User));
    store.load_builtin();
    assert_eq!(store.list().len(), crate::builtin::BUILTIN_SKILLS.len());
    let Some(review) = store.get_by_name("review") else {
        panic!("review skill must exist");
    };
    assert_eq!(review.location, SkillLocation::User);
}

#[test]
fn load_builtin_idempotent() {
    let mut store = SkillStore::new();
    store.load_builtin();
    store.load_builtin();
    assert_eq!(
        store.list().len(),
        crate::builtin::BUILTIN_SKILLS.len(),
        "calling load_builtin twice must not duplicate skills"
    );
}

#[test]
fn load_builtin_restores_sidecar_governance_from_persist_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let persist_path = dir.path().to_path_buf();
    let sidecar_path = persist_path.join("review.usage.json");
    let sidecar = SkillUsage {
        usage_count: 7,
        last_used: Some(Utc::now()),
        quality_score: 0.2,
        scored_samples: 40,
        governance_status: SkillGovernanceStatus::Retired,
        canary_ratio: 0.0,
        last_scored: Some(Utc::now()),
    };
    std::fs::write(
        &sidecar_path,
        serde_json::to_string(&sidecar).expect("serialize sidecar"),
    )
    .expect("write sidecar");

    let mut store = SkillStore::new().with_persist_dir(persist_path);
    store.load_builtin();
    let review = store
        .get_by_name("review")
        .expect("builtin review skill should exist");
    assert_eq!(review.usage_count, 7);
    assert_eq!(review.governance_status, SkillGovernanceStatus::Retired);
    assert_eq!(review.scored_samples, 40);
}

#[test]
fn get_by_name_returns_correct_skill() {
    let mut store = SkillStore::new();
    store.load_builtin();
    let Some(skill) = store.get_by_name("interview") else {
        panic!("interview skill must exist");
    };
    assert_eq!(skill.name, "interview");
    assert_eq!(skill.location, SkillLocation::System);
}

#[test]
fn get_by_name_returns_none_for_missing() {
    let store = SkillStore::new();
    assert!(store.get_by_name("nonexistent").is_none());
}

#[test]
fn load_builtin_skills_have_trigger_patterns() {
    let mut store = SkillStore::new();
    store.load_builtin();
    for skill in store.list() {
        assert!(
            !skill.trigger_patterns.is_empty(),
            "builtin skill '{}' should have trigger patterns parsed from content",
            skill.name
        );
    }
}

#[test]
fn create_parses_trigger_patterns_from_content() {
    let mut store = SkillStore::new();
    store.create(
        "my-skill".to_string(),
        "# My Skill\n<!-- trigger-patterns: my keyword, another pattern -->\nContent.".to_string(),
    );
    assert_eq!(
        store.list()[0].trigger_patterns,
        vec!["my keyword", "another pattern"]
    );
}

#[test]
fn match_prompt_returns_matching_skills() {
    let mut store = SkillStore::new();
    store.skills.push(make_skill_with_patterns(
        "review",
        SkillLocation::System,
        "review code",
        &["code review", "review pr"],
        "system",
    ));
    let matches = store.match_prompt("please do a code review of this PR");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "review");
}

#[test]
fn match_prompt_is_case_insensitive() {
    let mut store = SkillStore::new();
    store.skills.push(make_skill_with_patterns(
        "build-fix",
        SkillLocation::System,
        "fix builds",
        &["build error"],
        "system",
    ));
    let matches = store.match_prompt("I have a BUILD ERROR in my project");
    assert_eq!(matches.len(), 1);
}

#[test]
fn match_prompt_skips_skills_without_patterns() {
    let mut store = SkillStore::new();
    store
        .skills
        .push(make_skill("no-patterns", SkillLocation::System));
    let matches = store.match_prompt("any prompt with no-patterns keywords");
    assert!(
        matches.is_empty(),
        "skills without patterns should not be returned"
    );
}

#[test]
fn match_prompt_returns_empty_when_no_match() {
    let mut store = SkillStore::new();
    store.skills.push(make_skill_with_patterns(
        "review",
        SkillLocation::System,
        "review code",
        &["code review"],
        "system",
    ));
    let matches = store.match_prompt("implement feature X");
    assert!(matches.is_empty());
}

#[test]
fn parse_version_from_frontmatter_returns_version_field() {
    let content = "---\nversion: 2.3.4\n---\n# Title\n";
    assert_eq!(parse_version_from_frontmatter(content), "2.3.4");
}

#[test]
fn parse_version_from_frontmatter_defaults_when_absent() {
    let content = "# Title\nNo frontmatter here.";
    assert_eq!(parse_version_from_frontmatter(content), "1.0.0");
}

#[test]
fn parse_version_from_frontmatter_defaults_when_field_missing() {
    let content = "---\nauthor: alice\n---\n# Title\n";
    assert_eq!(parse_version_from_frontmatter(content), "1.0.0");
}

#[test]
fn parse_version_from_frontmatter_ignores_trailing_whitespace() {
    let content = "---\nversion:  1.2.3  \n---\n";
    assert_eq!(parse_version_from_frontmatter(content), "1.2.3");
}

#[test]
fn increment_patch_bumps_last_component() {
    assert_eq!(increment_patch("1.0.0"), "1.0.1");
    assert_eq!(increment_patch("2.5.9"), "2.5.10");
}

#[test]
fn increment_patch_does_not_touch_major_or_minor() {
    assert_eq!(increment_patch("3.7.2"), "3.7.3");
}

#[test]
fn update_increments_patch_when_content_changes() {
    let mut store = SkillStore::new();
    store.create("skill-a".to_string(), "original content".to_string());
    let id = store.list()[0].id.clone();
    assert_eq!(store.list()[0].version, "1.0.0");
    store.update(&id, "changed content".to_string());
    assert_eq!(store.list()[0].version, "1.0.1");
}

#[test]
fn update_does_not_increment_when_content_unchanged() {
    let mut store = SkillStore::new();
    store.create("skill-b".to_string(), "same content".to_string());
    let id = store.list()[0].id.clone();
    store.update(&id, "same content".to_string());
    assert_eq!(store.list()[0].version, "1.0.0");
}

#[test]
fn update_returns_none_for_unknown_id() {
    let mut store = SkillStore::new();
    let unknown = SkillId::new();
    assert!(store.update(&unknown, "content".to_string()).is_none());
}

#[test]
fn create_parses_version_from_frontmatter() {
    let mut store = SkillStore::new();
    store.create(
        "versioned".to_string(),
        "---\nversion: 3.1.4\n---\n# Title\n".to_string(),
    );
    assert_eq!(store.list()[0].version, "3.1.4");
}

#[test]
fn create_defaults_version_when_no_frontmatter() {
    let mut store = SkillStore::new();
    store.create(
        "plain".to_string(),
        "# Plain skill\nNo frontmatter.".to_string(),
    );
    assert_eq!(store.list()[0].version, "1.0.0");
}

#[test]
fn record_use_increments_counter() {
    let mut store = SkillStore::new();
    let skill = make_skill("deploy", SkillLocation::System);
    let id = skill.id.clone();
    store.skills.push(skill);
    store.record_use(&id);
    store.record_use(&id);
    let s = store.get(&id).unwrap();
    assert_eq!(s.usage_count, 2);
    assert!(s.last_used.is_some());
}

#[test]
fn record_use_unknown_id_is_noop() {
    let mut store = SkillStore::new();
    store
        .skills
        .push(make_skill("deploy", SkillLocation::System));
    let unknown = SkillId::new();
    store.record_use(&unknown);
    assert_eq!(store.list()[0].usage_count, 0);
}

#[test]
fn record_use_persists_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = SkillStore::new();
    let skill = make_skill("deploy", SkillLocation::User);
    let id = skill.id.clone();
    store.skills.push(skill);
    store
        .skill_dirs
        .insert("deploy".to_string(), tmp.path().to_path_buf());
    store.record_use(&id);
    let sidecar = tmp.path().join("deploy.usage.json");
    assert!(sidecar.exists(), "sidecar file should be created");
    let data: SkillUsage =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
    assert_eq!(data.usage_count, 1);
    assert!(data.last_used.is_some());
    assert_eq!(data.governance_status, SkillGovernanceStatus::Active);
    assert!(data.quality_score > 0.0);
}

#[test]
fn load_usage_sidecar_returns_defaults_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let usage = load_usage_sidecar(tmp.path(), "nonexistent");
    assert_eq!(usage.usage_count, 0);
    assert!(usage.last_used.is_none());
    assert_eq!(usage.governance_status, SkillGovernanceStatus::Active);
    assert_eq!(usage.canary_ratio, 1.0);
}

#[test]
fn load_usage_sidecar_restores_persisted_values() {
    let tmp = tempfile::tempdir().unwrap();
    let sidecar = tmp.path().join("myskill.usage.json");
    let stored = SkillUsage {
        usage_count: 42,
        last_used: Some(chrono::Utc::now()),
        ..Default::default()
    };
    std::fs::write(&sidecar, serde_json::to_string(&stored).unwrap()).unwrap();
    let loaded = load_usage_sidecar(tmp.path(), "myskill");
    assert_eq!(loaded.usage_count, 42);
    assert!(loaded.last_used.is_some());
}

#[test]
fn governance_update_can_quarantine_skill() {
    let mut store = SkillStore::new();
    store.create(
        "review-skill".to_string(),
        "# review\n<!-- trigger-patterns: review -->".to_string(),
    );
    let id = store.list()[0].id.clone();

    let update = store
        .apply_governance_outcome(
            &id,
            SkillGovernanceInput {
                success: 0,
                fail: 20,
                unknown: 0,
            },
        )
        .expect("governance update should exist");
    assert_eq!(update.current_status, SkillGovernanceStatus::Quarantine);
    assert_eq!(update.canary_ratio, 0.1);
    assert!(update.quality_score < 0.45);
}

#[test]
fn governance_update_ignores_unknown_only_samples() {
    let mut store = SkillStore::new();
    store.create(
        "unknown-skill".to_string(),
        "# unknown\n<!-- trigger-patterns: unknown -->".to_string(),
    );
    let id = store.list()[0].id.clone();

    let update = store.apply_governance_outcome(
        &id,
        SkillGovernanceInput {
            success: 0,
            fail: 0,
            unknown: 12,
        },
    );
    assert!(
        update.is_none(),
        "unknown-only samples should not change score"
    );

    let skill = store.get(&id).expect("skill should exist");
    assert_eq!(skill.scored_samples, 0);
    assert_eq!(skill.quality_score, default_quality_score());
}

#[test]
fn retired_skill_is_not_auto_injected() {
    let mut store = SkillStore::new();
    store.skills.push(make_retired_skill_with_patterns(
        "retired-review",
        SkillLocation::System,
        "review code",
        &["code review"],
        "system",
    ));
    let matches = store.match_prompt("please do a code review of this PR");
    assert!(matches.is_empty(), "retired skill should not auto-inject");
}

#[test]
fn discover_reloads_persisted_skills_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let persist_path = dir.path().to_path_buf();

    // First "session": create and persist a skill to disk.
    let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
    store.create(
        "custom-workflow".to_string(),
        "# Custom Workflow\nDoes custom things.".to_string(),
    );
    assert!(
        persist_path.join("custom-workflow.md").exists(),
        "skill file must be written to disk before restart"
    );

    // Simulate server restart: new store, same persist_dir.
    let mut restarted = SkillStore::new().with_persist_dir(persist_path.clone());
    restarted.load_builtin();
    restarted
        .discover()
        .expect("discover must not fail on restart");

    assert!(
        restarted.get_by_name("custom-workflow").is_some(),
        "persisted skill must survive simulated server restart"
    );
}

#[test]
fn persisted_skill_shadowing_builtin_survives_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let persist_path = dir.path().to_path_buf();

    // First "session": create a user skill whose name matches a builtin.
    let user_content = "# review\nMy custom review skill.".to_string();
    let mut store = SkillStore::new().with_persist_dir(persist_path.clone());
    store.create("review".to_string(), user_content.clone());
    assert!(
        persist_path.join("review.md").exists(),
        "skill file must be written to disk before restart"
    );

    // Simulate server restart: load builtins first (as http.rs does),
    // then discover persisted skills.
    let mut restarted = SkillStore::new().with_persist_dir(persist_path.clone());
    restarted.load_builtin();
    restarted
        .discover()
        .expect("discover must not fail on restart");

    // The user's override must win over the same-named builtin.
    let skill = restarted
        .get_by_name("review")
        .expect("shadowed builtin must survive restart");
    assert_eq!(
        skill.location,
        SkillLocation::User,
        "persisted skill must have User location, not System"
    );
    assert_eq!(
        skill.content, user_content,
        "persisted skill content must be the user version, not the builtin"
    );
}

// ── rebase-pr skill: playbook coverage ────────────────────────────────────────
//
// The rebase-pr skill is a markdown playbook for the agent. The acceptance
// criteria from issue #985 require unit tests covering: stray-skip,
// duplicate-skip, HEAD-supersedes, and escalation. These tests verify the
// loaded skill content names each strategy explicitly so future edits cannot
// silently drop one of the four classification arms.

fn rebase_pr_skill() -> Skill {
    let mut store = SkillStore::new();
    store.load_builtin();
    store
        .get_by_name("rebase-pr")
        .cloned()
        .expect("rebase-pr builtin must be registered")
}

#[test]
fn rebase_pr_skill_is_registered_and_triggers_on_rebase_prompt() {
    let mut store = SkillStore::new();
    store.load_builtin();
    let matches =
        store.match_prompt("PR #42 is conflicting; run git rebase origin/main and push the result");
    assert!(
        matches.iter().any(|s| s.name == "rebase-pr"),
        "rebase-pr skill must auto-inject on a rebase-themed prompt"
    );
}

#[test]
fn rebase_pr_playbook_covers_stray_skip() {
    let skill = rebase_pr_skill();
    let body = skill.content.to_lowercase();
    assert!(
        body.contains("stray commit") && body.contains("git rebase --skip"),
        "playbook must cover the stray-skip arm with the --skip command"
    );
}

#[test]
fn rebase_pr_playbook_covers_duplicate_skip() {
    let skill = rebase_pr_skill();
    let body = skill.content.to_lowercase();
    assert!(
        body.contains("duplicate commit") && body.contains("git rebase --skip"),
        "playbook must cover the duplicate-skip arm with the --skip command"
    );
}

#[test]
fn rebase_pr_playbook_covers_head_supersedes() {
    let skill = rebase_pr_skill();
    let body = skill.content.to_lowercase();
    assert!(
        body.contains("head supersedes") && body.contains("--ours"),
        "playbook must cover the HEAD-supersedes arm and instruct keeping our side"
    );
}

#[test]
fn rebase_pr_playbook_covers_escalation_with_structured_report() {
    let skill = rebase_pr_skill();
    let body = skill.content.to_lowercase();
    assert!(
        body.contains("semantic_conflict") && body.contains("conflicting_files"),
        "escalation arm must emit a structured report, not the bare \
         'manual resolution required' string"
    );
}

#[test]
fn rebase_pr_playbook_covers_cherry_pick_fallback() {
    let skill = rebase_pr_skill();
    let body = skill.content.to_lowercase();
    assert!(
        body.contains("cherry-pick") && body.contains("origin/$base"),
        "playbook must include a cherry-pick-onto-fresh-main fallback"
    );
}

/// Regression for #1078: the skill must prescribe terminal tokens that the
/// server's `parse_pr_review_prep_outcome` parser actually accepts. The parser
/// rejects bare `REBASE_CONFLICT` (without the mandatory `paths=...` suffix)
/// and any other `REBASE_*` token. If the skill prescribes
/// `REBASE_VERIFY_FAILED` or bare `REBASE_CONFLICT`, an agent following the
/// playbook produces output the server discards as malformed, losing the
/// structured report.
#[test]
fn rebase_pr_terminal_tokens_match_parser_contract() {
    let skill = rebase_pr_skill();
    let body = skill.content.as_str();

    // The three accepted tokens must all be documented.
    assert!(
        body.contains("REBASE_PUSHED"),
        "skill must document REBASE_PUSHED — the parser-accepted success token"
    );
    assert!(
        body.contains("REBASE_SKIPPED"),
        "skill must document REBASE_SKIPPED — the parser-accepted no-op token"
    );
    assert!(
        body.contains("REBASE_CONFLICT paths="),
        "skill must prescribe REBASE_CONFLICT with the mandatory paths= suffix; \
         the parser rejects bare REBASE_CONFLICT and the report is lost"
    );

    // Tokens the parser rejects must not appear as prescribed terminal tokens.
    // (Substring match is a deliberate over-approximation — even a stray
    // mention can mislead an agent following the playbook.)
    assert!(
        !body.contains("REBASE_VERIFY_FAILED"),
        "skill must not prescribe REBASE_VERIFY_FAILED — the parser rejects \
         it, so the structured report is discarded; verify-failed should be \
         emitted as REBASE_CONFLICT paths=... with classification=verify_failed \
        in the structured report"
    );
}

/// Regression for #1078: direct PR review-prep prompts provide their own
/// isolated worktree path, usually `/tmp/harness-pr-{pr}`. The skill must not
/// contradict that caller contract by sending the agent to a different
/// hard-coded path.
#[test]
fn rebase_pr_worktree_path_uses_caller_provided_contract() {
    let skill = rebase_pr_skill();
    let body = skill.content.as_str();

    assert!(
        body.contains("caller-provided isolated worktree"),
        "skill must tell agents to use the worktree path supplied by the caller"
    );
    assert!(
        body.contains("/tmp/harness-pr-{pr}"),
        "skill should document the direct PR review-prep worktree shape"
    );
    assert!(
        !body.contains("/tmp/harness-rebase"),
        "skill must not hard-code a second worktree path that conflicts with \
         the PR review-prep prompt"
    );
}
