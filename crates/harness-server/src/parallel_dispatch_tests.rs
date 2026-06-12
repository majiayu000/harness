use super::*;
use async_trait::async_trait;
use harness_core::{
    agent::StreamItem,
    config::misc::WorkspaceConfig,
    types::{Capability, TokenUsage},
};
use std::{
    collections::VecDeque,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

const GIT_LOCAL_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

struct SequencedAgent {
    outputs: Mutex<VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl SequencedAgent {
    fn new(outputs: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            outputs: Mutex::new(outputs.into_iter().map(str::to_string).collect()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn prompt_count(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

#[async_trait]
impl CodeAgent for SequencedAgent {
    fn name(&self) -> &str {
        "sequenced-test-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.prompts.lock().unwrap().push(req.prompt);
        let output = self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .expect("test agent output should exist");
        Ok(sequenced_agent_response(output))
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
}

fn sequenced_agent_response(output: String) -> AgentResponse {
    AgentResponse {
        output,
        stderr: String::new(),
        items: Vec::new(),
        token_usage: TokenUsage::default(),
        model: "test".to_string(),
        exit_code: Some(0),
    }
}

fn git_command() -> Command {
    let mut cmd = Command::new(std::env::var("HARNESS_GIT_BIN").unwrap_or_else(|_| "git".into()));
    for key in GIT_LOCAL_ENV_VARS {
        cmd.env_remove(key);
    }
    cmd
}

fn run_git(args: &[&str]) -> std::process::Output {
    let output = git_command()
        .args(args)
        .output()
        .expect("git command should spawn");
    assert!(
        output.status.success(),
        "git command failed: args={args:?}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn init_git_repo(dir: &Path) -> String {
    let path = dir.to_string_lossy();
    run_git(&["-C", &path, "init"]);
    run_git(&["-C", &path, "config", "user.email", "test@harness.test"]);
    run_git(&["-C", &path, "config", "user.name", "Harness Test"]);
    run_git(&["-C", &path, "commit", "--allow-empty", "-m", "init"]);
    run_git(&["-C", &path, "remote", "add", "origin", &path]);

    let output = run_git(&["-C", &path, "rev-parse", "--abbrev-ref", "HEAD"]);
    String::from_utf8(output.stdout)
        .expect("branch name should be utf8")
        .trim()
        .to_string()
}

#[tokio::test]
async fn sequential_whitespace_output_aborts_remaining_steps() -> anyhow::Result<()> {
    let source = tempfile::tempdir()?;
    let base_branch = init_git_repo(source.path());
    let workspaces = tempfile::tempdir()?;
    let workspace_mgr = Arc::new(WorkspaceManager::new(WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: true,
        ..Default::default()
    })?);
    let agent = Arc::new(SequencedAgent::new([" \n\t", "should not run"]));
    let subtasks = vec![
        SubtaskSpec {
            prompt: "step 1".to_string(),
            depends_on_indices: Vec::new(),
        },
        SubtaskSpec {
            prompt: "step 2".to_string(),
            depends_on_indices: vec![0],
        },
    ];

    let result = run_parallel_subtasks(
        &harness_core::types::TaskId("sequential-empty-output".to_string()),
        agent.clone(),
        subtasks,
        workspace_mgr,
        source.path(),
        "origin",
        &base_branch,
        Vec::new(),
        Duration::from_secs(5),
    )
    .await;

    assert!(result.is_sequential);
    assert_eq!(agent.prompt_count(), 1, "second step must not execute");
    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].index, 0);
    assert!(result.results[0].response.is_none());
    assert_eq!(
        result.results[0].error.as_deref(),
        Some("agent returned empty output")
    );

    Ok(())
}

#[tokio::test]
async fn concurrent_subtasks_release_pool_slots_before_creating_all_workspaces(
) -> anyhow::Result<()> {
    let source = tempfile::tempdir()?;
    let base_branch = init_git_repo(source.path());
    let workspaces = tempfile::tempdir()?;
    let workspace_mgr = Arc::new(WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            auto_cleanup: true,
            ..Default::default()
        },
        crate::workspace_pool::WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        None,
    )?);
    let agent = Arc::new(SequencedAgent::new(["first", "second", "third"]));
    let subtasks = (0..3)
        .map(|i| SubtaskSpec {
            prompt: format!("parallel step {i}"),
            depends_on_indices: Vec::new(),
        })
        .collect();

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_parallel_subtasks(
            &harness_core::types::TaskId("parallel-pool-capacity".to_string()),
            agent.clone(),
            subtasks,
            workspace_mgr.clone(),
            source.path(),
            "origin",
            &base_branch,
            Vec::new(),
            Duration::from_secs(5),
        ),
    )
    .await
    .expect("parallel subtasks should not stall on workspace pool capacity");

    assert!(!result.is_sequential);
    assert_eq!(agent.prompt_count(), 3);
    assert_eq!(result.results.len(), 3);
    assert!(
        result
            .results
            .iter()
            .all(|subtask| subtask.response.is_some() && subtask.error.is_none()),
        "all subtasks should complete successfully"
    );
    assert_eq!(workspace_mgr.live_count(), 0);

    Ok(())
}

#[test]
fn decompose_no_files_returns_original() {
    let prompt = "Fix the login bug";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 1);
    assert_eq!(subtasks[0].prompt, prompt);
}

#[test]
fn decompose_one_file_returns_original() {
    let prompt = "Fix the bug in src/main.rs";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 1);
    assert_eq!(subtasks[0].prompt, prompt);
}

#[test]
fn decompose_two_files_yields_two_subtasks() {
    let prompt = "Update src/auth.rs and src/db.rs";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 2);
}

#[test]
fn decompose_multiple_files_splits_into_two() {
    let prompt = "Refactor src/a.rs src/b.rs src/c.rs src/d.rs";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 2);
    assert!(subtasks[0].prompt.contains("[Parallel subtask 1/2]"));
    assert!(subtasks[1].prompt.contains("[Parallel subtask 2/2]"));
}

#[test]
fn decompose_subtasks_start_with_original_prompt() {
    let prompt = "Fix auth.rs and db.rs together";
    let subtasks = decompose(prompt).unwrap();
    for subtask in &subtasks {
        assert!(subtask.prompt.starts_with(prompt));
    }
}

#[test]
fn decompose_six_files_yields_two_subtasks() {
    let prompt = "Refactor src/a.rs src/b.rs src/c.rs src/d.rs src/e.rs src/f.rs";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 2);
}

#[test]
fn extract_file_refs_no_files() {
    let files = extract_file_refs("Fix the login bug");
    assert!(files.is_empty());
}

#[test]
fn extract_file_refs_deduplicates() {
    let files = extract_file_refs("src/auth.rs and src/auth.rs again");
    assert_eq!(files.len(), 1);
}

#[test]
fn extract_file_refs_returns_sorted() {
    let files = extract_file_refs("src/b.rs src/a.rs src/c.rs");
    assert_eq!(files, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
}

#[test]
fn extract_file_refs_normalizes_dot_slash_prefix() {
    let files = extract_file_refs("update ./src/auth.rs and src/auth.rs");
    assert_eq!(files, vec!["src/auth.rs"]);
}

#[test]
fn extract_file_refs_dot_slash_groups_with_plain_path() {
    let a = extract_file_refs("./src/auth.rs");
    let b = extract_file_refs("src/auth.rs");
    assert_eq!(a, b);
}

#[test]
fn extract_file_refs_extensionless_in_path() {
    let files = extract_file_refs("update docker/Dockerfile");
    assert_eq!(files, vec!["docker/Dockerfile"]);
}

#[test]
fn extract_file_refs_bare_dockerfile() {
    let files = extract_file_refs("update Dockerfile and src/main.rs");
    assert!(files.contains(&"Dockerfile".to_string()));
    assert!(files.contains(&"src/main.rs".to_string()));
}

#[test]
fn extract_file_refs_excludes_urls() {
    let files = extract_file_refs("see https://example.com/path for details");
    assert!(files.is_empty());
}

#[test]
fn decompose_subtask_contains_focus_directive() {
    let prompt = "Update auth.rs and db.rs";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 2);
    assert!(subtasks[0].prompt.contains("Focus on these files:"));
    assert!(subtasks[1].prompt.contains("Focus on these files:"));
}

#[test]
fn decompose_numbered_list_yields_sequential_specs() {
    let prompt = "1. Write the auth module\n2. Refactor the db layer";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 2);
    assert!(subtasks[0].depends_on_indices.is_empty());
    assert_eq!(subtasks[1].depends_on_indices, vec![0]);
}

#[test]
fn decompose_parallel_specs_have_no_dependencies() {
    let prompt = "Update src/auth.rs and src/db.rs";
    let subtasks = decompose(prompt).unwrap();
    assert_eq!(subtasks.len(), 2);
    assert!(subtasks[0].depends_on_indices.is_empty());
    assert!(subtasks[1].depends_on_indices.is_empty());
}

fn make_prompt(n: usize) -> String {
    (0..n)
        .map(|i| format!("src/file{i:02}.rs"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn test_decompose_small() {
    let subtasks = decompose(&make_prompt(2)).unwrap();
    assert_eq!(subtasks.len(), 2);
}

#[test]
fn test_decompose_medium() {
    let subtasks = decompose(&make_prompt(9)).unwrap();
    assert_eq!(subtasks.len(), 3);
}

#[test]
fn test_decompose_large() {
    let subtasks = decompose(&make_prompt(24)).unwrap();
    assert_eq!(subtasks.len(), 8);
}

#[test]
fn test_decompose_very_large() {
    let subtasks = decompose(&make_prompt(30)).unwrap();
    assert_eq!(subtasks.len(), 8);
}

#[test]
fn decompose_labels_reflect_actual_chunk_count() {
    let subtasks = decompose(&make_prompt(16)).unwrap();
    let actual = subtasks.len();
    assert_eq!(actual, 5, "expected 5 actual chunks for 16 files");
    for (i, s) in subtasks.iter().enumerate() {
        let expected = format!("[Parallel subtask {}/{}]", i + 1, actual);
        assert!(
            s.prompt.contains(&expected),
            "label mismatch: expected '{}' in prompt",
            expected
        );
    }
}

#[test]
fn test_decompose_chunk_count_monotone() {
    let chunks_24 = decompose(&make_prompt(24)).unwrap().len();
    let chunks_25 = decompose(&make_prompt(25)).unwrap().len();
    assert!(
        chunks_25 >= chunks_24,
        "expected monotone: 25 files ({chunks_25} chunks) >= 24 files ({chunks_24} chunks)"
    );
}

#[test]
fn decompose_numbered_list_preserves_all_steps_within_cap() {
    let n = MAX_PARALLEL + 2;
    let prompt: String = (1..=n)
        .map(|i| format!("{}. task {}", i, i))
        .collect::<Vec<_>>()
        .join("\n");
    let subtasks = decompose(&prompt).unwrap();
    assert_eq!(
        subtasks.len(),
        n,
        "expected all {} steps preserved, got {}",
        n,
        subtasks.len()
    );
}

#[test]
fn decompose_numbered_list_rejects_over_limit() {
    let n = MAX_SEQUENTIAL_STEPS + 5;
    let prompt: String = (1..=n)
        .map(|i| format!("{}. task {}", i, i))
        .collect::<Vec<_>>()
        .join("\n");
    let err = decompose(&prompt).unwrap_err();
    assert!(
        err.contains(&n.to_string()),
        "error should mention actual step count: {err}"
    );
    assert!(
        err.contains(&MAX_SEQUENTIAL_STEPS.to_string()),
        "error should mention the limit: {err}"
    );
}

#[test]
fn test_decompose_covers_all_files() {
    let n = 12;
    let prompt = make_prompt(n);
    let subtasks = decompose(&prompt).unwrap();
    let expected: Vec<String> = (0..n).map(|i| format!("src/file{i:02}.rs")).collect();
    let mut found: Vec<String> = subtasks
        .iter()
        .flat_map(|s| {
            s.prompt
                .split_whitespace()
                .filter(|t| t.ends_with(".rs") && t.starts_with("src/file"))
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    found.sort();
    let mut sorted_expected = expected.clone();
    sorted_expected.sort();
    assert_eq!(found, sorted_expected);
}
