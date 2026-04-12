use std::process::Command;

fn main() {
    // Auto-configure git hooks path on every build so contributors
    // never need to run `make setup` manually.
    if std::path::Path::new(".githooks/pre-commit").exists() {
        let _ = Command::new("git")
            .args(["config", "core.hooksPath", ".githooks"])
            .status();
    }
}
