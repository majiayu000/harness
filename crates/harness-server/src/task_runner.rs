use dashmap::DashMap;
use harness_core::{AgentRequest, CodeAgent};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string()[..8].to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Implementing,
    Waiting,
    Reviewing,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundResult {
    pub turn: u32,
    pub action: String,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub id: TaskId,
    pub status: TaskStatus,
    pub turn: u32,
    pub pr_url: Option<String>,
    pub rounds: Vec<RoundResult>,
    pub error: Option<String>,
}

impl TaskState {
    fn new(id: TaskId) -> Self {
        Self {
            id,
            status: TaskStatus::Pending,
            turn: 0,
            pr_url: None,
            rounds: Vec::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: Option<String>,
    pub issue: Option<u64>,
    pub pr: Option<u64>,
    #[serde(default = "default_project")]
    pub project: PathBuf,
    #[serde(default = "default_wait")]
    pub wait_secs: u64,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
}

fn default_project() -> PathBuf {
    PathBuf::from(".")
}
fn default_wait() -> u64 {
    120
}
fn default_max_rounds() -> u32 {
    5
}

pub type TaskStore = Arc<DashMap<String, TaskState>>;

pub fn new_task_store() -> TaskStore {
    Arc::new(DashMap::new())
}

pub fn spawn_task(
    store: TaskStore,
    agent: Arc<dyn CodeAgent>,
    req: CreateTaskRequest,
) -> TaskId {
    let task_id = TaskId::new();
    let state = TaskState::new(task_id.clone());
    store.insert(task_id.0.clone(), state);

    let id = task_id.clone();
    let store = store.clone();

    tokio::spawn(async move {
        if let Err(e) = run_task(&store, &id, agent.as_ref(), &req).await {
            if let Some(mut s) = store.get_mut(&id.0) {
                s.status = TaskStatus::Failed;
                s.error = Some(e.to_string());
            }
        }
    });

    task_id
}

async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
) -> anyhow::Result<()> {
    // Turn 1: implement
    update_status(store, task_id, TaskStatus::Implementing, 1);

    let implement_prompt = build_implement_prompt(req);
    let resp = agent
        .execute(AgentRequest {
            prompt: implement_prompt,
            project_root: req.project.clone(),
            ..Default::default()
        })
        .await?;

    let pr_url = parse_pr_url(&resp.output);
    let pr_number = pr_url.as_ref().and_then(|u| extract_pr_number(u));

    if let Some(mut s) = store.get_mut(&task_id.0) {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult {
            turn: 1,
            action: "implement".into(),
            result: if pr_url.is_some() {
                "pr_created".into()
            } else {
                "implemented".into()
            },
        });
    }

    // If no PR was created or no review loop needed, we're done
    let pr_num = match pr_number {
        Some(n) => n,
        None => {
            update_status(store, task_id, TaskStatus::Done, 1);
            return Ok(());
        }
    };

    // Review loop: Turn 2..N
    let issue_context = match (req.issue, req.pr) {
        (Some(n), _) => format!("你之前为 issue #{n} 创建了 PR #{pr_num}。\n"),
        (_, Some(n)) => format!("检查 PR #{n}。\n"),
        _ => format!("检查 PR #{pr_num}。\n"),
    };

    for round in 2..=(req.max_rounds + 1) {
        update_status(store, task_id, TaskStatus::Waiting, round);
        sleep(Duration::from_secs(req.wait_secs)).await;

        update_status(store, task_id, TaskStatus::Reviewing, round);

        let review_prompt = format!(
            "{issue_context}\
             现在用 gh pr checks 检查 CI 状态，\
             用 gh pr view 和 gh api 读取 review comments。\
             如果 CI 通过且没有需要处理的 review comments，在最后一行单独输出 LGTM。\
             否则根据反馈修复代码，commit，push，在最后一行单独输出 FIXED。"
        );

        let resp = agent
            .execute(AgentRequest {
                prompt: review_prompt,
                project_root: req.project.clone(),
                ..Default::default()
            })
            .await?;

        let is_lgtm = resp.output.trim().ends_with("LGTM");

        if let Some(mut s) = store.get_mut(&task_id.0) {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: if is_lgtm {
                    "lgtm".into()
                } else {
                    "fixed".into()
                },
            });
        }

        if is_lgtm {
            update_status(store, task_id, TaskStatus::Done, round);
            return Ok(());
        }
    }

    update_status(store, task_id, TaskStatus::Done, req.max_rounds + 1);
    Ok(())
}

fn build_implement_prompt(req: &CreateTaskRequest) -> String {
    if let Some(issue) = req.issue {
        format!(
            "读 GitHub issue #{issue}，理解需求，在当前项目中实现代码，\
             运行 cargo check 和 cargo test，创建功能分支，commit，push，\
             用 gh pr create 创建 PR。\
             完成后在输出的最后一行单独输出 PR_URL=<完整PR URL>"
        )
    } else if let Some(pr) = req.pr {
        format!(
            "检查 PR #{pr} 的 CI 状态和 review comments。\
             如果有问题就修复代码，commit，push。\
             如果没有问题，在最后一行单独输出 LGTM。\
             否则在最后一行单独输出 FIXED。"
        )
    } else if let Some(ref prompt) = req.prompt {
        format!(
            "{prompt}\n\n\
             完成后如果创建了 PR，在输出的最后一行单独输出 PR_URL=<完整PR URL>"
        )
    } else {
        "No task specified.".into()
    }
}

fn update_status(store: &TaskStore, task_id: &TaskId, status: TaskStatus, turn: u32) {
    if let Some(mut s) = store.get_mut(&task_id.0) {
        s.status = status;
        s.turn = turn;
    }
}

fn parse_pr_url(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("PR_URL=") {
            return Some(url.trim().to_string());
        }
    }
    None
}

fn extract_pr_number(url: &str) -> Option<u64> {
    url.split('/').last()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pr_url() {
        let output = "Some output\nPR_URL=https://github.com/owner/repo/pull/42";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/42".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_not_found() {
        assert_eq!(parse_pr_url("no url here"), None);
    }

    #[test]
    fn test_extract_pr_number() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42"),
            Some(42)
        );
    }

    #[test]
    fn test_build_prompt_issue() {
        let req = CreateTaskRequest {
            prompt: None,
            issue: Some(9),
            pr: None,
            project: PathBuf::from("."),
            wait_secs: 120,
            max_rounds: 5,
        };
        let p = build_implement_prompt(&req);
        assert!(p.contains("issue #9"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_build_prompt_free_text() {
        let req = CreateTaskRequest {
            prompt: Some("fix the bug".into()),
            issue: None,
            pr: None,
            project: PathBuf::from("."),
            wait_secs: 120,
            max_rounds: 5,
        };
        let p = build_implement_prompt(&req);
        assert!(p.contains("fix the bug"));
    }

    #[test]
    fn test_task_state_new() {
        let id = TaskId::new();
        let state = TaskState::new(id);
        assert!(matches!(state.status, TaskStatus::Pending));
        assert_eq!(state.turn, 0);
        assert!(state.pr_url.is_none());
    }
}
