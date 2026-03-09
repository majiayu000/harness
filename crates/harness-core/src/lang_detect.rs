use crate::Language;
use std::path::Path;

/// Detect the primary language/toolchain of a project from well-known indicator files.
///
/// Detection order: Rust → Go → TypeScript → Python → Common (fallback).
pub fn detect_language(project_root: &Path) -> Language {
    if project_root.join("Cargo.toml").exists() {
        return Language::Rust;
    }
    if project_root.join("go.mod").exists() {
        return Language::Go;
    }
    if project_root.join("package.json").exists() {
        return Language::TypeScript;
    }
    if project_root.join("pyproject.toml").exists()
        || project_root.join("setup.py").exists()
        || project_root.join("requirements.txt").exists()
    {
        return Language::Python;
    }
    Language::Common
}

/// Default pre-commit validation commands for the detected language.
///
/// These commands run after agent output to verify formatting, compilation,
/// and linting before continuing to the review loop.
pub fn default_pre_commit_commands(lang: Language) -> Vec<String> {
    match lang {
        Language::Rust => vec![
            "cargo fmt --all -- --check".to_string(),
            "cargo check --workspace --all-targets".to_string(),
            "cargo clippy --workspace -- -D warnings".to_string(),
        ],
        Language::TypeScript => vec!["npx tsc --noEmit".to_string()],
        Language::Python => vec!["ruff check .".to_string()],
        Language::Go => vec![
            "test -z \"$(gofmt -l .)\"".to_string(),
            "go build ./...".to_string(),
        ],
        Language::Common => vec![],
    }
}

/// Default pre-push validation commands for the detected language.
pub fn default_pre_push_commands(lang: Language) -> Vec<String> {
    match lang {
        Language::Rust => vec!["cargo test --workspace".to_string()],
        Language::TypeScript => vec!["npm test".to_string()],
        Language::Python => vec!["pytest".to_string()],
        Language::Go => vec!["go test ./...".to_string()],
        Language::Common => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    #[test]
    fn detects_rust_project() {
        let dir = tmpdir();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Rust);
    }

    #[test]
    fn detects_go_project() {
        let dir = tmpdir();
        fs::write(dir.path().join("go.mod"), "module example.com/m").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Go);
    }

    #[test]
    fn detects_typescript_project() {
        let dir = tmpdir();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_language(dir.path()), Language::TypeScript);
    }

    #[test]
    fn detects_python_via_requirements() {
        let dir = tmpdir();
        fs::write(dir.path().join("requirements.txt"), "flask").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Python);
    }

    #[test]
    fn detects_python_via_pyproject() {
        let dir = tmpdir();
        fs::write(dir.path().join("pyproject.toml"), "[tool.poetry]").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Python);
    }

    #[test]
    fn falls_back_to_common_for_unknown() {
        let dir = tmpdir();
        assert_eq!(detect_language(dir.path()), Language::Common);
    }

    #[test]
    fn rust_takes_priority_over_go_when_both_present() {
        let dir = tmpdir();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        fs::write(dir.path().join("go.mod"), "module m").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Rust);
    }

    #[test]
    fn rust_pre_commit_commands_include_fmt_and_check() {
        let cmds = default_pre_commit_commands(Language::Rust);
        assert!(cmds.iter().any(|c| c.contains("cargo fmt")));
        assert!(cmds.iter().any(|c| c.contains("cargo check")));
        assert!(cmds.iter().any(|c| c.contains("cargo clippy")));
    }

    #[test]
    fn common_language_returns_empty_commands() {
        assert!(default_pre_commit_commands(Language::Common).is_empty());
        assert!(default_pre_push_commands(Language::Common).is_empty());
    }
}
