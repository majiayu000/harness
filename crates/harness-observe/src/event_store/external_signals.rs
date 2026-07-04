use super::EventStore;
use chrono::{DateTime, Utc};
use harness_core::types::{ExternalSignal, ExternalSignalId};
use std::io::{BufRead, Write};

impl EventStore {
    fn signals_file(&self) -> std::path::PathBuf {
        self.data_dir.join("signals.jsonl")
    }

    pub fn log_external_signal(&self, signal: &ExternalSignal) -> anyhow::Result<ExternalSignalId> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.signals_file())?;
        let line = serde_json::to_string(signal)?;
        writeln!(file, "{line}")?;
        Ok(signal.id.clone())
    }

    pub fn query_external_signals(
        &self,
        since: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<ExternalSignal>> {
        let path = self.signals_file();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let mut signals = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(sig) = serde_json::from_str::<ExternalSignal>(&line) {
                if let Some(since) = since {
                    if sig.received_at < since {
                        continue;
                    }
                }
                signals.push(sig);
            }
        }
        Ok(signals)
    }
}
