use anyhow::Context;
use harness_core::SessionId;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    pub fn current_session(&self) -> anyhow::Result<SessionId> {
        if let Some(id) = self.load_valid_session()? {
            return Ok(id);
        }

        let id = SessionId::new();
        std::fs::write(&self.session_file, id.as_str())
            .map_err(|error| {
                tracing::warn!(
                    path = %self.session_file.display(),
                    %error,
                    "failed to persist new session id",
                );
                error
            })
            .with_context(|| {
                format!(
                    "failed to persist session id to {}",
                    self.session_file.display()
                )
            })?;
        Ok(id)
    }

    fn load_valid_session(&self) -> anyhow::Result<Option<SessionId>> {
        let metadata = match std::fs::metadata(&self.session_file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                tracing::warn!(
                    path = %self.session_file.display(),
                    %error,
                    "failed to read session metadata",
                );
                return Err(error).with_context(|| {
                    format!(
                        "failed to read metadata for session file {}",
                        self.session_file.display()
                    )
                });
            }
        };

        let modified = metadata
            .modified()
            .map_err(|error| {
                tracing::warn!(
                    path = %self.session_file.display(),
                    %error,
                    "failed to read session modified time",
                );
                error
            })
            .with_context(|| {
                format!(
                    "failed to read modified time for session file {}",
                    self.session_file.display()
                )
            })?;

        let elapsed = match SystemTime::now().duration_since(modified) {
            Ok(elapsed) => elapsed,
            Err(_) => return Ok(None),
        };

        if elapsed >= Duration::from_secs(self.renewal_secs) {
            return Ok(None);
        }

        let id = std::fs::read_to_string(&self.session_file)
            .map_err(|error| {
                tracing::warn!(
                    path = %self.session_file.display(),
                    %error,
                    "failed to read existing session id",
                );
                error
            })
            .with_context(|| {
                format!(
                    "failed to read existing session id from {}",
                    self.session_file.display()
                )
            })?;
        let id = id.trim().to_string();
        if id.is_empty() {
            return Ok(None);
        }

        let file = std::fs::File::options()
            .write(true)
            .open(&self.session_file)
            .map_err(|error| {
                tracing::warn!(
                    path = %self.session_file.display(),
                    %error,
                    "failed to open session file for renewal",
                );
                error
            })
            .with_context(|| {
                format!(
                    "failed to open session file for renewal at {}",
                    self.session_file.display()
                )
            })?;
        file.set_modified(SystemTime::now())
            .map_err(|error| {
                tracing::warn!(
                    path = %self.session_file.display(),
                    %error,
                    "failed to refresh session file modified time",
                );
                error
            })
            .with_context(|| {
                format!(
                    "failed to refresh session file modified time at {}",
                    self.session_file.display()
                )
            })?;

        Ok(Some(SessionId(id)))
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

#[cfg(test)]
mod tests {
    use super::SessionManager;

    #[test]
    fn current_session_returns_error_when_persist_fails() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let manager = SessionManager::new(&dir.path().join("missing"), 60);

        let err = manager
            .current_session()
            .expect_err("missing parent directory should fail");

        assert!(
            err.to_string().contains("failed to persist session id"),
            "unexpected error: {err:#}",
        );
    }

    #[test]
    fn current_session_returns_error_when_existing_session_is_unreadable() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let session_path = dir.path().join(".session");
        std::fs::create_dir(&session_path).expect("create session directory");
        let manager = SessionManager::new(dir.path(), 60);

        let err = manager
            .current_session()
            .expect_err("directory-backed session file should fail read");

        assert!(
            err.to_string()
                .contains("failed to read existing session id"),
            "unexpected error: {err:#}",
        );
    }

    #[test]
    fn current_session_roundtrips_when_persist_succeeds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let manager = SessionManager::new(dir.path(), 60);

        let first = manager.current_session()?;
        let second = manager.current_session()?;

        assert_eq!(first.as_str(), second.as_str());
        Ok(())
    }
}
