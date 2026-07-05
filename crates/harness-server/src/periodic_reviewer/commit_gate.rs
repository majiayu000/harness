use super::ProjectInfo;
use crate::http::AppState;
use chrono::{DateTime, Utc};
use harness_core::types::{Decision, Event, SessionId};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitGateRunReason {
    FirstRun,
    NewCommits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommitGateDecision {
    Run(CommitGateRunReason),
    SkipNoNewCommits,
    Unknown(String),
}

pub(super) async fn should_skip_review(
    state: &Arc<AppState>,
    project: &ProjectInfo,
    hook_key: &str,
    last_review_ts: Option<DateTime<Utc>>,
    since_arg: &str,
) -> bool {
    match evaluate(&project.root, last_review_ts).await {
        CommitGateDecision::Run(reason) => {
            tracing::debug!(
                project = %project.name,
                project_root = %project.root.display(),
                since = %since_arg,
                reason = ?reason,
                "scheduler: local periodic review commit gate allows review"
            );
            false
        }
        CommitGateDecision::SkipNoNewCommits => {
            tracing::info!(
                project = %project.name,
                project_root = %project.root.display(),
                since = %since_arg,
                "scheduler: local periodic review commit gate skipped review; no new commits"
            );
            log_skip_event(state, project, hook_key, since_arg).await;
            true
        }
        CommitGateDecision::Unknown(reason) => {
            tracing::warn!(
                project = %project.name,
                project_root = %project.root.display(),
                since = %since_arg,
                error = %reason,
                "scheduler: local periodic review commit gate failed; falling back to agent-side REVIEW_SKIPPED"
            );
            false
        }
    }
}

async fn log_skip_event(
    state: &Arc<AppState>,
    project: &ProjectInfo,
    hook_key: &str,
    since_arg: &str,
) {
    let mut event = Event::new(
        SessionId::new(),
        &project_skip_hook_key(hook_key),
        "scheduler",
        Decision::Gate,
    );
    event.reason = Some("no_new_commits".to_string());
    event.detail = Some(format!(
        "project={} root={} since={}",
        project.name,
        project.root.display(),
        since_arg
    ));
    if let Err(err) = state.observability.events.log(&event).await {
        tracing::warn!(
            error = %err,
            project = %project.name,
            project_root = %project.root.display(),
            "scheduler: failed to log local periodic review skip event"
        );
    }
}

fn project_skip_hook_key(hook_key: &str) -> String {
    format!("{hook_key}:skip")
}

async fn evaluate(
    project_root: &Path,
    last_review_ts: Option<DateTime<Utc>>,
) -> CommitGateDecision {
    let Some(since) = last_review_ts else {
        return CommitGateDecision::Run(CommitGateRunReason::FirstRun);
    };

    match has_commit_since(project_root, since).await {
        Ok(true) => CommitGateDecision::Run(CommitGateRunReason::NewCommits),
        Ok(false) => CommitGateDecision::SkipNoNewCommits,
        Err(err) => CommitGateDecision::Unknown(err.to_string()),
    }
}

async fn has_commit_since(project_root: &Path, since: DateTime<Utc>) -> anyhow::Result<bool> {
    let since_arg = format!("--since={}", since.to_rfc3339());
    let output = crate::workspace::git_command()
        .arg("-C")
        .arg(project_root)
        .args(["log", "--format=%H", "-1"])
        .arg(since_arg)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::periodic_reviewer::project_hook_key;
    use std::io::Write;

    fn init_repo() -> anyhow::Result<tempfile::TempDir> {
        let repo = tempfile::tempdir()?;
        let repo_path = repo.path().to_string_lossy().into_owned();
        crate::workspace::test_support::run_git(&["-C", &repo_path, "init"]);
        crate::workspace::test_support::run_git(&[
            "-C",
            &repo_path,
            "config",
            "user.email",
            "test@harness.test",
        ]);
        crate::workspace::test_support::run_git(&[
            "-C",
            &repo_path,
            "config",
            "user.name",
            "Harness Test",
        ]);
        let mut file = std::fs::File::create(repo.path().join("file.txt"))?;
        file.write_all(b"initial\n")?;
        crate::workspace::test_support::run_git(&["-C", &repo_path, "add", "file.txt"]);
        crate::workspace::test_support::run_git(&[
            "-C",
            &repo_path,
            "commit",
            "--no-gpg-sign",
            "-m",
            "initial",
        ]);
        Ok(repo)
    }

    #[tokio::test]
    async fn no_watermark_runs_without_touching_git() -> anyhow::Result<()> {
        let repo = tempfile::tempdir()?;

        let decision = evaluate(repo.path(), None).await;

        assert_eq!(
            decision,
            CommitGateDecision::Run(CommitGateRunReason::FirstRun)
        );
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_repo_after_watermark_skips() -> anyhow::Result<()> {
        let repo = init_repo()?;
        let future_watermark = Utc::now() + chrono::Duration::days(1);

        let decision = evaluate(repo.path(), Some(future_watermark)).await;

        assert_eq!(decision, CommitGateDecision::SkipNoNewCommits);
        Ok(())
    }

    #[tokio::test]
    async fn repo_with_commit_after_watermark_runs() -> anyhow::Result<()> {
        let repo = init_repo()?;
        let epoch: DateTime<Utc> = DateTime::from(std::time::SystemTime::UNIX_EPOCH);

        let decision = evaluate(repo.path(), Some(epoch)).await;

        assert_eq!(
            decision,
            CommitGateDecision::Run(CommitGateRunReason::NewCommits)
        );
        Ok(())
    }

    #[tokio::test]
    async fn unreadable_commit_state_falls_back_to_unknown() -> anyhow::Result<()> {
        let repo = tempfile::tempdir()?;
        let epoch: DateTime<Utc> = DateTime::from(std::time::SystemTime::UNIX_EPOCH);

        let decision = evaluate(repo.path(), Some(epoch)).await;

        assert!(matches!(decision, CommitGateDecision::Unknown(_)));
        Ok(())
    }

    #[test]
    fn skip_hook_key_does_not_advance_review_watermark_key() {
        let review_hook = project_hook_key("project", std::path::Path::new("/repo"));
        let skip_hook = project_skip_hook_key(&review_hook);

        assert_ne!(skip_hook, review_hook);
        assert!(skip_hook.ends_with(":skip"));
    }

    #[test]
    fn local_gate_runs_before_primary_enqueue() {
        let source = include_str!("tick.rs");
        let Some(gate_index) = source.find("should_skip_review") else {
            panic!("gate call exists");
        };
        let Some(enqueue_index) = source.find("enqueue_task_background_in_domain") else {
            panic!("primary enqueue exists");
        };

        assert!(gate_index < enqueue_index);
    }
}
