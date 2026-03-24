use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write as IoWrite;
use std::path::Path;

/// User verdict on a single guard warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// True positive — genuine violation the guard should catch.
    Tp,
    /// False positive — spurious warning, should not have fired.
    Fp,
    /// Acceptable — intentional pattern; not a defect, but not a guard error.
    Acceptable,
}

/// One feedback record appended to `triage.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageRecord {
    pub ts: DateTime<Utc>,
    pub rule_id: String,
    pub file: String,
    pub line: Option<usize>,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl TriageRecord {
    pub fn new(rule_id: impl Into<String>, file: impl Into<String>, verdict: Verdict) -> Self {
        Self {
            ts: Utc::now(),
            rule_id: rule_id.into(),
            file: file.into(),
            line: None,
            verdict,
            note: String::new(),
        }
    }
}

/// Append a single triage record to the JSONL file, creating it if absent.
pub fn append_record(path: &Path, record: &TriageRecord) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(record)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Read all triage records from the JSONL file.
///
/// Lines that fail to parse are skipped with a warning.
pub fn read_records(path: &Path) -> anyhow::Result<Vec<TriageRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TriageRecord>(line) {
            Ok(r) => records.push(r),
            Err(e) => tracing::warn!(line = i + 1, "triage.jsonl parse error: {e}"),
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn append_and_read_roundtrip() -> anyhow::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path();

        let mut r1 = TriageRecord::new("RS-03", "src/main.rs", Verdict::Tp);
        r1.line = Some(42);
        r1.note = "genuine panic risk".to_string();

        let r2 = TriageRecord::new("SEC-01", "src/db.rs", Verdict::Fp);

        append_record(path, &r1)?;
        append_record(path, &r2)?;

        let records = read_records(path)?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].rule_id, "RS-03");
        assert_eq!(records[0].verdict, Verdict::Tp);
        assert_eq!(records[0].line, Some(42));
        assert_eq!(records[1].rule_id, "SEC-01");
        assert_eq!(records[1].verdict, Verdict::Fp);
        Ok(())
    }

    #[test]
    fn read_missing_file_returns_empty() -> anyhow::Result<()> {
        let records = read_records(Path::new("/nonexistent/triage.jsonl"))?;
        assert!(records.is_empty());
        Ok(())
    }

    #[test]
    fn read_skips_malformed_lines() -> anyhow::Result<()> {
        let tmp = NamedTempFile::new()?;
        let path = tmp.path();
        std::fs::write(path, "not json\n{\"ts\":\"2026-01-01T00:00:00Z\",\"rule_id\":\"RS-03\",\"file\":\"f\",\"verdict\":\"tp\"}\n")?;
        let records = read_records(path)?;
        assert_eq!(records.len(), 1);
        Ok(())
    }

    #[test]
    fn verdict_serde_snake_case() -> anyhow::Result<()> {
        assert_eq!(serde_json::to_string(&Verdict::Tp)?, "\"tp\"");
        assert_eq!(serde_json::to_string(&Verdict::Fp)?, "\"fp\"");
        assert_eq!(
            serde_json::to_string(&Verdict::Acceptable)?,
            "\"acceptable\""
        );
        Ok(())
    }
}
