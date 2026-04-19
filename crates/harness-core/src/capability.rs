use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

/// Scoped write capability for a single subtask in parallel dispatch.
///
/// A token is minted at dispatch time, carries the worktree paths the subtask
/// is permitted to write, and expires after `ttl`. The agent checks expiry
/// before spawning; allowed paths inform the sandbox write policy.
///
/// Paths are canonicalized at creation time to avoid TOCTOU races in
/// subsequent `permits_write` checks.
#[derive(Debug, Clone)]
pub struct CapabilityToken {
    pub token_id: Uuid,
    pub subtask_index: usize,
    /// Absolute, canonicalized paths this subtask may write.
    pub allowed_write_paths: Vec<PathBuf>,
    pub issued_at: SystemTime,
    pub expires_at: SystemTime,
}

impl CapabilityToken {
    pub fn new(subtask_index: usize, paths: Vec<PathBuf>, ttl: Duration) -> Self {
        let now = SystemTime::now();
        let allowed_write_paths = paths
            .into_iter()
            .map(|p| p.canonicalize().unwrap_or(p))
            .collect();
        Self {
            token_id: Uuid::new_v4(),
            subtask_index,
            allowed_write_paths,
            issued_at: now,
            expires_at: now + ttl,
        }
    }

    pub fn is_expired(&self) -> bool {
        SystemTime::now() > self.expires_at
    }

    /// Returns true when `path` is inside one of the allowed write paths.
    pub fn permits_write(&self, path: &Path) -> bool {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.allowed_write_paths
            .iter()
            .any(|allowed| canonical.starts_with(allowed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_not_expired_when_fresh() {
        let token = CapabilityToken::new(0, vec![], Duration::from_secs(3600));
        assert!(!token.is_expired());
    }

    #[test]
    fn token_expired_after_ttl() {
        let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let token = CapabilityToken {
            token_id: Uuid::new_v4(),
            subtask_index: 0,
            allowed_write_paths: vec![],
            issued_at: past,
            expires_at: past + Duration::from_secs(60),
        };
        assert!(token.is_expired());
    }

    #[test]
    fn permits_write_inside_path() {
        let base = PathBuf::from("/tmp/harness-test-wt0");
        let token = CapabilityToken {
            token_id: Uuid::new_v4(),
            subtask_index: 0,
            allowed_write_paths: vec![base.clone()],
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        };
        assert!(token.permits_write(&base));
        assert!(token.permits_write(&base.join("src").join("main.rs")));
    }

    #[test]
    fn permits_write_outside_path() {
        let base = PathBuf::from("/tmp/harness-test-wt0");
        let sibling = PathBuf::from("/tmp/harness-test-wt1");
        let token = CapabilityToken {
            token_id: Uuid::new_v4(),
            subtask_index: 0,
            allowed_write_paths: vec![base],
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        };
        assert!(!token.permits_write(&sibling));
        assert!(!token.permits_write(&sibling.join("src").join("main.rs")));
    }
}
