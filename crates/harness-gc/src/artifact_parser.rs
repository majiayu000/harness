use harness_core::{Artifact, ArtifactType, RemediationType, Signal};
use std::path::PathBuf;

/// Parses agent output into structured artifacts.
///
/// Extracts:
/// - Fenced code blocks with file paths (` ```lang path/to/file `)
/// - Unified diff hunks (` ```diff ` blocks or standalone `diff --git` blocks)
/// - Falls back to a single markdown blob if no structured content is found.
pub fn parse_artifacts(output: &str, signal: &Signal) -> Vec<Artifact> {
    let artifact_type = remediation_to_artifact_type(signal.remediation);
    let mut artifacts = parse_structured_blocks(output, artifact_type);
    if artifacts.is_empty() {
        artifacts.push(Artifact {
            artifact_type,
            target_path: PathBuf::from(format!(".harness/drafts/{}.md", signal.id)),
            content: output.to_string(),
        });
    }
    artifacts
}

fn remediation_to_artifact_type(remediation: RemediationType) -> ArtifactType {
    match remediation {
        RemediationType::Guard => ArtifactType::Guard,
        RemediationType::Rule => ArtifactType::Rule,
        RemediationType::Hook => ArtifactType::Hook,
        RemediationType::Skill => ArtifactType::Skill,
    }
}

/// Parses fenced code blocks and standalone diff blocks from markdown output.
///
/// Recognized patterns:
/// 1. ` ```lang path/to/file ` — code block with explicit file path
/// 2. ` ```diff ` — unified diff block; target path extracted from `+++ b/` line
/// 3. `diff --git a/path b/path` — standalone diff outside fences
fn parse_structured_blocks(output: &str, artifact_type: ArtifactType) -> Vec<Artifact> {
    let mut artifacts = parse_fenced_blocks(output, artifact_type);
    if artifacts.is_empty() {
        artifacts = parse_standalone_diffs(output, artifact_type);
    }
    artifacts
}

/// Scans for fenced code blocks (` ``` `).
fn parse_fenced_blocks(output: &str, artifact_type: ArtifactType) -> Vec<Artifact> {
    let mut artifacts = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("```") {
            // ``` opens a fence — parse header tokens
            let (lang, path_hint) = parse_fence_header(rest);
            i += 1;

            // Collect lines until closing ```
            let block_start = i;
            while i < lines.len() && lines[i].trim() != "```" {
                i += 1;
            }
            let block_lines = &lines[block_start..i];
            // skip the closing ```
            if i < lines.len() {
                i += 1;
            }

            if lang == "diff" {
                // Diff block: target path from explicit hint or from +++ line
                let target = path_hint
                    .and_then(valid_relative)
                    .or_else(|| diff_target_path(block_lines));
                if let Some(target_path) = target {
                    artifacts.push(Artifact {
                        artifact_type,
                        target_path,
                        content: block_lines.join("\n"),
                    });
                }
            } else if let Some(path_str) = path_hint.and_then(valid_relative) {
                // Code block with explicit file path
                artifacts.push(Artifact {
                    artifact_type,
                    target_path: path_str,
                    content: block_lines.join("\n"),
                });
            }
            // Fence without path hint → skip
        } else {
            i += 1;
        }
    }

    artifacts
}

/// Parses the header of a fence line (`lang [path]`) into (lang, Option<path>).
fn parse_fence_header(rest: &str) -> (&str, Option<&str>) {
    let rest = rest.trim();
    if rest.is_empty() {
        return ("", None);
    }
    match rest.find(' ') {
        Some(pos) => {
            let lang = &rest[..pos];
            let path = rest[pos + 1..].trim();
            (lang, if path.is_empty() { None } else { Some(path) })
        }
        None => (rest, None),
    }
}

/// Scans for standalone `diff --git` blocks that appear outside fenced code blocks.
fn parse_standalone_diffs(output: &str, artifact_type: ArtifactType) -> Vec<Artifact> {
    let mut artifacts = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;
    let mut in_fence = false;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Track fence state
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }

        if !in_fence && trimmed.starts_with("diff --git ") {
            let block_start = i;
            i += 1;
            // Collect until next diff header or end
            while i < lines.len() && !lines[i].trim().starts_with("diff --git ") {
                i += 1;
            }
            let block_lines = &lines[block_start..i];
            if let Some(target_path) = diff_target_path(block_lines) {
                artifacts.push(Artifact {
                    artifact_type,
                    target_path,
                    content: block_lines.join("\n"),
                });
            }
            continue;
        }

        i += 1;
    }

    artifacts
}

/// Extracts the target file path from unified diff lines.
///
/// Looks for `+++ b/path` (git diff format) or `+++ path` (plain unified diff).
/// Returns `None` if the target is `/dev/null` (deleted file) or no path found.
fn diff_target_path(lines: &[&str]) -> Option<PathBuf> {
    for line in lines {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim().trim_start_matches("b/");
            if path != "/dev/null" {
                return valid_relative(path);
            }
        }
    }
    None
}

/// Returns `Some(PathBuf)` if `s` is a non-empty relative path with no `..` components.
fn valid_relative(s: &str) -> Option<PathBuf> {
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    if p.is_absolute() {
        return None;
    }
    for component in p.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return None;
        }
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{ProjectId, RemediationType, Signal, SignalType};

    fn guard_signal() -> Signal {
        Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({}),
            RemediationType::Guard,
        )
    }

    // ── code block with file path ──────────────────────────────────────────────

    #[test]
    fn parses_rust_code_block_with_path() {
        let output = "Here is the fix:\n\
            ```rust src/lib.rs\n\
            pub fn hello() {}\n\
            ```\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].target_path, PathBuf::from("src/lib.rs"));
        assert_eq!(artifacts[0].content, "pub fn hello() {}");
    }

    #[test]
    fn parses_multiple_code_blocks_with_paths() {
        let output = "\
            ```rust src/a.rs\n\
            fn a() {}\n\
            ```\n\
            some text\n\
            ```python scripts/b.py\n\
            def b(): pass\n\
            ```\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].target_path, PathBuf::from("src/a.rs"));
        assert_eq!(artifacts[1].target_path, PathBuf::from("scripts/b.py"));
    }

    // ── diff blocks ────────────────────────────────────────────────────────────

    #[test]
    fn parses_fenced_diff_block() {
        let output = "\
            ```diff\n\
            --- a/src/main.rs\n\
            +++ b/src/main.rs\n\
            @@ -1,3 +1,4 @@\n\
             fn main() {\n\
            +    println!(\"hello\");\n\
             }\n\
            ```\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].target_path, PathBuf::from("src/main.rs"));
        assert!(artifacts[0].content.contains("+++ b/src/main.rs"));
    }

    #[test]
    fn parses_diff_block_with_explicit_path_hint() {
        let output = "\
            ```diff src/config.rs\n\
            --- a/src/config.rs\n\
            +++ b/src/config.rs\n\
            @@ -1 +1 @@\n\
            -old\n\
            +new\n\
            ```\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].target_path, PathBuf::from("src/config.rs"));
    }

    #[test]
    fn parses_standalone_diff_git_block() {
        let output = "\
            Some description.\n\
            diff --git a/foo.txt b/foo.txt\n\
            --- a/foo.txt\n\
            +++ b/foo.txt\n\
            @@ -1 +1 @@\n\
            -old\n\
            +new\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].target_path, PathBuf::from("foo.txt"));
    }

    #[test]
    fn parses_multiple_standalone_diffs() {
        let output = "\
            diff --git a/a.txt b/a.txt\n\
            --- a/a.txt\n\
            +++ b/a.txt\n\
            @@ -1 +1 @@\n\
            -x\n\
            +y\n\
            diff --git a/b.txt b/b.txt\n\
            --- a/b.txt\n\
            +++ b/b.txt\n\
            @@ -1 +1 @@\n\
            -p\n\
            +q\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].target_path, PathBuf::from("a.txt"));
        assert_eq!(artifacts[1].target_path, PathBuf::from("b.txt"));
    }

    // ── fallback ───────────────────────────────────────────────────────────────

    #[test]
    fn falls_back_to_single_artifact_when_no_structure() {
        let output = "Just some plain text with no code blocks.";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 1);
        assert!(
            artifacts[0].target_path.starts_with(".harness/drafts/"),
            "expected fallback path, got: {:?}",
            artifacts[0].target_path
        );
        assert_eq!(artifacts[0].content, output);
    }

    // ── path safety ────────────────────────────────────────────────────────────

    #[test]
    fn rejects_absolute_path_in_code_block() {
        let output = "```rust /etc/passwd\nmalicious\n```\n";
        let signal = guard_signal();
        // Should fall back to single blob, not produce an artifact with /etc/passwd
        let artifacts = parse_artifacts(output, &signal);
        for a in &artifacts {
            assert!(
                !a.target_path.is_absolute(),
                "produced absolute path: {:?}",
                a.target_path
            );
        }
    }

    #[test]
    fn rejects_parent_traversal_in_code_block() {
        let output = "```rust ../../etc/shadow\ntraversal\n```\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        for a in &artifacts {
            let path_str = a.target_path.to_string_lossy();
            assert!(
                !path_str.contains(".."),
                "produced traversal path: {:?}",
                a.target_path
            );
        }
    }

    #[test]
    fn skips_dev_null_diff_target() {
        // Deleted file: +++ /dev/null should not produce an artifact
        let output = "\
            ```diff\n\
            --- a/deleted.rs\n\
            +++ /dev/null\n\
            @@ -1 +0,0 @@\n\
            -gone\n\
            ```\n";
        let signal = guard_signal();
        let artifacts = parse_artifacts(output, &signal);
        // No artifact from diff; falls back to single blob
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0].target_path.starts_with(".harness/drafts/"));
    }
}
