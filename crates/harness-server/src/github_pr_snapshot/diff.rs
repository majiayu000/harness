use super::{value_string, value_u64, GitHubPrSnapshotTarget};
use anyhow::Context;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;

pub(crate) async fn fetch_complete_pr_diff(
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    normalized_snapshot: &Value,
) -> anyhow::Result<Value> {
    let client = reqwest::Client::new();
    fetch_complete_pr_diff_with_client(
        &client,
        target,
        github_token,
        normalized_snapshot,
        &crate::github_client::github_api_base_url(),
    )
    .await
}

async fn fetch_complete_pr_diff_with_client(
    client: &reqwest::Client,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    normalized_snapshot: &Value,
    api_base: &str,
) -> anyhow::Result<Value> {
    if normalized_snapshot
        .get("changed_files_complete")
        .and_then(Value::as_bool)
        != Some(true)
    {
        anyhow::bail!("server PR snapshot did not completely enumerate changed files");
    }
    let base_oid = required_oid(normalized_snapshot, "base_oid")?;
    let head_oid = required_oid(normalized_snapshot, "head_oid")?;
    let (owner, repo) = target
        .repo_slug
        .split_once('/')
        .context("validated repo slug should contain owner and repo")?;
    let compare_url = format!(
        "{}/repos/{owner}/{repo}/compare/{base_oid}...{head_oid}",
        api_base.trim_end_matches('/')
    );
    let request = crate::github_client::apply_github_headers(
        client
            .get(compare_url)
            .header(reqwest::header::ACCEPT, "application/vnd.github.v3.diff"),
        github_token,
    );
    let response = tokio::time::timeout(Duration::from_secs(15), request.send()).await??;
    let status = response.status();
    let unified_diff = response.text().await?;
    if !status.is_success() {
        anyhow::bail!("GitHub immutable compare diff failed with status {status}: {unified_diff}");
    }
    build_complete_diff_facts(normalized_snapshot, &base_oid, &head_oid, unified_diff)
}

fn required_oid(snapshot: &Value, field: &str) -> anyhow::Result<String> {
    value_string(snapshot.get(field))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .with_context(|| format!("server PR snapshot has no {field}"))
}

fn build_complete_diff_facts(
    normalized_snapshot: &Value,
    base_oid: &str,
    head_oid: &str,
    unified_diff: String,
) -> anyhow::Result<Value> {
    let files = normalized_snapshot
        .get("changed_files")
        .and_then(Value::as_array)
        .context("server PR snapshot changed_files is missing")?;
    if !files.is_empty() && unified_diff.trim().is_empty() {
        anyhow::bail!("GitHub immutable compare returned an empty diff for changed files");
    }

    let mut identities = BTreeMap::new();
    for file in files {
        let path = value_string(file.get("path")).context("changed file path is missing")?;
        let additions =
            value_u64(file.get("additions")).context("changed file additions are missing")?;
        let deletions =
            value_u64(file.get("deletions")).context("changed file deletions are missing")?;
        let change_type =
            value_string(file.get("change_type")).context("changed file type is missing")?;
        if identities
            .insert(path.clone(), (additions, deletions, change_type))
            .is_some()
        {
            anyhow::bail!("server PR snapshot contains duplicate changed file `{path}`");
        }
    }
    validate_immutable_diff_sections(&identities, &unified_diff)?;
    let unified_diff_sha256 = format!("{:x}", Sha256::digest(unified_diff.as_bytes()));
    Ok(json!({
        "base_oid": base_oid,
        "head_oid": head_oid,
        "files_complete": true,
        "patches_complete": true,
        "files": files,
        "unified_diff": unified_diff,
        "unified_diff_sha256": unified_diff_sha256,
    }))
}

fn validate_immutable_diff_sections(
    expected: &BTreeMap<String, (u64, u64, String)>,
    diff: &str,
) -> anyhow::Result<()> {
    let mut observed = BTreeMap::new();
    let mut current_path = None;
    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            finish_diff_section(&mut observed, current_path.take(), additions, deletions)?;
            current_path = Some(path_for_diff_header(line, expected)?);
            additions = 0;
            deletions = 0;
        } else if line.starts_with('+') && !line.starts_with("+++ ") {
            additions = additions
                .checked_add(1)
                .context("immutable diff addition count overflowed")?;
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            deletions = deletions
                .checked_add(1)
                .context("immutable diff deletion count overflowed")?;
        }
    }
    finish_diff_section(&mut observed, current_path, additions, deletions)?;

    let expected_counts = expected
        .iter()
        .map(|(path, (additions, deletions, _))| (path.clone(), (*additions, *deletions)))
        .collect::<BTreeMap<_, _>>();
    if observed != expected_counts {
        anyhow::bail!(
            "GitHub immutable compare diff per-file identities or line counts are incomplete"
        );
    }
    Ok(())
}

fn path_for_diff_header(
    header: &str,
    expected: &BTreeMap<String, (u64, u64, String)>,
) -> anyhow::Result<String> {
    let candidates = expected
        .keys()
        .filter(|path| {
            header.ends_with(&format!(" b/{path}")) || header.ends_with(&format!("\"b/{path}\""))
        })
        .cloned()
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => anyhow::bail!("GitHub immutable compare diff contains an unexpected file header"),
        _ => anyhow::bail!("GitHub immutable compare diff file header is ambiguous"),
    }
}

fn finish_diff_section(
    observed: &mut BTreeMap<String, (u64, u64)>,
    path: Option<String>,
    additions: u64,
    deletions: u64,
) -> anyhow::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if observed
        .insert(path.clone(), (additions, deletions))
        .is_some()
    {
        anyhow::bail!("GitHub immutable compare diff repeats file `{path}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_response(
        body: String,
    ) -> anyhow::Result<(String, std::sync::Arc<tokio::sync::Mutex<Vec<String>>>)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let received = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let received_server = std::sync::Arc::clone(&received);
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let Ok(read) = socket.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            received_server
                .lock()
                .await
                .push(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        Ok((format!("http://{address}"), received))
    }

    fn snapshot(additions: u64, deletions: u64) -> Value {
        json!({
            "base_oid": "base-oid",
            "head_oid": "head-oid",
            "changed_files_complete": true,
            "changed_files": [{
                "path": "src/lib.rs",
                "additions": additions,
                "deletions": deletions,
                "change_type": "MODIFIED",
            }],
        })
    }

    #[test]
    fn rejects_truncated_immutable_diff() {
        let error = build_complete_diff_facts(
            &snapshot(2, 1),
            "base-oid",
            "head-oid",
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+new\n".to_string(),
        )
        .expect_err("a diff missing one added line must fail closed");

        assert!(error.to_string().contains("incomplete"));
    }

    #[test]
    fn rejects_empty_diff_for_zero_count_changed_file() {
        let error =
            build_complete_diff_facts(&snapshot(0, 0), "base-oid", "head-oid", String::new())
                .expect_err("a changed binary or mode-only file still requires diff evidence");

        assert!(error.to_string().contains("empty diff"));
    }

    #[test]
    fn rejects_diff_that_omits_a_zero_count_binary_file() {
        let snapshot = json!({
            "changed_files": [
                {"path": "src/lib.rs", "additions": 1, "deletions": 1, "change_type": "MODIFIED"},
                {"path": "assets/logo.png", "additions": 0, "deletions": 0, "change_type": "MODIFIED"}
            ]
        });
        let error = build_complete_diff_facts(
            &snapshot,
            "base-oid",
            "head-oid",
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+new\n".to_string(),
        )
        .expect_err("every zero-count changed file must have its own immutable diff section");

        assert!(error.to_string().contains("per-file"));
    }

    #[tokio::test]
    async fn fetches_diff_from_immutable_base_and_head() -> anyhow::Result<()> {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n-old\n+new\n";
        let (api_base, received) = spawn_response(diff.to_string()).await?;
        let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?;

        let facts = fetch_complete_pr_diff_with_client(
            &reqwest::Client::new(),
            &target,
            None,
            &snapshot(1, 1),
            &api_base,
        )
        .await?;

        assert_eq!(facts["base_oid"], "base-oid");
        assert_eq!(facts["head_oid"], "head-oid");
        assert_eq!(facts["patches_complete"], true);
        let received = received.lock().await;
        assert_eq!(received.len(), 1);
        assert!(received[0].contains("/repos/owner/repo/compare/base-oid...head-oid"));
        assert!(received[0]
            .to_ascii_lowercase()
            .contains("accept: application/vnd.github.v3.diff"));
        Ok(())
    }
}
