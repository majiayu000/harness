use std::path::{Path, PathBuf};

const MAX_COMBINED_BYTES: usize = 32 * 1024;

/// Load and merge cascading AGENTS.md files.
///
/// Discovery order (later entries override earlier):
/// 1. `~/.harness/AGENTS.md` (global)
/// 2. `<project_root>/AGENTS.md`
/// 3. Subdirectory AGENTS.md files toward cwd (if different from project root)
///
/// `AGENTS.override.md` at any level replaces all content accumulated so far.
/// Combined output is capped at `MAX_COMBINED_BYTES` (32KB).
pub fn load_agents_md(project_root: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();

    // 1. Global ~/.harness/AGENTS.md
    if let Ok(home) = std::env::var("HOME") {
        let global = PathBuf::from(home).join(".harness").join("AGENTS.md");
        if let Some(content) = read_md(&global) {
            parts.push(content);
        }
    }

    // 2. Project root AGENTS.md / AGENTS.override.md
    let override_path = project_root.join("AGENTS.override.md");
    let agents_path = project_root.join("AGENTS.md");
    if let Some(content) = read_md(&override_path) {
        parts.clear();
        parts.push(content);
    } else if let Some(content) = read_md(&agents_path) {
        parts.push(content);
    }

    // 3. Subdirectories — scan common code directories
    for subdir in &["src", "crates", "lib", "pkg"] {
        let sub_override = project_root.join(subdir).join("AGENTS.override.md");
        let sub_agents = project_root.join(subdir).join("AGENTS.md");
        if let Some(content) = read_md(&sub_override) {
            parts.clear();
            parts.push(content);
        } else if let Some(content) = read_md(&sub_agents) {
            parts.push(content);
        }
    }

    // Enforce size cap
    let mut combined = parts.join("\n\n---\n\n");
    if combined.len() > MAX_COMBINED_BYTES {
        combined.truncate(MAX_COMBINED_BYTES);
    }
    combined
}

/// Discover paths that would be checked for AGENTS.md files.
pub fn discovery_paths(project_root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".harness").join("AGENTS.md"));
    }
    paths.push(project_root.join("AGENTS.md"));
    paths.push(project_root.join("AGENTS.override.md"));
    for subdir in &["src", "crates", "lib", "pkg"] {
        paths.push(project_root.join(subdir).join("AGENTS.md"));
    }
    paths
}

fn read_md(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn load_empty_project_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_agents_md(dir.path());
        assert!(result.is_empty() || result.trim().is_empty());
    }

    #[test]
    fn load_project_root_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Project instructions").unwrap();
        let result = load_agents_md(dir.path());
        assert!(result.contains("Project instructions"));
    }

    #[test]
    fn override_replaces_accumulated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "base instructions").unwrap();
        fs::write(
            dir.path().join("AGENTS.override.md"),
            "override instructions",
        )
        .unwrap();
        let result = load_agents_md(dir.path());
        assert!(result.contains("override instructions"));
        assert!(!result.contains("base instructions"));
    }

    #[test]
    fn subdirectory_agents_md_merged() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/AGENTS.md"), "src specific").unwrap();
        let result = load_agents_md(dir.path());
        assert!(result.contains("root"));
        assert!(result.contains("src specific"));
    }

    #[test]
    fn combined_output_capped_at_32kb() {
        let dir = tempfile::tempdir().unwrap();
        let large = "x".repeat(40_000);
        fs::write(dir.path().join("AGENTS.md"), &large).unwrap();
        let result = load_agents_md(dir.path());
        assert!(result.len() <= MAX_COMBINED_BYTES);
    }

    #[test]
    fn discovery_paths_includes_expected_locations() {
        let dir = tempfile::tempdir().unwrap();
        let paths = discovery_paths(dir.path());
        assert!(paths.iter().any(|p| p.ends_with("AGENTS.md")));
        assert!(paths.iter().any(|p| p.ends_with("AGENTS.override.md")));
    }
}
