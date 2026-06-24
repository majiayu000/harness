use std::sync::Arc;

use crate::http::AppState;
use crate::task_runner::TaskId;

use super::{IncomingIssue, IntakeSource};

const DEPENDENCY_MARKERS: &[&str] = &[
    "depends on",
    "blocked by",
    "requires",
    "after",
    "dependency:",
    "dependencies:",
];

const NON_DEPENDENCY_REFERENCE_MARKERS: &[&str] = &[
    " fixes #",
    " fix #",
    " fixed #",
    " closes #",
    " close #",
    " closed #",
    " resolves #",
    " resolve #",
    " resolved #",
    " addresses #",
    " address #",
    " references #",
    " reference #",
    " related #",
];

pub(super) struct DirectDispatchIssue {
    pub source: Arc<dyn IntakeSource>,
    pub issue: IncomingIssue,
    pub fact_hash: Option<String>,
}

#[cfg(test)]
fn issue_requires_dependency_analysis(issue: &IncomingIssue) -> bool {
    let text = issue_dependency_text(issue).to_ascii_lowercase();
    !dependency_clause_ranges(&text).is_empty()
}

fn issue_dependency_text(issue: &IncomingIssue) -> String {
    let mut text = issue.title.clone();
    if let Some(description) = issue.description.as_deref() {
        text.push('\n');
        text.push_str(description);
    }
    text
}

fn dependency_clause_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    for marker in DEPENDENCY_MARKERS {
        let mut offset = 0usize;
        while let Some(relative_start) = text[offset..].find(marker) {
            let marker_start = offset + relative_start;
            let clause_start = marker_start + marker.len();
            let clause_end = clause_start + dependency_clause_len(&text[clause_start..]);
            ranges.push(clause_start..clause_end);
            offset = clause_start;
        }
    }
    ranges.sort_by_key(|range| range.start);
    ranges
}

fn dependency_clause_len(text: &str) -> usize {
    let punctuation_boundary = text
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '\n' | '.' | ';' | '!' | '?').then_some(idx));
    let non_dependency_boundary = NON_DEPENDENCY_REFERENCE_MARKERS
        .iter()
        .filter_map(|marker| text.find(marker))
        .min();
    punctuation_boundary
        .into_iter()
        .chain(non_dependency_boundary)
        .min()
        .unwrap_or(text.len())
}

fn dependency_task_ids(issue: &IncomingIssue, repo: &str) -> Vec<TaskId> {
    let text = issue_dependency_text(issue).to_ascii_lowercase();
    let mut ids = Vec::new();
    for range in dependency_clause_ranges(&text) {
        for segment in text[range].split('#').skip(1) {
            let number = segment
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            let Some(issue_number) = number.parse::<u64>().ok() else {
                continue;
            };
            let task_id = TaskId::from_str(&format!("github-issue:{repo}:issue:{issue_number}"));
            if !ids.iter().any(|existing| existing == &task_id) {
                ids.push(task_id);
            }
        }
    }
    ids
}

pub(super) async fn run_direct_issue_dispatch(
    state: &Arc<AppState>,
    repo: &str,
    issues: Vec<DirectDispatchIssue>,
    project_root: std::path::PathBuf,
    project_id: String,
) {
    let Some(runtime_store) = state.core.workflow_runtime_store.as_deref() else {
        tracing::error!(
            repo,
            "intake: workflow runtime store is unavailable; cannot dispatch GitHub issues"
        );
        return;
    };
    let available = direct_available_slots(&state.concurrency.task_queue.diagnostics(&project_id));
    if available == 0 {
        tracing::debug!(repo, "intake: no direct dispatch slots available");
        return;
    }

    if let Some(workflows) = state.core.project_workflow_store.as_ref() {
        if let Err(error) = workflows
            .record_dispatch_started(&project_id, Some(repo))
            .await
        {
            tracing::warn!(
                repo,
                "intake: failed to mark project workflow dispatching state: {error}"
            );
        }
    }

    let mut dispatched = 0usize;
    for pending in issues.into_iter().take(available) {
        let DirectDispatchIssue {
            source,
            issue,
            fact_hash,
        } = pending;
        let ext_id = issue.external_id.clone();
        let Some(issue_number) = ext_id.parse::<u64>().ok() else {
            tracing::warn!(
                repo,
                external_id = %ext_id,
                "intake: direct workflow dispatch skipped non-numeric GitHub issue id"
            );
            continue;
        };

        let task_id = TaskId::from_str(&format!("github-issue:{repo}:issue:{issue_number}"));
        let depends_on = dependency_task_ids(&issue, repo);
        match crate::workflow_runtime_submission::record_issue_submission(
            runtime_store,
            crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some(repo),
                issue_number,
                task_id: &task_id,
                labels: &issue.labels,
                force_execute: true,
                additional_prompt: None,
                depends_on: &depends_on,
                dependencies_blocked: !depends_on.is_empty(),
                source: Some(source.name()),
                external_id: Some(ext_id.as_str()),
                remote_fact_hash: fact_hash.as_deref(),
            },
        )
        .await
        {
            Ok(record) => {
                dispatched += 1;
                tracing::info!(
                    repo,
                    external_id = %ext_id,
                    task_id = %task_id,
                    workflow_id = %record.workflow_id,
                    command_count = record.command_ids.len(),
                    "intake: directly submitted GitHub issue into workflow runtime"
                );
            }
            Err(error) => {
                tracing::error!(
                    repo,
                    external_id = %ext_id,
                    "intake: direct workflow submission failed: {error}"
                );
            }
        }
    }

    if let Some(workflows) = state.core.project_workflow_store.as_ref() {
        if let Err(error) = workflows.record_idle(&project_id, Some(repo)).await {
            tracing::warn!(
                repo,
                dispatched,
                "intake: failed to mark project workflow idle after direct dispatch: {error}"
            );
        }
    }
}

fn direct_available_slots(diag: &crate::task_queue::QueueDiagnostics) -> usize {
    let project_headroom = diag
        .project_limit
        .saturating_sub(diag.project_running + diag.project_awaiting_global);
    let global_headroom = diag.global_limit.saturating_sub(diag.global_running);
    project_headroom.min(global_headroom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(title: &str, description: Option<&str>) -> IncomingIssue {
        IncomingIssue {
            source: "github".to_string(),
            external_id: "7".to_string(),
            identifier: "#7".to_string(),
            title: title.to_string(),
            description: description.map(str::to_string),
            repo: Some("owner/repo".to_string()),
            url: None,
            priority: None,
            labels: vec![],
            created_at: None,
            project_root: None,
        }
    }

    #[test]
    fn dependency_markers_require_analysis() {
        assert!(issue_requires_dependency_analysis(&issue(
            "Implement parser",
            Some("Depends on #12")
        )));
        assert!(issue_requires_dependency_analysis(&issue(
            "Blocked by #2",
            None
        )));
        assert!(!issue_requires_dependency_analysis(&issue(
            "Fix typo",
            Some("Small standalone change")
        )));
    }

    #[test]
    fn dependency_task_ids_extracts_unique_github_issue_handles() {
        let issue = issue(
            "Implement parser",
            Some("Depends on #12 and blocked by #12 after #15"),
        );

        let ids = dependency_task_ids(&issue, "owner/repo")
            .into_iter()
            .map(|task_id| task_id.as_str().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                "github-issue:owner/repo:issue:12",
                "github-issue:owner/repo:issue:15"
            ]
        );
    }

    #[test]
    fn dependency_task_ids_ignore_non_dependency_issue_references() {
        let issue = issue("Implement parser", Some("Depends on #12; fixes #34"));

        let ids = dependency_task_ids(&issue, "owner/repo")
            .into_iter()
            .map(|task_id| task_id.as_str().to_string())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["github-issue:owner/repo:issue:12"]);
    }

    #[test]
    fn dependency_task_ids_stop_before_inline_closing_references() {
        let issue = issue(
            "Implement parser",
            Some("Requires #12 and #15 and fixes #34"),
        );

        let ids = dependency_task_ids(&issue, "owner/repo")
            .into_iter()
            .map(|task_id| task_id.as_str().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                "github-issue:owner/repo:issue:12",
                "github-issue:owner/repo:issue:15"
            ]
        );
    }

    #[test]
    fn direct_available_slots_clamps_to_global_headroom() {
        let diag = crate::task_queue::QueueDiagnostics {
            global_running: 3,
            global_queued: 0,
            global_limit: 4,
            project_running: 1,
            project_waiting_for_project: 0,
            project_awaiting_global: 0,
            project_limit: 8,
        };
        assert_eq!(direct_available_slots(&diag), 1);
    }
}
