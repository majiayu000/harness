#[path = "../build_support/hook_install.rs"]
mod hook_install;

use std::fs;

#[test]
fn installs_hook_in_normal_checkout() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    fs::create_dir(root.join(".git")).expect("git dir");
    fs::create_dir(root.join(".githooks")).expect("hooks source dir");
    fs::write(root.join(".githooks/pre-commit"), "#!/bin/sh\nexit 0\n").expect("hook");

    let installed = hook_install::install_pre_commit_hook(root)
        .expect("install")
        .expect("git checkout");

    assert_eq!(installed, root.join(".git/hooks/pre-commit"));
    assert_eq!(
        fs::read_to_string(installed).expect("installed hook"),
        "#!/bin/sh\nexit 0\n"
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

    let installed = hook_install::install_pre_commit_hook(&root)
        .expect("install")
        .expect("linked worktree");

    assert_eq!(
        installed,
        common
            .canonicalize()
            .expect("canonical common git dir")
            .join("hooks/pre-commit")
    );
    assert_eq!(
        fs::read_to_string(installed).expect("installed hook"),
        "linked\n"
    );
}

#[test]
fn missing_repository_is_a_no_op() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::create_dir(temp.path().join(".githooks")).expect("hooks source dir");
    fs::write(temp.path().join(".githooks/pre-commit"), "hook\n").expect("hook");

    assert_eq!(
        hook_install::install_pre_commit_hook(temp.path()).expect("no-op"),
        None
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
