//! Trial log: one JSON line per call, for the two-week adoption trial.
//!
//! Fields are chosen to answer the trial's questions without reading
//! transcripts: did it work, did it ever claim success it did not have, did
//! it leave processes behind, how long did it take. Prompts and output are
//! never written.

use agent_runtime::core::{CloseReport, TurnEnd, TurnOutcome};
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `<data dir>/harness/agent-runtime-trial.jsonl`.
pub fn default_trial_log() -> PathBuf {
    harness_core::config::dirs::dirs_data_dir()
        .join("harness")
        .join("agent-runtime-trial.jsonl")
}

#[derive(Debug, Default, Serialize)]
pub struct TrialRecord {
    pub ts_ms: u128,
    pub harness_agent: String,
    pub runtime_agent: String,
    pub channel: Option<String>,
    pub mode: Option<String>,
    pub model: Option<String>,
    /// `completed`, `failed`, `cancelled`, `unknown`, ... or `none` when no turn ended.
    pub outcome: String,
    pub failure_class: Option<String>,
    pub saw_output: Option<bool>,
    /// What harness was told: `true` only when the call returned `Ok`.
    pub reported_success: bool,
    /// `reported_success` without a `completed` outcome, or `completed` with
    /// no output at all. Should always be `false`; any `true` is a trial failure.
    pub suspect_false_success: bool,
    pub leftovers: Option<usize>,
    pub close_graceful: Option<bool>,
    /// Provider-reported cost of the call, when the channel reports one.
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub approvals_asked: u32,
    pub approvals_granted: u32,
    pub duration_ms: u128,
    pub error: Option<String>,
    pub harness_version: &'static str,
}

impl TrialRecord {
    pub fn start(
        harness_agent: &str,
        runtime_agent: &str,
        channel: Option<String>,
        mode: Option<String>,
        model: Option<String>,
    ) -> Self {
        Self {
            ts_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default(),
            harness_agent: harness_agent.to_string(),
            runtime_agent: runtime_agent.to_string(),
            channel,
            mode,
            model,
            outcome: "none".into(),
            harness_version: env!("CARGO_PKG_VERSION"),
            ..Self::default()
        }
    }

    pub fn approval(&mut self, granted: bool) {
        self.approvals_asked += 1;
        if granted {
            self.approvals_granted += 1;
        }
    }

    pub fn outcome(&mut self, end: &TurnEnd) {
        self.saw_output = Some(end.saw_output);
        let value = serde_json::to_value(&end.outcome).unwrap_or_default();
        self.outcome = value["outcome"].as_str().unwrap_or("unknown").to_string();
        if let TurnOutcome::Failed { failure } = &end.outcome {
            self.failure_class = serde_json::to_value(failure)
                .ok()
                .and_then(|v| v["class"].as_str().map(str::to_string));
        }
    }

    pub fn usage(&mut self, input: u64, output: u64, cost: Option<f64>) {
        self.input_tokens = Some(input);
        self.output_tokens = Some(output);
        if cost.is_some() {
            self.cost_usd = cost;
        }
    }

    pub fn closed(&mut self, report: &CloseReport) {
        self.leftovers = Some(report.leftovers.len());
        self.close_graceful = Some(report.graceful);
    }

    pub fn close_error(&mut self, error: &str) {
        self.error.get_or_insert_with(|| format!("close: {error}"));
    }

    pub fn finish<T>(&mut self, elapsed: Duration, result: &harness_core::error::Result<T>) {
        self.duration_ms = elapsed.as_millis();
        self.reported_success = result.is_ok();
        if let Err(error) = result {
            self.error = Some(error.to_string().chars().take(300).collect());
        }
        self.suspect_false_success = self.reported_success
            && (self.outcome != "completed" || self.saw_output == Some(false));
    }

    /// Best effort: a trial log that cannot be written must not fail the task.
    pub fn append(&self, path: &Path) {
        let Ok(line) = serde_json::to_string(self) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{line}");
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "agent-runtime trial log not written")
            }
        }
    }
}
