use super::{
    configured_github_webhook_project_root, github_webhook_project_root_error_response,
    GitHubWebhookProjectRootError,
};
use axum::http::StatusCode;
use harness_core::config::intake::{GitHubIntakeConfig, GitHubRepoConfig};
use std::path::PathBuf;

#[test]
fn multi_repo_github_webhook_uses_repo_specific_project_root_override() {
    let default_root = PathBuf::from("/srv/repo-a");
    let github = GitHubIntakeConfig {
        enabled: true,
        repos: vec![
            GitHubRepoConfig {
                repo: "org/repo-a".to_string(),
                label: "harness".to_string(),
                project_root: None,
                auto_merge: None,
                auto_recovery: None,
                merge_method: None,
                delete_branch: None,
                require_review_threads_resolved: None,
                require_clean_merge_state: None,
            },
            GitHubRepoConfig {
                repo: "org/repo-b".to_string(),
                label: "harness".to_string(),
                project_root: Some("/srv/repo-b".to_string()),
                auto_merge: None,
                auto_recovery: None,
                merge_method: None,
                delete_branch: None,
                require_review_threads_resolved: None,
                require_clean_merge_state: None,
            },
        ],
        ..Default::default()
    };

    let resolved =
        configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-b");

    assert_eq!(resolved, Some(PathBuf::from("/srv/repo-b")));
}

#[test]
fn configured_github_repo_without_override_falls_back_to_default_project_root() {
    let default_root = PathBuf::from("/srv/repo-a");
    let github = GitHubIntakeConfig {
        enabled: true,
        repos: vec![GitHubRepoConfig {
            repo: "org/repo-a".to_string(),
            label: "harness".to_string(),
            project_root: None,
            auto_merge: None,
            auto_recovery: None,
            merge_method: None,
            delete_branch: None,
            require_review_threads_resolved: None,
            require_clean_merge_state: None,
        }],
        ..Default::default()
    };

    let resolved =
        configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-a");

    assert_eq!(resolved, Some(default_root));
}

#[test]
fn unconfigured_github_repo_has_no_configured_project_root() {
    let default_root = PathBuf::from("/srv/repo-a");
    let github = GitHubIntakeConfig {
        enabled: true,
        repos: vec![GitHubRepoConfig {
            repo: "org/repo-a".to_string(),
            label: "harness".to_string(),
            project_root: None,
            auto_merge: None,
            auto_recovery: None,
            merge_method: None,
            delete_branch: None,
            require_review_threads_resolved: None,
            require_clean_merge_state: None,
        }],
        ..Default::default()
    };

    let resolved =
        configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-b");

    assert_eq!(resolved, None);
}

#[test]
fn unconfigured_github_repo_returns_ignored_response() {
    let (status, body) = github_webhook_project_root_error_response(
        GitHubWebhookProjectRootError::RepoNotConfigured(
            "webhook repository 'org/repo-b' is not configured".to_string(),
        ),
    );

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.0["status"], "ignored");
    assert!(body.0["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("not configured"));
}

#[test]
fn registry_lookup_failures_return_internal_server_error() {
    let (status, body) =
        github_webhook_project_root_error_response(GitHubWebhookProjectRootError::RegistryLookup(
            "project registry lookup failed: boom".to_string(),
        ));

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body.0["error"], "project registry lookup failed: boom");
}
