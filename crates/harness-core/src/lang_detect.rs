use crate::Language;
use std::path::Path;

/// Detect the primary language/toolchain of a project from well-known indicator files.
///
/// Detection order: Rust → Go → TypeScript → Python → Java → CSharp → Ruby → Common.
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
    if project_root.join("build.gradle").exists()
        || project_root.join("build.gradle.kts").exists()
        || project_root.join("pom.xml").exists()
    {
        return Language::Java;
    }
    if has_csharp_project_files(project_root) {
        return Language::CSharp;
    }
    if project_root.join("Gemfile").exists() {
        return Language::Ruby;
    }
    Language::Common
}

// ── Rust helpers ─────────────────────────────────────────────────────────────

/// Returns `true` when the root `Cargo.toml` contains a `[workspace]` section.
fn is_rust_workspace(project_root: &Path) -> bool {
    std::fs::read_to_string(project_root.join("Cargo.toml"))
        .map(|s| s.contains("[workspace]"))
        .unwrap_or(false)
}

// ── TypeScript helpers ────────────────────────────────────────────────────────

/// Detect the package manager from lock-file presence.
fn detect_ts_package_manager(project_root: &Path) -> &'static str {
    if project_root.join("yarn.lock").exists() {
        "yarn"
    } else if project_root.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else {
        "npm"
    }
}

/// Returns `true` when an ESLint configuration file is present.
fn has_eslint_config(project_root: &Path) -> bool {
    const NAMES: &[&str] = &[
        ".eslintrc",
        ".eslintrc.js",
        ".eslintrc.cjs",
        ".eslintrc.json",
        ".eslintrc.yaml",
        ".eslintrc.yml",
        "eslint.config.js",
        "eslint.config.mjs",
        "eslint.config.cjs",
    ];
    NAMES.iter().any(|n| project_root.join(n).exists())
}

/// Returns `true` when a Biome configuration file is present.
fn has_biome_config(project_root: &Path) -> bool {
    project_root.join("biome.json").exists()
}

// ── Java helpers ──────────────────────────────────────────────────────────────

/// Returns `true` when the project uses Gradle (`build.gradle` / `build.gradle.kts`).
fn is_gradle_project(project_root: &Path) -> bool {
    project_root.join("build.gradle").exists() || project_root.join("build.gradle.kts").exists()
}

// ── C# helpers ────────────────────────────────────────────────────────────────

fn has_csharp_project_files(project_root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(project_root) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_str()
            .map(|n| n.ends_with(".csproj") || n.ends_with(".sln"))
            .unwrap_or(false)
    })
}

// ── Ruby helpers ──────────────────────────────────────────────────────────────

fn has_rubocop_config(project_root: &Path) -> bool {
    project_root.join(".rubocop.yml").exists()
}

fn has_spec_dir(project_root: &Path) -> bool {
    project_root.join("spec").is_dir()
}

// ── Command builders ──────────────────────────────────────────────────────────

/// Default pre-commit validation commands for the detected language.
///
/// These commands run after agent output to verify formatting, compilation,
/// and linting before continuing to the review loop.
pub fn default_pre_commit_commands(lang: Language, project_root: &Path) -> Vec<String> {
    match lang {
        Language::Rust => {
            let scope = if is_rust_workspace(project_root) {
                "--workspace --all-targets"
            } else {
                "--all-targets"
            };
            vec![
                "cargo fmt --all -- --check".to_string(),
                format!("cargo check {scope}"),
                format!("cargo clippy {scope} -- -D warnings"),
            ]
        }
        Language::Go => vec![
            "test -z \"$(gofmt -l .)\"".to_string(),
            "go vet ./...".to_string(),
            "go build ./...".to_string(),
        ],
        Language::TypeScript => {
            let mut cmds = vec!["npx tsc --noEmit".to_string()];
            if has_biome_config(project_root) {
                cmds.push("npx biome check .".to_string());
            } else if has_eslint_config(project_root) {
                cmds.push("npx eslint .".to_string());
            }
            cmds
        }
        Language::Python => vec![
            "ruff format --check .".to_string(),
            "ruff check .".to_string(),
        ],
        Language::Java => {
            if is_gradle_project(project_root) {
                vec!["./gradlew check".to_string()]
            } else {
                vec!["mvn compile -B".to_string()]
            }
        }
        Language::CSharp => vec![
            "dotnet format --verify-no-changes".to_string(),
            "dotnet build".to_string(),
        ],
        Language::Ruby => {
            if has_rubocop_config(project_root) {
                vec!["bundle exec rubocop".to_string()]
            } else {
                vec![]
            }
        }
        Language::Common => vec![],
    }
}

/// Default pre-push validation commands for the detected language.
pub fn default_pre_push_commands(lang: Language, project_root: &Path) -> Vec<String> {
    match lang {
        Language::Rust => {
            if is_rust_workspace(project_root) {
                vec!["cargo test --workspace".to_string()]
            } else {
                vec!["cargo test".to_string()]
            }
        }
        Language::Go => vec!["go test ./...".to_string()],
        Language::TypeScript => {
            let pm = detect_ts_package_manager(project_root);
            vec![format!("{pm} test")]
        }
        Language::Python => vec!["pytest".to_string()],
        Language::Java => {
            if is_gradle_project(project_root) {
                vec!["./gradlew test".to_string()]
            } else {
                vec!["mvn verify -B".to_string()]
            }
        }
        Language::CSharp => vec!["dotnet test".to_string()],
        Language::Ruby => {
            if has_spec_dir(project_root) {
                vec!["bundle exec rspec".to_string()]
            } else {
                vec!["bundle exec rake test".to_string()]
            }
        }
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

    // ── detect_language ───────────────────────────────────────────────────────

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
    fn detects_python_via_setup_py() {
        let dir = tmpdir();
        fs::write(dir.path().join("setup.py"), "from setuptools import setup").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Python);
    }

    #[test]
    fn detects_java_gradle() {
        let dir = tmpdir();
        fs::write(dir.path().join("build.gradle"), "").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Java);
    }

    #[test]
    fn detects_java_gradle_kts() {
        let dir = tmpdir();
        fs::write(dir.path().join("build.gradle.kts"), "").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Java);
    }

    #[test]
    fn detects_java_maven() {
        let dir = tmpdir();
        fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Java);
    }

    #[test]
    fn detects_csharp_via_csproj() {
        let dir = tmpdir();
        fs::write(dir.path().join("MyApp.csproj"), "").unwrap();
        assert_eq!(detect_language(dir.path()), Language::CSharp);
    }

    #[test]
    fn detects_csharp_via_sln() {
        let dir = tmpdir();
        fs::write(dir.path().join("MySolution.sln"), "").unwrap();
        assert_eq!(detect_language(dir.path()), Language::CSharp);
    }

    #[test]
    fn detects_ruby_via_gemfile() {
        let dir = tmpdir();
        fs::write(dir.path().join("Gemfile"), "source 'https://rubygems.org'").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Ruby);
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
    fn python_takes_priority_over_java_when_both_present() {
        let dir = tmpdir();
        fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        fs::write(dir.path().join("pom.xml"), "").unwrap();
        assert_eq!(detect_language(dir.path()), Language::Python);
    }

    // ── Rust commands ─────────────────────────────────────────────────────────

    #[test]
    fn rust_non_workspace_pre_commit_excludes_workspace_flag() {
        let dir = tmpdir();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"foo\"").unwrap();
        let cmds = default_pre_commit_commands(Language::Rust, dir.path());
        assert!(cmds.iter().any(|c| c.contains("cargo fmt")));
        assert!(cmds
            .iter()
            .any(|c| c.contains("cargo check") && !c.contains("--workspace")));
        assert!(cmds.iter().any(|c| c.contains("cargo clippy")
            && c.contains("-D warnings")
            && !c.contains("--workspace")));
    }

    #[test]
    fn rust_workspace_pre_commit_includes_workspace_and_all_targets() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]",
        )
        .unwrap();
        let cmds = default_pre_commit_commands(Language::Rust, dir.path());
        assert!(cmds
            .iter()
            .any(|c| c.contains("--workspace") && c.contains("cargo check")));
        assert!(cmds
            .iter()
            .any(|c| c.contains("--workspace") && c.contains("cargo clippy")));
        assert!(cmds.iter().any(|c| c.contains("--all-targets")));
    }

    #[test]
    fn rust_workspace_pre_push_uses_workspace_flag() {
        let dir = tmpdir();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]",
        )
        .unwrap();
        let cmds = default_pre_push_commands(Language::Rust, dir.path());
        assert_eq!(cmds, vec!["cargo test --workspace"]);
    }

    #[test]
    fn rust_non_workspace_pre_push_omits_workspace_flag() {
        let dir = tmpdir();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"foo\"").unwrap();
        let cmds = default_pre_push_commands(Language::Rust, dir.path());
        assert_eq!(cmds, vec!["cargo test"]);
    }

    // ── Go commands ───────────────────────────────────────────────────────────

    #[test]
    fn go_pre_commit_includes_vet() {
        let dir = tmpdir();
        let cmds = default_pre_commit_commands(Language::Go, dir.path());
        assert!(cmds.iter().any(|c| c == "go vet ./..."));
    }

    #[test]
    fn go_pre_commit_order_gofmt_vet_build() {
        let dir = tmpdir();
        let cmds = default_pre_commit_commands(Language::Go, dir.path());
        let gofmt_pos = cmds.iter().position(|c| c.contains("gofmt")).unwrap();
        let vet_pos = cmds.iter().position(|c| c == "go vet ./...").unwrap();
        let build_pos = cmds.iter().position(|c| c == "go build ./...").unwrap();
        assert!(gofmt_pos < vet_pos && vet_pos < build_pos);
    }

    // ── TypeScript commands ───────────────────────────────────────────────────

    #[test]
    fn typescript_pre_push_npm_when_no_lockfile() {
        let dir = tmpdir();
        let cmds = default_pre_push_commands(Language::TypeScript, dir.path());
        assert_eq!(cmds, vec!["npm test"]);
    }

    #[test]
    fn typescript_pre_push_yarn_when_yarn_lock() {
        let dir = tmpdir();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        let cmds = default_pre_push_commands(Language::TypeScript, dir.path());
        assert_eq!(cmds, vec!["yarn test"]);
    }

    #[test]
    fn typescript_pre_push_pnpm_when_pnpm_lock() {
        let dir = tmpdir();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        let cmds = default_pre_push_commands(Language::TypeScript, dir.path());
        assert_eq!(cmds, vec!["pnpm test"]);
    }

    #[test]
    fn typescript_pre_commit_adds_eslint_when_config_exists() {
        let dir = tmpdir();
        fs::write(dir.path().join(".eslintrc.json"), "{}").unwrap();
        let cmds = default_pre_commit_commands(Language::TypeScript, dir.path());
        assert!(cmds.iter().any(|c| c == "npx eslint ."));
    }

    #[test]
    fn typescript_pre_commit_adds_biome_when_config_exists() {
        let dir = tmpdir();
        fs::write(dir.path().join("biome.json"), "{}").unwrap();
        let cmds = default_pre_commit_commands(Language::TypeScript, dir.path());
        assert!(cmds.iter().any(|c| c == "npx biome check ."));
        assert!(!cmds.iter().any(|c| c == "npx eslint ."));
    }

    #[test]
    fn typescript_biome_takes_precedence_over_eslint() {
        let dir = tmpdir();
        fs::write(dir.path().join("biome.json"), "{}").unwrap();
        fs::write(dir.path().join(".eslintrc.json"), "{}").unwrap();
        let cmds = default_pre_commit_commands(Language::TypeScript, dir.path());
        assert!(cmds.iter().any(|c| c == "npx biome check ."));
        assert!(!cmds.iter().any(|c| c == "npx eslint ."));
    }

    // ── Python commands ───────────────────────────────────────────────────────

    #[test]
    fn python_pre_commit_includes_ruff_format_check() {
        let dir = tmpdir();
        let cmds = default_pre_commit_commands(Language::Python, dir.path());
        assert!(cmds.iter().any(|c| c == "ruff format --check ."));
        assert!(cmds.iter().any(|c| c == "ruff check ."));
    }

    // ── Java commands ─────────────────────────────────────────────────────────

    #[test]
    fn java_gradle_pre_commit_uses_gradlew_check() {
        let dir = tmpdir();
        fs::write(dir.path().join("build.gradle"), "").unwrap();
        let cmds = default_pre_commit_commands(Language::Java, dir.path());
        assert_eq!(cmds, vec!["./gradlew check"]);
    }

    #[test]
    fn java_gradle_pre_push_uses_gradlew_test() {
        let dir = tmpdir();
        fs::write(dir.path().join("build.gradle"), "").unwrap();
        let cmds = default_pre_push_commands(Language::Java, dir.path());
        assert_eq!(cmds, vec!["./gradlew test"]);
    }

    #[test]
    fn java_maven_pre_commit_uses_mvn_compile() {
        let dir = tmpdir();
        // no build.gradle → Maven fallback
        let cmds = default_pre_commit_commands(Language::Java, dir.path());
        assert_eq!(cmds, vec!["mvn compile -B"]);
    }

    #[test]
    fn java_maven_pre_push_uses_mvn_verify() {
        let dir = tmpdir();
        let cmds = default_pre_push_commands(Language::Java, dir.path());
        assert_eq!(cmds, vec!["mvn verify -B"]);
    }

    // ── C# commands ───────────────────────────────────────────────────────────

    #[test]
    fn csharp_pre_commit_commands() {
        let dir = tmpdir();
        let cmds = default_pre_commit_commands(Language::CSharp, dir.path());
        assert_eq!(
            cmds,
            vec!["dotnet format --verify-no-changes", "dotnet build"]
        );
    }

    #[test]
    fn csharp_pre_push_commands() {
        let dir = tmpdir();
        let cmds = default_pre_push_commands(Language::CSharp, dir.path());
        assert_eq!(cmds, vec!["dotnet test"]);
    }

    // ── Ruby commands ─────────────────────────────────────────────────────────

    #[test]
    fn ruby_pre_commit_rubocop_when_config_exists() {
        let dir = tmpdir();
        fs::write(dir.path().join(".rubocop.yml"), "").unwrap();
        let cmds = default_pre_commit_commands(Language::Ruby, dir.path());
        assert_eq!(cmds, vec!["bundle exec rubocop"]);
    }

    #[test]
    fn ruby_pre_commit_empty_without_rubocop_config() {
        let dir = tmpdir();
        let cmds = default_pre_commit_commands(Language::Ruby, dir.path());
        assert!(cmds.is_empty());
    }

    #[test]
    fn ruby_pre_push_rspec_when_spec_dir_exists() {
        let dir = tmpdir();
        fs::create_dir(dir.path().join("spec")).unwrap();
        let cmds = default_pre_push_commands(Language::Ruby, dir.path());
        assert_eq!(cmds, vec!["bundle exec rspec"]);
    }

    #[test]
    fn ruby_pre_push_rake_test_without_spec_dir() {
        let dir = tmpdir();
        let cmds = default_pre_push_commands(Language::Ruby, dir.path());
        assert_eq!(cmds, vec!["bundle exec rake test"]);
    }

    // ── Common ────────────────────────────────────────────────────────────────

    #[test]
    fn common_language_returns_empty_commands() {
        let dir = tmpdir();
        assert!(default_pre_commit_commands(Language::Common, dir.path()).is_empty());
        assert!(default_pre_push_commands(Language::Common, dir.path()).is_empty());
    }
}
