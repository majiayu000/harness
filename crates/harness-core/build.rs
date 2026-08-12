#[path = "build_support/hook_install.rs"]
mod hook_install;

use std::path::PathBuf;

fn main() {
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let manifest_dir = PathBuf::from(manifest_dir);
    let Some(workspace_root) = manifest_dir.parent().and_then(|path| path.parent()) else {
        return;
    };
    let hook = workspace_root.join(".githooks").join("pre-commit");
    println!("cargo:rerun-if-changed={}", hook.display());

    if let Err(error) = hook_install::install_pre_commit_hook(workspace_root) {
        println!("cargo:warning=unable to install the project pre-commit hook: {error}");
    }
}
