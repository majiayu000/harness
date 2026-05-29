use super::support::{
    array_items, event_workflow_command, json_value_u64, non_empty_json_string, string_array_field,
    u64_array_field,
};
use crate::runtime::model::{
    ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowEvent, WorkflowEvidence,
};
use crate::runtime::prompt_task::PROMPT_TASK_DEFINITION_ID;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RepoBacklogIssueCandidate {
    pub(super) issue_number: u64,
    pub(super) repo: Option<String>,
    pub(super) issue_url: Option<String>,
    pub(super) title: Option<String>,
    pub(super) labels: Vec<String>,
    pub(super) depends_on: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RepoBacklogPrFeedbackCandidate {
    pub(super) pr_number: u64,
    pub(super) repo: Option<String>,
    pub(super) pr_url: Option<String>,
    pub(super) title: Option<String>,
    pub(super) feedback_count: Option<u64>,
    pub(super) summary: Option<String>,
}

impl RepoBacklogPrFeedbackCandidate {
    fn from_value(value: &Value, parent_repo: Option<&str>) -> Option<Self> {
        let pr_number = value
            .get("pr_number")
            .or_else(|| value.get("pull_request"))
            .or_else(|| value.get("pr"))
            .and_then(json_value_u64)?;
        let repo = value
            .get("repo")
            .and_then(non_empty_json_string)
            .or_else(|| parent_repo.map(ToOwned::to_owned));
        let pr_url = value
            .get("pr_url")
            .or_else(|| value.get("pull_request_url"))
            .and_then(non_empty_json_string);
        let title = value.get("title").and_then(non_empty_json_string);
        let feedback_count = value
            .get("feedback_count")
            .or_else(|| value.get("review_thread_count"))
            .or_else(|| value.get("thread_count"))
            .and_then(json_value_u64);
        let summary = value
            .get("summary")
            .or_else(|| value.get("reason"))
            .and_then(non_empty_json_string);
        Some(Self {
            pr_number,
            repo,
            pr_url,
            title,
            feedback_count,
            summary,
        })
    }

    pub(super) fn start_prompt_task_command(&self, event: &WorkflowEvent) -> WorkflowCommand {
        let repo_key = self.repo.as_deref().unwrap_or("<none>");
        let external_id = format!("pr-feedback:{repo_key}:{}", self.pr_number);
        WorkflowCommand::new(
            WorkflowCommandType::StartChildWorkflow,
            format!("repo-backlog:{repo_key}:pr:{}:feedback", self.pr_number),
            json!({
                "definition_id": PROMPT_TASK_DEFINITION_ID,
                "subject_key": format!("pr:{}:feedback", self.pr_number),
                "repo": self.repo,
                "pr_number": self.pr_number,
                "pr_url": self.pr_url,
                "title": self.title,
                "feedback_count": self.feedback_count,
                "source": "github_pr_feedback",
                "external_id": external_id,
                "task_id": format!("repo-backlog:{repo_key}:pr:{}:feedback", self.pr_number),
                "prompt": self.prompt(),
                "source_event_id": event.id,
            }),
        )
    }

    pub(super) fn to_command_value(&self) -> Value {
        json!({
            "pr_number": self.pr_number,
            "repo": self.repo,
            "pr_url": self.pr_url,
            "title": self.title,
            "feedback_count": self.feedback_count,
            "summary": self.summary,
        })
    }

    fn prompt(&self) -> String {
        let repo = self.repo.as_deref().unwrap_or("<unknown repo>");
        let pr_ref = self
            .pr_url
            .as_deref()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{repo}#{}", self.pr_number));
        let title = self.title.as_deref().unwrap_or("<unknown title>");
        let feedback_count = self
            .feedback_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let summary = self.summary.as_deref().unwrap_or("No summary provided.");
        format!(
            "Handle unresolved review feedback for PR {pr_ref}.\n\n\
Repository: {repo}\n\
PR number: {}\n\
PR title: {title}\n\
Unresolved/actionable feedback count: {feedback_count}\n\
Discovery summary: {summary}\n\n\
Inspect the PR review threads, CI state, and mergeability. Address valid requested changes on the PR branch, run the relevant validation commands, push the branch, and reply to or resolve addressed review threads when possible. Do not make unrelated changes.",
            self.pr_number
        )
    }

    pub(super) fn github_pr_evidence(&self) -> WorkflowEvidence {
        let repo = self.repo.as_deref().unwrap_or("<none>");
        let url = self.pr_url.as_deref().unwrap_or("<unknown>");
        WorkflowEvidence::new(
            "github_pr_feedback",
            format!("repo={repo} pr={} url={url}", self.pr_number),
        )
    }
}

impl RepoBacklogIssueCandidate {
    fn from_value(value: &Value, parent_repo: Option<&str>) -> Option<Self> {
        let issue_number = value
            .get("issue_number")
            .or_else(|| value.get("issue"))
            .and_then(json_value_u64)?;
        let repo = value
            .get("repo")
            .and_then(non_empty_json_string)
            .or_else(|| parent_repo.map(ToOwned::to_owned));
        let issue_url = value.get("issue_url").and_then(non_empty_json_string);
        let title = value.get("title").and_then(non_empty_json_string);
        let labels = string_array_field(value, "labels");
        let depends_on = u64_array_field(value, "depends_on");
        Some(Self {
            issue_number,
            repo,
            issue_url,
            title,
            labels,
            depends_on,
        })
    }

    fn from_sprint_task(
        value: &Value,
        candidates: &[Self],
        parent_repo: Option<&str>,
    ) -> Option<Self> {
        let issue_number = value
            .get("issue_number")
            .or_else(|| value.get("issue"))
            .and_then(json_value_u64)?;
        let mut candidate = candidates
            .iter()
            .find(|candidate| candidate.issue_number == issue_number)
            .cloned()
            .unwrap_or(Self {
                issue_number,
                repo: parent_repo.map(ToOwned::to_owned),
                issue_url: None,
                title: None,
                labels: Vec::new(),
                depends_on: Vec::new(),
            });
        if let Some(repo) = value.get("repo").and_then(non_empty_json_string) {
            candidate.repo = Some(repo);
        }
        if let Some(issue_url) = value.get("issue_url").and_then(non_empty_json_string) {
            candidate.issue_url = Some(issue_url);
        }
        if let Some(title) = value.get("title").and_then(non_empty_json_string) {
            candidate.title = Some(title);
        }
        let labels = string_array_field(value, "labels");
        if !labels.is_empty() {
            candidate.labels = labels;
        }
        if value.get("depends_on").is_some() {
            candidate.depends_on = u64_array_field(value, "depends_on");
        }
        Some(candidate)
    }

    pub(super) fn to_command_value(&self) -> Value {
        json!({
            "issue_number": self.issue_number,
            "repo": self.repo,
            "issue_url": self.issue_url,
            "title": self.title,
            "labels": self.labels,
            "depends_on": self.depends_on,
        })
    }

    fn merge_from(&mut self, other: Self) {
        if self.repo.is_none() {
            self.repo = other.repo;
        }
        if self.issue_url.is_none() {
            self.issue_url = other.issue_url;
        }
        if self.title.is_none() {
            self.title = other.title;
        }
        append_missing_strings(&mut self.labels, other.labels);
        append_missing_u64s(&mut self.depends_on, other.depends_on);
    }

    pub(super) fn github_issue_evidence(&self) -> WorkflowEvidence {
        let repo_key = self.repo.as_deref().unwrap_or("<none>");
        WorkflowEvidence::new(
            "github_issue",
            match self.issue_url.as_deref() {
                Some(url) => format!("repo={repo_key} issue={} url={url}", self.issue_number),
                None => format!("repo={repo_key} issue={}", self.issue_number),
            },
        )
    }
}

pub(super) fn issue_candidates_from_signals(
    result: &ActivityResult,
    signal_type: &str,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == signal_type)
        .filter_map(|signal| RepoBacklogIssueCandidate::from_value(&signal.signal, parent_repo))
        .collect()
}

pub(super) fn pr_feedback_candidates_from_signals(
    result: &ActivityResult,
    signal_type: &str,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogPrFeedbackCandidate> {
    result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == signal_type)
        .filter_map(|signal| {
            RepoBacklogPrFeedbackCandidate::from_value(&signal.signal, parent_repo)
        })
        .collect()
}

pub(super) fn known_dependency_candidates_from_skipped_signals(
    result: &ActivityResult,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == "IssueSkipped")
        .filter(|signal| skipped_issue_has_known_workflow(&signal.signal))
        .filter_map(|signal| RepoBacklogIssueCandidate::from_value(&signal.signal, parent_repo))
        .collect()
}

fn skipped_issue_has_known_workflow(value: &Value) -> bool {
    value
        .get("existing_workflow")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("has_workflow")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || value
            .get("workflow_state")
            .and_then(non_empty_json_string)
            .is_some()
        || value
            .get("runtime_workflow_state")
            .and_then(non_empty_json_string)
            .is_some()
        || value
            .get("issue_workflow_state")
            .and_then(non_empty_json_string)
            .is_some()
}

pub(super) fn sprint_plan_selected_issues(
    result: &ActivityResult,
    event: &WorkflowEvent,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    let mut scan_candidates = sprint_plan_candidate_inputs(event, parent_repo);
    scan_candidates.extend(sprint_plan_candidate_artifacts(result, parent_repo));
    let mut selected = sprint_plan_selected_signals(result, &scan_candidates, parent_repo);
    selected.extend(sprint_plan_selected_artifacts(
        result,
        &scan_candidates,
        parent_repo,
    ));
    dedupe_issue_candidates(selected)
}

pub(super) fn sprint_plan_known_dependency_inputs(
    event: &WorkflowEvent,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    event_workflow_command(event)
        .and_then(|command| {
            command
                .command
                .get("known_dependencies")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| {
                            RepoBacklogIssueCandidate::from_value(value, parent_repo)
                        })
                        .collect()
                })
        })
        .unwrap_or_default()
}

pub(super) fn sprint_plan_pr_feedback_inputs(
    event: &WorkflowEvent,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogPrFeedbackCandidate> {
    event_workflow_command(event)
        .and_then(|command| {
            command
                .command
                .get("open_pr_feedback")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| {
                            RepoBacklogPrFeedbackCandidate::from_value(value, parent_repo)
                        })
                        .collect()
                })
        })
        .unwrap_or_default()
}

fn sprint_plan_candidate_inputs(
    event: &WorkflowEvent,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    event_workflow_command(event)
        .and_then(|command| {
            command
                .command
                .get("issues")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| {
                            RepoBacklogIssueCandidate::from_value(value, parent_repo)
                        })
                        .collect()
                })
        })
        .unwrap_or_default()
}

fn sprint_plan_candidate_artifacts(
    result: &ActivityResult,
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "repo_backlog_candidates")
        .flat_map(|artifact| array_items(&artifact.artifact, "issues"))
        .filter_map(|value| RepoBacklogIssueCandidate::from_value(value, parent_repo))
        .collect()
}

fn sprint_plan_selected_signals(
    result: &ActivityResult,
    candidates: &[RepoBacklogIssueCandidate],
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == "SprintTaskSelected")
        .filter_map(|signal| {
            RepoBacklogIssueCandidate::from_sprint_task(&signal.signal, candidates, parent_repo)
        })
        .collect()
}

fn sprint_plan_selected_artifacts(
    result: &ActivityResult,
    candidates: &[RepoBacklogIssueCandidate],
    parent_repo: Option<&str>,
) -> Vec<RepoBacklogIssueCandidate> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "sprint_plan")
        .flat_map(|artifact| array_items(&artifact.artifact, "tasks"))
        .filter_map(|value| {
            RepoBacklogIssueCandidate::from_sprint_task(value, candidates, parent_repo)
        })
        .collect()
}

fn dedupe_issue_candidates(
    candidates: Vec<RepoBacklogIssueCandidate>,
) -> Vec<RepoBacklogIssueCandidate> {
    let mut index_by_key = std::collections::BTreeMap::<(String, u64), usize>::new();
    let mut merged = Vec::<RepoBacklogIssueCandidate>::new();
    for candidate in candidates {
        let key = (
            candidate.repo.clone().unwrap_or_default(),
            candidate.issue_number,
        );
        if let Some(index) = index_by_key.get(&key).copied() {
            merged[index].merge_from(candidate);
        } else {
            index_by_key.insert(key, merged.len());
            merged.push(candidate);
        }
    }
    merged
}

pub(super) fn normalize_selected_sprint_dependencies(
    mut candidates: Vec<RepoBacklogIssueCandidate>,
    known_dependencies: &[RepoBacklogIssueCandidate],
) -> Vec<RepoBacklogIssueCandidate> {
    let mut valid_dependencies: std::collections::BTreeSet<_> = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.repo.clone().unwrap_or_default(),
                candidate.issue_number,
            )
        })
        .collect();
    valid_dependencies.extend(known_dependencies.iter().map(|candidate| {
        (
            candidate.repo.clone().unwrap_or_default(),
            candidate.issue_number,
        )
    }));

    for candidate in &mut candidates {
        let repo = candidate.repo.clone().unwrap_or_default();
        let mut seen = std::collections::BTreeSet::new();
        let issue_number = candidate.issue_number;
        candidate.depends_on.retain(|dependency| {
            *dependency != issue_number
                && valid_dependencies.contains(&(repo.clone(), *dependency))
                && seen.insert(*dependency)
        });
    }

    candidates
}

fn append_missing_strings(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn append_missing_u64s(target: &mut Vec<u64>, values: Vec<u64>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}
