#[path = "../build_support/hook_install.rs"]
mod hook_install;

use hook_install::InstallOutcome;
use std::fs;

#[test]
fn installs_hook_in_normal_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    fs::create_dir(root.join(".git")).expect("git dir");
    fs::create_dir(root.join(".githooks")).expect("hooks source dir");
    fs::write(root.join(".githooks/pre-commit"), "#!/bin/sh\nexit 0\n").expect("hook");

    let InstallOutcome::Installed(installed) =
        hook_install::install_pre_commit_hook(root).expect("install")
    else {
        panic!("expected a new managed hook");
    };

    assert_eq!(installed, root.join(".git/hooks/pre-commit"));
    assert_eq!(
        fs::read_to_string(installed).expect("installed hook"),
        "#!/bin/sh\n# harness-managed-pre-commit-v1\nset -eu\nrepo_root=$(pwd -P)\nexec \"$repo_root/.githooks/pre-commit\" \"$@\"\n"
    );
}

#[test]
fn linked_worktree_installs_hook_in_common_git_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("worktree");
    let common = temp.path().join("repo.git");
    let worktree_git_dir = common.join("worktrees/manual-pr");
    fs::create_dir_all(root.join(".githooks")).expect("hooks source dir");
    fs::create_dir_all(&worktree_git_dir).expect("worktree git dir");
    fs::write(
        root.join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .expect("git pointer");
    fs::write(worktree_git_dir.join("commondir"), "../..\n").expect("commondir");
    fs::write(root.join(".githooks/pre-commit"), "linked\n").expect("hook");

    let InstallOutcome::Installed(installed) =
        hook_install::install_pre_commit_hook(&root).expect("install")
    else {
        panic!("expected a new managed hook");
    };

    assert_eq!(
        installed,
        common
            .canonicalize()
            .expect("canonical common git dir")
            .join("hooks/pre-commit")
    );
    assert_eq!(
        fs::read_to_string(installed).expect("installed hook"),
        "#!/bin/sh\n# harness-managed-pre-commit-v1\nset -eu\nrepo_root=$(pwd -P)\nexec \"$repo_root/.githooks/pre-commit\" \"$@\"\n"
    );
}

#[test]
fn missing_repository_is_a_no_op() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::create_dir(temp.path().join(".githooks")).expect("hooks source dir");
    fs::write(temp.path().join(".githooks/pre-commit"), "hook\n").expect("hook");

    assert_eq!(
        hook_install::install_pre_commit_hook(temp.path()).expect("no-op"),
        InstallOutcome::NotRepository
    );
}

#[test]
fn malformed_worktree_pointer_fails_visibly() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::create_dir(temp.path().join(".githooks")).expect("hooks source dir");
    fs::write(temp.path().join(".githooks/pre-commit"), "hook\n").expect("hook");
    fs::write(temp.path().join(".git"), "not-a-git-pointer\n").expect("git pointer");

    let error = hook_install::install_pre_commit_hook(temp.path()).expect_err("invalid pointer");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn preserves_an_unmanaged_existing_hook() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    fs::create_dir_all(root.join(".git/hooks")).expect("hooks dir");
    fs::create_dir(root.join(".githooks")).expect("source dir");
    fs::write(root.join(".githooks/pre-commit"), "project hook\n").expect("source");
    let destination = root.join(".git/hooks/pre-commit");
    fs::write(&destination, "user hook\n").expect("user hook");

    assert_eq!(
        hook_install::install_pre_commit_hook(root).expect("preserve"),
        InstallOutcome::UnmanagedHookPreserved(destination.clone())
    );
    assert_eq!(
        fs::read_to_string(destination).expect("user hook"),
        "user hook\n"
    );
}

#[test]
fn linked_worktrees_install_the_same_branch_neutral_hook() {
    let temp = tempfile::tempdir().expect("tempdir");
    let common = temp.path().join("repo.git");
    let mut roots = Vec::new();
    for name in ["one", "two"] {
        let root = temp.path().join(name);
        let worktree_git_dir = common.join("worktrees").join(name);
        fs::create_dir_all(root.join(".githooks")).expect("source dir");
        fs::create_dir_all(&worktree_git_dir).expect("worktree git dir");
        fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .expect("git pointer");
        fs::write(worktree_git_dir.join("commondir"), "../..\n").expect("commondir");
        fs::write(
            root.join(".githooks/pre-commit"),
            format!("{name} branch hook\n"),
        )
        .expect("source hook");
        roots.push(root);
    }

    let threads: Vec<_> = roots
        .into_iter()
        .map(|root| std::thread::spawn(move || hook_install::install_pre_commit_hook(&root)))
        .collect();
    for thread in threads {
        let outcome = thread.join().expect("thread").expect("install");
        assert!(matches!(
            outcome,
            InstallOutcome::Installed(_) | InstallOutcome::AlreadyManaged(_)
        ));
    }

    let installed = fs::read_to_string(common.join("hooks/pre-commit")).expect("managed hook");
    assert!(installed.contains("harness-managed-pre-commit-v1"));
    assert!(installed.contains("$repo_root/.githooks/pre-commit"));
    assert!(!installed.contains("one branch hook"));
    assert!(!installed.contains("two branch hook"));
}
