use harness_core::SessionId;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, Duration};

pub struct SessionManager {
    session_file: PathBuf,
    renewal_secs: u64,
}

impl SessionManager {
    pub fn new(data_dir: &Path, renewal_secs: u64) -> Self {
        Self {
            session_file: data_dir.join(".session"),
            renewal_secs,
        }
    }

    /// Get or create the current session ID. Renews if within renewal window.
    pub fn current_session(&self) -> SessionId {
        if let Ok(metadata) = std::fs::metadata(&self.session_file) {
            if let Ok(modified) = metadata.modified() {
                if let Ok(elapsed) = SystemTime::now().duration_since(modified) {
                    if elapsed < Duration::from_secs(self.renewal_secs) {
                        // Still valid, read existing ID
                        if let Ok(id) = std::fs::read_to_string(&self.session_file) {
                            let id = id.trim().to_string();
                            if !id.is_empty() {
                                // Touch file to renew
                                let _ = std::fs::File::options()
                                    .write(true)
                                    .open(&self.session_file)
                                    .and_then(|f| f.set_modified(SystemTime::now()));
                                return SessionId(id);
                            }
                        }
                    }
                }
            }
        }

        // Create new session
        let id = SessionId::new();
        let _ = std::fs::write(&self.session_file, id.as_str());
        id
    }

    pub fn renew(&self) -> anyhow::Result<()> {
        if self.session_file.exists() {
            let file = std::fs::File::options()
                .write(true)
                .open(&self.session_file)?;
            file.set_modified(SystemTime::now())?;
        }
        Ok(())
    }
}
