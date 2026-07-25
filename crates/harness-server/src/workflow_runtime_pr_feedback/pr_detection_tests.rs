use super::*;
// --- parse_harness_mention_command (pre-existing, light coverage) ---

#[test]
fn parses_fix_ci_command() {
    assert_eq!(
        parse_harness_mention_command("@harness fix ci please"),
        Some(HarnessMentionCommand::FixCi)
    );
}

#[test]
fn parses_review_command() {
    assert_eq!(
        parse_harness_mention_command("@harness review"),
        Some(HarnessMentionCommand::Review)
    );
}

#[test]
fn parses_plain_mention() {
    assert_eq!(
        parse_harness_mention_command("hey @harness, take a look"),
        Some(HarnessMentionCommand::Mention)
    );
}

#[test]
fn no_mention_returns_none() {
    assert_eq!(parse_harness_mention_command("nothing here"), None);
}

// --- parse_repo_slug_from_remote_url ---

#[test]
fn parses_ssh_remote() {
    assert_eq!(
        parse_repo_slug_from_remote_url("git@github.com:owner/repo.git"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn parses_https_remote() {
    assert_eq!(
        parse_repo_slug_from_remote_url("https://github.com/owner/repo.git"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn parses_https_remote_without_git_suffix() {
    assert_eq!(
        parse_repo_slug_from_remote_url("https://github.com/owner/repo"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn parses_ssh_scheme_remote() {
    // ssh://git@github.com/owner/repo.git — distinct from SCP-style
    // git@github.com:owner/repo.git; without this branch, detect_repo_slug
    // returns None and the cross-repo guard is silently disabled.
    assert_eq!(
        parse_repo_slug_from_remote_url("ssh://git@github.com/owner/repo.git"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn rejects_unknown_remote() {
    assert_eq!(
        parse_repo_slug_from_remote_url("https://gitlab.com/owner/repo.git"),
        None
    );
}

#[test]
fn parse_git_config_accepts_dotted_remote_section() {
    let config = r#"
        [remote.origin]
            url = https://github.com/owner/repo.git
    "#;
    assert_eq!(
        parse_remote_urls_from_git_config(config),
        vec![(
            "origin".to_string(),
            "https://github.com/owner/repo.git".to_string()
        )]
    );
}

#[test]
fn parse_git_config_accepts_quoted_remote_with_spacing_and_comments() {
    let config = r#"
        # ignored
        [remote "upstream"]
            fetch = +refs/heads/*:refs/remotes/upstream/*
            url=git@github.com:owner/repo.git ; mirror used by tests
        [branch "main"]
            remote = upstream
    "#;
    assert_eq!(
        parse_remote_urls_from_git_config(config),
        vec![(
            "upstream".to_string(),
            "git@github.com:owner/repo.git".to_string()
        )]
    );
}
