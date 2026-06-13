use super::*;
use crate::task_runner::TaskId;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn git_command_std() -> std::process::Command {
    let mut cmd = std::process::Command::new(git_binary());
    for key in GIT_LOCAL_ENV_VARS {
        cmd.env_remove(key);
    }
    cmd
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn async_env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn async_cwd_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct ScopedEnvVar {
    key: String,
    original: Option<String>,
    _guard: MutexGuard<'static, ()>,
}

impl ScopedEnvVar {
    fn set(key: &str, value: &str) -> Self {
        let guard = env_lock().lock().expect("env lock should not be poisoned");
        let original = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            original,
            _guard: guard,
        }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        if let Some(value) = &self.original {
            unsafe { std::env::set_var(&self.key, value) };
        } else {
            unsafe { std::env::remove_var(&self.key) };
        }
    }
}

fn run_git(args: &[&str]) -> std::process::Output {
    let output = git_command_std()
        .args(args)
        .output()
        .expect("git command failed to spawn");
    assert!(
        output.status.success(),
        "git command failed: args={args:?}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

async fn github_state_server(path: &'static str, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind GitHub mock");
    let addr = listener.local_addr().expect("GitHub mock address");
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0_u8; 2048];
        let Ok(n) = socket.read(&mut buf).await else {
            return;
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let (status, response_body) = if request.starts_with(&format!("GET {path} ")) {
            ("200 OK", body)
        } else {
            ("404 Not Found", "{}")
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    format!("http://{addr}")
}

fn init_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        run_git(args);
    };
    run(&["-C", &dir.to_string_lossy(), "init"]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "config",
        "user.email",
        "test@harness.test",
    ]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "config",
        "user.name",
        "Harness Test",
    ]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "remote",
        "add",
        "origin",
        &dir.to_string_lossy(),
    ]);
}

fn current_branch(repo: &Path) -> String {
    let out = run_git(&[
        "-C",
        &repo.to_string_lossy(),
        "rev-parse",
        "--abbrev-ref",
        "HEAD",
    ]);
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

#[test]
fn valid_branch_names_accepted() {
    assert!(is_valid_branch_name("main"));
    assert!(is_valid_branch_name("feature/my-branch"));
    assert!(is_valid_branch_name("release/v1.0.0"));
    assert!(is_valid_branch_name("fix_issue_42"));
}

#[test]
fn invalid_branch_names_rejected() {
    assert!(!is_valid_branch_name(""));
    assert!(!is_valid_branch_name("-starts-with-dash"));
    assert!(!is_valid_branch_name("has spaces"));
    assert!(!is_valid_branch_name("has..dotdot"));
    assert!(!is_valid_branch_name("semi;colon"));
    assert!(!is_valid_branch_name("back`tick"));
    assert!(!is_valid_branch_name("dollar$sign"));
}

#[test]
fn sanitize_task_id_replaces_non_alphanumeric() {
    assert_eq!(sanitize_task_id("abc-123"), "abc-123");
    assert_eq!(sanitize_task_id("abc_123"), "abc_123");
    assert_eq!(sanitize_task_id("abc 123"), "abc_123");
    assert_eq!(sanitize_task_id("abc/123"), "abc_123");
    assert_eq!(sanitize_task_id("abc.123"), "abc_123");
}

fn test_task_id() -> TaskId {
    harness_core::types::TaskId("550e8400-e29b-41d4-a716-446655440000".to_string())
}

#[test]
fn derive_workspace_key_issue() {
    let path = std::path::Path::new("/projects/my-project");
    // canonicalize fails in test env; fnv1a_8 hashes the given path string
    let prefix = fnv1a_8("/projects/my-project");
    let key = derive_workspace_key(
        &test_task_id(),
        Some("issue:42"),
        Some("myorg/my-repo"),
        Some(path),
    );
    assert_eq!(key, format!("{prefix}__myorg_my-repo__issue_42"));
}

#[test]
fn derive_workspace_key_pr() {
    let prefix = fnv1a_8("/projects/my-project");
    let key = derive_workspace_key(
        &test_task_id(),
        Some("pr:7"),
        Some("myorg/my-repo"),
        Some(std::path::Path::new("/projects/my-project")),
    );
    assert_eq!(key, format!("{prefix}__myorg_my-repo__pr_7"));
}

#[test]
fn derive_workspace_key_prompt_falls_back_to_uuid() {
    let id = test_task_id();
    let key = derive_workspace_key(&id, None, None, None);
    assert_eq!(key, sanitize_task_id(&id.0));
}

#[test]
fn derive_workspace_key_missing_repo_falls_back() {
    let id = test_task_id();
    let key = derive_workspace_key(&id, Some("issue:42"), None, None);
    assert_eq!(key, sanitize_task_id(&id.0));
}

#[test]
fn derive_workspace_key_special_chars_in_repo() {
    // sanitize_repo_slug preserves dots: "my.org/repo name" -> "my.org_repo_name"
    // (distinct from "my_org/repo_name" -> "my_org_repo_name")
    let prefix = fnv1a_8("/projects/my-project");
    let key = derive_workspace_key(
        &test_task_id(),
        Some("issue:99"),
        Some("my.org/repo name"),
        Some(std::path::Path::new("/projects/my-project")),
    );
    assert_eq!(key, format!("{prefix}__my.org_repo_name__issue_99"));
}

#[test]
fn derive_workspace_key_no_source_repo_omits_prefix() {
    let key = derive_workspace_key(
        &test_task_id(),
        Some("issue:42"),
        Some("myorg/my-repo"),
        None,
    );
    assert_eq!(key, "myorg_my-repo__issue_42");
}

#[test]
fn sanitize_repo_slug_preserves_dots() {
    assert_eq!(sanitize_repo_slug("my.org/my-repo"), "my.org_my-repo");
    assert_eq!(sanitize_repo_slug("my_org/my-repo"), "my_org_my-repo");
    // dots and underscores produce distinct keys
    assert_ne!(
        sanitize_repo_slug("my.org/repo"),
        sanitize_repo_slug("my_org/repo")
    );
}

#[test]
fn derive_workspace_key_different_projects_same_dirname_differ() {
    // Two projects with the same dir name but different parent paths must
    // produce different workspace keys (hash of full path, not just file_name).
    let key_a = derive_workspace_key(
        &test_task_id(),
        Some("issue:1"),
        Some("org/repo"),
        Some(std::path::Path::new("/home/user/app")),
    );
    let key_b = derive_workspace_key(
        &test_task_id(),
        Some("issue:1"),
        Some("org/repo"),
        Some(std::path::Path::new("/opt/app")),
    );
    assert_ne!(
        key_a, key_b,
        "projects at different paths must produce different keys"
    );
}

#[test]
fn workspace_manager_new_creates_root_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspaces");
    let config = WorkspaceConfig {
        root: root.clone(),
        ..Default::default()
    };
    let _mgr = WorkspaceManager::new(config).expect("new");
    assert!(root.is_dir());
}

#[tokio::test]
async fn create_and_remove_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: true,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-001".to_string());

    let ws_path = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");
    assert!(ws_path.workspace_path.is_dir());
    assert!(mgr.get_workspace(&task_id).is_some());

    mgr.remove_workspace(&task_id).await.expect("remove");
    assert!(mgr.get_workspace(&task_id).is_none());
}

#[tokio::test]
async fn create_workspace_persists_owner_record_outside_checkout_root() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-owner-record".to_string());

    let lease = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");

    let metadata_path = owner_record_path(&lease.workspace_path).expect("owner record path");
    assert!(
        metadata_path.exists(),
        "owner record should be written into git metadata"
    );
    assert!(
        !lease
            .workspace_path
            .join(format!(".{OWNER_RECORD_FILE}"))
            .exists()
            && !lease.workspace_path.join(OWNER_RECORD_FILE).exists(),
        "owner record must not dirty the checkout root"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn pool_slot_reuse_preserves_existing_directory_for_same_workspace_identity() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: false,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let first_task = harness_core::types::TaskId("task-first".to_string());
    let second_task = harness_core::types::TaskId("task-second".to_string());

    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create first workspace");
    let marker = first.workspace_path.join("handoff.txt");
    std::fs::write(&marker, "keep this file").expect("write marker");
    mgr.release_workspace(&first_task).await;

    let second = mgr
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("reuse deterministic workspace");

    assert_eq!(first.workspace_path, second.workspace_path);
    assert_eq!(second.decision, WorkspaceAcquireDecision::ReusedRecovered);
    assert!(
        second.workspace_path.join("handoff.txt").exists(),
        "same issue workspace identity should preserve non-terminal handoff state"
    );
    let owner = read_owner_record(&second.workspace_path).expect("owner record");
    assert_eq!(owner.task_id, "issue:42");
    let expected_owner_key = derive_workspace_key(
        &second_task,
        Some("issue:42"),
        Some("owner/repo"),
        Some(source.path()),
    );
    assert_eq!(
        owner.workspace_key.as_deref(),
        Some(expected_owner_key.as_str())
    );
    assert_eq!(
        owner
            .workspace_key
            .as_deref()
            .and_then(repo_slug_from_workspace_key)
            .as_deref(),
        Some("owner/repo")
    );

    mgr.remove_workspace(&second_task).await.expect("remove");
}

#[tokio::test]
async fn pool_slot_reuse_preserves_released_same_task_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: false,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("runtime-workflow-task".to_string());

    let first = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create first workspace");
    let marker = first.workspace_path.join("handoff.txt");
    std::fs::write(&marker, "preserve this file").expect("write marker");
    mgr.release_workspace(&task_id).await;

    let second = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("reuse released workflow workspace");

    assert_eq!(first.workspace_path, second.workspace_path);
    assert_eq!(second.decision, WorkspaceAcquireDecision::ReusedRecovered);
    assert!(
        second.workspace_path.join("handoff.txt").exists(),
        "same workflow workspace reuse must preserve non-terminal activity state"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn pool_slot_reuse_uses_workspace_identity_for_new_task_id() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: false,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new_with_pool(
        config,
        crate::workspace_pool::WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        None,
    )
    .expect("new");
    let blocker_task = harness_core::types::TaskId("slot-zero-blocker".to_string());
    let first_task = harness_core::types::TaskId("issue-task-old-internal-id".to_string());
    let second_task = harness_core::types::TaskId("issue-task-new-internal-id".to_string());

    let blocker = mgr
        .create_workspace(
            &blocker_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:7"),
            Some("owner/repo"),
        )
        .await
        .expect("create blocker workspace");
    assert_eq!(blocker.slot_index, 0);
    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create first issue workspace");
    assert_eq!(first.slot_index, 1);
    let marker = first.workspace_path.join("handoff.txt");
    std::fs::write(&marker, "preserve this file").expect("write marker");
    mgr.release_workspace(&first_task).await;
    mgr.remove_workspace(&blocker_task)
        .await
        .expect("remove blocker so slot 0 is free");

    let second = mgr
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("reuse released issue workspace by workspace identity");

    assert_eq!(second.slot_index, 1);
    assert_eq!(first.workspace_path, second.workspace_path);
    assert_eq!(second.decision, WorkspaceAcquireDecision::ReusedRecovered);
    assert!(
        second.workspace_path.join("handoff.txt").exists(),
        "successor issue task should reuse the preserved workspace identity"
    );

    mgr.remove_workspace(&second_task).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_allocates_distinct_slots_for_concurrent_same_repo_tasks() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let first_task = harness_core::types::TaskId("task-first".to_string());
    let second_task = harness_core::types::TaskId("task-second".to_string());
    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("first task should acquire a pool slot");
    let second = mgr
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:43"),
            Some("owner/repo"),
        )
        .await
        .expect("second task should acquire a different pool slot");

    assert_ne!(first.workspace_path, second.workspace_path);
    assert_eq!(first.slot_index, 0);
    assert_eq!(second.slot_index, 1);
    assert_eq!(mgr.live_count(), 2);

    mgr.remove_workspace(&first_task)
        .await
        .expect("remove first");
    mgr.remove_workspace(&second_task)
        .await
        .expect("remove second");
}

#[tokio::test]
async fn create_workspace_waits_when_project_pool_is_full() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(
        WorkspaceManager::new_with_pool(
            config,
            WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
            None,
        )
        .expect("new"),
    );
    let first_task = harness_core::types::TaskId("pool-full-first".to_string());
    let second_task = harness_core::types::TaskId("pool-full-second".to_string());

    let first = mgr
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("first task should acquire the only slot");
    assert_eq!(first.slot_index, 0);

    let mgr_for_second = mgr.clone();
    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let second_handle = tokio::spawn(async move {
        mgr_for_second
            .create_workspace(
                &second_task_for_spawn,
                &source_path,
                "origin",
                &branch_for_second,
                1,
                Some("issue:43"),
                Some("owner/repo"),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !second_handle.is_finished(),
        "second acquire should wait while the only slot is leased"
    );

    mgr.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")
        .expect("second task should acquire after release");
    assert_eq!(second.slot_index, 0);
    assert_eq!(second.decision, WorkspaceAcquireDecision::RecreatedStale);

    mgr.remove_workspace(&second_task)
        .await
        .expect("remove second");
}

#[tokio::test]
async fn workspace_lease_store_persists_and_releases_active_slots() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let task_id = harness_core::types::TaskId("lease-store-task".to_string());
    let record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: task_id.clone(),
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-1".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
    };

    assert!(
        store.try_acquire_lease(&record).await?,
        "initial lease should acquire an empty slot"
    );
    assert_eq!(store.list_leased().await?.len(), 1);
    assert_eq!(
        store.latest_workspace_path_for_task(&task_id).await?,
        Some(record.workspace_path.clone())
    );
    assert!(
        store
            .release_slot(&record.project_key, record.slot_index, &task_id)
            .await?,
        "release should update the active lease"
    );
    assert!(store.list_leased().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_does_not_steal_live_foreign_slot() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let first_task = harness_core::types::TaskId("lease-store-first".to_string());
    let second_task = harness_core::types::TaskId("lease-store-second".to_string());
    let first_record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: first_task,
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-1".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
    };
    let second_record = WorkspaceLeaseRecord {
        task_id: second_task,
        owner_session: "session-b".to_string(),
        runtime_workflow_id: Some("workflow-2".to_string()),
        ..first_record.clone()
    };

    assert!(store.try_acquire_lease(&first_record).await?);
    assert!(
        !store.try_acquire_lease(&second_record).await?,
        "foreign live lease must not be overwritten"
    );
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].task_id, first_record.task_id);
    assert_eq!(leased[0].owner_session, first_record.owner_session);

    assert!(
        store
            .release_slot(
                &first_record.project_key,
                first_record.slot_index,
                &first_record.task_id
            )
            .await?
    );
    assert!(
        store.try_acquire_lease(&second_record).await?,
        "released slots should be reusable"
    );

    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_releases_only_dead_foreign_processes() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let live_record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("live-foreign-task".to_string()),
        workspace_key: "workspace-live".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-live".to_string()),
        owner_session: "session-live".to_string(),
        run_generation: 1,
        process_id: std::process::id(),
    };
    let dead_record = WorkspaceLeaseRecord {
        slot_index: 1,
        task_id: harness_core::types::TaskId("dead-foreign-task".to_string()),
        workspace_key: "workspace-dead".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-1"),
        runtime_workflow_id: Some("workflow-dead".to_string()),
        owner_session: "session-dead".to_string(),
        process_id: u32::MAX,
        ..live_record.clone()
    };

    assert!(store.try_acquire_lease(&live_record).await?);
    assert!(store.try_acquire_lease(&dead_record).await?);

    let released = store
        .release_foreign_orphaned_leases("current-session")
        .await?;
    assert_eq!(released, 1);
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].task_id, live_record.task_id);

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_allocates_next_slot_without_stealing_live_foreign_lease(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(2, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = WorkspaceManager::new_with_pool(config, pool_config, Some(store.clone()))?;
    let first_task = harness_core::types::TaskId("shared-store-first".to_string());
    let second_task = harness_core::types::TaskId("shared-store-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    let second = mgr_b
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:43"),
            Some("owner/repo"),
        )
        .await?;

    assert_eq!(first.slot_index, 0);
    assert_eq!(second.slot_index, 1);
    assert_ne!(first.workspace_path, second.workspace_path);
    assert_eq!(store.list_leased().await?.len(), 2);

    mgr_a.remove_workspace(&first_task).await?;
    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_waits_when_persisted_slots_are_full() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(1, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        pool_config,
        Some(store.clone()),
    )?);
    let first_task = harness_core::types::TaskId("shared-store-full-first".to_string());
    let second_task = harness_core::types::TaskId("shared-store-full-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    assert_eq!(first.slot_index, 0);

    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let mgr_b_for_spawn = mgr_b.clone();
    let second_handle = tokio::spawn(async move {
        mgr_b_for_spawn
            .create_workspace(
                &second_task_for_spawn,
                &source_path,
                "origin",
                &branch_for_second,
                1,
                Some("issue:43"),
                Some("owner/repo"),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !second_handle.is_finished(),
        "second manager should wait while the shared lease table is full"
    );

    mgr_a.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")?;
    assert_eq!(second.slot_index, 0);
    assert_eq!(second.workspace_path, first.workspace_path);

    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn remove_workspace_releases_persisted_slot_after_cleanup_hook_finishes() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        before_remove_hook: Some("sh hold-remove.sh".to_string()),
        hook_timeout_secs: 5,
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let task_id = harness_core::types::TaskId("remove-holds-lease".to_string());

    let lease = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    std::fs::write(lease.workspace_path.join("hold-remove.sh"), "sleep 1\n")?;
    assert_eq!(store.list_leased().await?.len(), 1);

    let task_id_for_spawn = task_id.clone();
    let mgr_for_spawn = mgr.clone();
    let remove_handle =
        tokio::spawn(async move { mgr_for_spawn.remove_workspace(&task_id_for_spawn).await });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.list_leased().await?.len(),
        1,
        "persisted lease should remain held while removal hook is still running"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), remove_handle)
        .await
        .expect("remove should finish")
        .expect("remove task should join")?;
    assert!(
        store.list_leased().await?.is_empty(),
        "persisted lease should release after cleanup completes"
    );

    Ok(())
}

#[tokio::test]
async fn cleanup_workspace_for_retry_releases_persisted_slot_after_cleanup_finishes(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        crate::workspace_pool::WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let task_id = harness_core::types::TaskId("retry-cleanup-holds-lease".to_string());

    let lease = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    assert_eq!(store.list_leased().await?.len(), 1);

    let git_ops_guard = mgr.git_ops.lock().await;
    let mgr_for_spawn = mgr.clone();
    let task_id_for_spawn = task_id.clone();
    let source_path = source.path().to_path_buf();
    let workspace_path = lease.workspace_path.clone();
    let cleanup_handle = tokio::spawn(async move {
        mgr_for_spawn
            .cleanup_workspace_for_retry(&task_id_for_spawn, &source_path, Some(&workspace_path))
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.list_leased().await?.len(),
        1,
        "persisted lease should remain held while retry cleanup is waiting"
    );

    drop(git_ops_guard);
    tokio::time::timeout(std::time::Duration::from_secs(5), cleanup_handle)
        .await
        .expect("retry cleanup should finish")
        .expect("retry cleanup task should join")?;
    assert!(
        store.list_leased().await?.is_empty(),
        "persisted lease should release after retry cleanup finishes"
    );
    assert!(
        !lease.workspace_path.exists(),
        "retry cleanup should remove the workspace"
    );

    Ok(())
}

#[tokio::test]
async fn cleanup_workspace_for_retry_skips_path_reserved_by_different_active_task() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let active_task = harness_core::types::TaskId("active-issue-task".to_string());
    let stale_task = harness_core::types::TaskId("stale-retry-task".to_string());

    let lease = mgr
        .create_workspace(
            &active_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create active workspace");

    mgr.cleanup_workspace_for_retry(&stale_task, source.path(), Some(&lease.workspace_path))
        .await
        .expect("retry cleanup should skip foreign active path");

    assert!(
        lease.workspace_path.exists(),
        "retry cleanup must not delete another active task's workspace"
    );
    assert!(
        mgr.get_workspace(&active_task).is_some(),
        "active task should remain tracked"
    );

    mgr.remove_workspace(&active_task)
        .await
        .expect("remove active");
}

#[tokio::test]
async fn remove_workspace_idempotent() {
    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("nonexistent-task".to_string());

    // Should succeed even though workspace was never created.
    mgr.remove_workspace(&task_id).await.expect("first remove");
    mgr.remove_workspace(&task_id).await.expect("second remove");
}

#[tokio::test]
async fn create_workspace_idempotent() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-002".to_string());

    let path1 = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create first");
    let path2 = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create second");
    assert_eq!(path1.workspace_path, path2.workspace_path);

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_ignores_inherited_git_index_file() {
    let _guard = ScopedEnvVar::set("GIT_INDEX_FILE", ".git/index");

    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-git-index-file".to_string());

    let ws_path = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");
    assert!(ws_path.workspace_path.is_dir());

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_requires_remote_head_by_default() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    run_git(&[
        "-C",
        &source.path().to_string_lossy(),
        "remote",
        "remove",
        "origin",
    ]);

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("missing-remote-head".to_string());

    let err = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect_err("missing remote head should fail admission");
    assert!(
        err.to_string().contains("git fetch origin"),
        "error should report the failed remote fetch: {err}"
    );
    assert!(mgr.get_workspace(&task_id).is_none());
}

#[tokio::test]
async fn create_workspace_can_opt_into_local_base_fallback() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    run_git(&[
        "-C",
        &source.path().to_string_lossy(),
        "remote",
        "remove",
        "origin",
    ]);

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("local-base-fallback".to_string());

    let lease = mgr
        .create_workspace_with_options(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            None,
            None,
            WorkspaceCreateOptions {
                require_remote_head: false,
                ..Default::default()
            },
        )
        .await
        .expect("explicit local fallback should be allowed");
    assert!(lease.workspace_path.is_dir());

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_uses_custom_branch_prefix() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("custom-prefix".to_string());

    let lease = mgr
        .create_workspace_with_options(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            None,
            None,
            WorkspaceCreateOptions {
                branch_prefix: "task/".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("workspace should be created with custom branch prefix");
    assert_eq!(current_branch(&lease.workspace_path), "task/custom-prefix");

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn after_create_hook_runs_with_workspace_cwd() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let marker = workspaces.path().join("hook_ran.marker");
    let hook = format!("touch {}", marker.display());

    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        after_create_hook: Some(hook),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-003".to_string());

    mgr.create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");
    assert!(
        marker.exists(),
        "after_create_hook should have created marker file"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn after_create_hook_failure_removes_partial_worktree() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        after_create_hook: Some("exit 1".to_string()),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-004".to_string());

    let result = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await;
    assert!(result.is_err(), "should fail when hook exits 1");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("after_create_hook"),
        "error should mention hook"
    );
    // Should not be tracked in active map.
    assert!(mgr.get_workspace(&task_id).is_none());
}

#[tokio::test]
async fn create_workspace_reconciles_stale_directory() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("stale-task-check-001".to_string());

    // Pre-create the directory to simulate a stale worktree from a previous failed run.
    let pool_key = crate::workspace_pool::derive_workspace_pool_key(source.path(), None);
    let stale_path = workspaces
        .path()
        .join(crate::workspace_pool::workspace_slot_key(&pool_key, 0));
    std::fs::create_dir_all(&stale_path).expect("create stale dir");

    let result = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await;
    assert!(
        result.is_ok(),
        "stale directory should be reconciled automatically"
    );
    assert!(
        stale_path.join(".git").exists(),
        "workspace should be recreated as a git worktree"
    );
    assert!(
        mgr.get_workspace(&task_id).is_some(),
        "task should be tracked after reconciliation"
    );
}

#[tokio::test]
async fn create_workspace_reconciles_missing_registered_worktree() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let task_id = harness_core::types::TaskId("missing-registered-task".to_string());
    let first_mgr = WorkspaceManager::new(config.clone()).expect("first manager");
    let first = first_mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create initial workspace");
    std::fs::remove_dir_all(&first.workspace_path).expect("remove worktree directory");
    assert!(
        is_registered_worktree(source.path(), &first.workspace_path).await,
        "removed workspace path should still be registered in git metadata"
    );

    let second_mgr = WorkspaceManager::new(config).expect("second manager");
    let recreated = second_mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("missing registered worktree should be pruned and recreated");

    assert_eq!(recreated.decision, WorkspaceAcquireDecision::RecreatedStale);
    assert!(recreated.workspace_path.exists());
    assert!(
        is_registered_worktree(source.path(), &recreated.workspace_path).await,
        "recreated workspace should be registered as a live worktree"
    );
}

#[tokio::test]
async fn create_workspace_blocks_live_foreign_owner() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let task_id = harness_core::types::TaskId("foreign-owner-task".to_string());

    mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create first owner");

    let err = mgr_b
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect_err("second owner should be blocked");
    assert!(
        err.to_string().contains("manual resolution required"),
        "foreign live owner should remain a protected hard stop: {err}"
    );
}

#[tokio::test]
async fn reconcile_startup_removes_generation_drifted_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let task_id = harness_core::types::TaskId("startup-reconcile-task".to_string());

    let lease = mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    assert!(lease.workspace_path.exists());

    let task_summary = crate::task_runner::TaskSummary {
        id: task_id.clone(),
        status: crate::task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: None,
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 2,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };

    let summary = mgr_b
        .reconcile_startup(source.path(), &[task_summary])
        .await
        .expect("startup reconcile");
    assert_eq!(summary.removed, 1);
    assert!(
        !lease.workspace_path.exists(),
        "generation drift should be cleaned"
    );
}

#[tokio::test]
async fn reconcile_startup_preserves_shared_issue_workspace_when_any_attempt_active() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let active_task_id = harness_core::types::TaskId("active-issue-42-task".to_string());

    let lease = mgr_a
        .create_workspace(
            &active_task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create issue workspace");
    std::fs::remove_file(owner_record_path(&lease.workspace_path).expect("owner path"))
        .expect("remove owner record");

    let mut active_task = crate::task_runner::TaskSummary {
        id: active_task_id,
        status: crate::task_runner::TaskStatus::Implementing,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: Some("issue:42".to_string()),
        repo: Some("owner/repo".to_string()),
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 1,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };
    let terminal_task_id = harness_core::types::TaskId("terminal-issue-42-task".to_string());
    let terminal_task = crate::task_runner::TaskSummary {
        id: terminal_task_id,
        status: crate::task_runner::TaskStatus::Failed,
        workspace_path: active_task.workspace_path.clone(),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        project: active_task.project.clone(),
        external_id: active_task.external_id.clone(),
        repo: active_task.repo.clone(),
        ..active_task.clone()
    };
    active_task.status = crate::task_runner::TaskStatus::Implementing;

    let summary = mgr_b
        .reconcile_startup(source.path(), &[active_task, terminal_task])
        .await
        .expect("startup reconcile");

    assert_eq!(summary.preserved, 1);
    assert!(
        lease.workspace_path.exists(),
        "startup reconciliation must not delete a shared issue workspace while any attempt is active"
    );

    cleanup_workspace_path(source.path(), &lease.workspace_path)
        .await
        .expect("cleanup test workspace");
}

#[tokio::test]
async fn reconcile_startup_cleans_up_with_workspace_owning_repo() {
    let source_a = tempfile::tempdir().expect("tempdir");
    init_git_repo(source_a.path());
    let branch_a = current_branch(source_a.path());

    let source_b = tempfile::tempdir().expect("tempdir");
    init_git_repo(source_b.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let task_id = harness_core::types::TaskId("startup-owning-repo-task".to_string());

    let lease = mgr_a
        .create_workspace(
            &task_id,
            source_a.path(),
            "origin",
            &branch_a,
            1,
            None,
            None,
        )
        .await
        .expect("create workspace");
    assert!(lease.workspace_path.exists());

    let task_summary = crate::task_runner::TaskSummary {
        id: task_id,
        status: crate::task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: None,
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source_a.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 2,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };

    let summary = mgr_b
        .reconcile_startup(source_b.path(), &[task_summary])
        .await
        .expect("startup reconcile");
    assert_eq!(summary.removed, 1);
    assert!(
        !lease.workspace_path.exists(),
        "generation drift should be cleaned"
    );

    let listed = String::from_utf8(
        run_git(&[
            "-C",
            &source_a.path().to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .stdout,
    )
    .expect("utf8");
    assert!(
        !listed.lines().any(|line| {
            line.strip_prefix("worktree ")
                .is_some_and(|entry| entry == lease.workspace_path.to_string_lossy())
        }),
        "startup cleanup must prune the owning repo entry"
    );
}

#[tokio::test]
async fn cleanup_workspace_path_prunes_missing_registered_worktree() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("missing-registered-task".to_string());

    let lease = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    std::fs::remove_dir_all(&lease.workspace_path).expect("remove checkout dir");

    assert!(
        is_registered_worktree(source.path(), &lease.workspace_path).await,
        "git should still have a stale worktree registration"
    );

    cleanup_workspace_path(source.path(), &lease.workspace_path)
        .await
        .expect("cleanup missing registered worktree");

    assert!(
        !is_registered_worktree(source.path(), &lease.workspace_path).await,
        "cleanup should prune missing worktree registration"
    );
}

#[tokio::test]
async fn reconcile_startup_prunes_missing_registered_worktree_for_tracked_task() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let task_id = harness_core::types::TaskId("startup-missing-registered-task".to_string());

    let lease = mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    std::fs::remove_dir_all(&lease.workspace_path).expect("remove checkout dir");

    assert!(
        is_registered_worktree(source.path(), &lease.workspace_path).await,
        "git should still have a stale worktree registration"
    );

    let task_summary = crate::task_runner::TaskSummary {
        id: task_id.clone(),
        status: crate::task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: None,
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 1,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };

    let summary = mgr_b
        .reconcile_startup(source.path(), &[task_summary])
        .await
        .expect("startup reconcile");
    assert_eq!(summary.removed, 1);
    assert!(
        !is_registered_worktree(source.path(), &lease.workspace_path).await,
        "startup reconcile should prune missing worktree registration"
    );

    let recreated = mgr_b
        .create_workspace(&task_id, source.path(), "origin", &branch, 2, None, None)
        .await
        .expect("recreate workspace after startup reconcile");
    assert!(
        recreated.workspace_path.exists(),
        "startup reconcile should unblock the next worktree add"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_startup_continues_after_workspace_cleanup_failure() {
    use std::os::unix::fs::PermissionsExt;

    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");

    let bad_path = workspaces.path().join("bad-stale-workspace");
    std::fs::create_dir_all(&bad_path).expect("create bad workspace");
    std::fs::write(bad_path.join("locked"), "locked").expect("write locked child");
    std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0o500))
        .expect("lock bad workspace");

    let good_path = workspaces.path().join("good-stale-workspace");
    std::fs::create_dir_all(&good_path).expect("create good workspace");

    let summary = mgr
        .reconcile_startup(source.path(), &[])
        .await
        .expect("startup reconcile should continue after per-entry cleanup failure");

    std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0o700))
        .expect("unlock bad workspace");

    assert_eq!(
        summary.removed, 1,
        "successful entries should still be counted after another entry fails"
    );
    assert!(
        !good_path.exists(),
        "good stale workspace should still be cleaned"
    );
    assert!(
        bad_path.exists(),
        "failed stale workspace should remain for later retry or operator cleanup"
    );
}

struct CwdGuard {
    original: PathBuf,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl CwdGuard {
    async fn switch_to(path: &Path) -> anyhow::Result<Self> {
        let guard = async_cwd_lock().lock().await;
        let original = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self {
            original,
            _guard: guard,
        })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

#[tokio::test]
async fn is_registered_worktree_matches_relative_workspace_paths() {
    let _guard = async_env_lock().lock().await;

    let sandbox = tempfile::tempdir().expect("tempdir");
    let _cwd_guard = CwdGuard::switch_to(sandbox.path())
        .await
        .expect("switch cwd");

    let source = sandbox.path().join("source");
    std::fs::create_dir_all(&source).expect("create source");
    init_git_repo(&source);
    let branch = current_branch(&source);

    let config = WorkspaceConfig {
        root: PathBuf::from("workspaces"),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-relative-path".to_string());

    let lease = mgr
        .create_workspace(&task_id, &source, "origin", &branch, 1, None, None)
        .await
        .expect("create");
    let relative_workspace_path = lease
        .workspace_path
        .strip_prefix(std::fs::canonicalize(sandbox.path()).expect("canonical sandbox"))
        .expect("workspace should be under sandbox")
        .to_path_buf();

    assert!(
        is_registered_worktree(&source, &relative_workspace_path).await,
        "registered worktree lookup should canonicalize relative paths"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn cleanup_terminal_removes_all_workspaces() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = Arc::new(WorkspaceManager::new(config).expect("new"));

    let ids: Vec<TaskId> = (1..=3)
        .map(|i| harness_core::types::TaskId(format!("cleanup-task-{i:03}")))
        .collect();

    for id in &ids {
        mgr.create_workspace(id, source.path(), "origin", &branch, 1, None, None)
            .await
            .expect("create");
    }

    mgr.cleanup_terminal(&ids).await.expect("cleanup_terminal");

    for id in &ids {
        assert!(
            mgr.get_workspace(id).is_none(),
            "workspace for {id:?} should have been cleaned up"
        );
    }
}

// ── GC trigger demotion tests (issue #969) ────────────────────────────

fn make_task_summary(
    id: &str,
    status: crate::task_runner::TaskStatus,
    external_id: Option<&str>,
) -> crate::task_runner::TaskSummary {
    crate::task_runner::TaskSummary {
        id: harness_core::types::TaskId(id.to_string()),
        status,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: external_id.map(|s| s.to_string()),
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 1,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    }
}

/// reconcile_startup counts issue-keyed terminal dirs as `migrated`, not `removed`.
/// Uses separate managers so the workspace dirs are not tracked as active by the
/// reconciling manager (matching the real server startup scenario).
#[tokio::test]
async fn reconcile_startup_migration_counts_new_key_terminal_as_migrated() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };

    // mgr_a creates the UUID workspace (simulates the previous server session).
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr_a");
    let uuid_id = harness_core::types::TaskId("uuid-task-migration-123".to_string());
    let uuid_lease = mgr_a
        .create_workspace(&uuid_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create uuid workspace");

    // Simulate a new-key workspace: dir with owner record where task_id = "issue:42".
    let issue_dir = workspaces.path().join("issue_42");
    std::fs::create_dir_all(issue_dir.join(".git")).expect("create issue dir");
    std::fs::write(
        issue_dir.join(".git").join(OWNER_RECORD_FILE),
        serde_json::to_vec(&WorkspaceOwnerRecord {
            task_id: "issue:42".to_string(),
            run_generation: 1,
            owner_session: "test-session".to_string(),
            workspace_key: None,
        })
        .expect("serialize"),
    )
    .expect("write owner record");

    // mgr_b is the fresh server-startup manager — has no active workspaces.
    let mgr_b = WorkspaceManager::new(config).expect("mgr_b");
    let uuid_task = make_task_summary(
        "uuid-task-migration-123",
        crate::task_runner::TaskStatus::Done,
        None,
    );

    let summary = mgr_b
        .reconcile_startup(source.path(), &[uuid_task])
        .await
        .expect("reconcile startup");

    assert_eq!(summary.removed, 1, "UUID terminal dir counted as removed");
    assert_eq!(
        summary.migrated, 1,
        "issue-keyed terminal dir counted as migrated"
    );
    assert!(
        !uuid_lease.workspace_path.exists(),
        "uuid workspace cleaned up"
    );
    assert!(!issue_dir.exists(), "issue-keyed workspace cleaned up");
}

/// reconcile_startup orphan UUID dir (no task) → removed, migrated stays 0.
#[tokio::test]
async fn reconcile_startup_uuid_orphan_removed() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    // mgr_a creates the workspace; mgr_b is the fresh startup manager.
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr_a");
    let task_id = harness_core::types::TaskId("orphan-uuid-task".to_string());

    let lease = mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");

    let mgr_b = WorkspaceManager::new(config).expect("mgr_b");
    let summary = mgr_b
        .reconcile_startup(source.path(), &[])
        .await
        .expect("reconcile startup");

    assert_eq!(summary.removed, 1);
    assert_eq!(summary.migrated, 0);
    assert!(
        !lease.workspace_path.exists(),
        "orphan uuid workspace removed"
    );
}

/// reconcile_disk_workspaces: UUID-keyed dirs are skipped (not touched).
#[tokio::test]
async fn reconcile_disk_skips_uuid_keyed_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");
    let task_id = harness_core::types::TaskId("some-uuid-task".to_string());

    let lease = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    mgr.release_workspace(&task_id).await;

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.skipped_uuid, 1);
    assert_eq!(summary.removed, 0);
    assert!(
        lease.workspace_path.exists(),
        "uuid dir preserved by disk GC"
    );

    let _ = cleanup_workspace_path(source.path(), &lease.workspace_path).await;
}

/// reconcile_disk_workspaces: removes a closed-issue workspace.
#[tokio::test]
async fn reconcile_disk_removes_closed_issue_workspace() {
    let _env_guard = async_env_lock().lock().await;
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");

    let issue_dir = workspaces.path().join("myorg_my-repo__issue_42");
    std::fs::create_dir_all(issue_dir.join(".git")).expect("mkdir");
    std::fs::write(
        issue_dir.join(".git").join(OWNER_RECORD_FILE),
        serde_json::to_vec(&WorkspaceOwnerRecord {
            task_id: "issue:42".to_string(),
            run_generation: 1,
            owner_session: "s".to_string(),
            workspace_key: Some("myorg_my-repo__issue_42".to_string()),
        })
        .expect("serialize"),
    )
    .expect("write record");

    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/42", r#"{"state":"closed"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.removed, 1);
    assert!(!issue_dir.exists(), "closed issue workspace removed");
}

#[tokio::test]
async fn reconcile_disk_removes_closed_issue_pool_slot_workspace() {
    let _env_guard = async_env_lock().lock().await;
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");
    let task_id = harness_core::types::TaskId("closed-issue-pool-slot".to_string());

    let lease = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("myorg/my-repo"),
        )
        .await
        .expect("create pool slot workspace");
    mgr.release_workspace(&task_id).await;

    let owner = read_owner_record(&lease.workspace_path).expect("owner record");
    assert!(
        owner
            .workspace_key
            .as_deref()
            .and_then(repo_slug_from_workspace_key)
            .is_some(),
        "owner record should retain parseable repo identity"
    );
    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/42", r#"{"state":"closed"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.removed, 1);
    assert!(
        !lease.workspace_path.exists(),
        "closed issue pool slot workspace removed"
    );
}

/// reconcile_disk_workspaces: preserves an open-issue workspace.
#[tokio::test]
async fn reconcile_disk_skips_open_issue_workspace() {
    let _env_guard = async_env_lock().lock().await;
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");

    let issue_dir = workspaces.path().join("myorg_my-repo__issue_7");
    std::fs::create_dir_all(issue_dir.join(".git")).expect("mkdir");
    std::fs::write(
        issue_dir.join(".git").join(OWNER_RECORD_FILE),
        serde_json::to_vec(&WorkspaceOwnerRecord {
            task_id: "issue:7".to_string(),
            run_generation: 1,
            owner_session: "s".to_string(),
            workspace_key: Some("myorg_my-repo__issue_7".to_string()),
        })
        .expect("serialize"),
    )
    .expect("write record");

    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/7", r#"{"state":"open"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.removed, 0);
    assert_eq!(summary.skipped_open, 1);
    assert!(issue_dir.exists(), "open issue workspace preserved");
}
