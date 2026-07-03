use crate::run_id::{RunId, AGENT_RUN_PARENT_ENV};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const REGISTRY_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const REGISTRY_FILE_NAME: &str = "bindings.jsonl";
pub const ROTATED_REGISTRY_FILE_NAME: &str = "bindings.1.jsonl";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeBinding {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingRecord {
    pub v: u8,
    pub run_id: RunId,
    pub parent: Option<RunId>,
    pub native: NativeBinding,
    pub pid: u32,
    pub cwd: PathBuf,
    pub started_at: DateTime<Utc>,
    pub source: String,
}

impl BindingRecord {
    pub fn provisional(
        run_id: RunId,
        parent: Option<RunId>,
        native_kind: impl Into<String>,
        pid: u32,
        cwd: PathBuf,
        source: impl Into<String>,
    ) -> Self {
        Self {
            v: 1,
            run_id,
            parent,
            native: NativeBinding {
                kind: native_kind.into(),
                id: String::new(),
            },
            pid,
            cwd,
            started_at: Utc::now(),
            source: source.into(),
        }
    }
}

pub fn default_registry_path() -> PathBuf {
    let state_root = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("agent-run-state"));
    state_root.join("agent-run").join(REGISTRY_FILE_NAME)
}

pub fn append_binding(record: &BindingRecord) -> anyhow::Result<()> {
    append_binding_at(&default_registry_path(), record)
}

pub fn append_binding_at(path: &Path, record: &BindingRecord) -> anyhow::Result<()> {
    rotate_if_needed(path, REGISTRY_MAX_BYTES)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("registry path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut line = serde_json::to_vec(record)?;
    if line.len() >= 4096 {
        anyhow::bail!("binding record is too large: {} bytes", line.len());
    }
    line.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&line)?;
    Ok(())
}

pub fn append_binding_nonblocking(record: &BindingRecord) {
    if let Err(error) = append_binding(record) {
        tracing::error!(
            run_id = record.run_id.as_str(),
            native_kind = record.native.kind.as_str(),
            source = record.source.as_str(),
            parent_env = AGENT_RUN_PARENT_ENV,
            "failed to write agent run binding: {error}"
        );
    }
}

pub fn read_latest_bindings() -> anyhow::Result<Vec<BindingRecord>> {
    read_latest_bindings_at(&default_registry_path())
}

pub fn read_latest_bindings_at(path: &Path) -> anyhow::Result<Vec<BindingRecord>> {
    let mut latest: HashMap<(String, String), BindingRecord> = HashMap::new();
    for candidate in [rotated_path(path), path.to_path_buf()] {
        for record in read_records_from_file(&candidate)? {
            let key = (record.run_id.to_string(), record.native.id.clone());
            match latest.get(&key) {
                Some(existing) if existing.started_at >= record.started_at => {}
                _ => {
                    latest.insert(key, record);
                }
            }
        }
    }
    Ok(latest.into_values().collect())
}

fn read_records_from_file(path: &Path) -> anyhow::Result<Vec<BindingRecord>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(contents
        .lines()
        .filter_map(|line| serde_json::from_str::<BindingRecord>(line).ok())
        .collect())
}

fn rotate_if_needed(path: &Path, max_bytes: u64) -> anyhow::Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() < max_bytes {
        return Ok(());
    }
    let rotated = rotated_path(path);
    if rotated.exists() {
        fs::remove_file(&rotated)?;
    }
    fs::rename(path, rotated)?;
    Ok(())
}

fn rotated_path(path: &Path) -> PathBuf {
    path.with_file_name(ROTATED_REGISTRY_FILE_NAME)
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::run_id::RunId;
    use std::str::FromStr;

    fn record(run_id: &str, native_id: &str, started_at: DateTime<Utc>) -> BindingRecord {
        BindingRecord {
            v: 1,
            run_id: RunId::from_str(run_id).expect("valid run id"),
            parent: None,
            native: NativeBinding {
                kind: "codex".to_string(),
                id: native_id.to_string(),
            },
            pid: 123,
            cwd: PathBuf::from("/tmp/project"),
            started_at,
            source: "test".to_string(),
        }
    }

    #[test]
    fn registry_appends_and_reads_latest_binding() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent-run").join(REGISTRY_FILE_NAME);
        let old = record(
            "ar-01j1qb3c9r7v5m2k8x4tznq6wd",
            "native-1",
            "2026-07-03T01:00:00Z".parse()?,
        );
        let mut new = old.clone();
        new.pid = 456;
        new.started_at = "2026-07-03T02:00:00Z".parse()?;

        append_binding_at(&path, &old)?;
        append_binding_at(&path, &new)?;

        let bindings = read_latest_bindings_at(&path)?;
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].pid, 456);
        Ok(())
    }

    #[test]
    fn registry_reader_skips_malformed_lines() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent-run").join(REGISTRY_FILE_NAME);
        let valid = record(
            "ar-01j1qb3c9r7v5m2k8x4tznq6wd",
            "native-1",
            "2026-07-03T01:00:00Z".parse()?,
        );
        fs::create_dir_all(path.parent().expect("parent"))?;
        fs::write(
            &path,
            format!(
                "not-json\n{}\n{{\"v\":1,\"run_id\":\"bad\"}}\n",
                serde_json::to_string(&valid)?
            ),
        )?;

        let bindings = read_latest_bindings_at(&path)?;
        assert_eq!(bindings, vec![valid]);
        Ok(())
    }

    #[test]
    fn registry_rotates_at_ten_mb_and_keeps_one_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("agent-run").join(REGISTRY_FILE_NAME);
        fs::create_dir_all(path.parent().expect("parent"))?;
        fs::write(&path, vec![b'a'; REGISTRY_MAX_BYTES as usize])?;
        let valid = record(
            "ar-01j1qb3c9r7v5m2k8x4tznq6wd",
            "native-1",
            "2026-07-03T01:00:00Z".parse()?,
        );

        append_binding_at(&path, &valid)?;

        assert!(path.exists());
        assert!(path.with_file_name(ROTATED_REGISTRY_FILE_NAME).exists());
        let current = fs::read_to_string(&path)?;
        assert!(current.contains("native-1"));
        Ok(())
    }

    #[test]
    fn registry_write_failure_is_available_to_nonblocking_wrapper() {
        let invalid_record = BindingRecord {
            v: 1,
            run_id: RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wd").unwrap(),
            parent: None,
            native: NativeBinding {
                kind: "codex".to_string(),
                id: "x".repeat(5000),
            },
            pid: 1,
            cwd: PathBuf::from("/tmp/project"),
            started_at: Utc::now(),
            source: "test".to_string(),
        };

        append_binding_nonblocking(&invalid_record);
    }
}
