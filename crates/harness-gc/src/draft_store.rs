use chrono::Utc;
use harness_core::{Draft, DraftId, DraftStatus};
use std::path::{Path, PathBuf};

pub struct DraftStore {
    data_dir: PathBuf,
}

impl DraftStore {
    pub fn new(data_dir: &Path) -> anyhow::Result<Self> {
        let dir = data_dir.join("drafts");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { data_dir: dir })
    }

    pub fn save(&self, draft: &Draft) -> anyhow::Result<()> {
        let path = self.draft_path(&draft.id);
        let content = serde_json::to_string_pretty(draft)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    pub fn get(&self, id: &DraftId) -> anyhow::Result<Option<Draft>> {
        let path = self.draft_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        let draft: Draft = serde_json::from_str(&content)?;
        Ok(Some(draft))
    }

    pub fn list(&self) -> anyhow::Result<Vec<Draft>> {
        let mut drafts = Vec::new();
        if !self.data_dir.exists() {
            return Ok(drafts);
        }
        for entry in std::fs::read_dir(&self.data_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(draft) = serde_json::from_str::<Draft>(&content) {
                        drafts.push(draft);
                    }
                }
            }
        }
        Ok(drafts)
    }

    pub fn delete(&self, id: &DraftId) -> anyhow::Result<()> {
        let path = self.draft_path(id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Expire all `Pending` drafts older than `ttl_hours` hours.
    ///
    /// Each expired draft is removed from disk. Returns the number of drafts expired.
    pub fn expire_stale_drafts(&self, ttl_hours: u64) -> anyhow::Result<usize> {
        let now = Utc::now();
        let ttl = chrono::Duration::hours(ttl_hours as i64);
        let mut expired = 0;

        for mut draft in self.list()? {
            if draft.status == DraftStatus::Pending && (now - draft.generated_at) > ttl {
                draft.status = DraftStatus::Expired;
                self.delete(&draft.id)?;
                expired += 1;
            }
        }

        Ok(expired)
    }

    fn draft_path(&self, id: &DraftId) -> PathBuf {
        self.data_dir.join(format!("{}.json", id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType, Signal,
        SignalType,
    };

    fn make_draft(status: DraftStatus, generated_at: chrono::DateTime<Utc>) -> Draft {
        let signal = Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({}),
            RemediationType::Guard,
        );
        Draft {
            id: DraftId::new(),
            status,
            signal,
            artifacts: vec![Artifact {
                artifact_type: ArtifactType::Guard,
                target_path: std::path::PathBuf::from("test.md"),
                content: "content".into(),
            }],
            rationale: "test".into(),
            validation: "test".into(),
            generated_at,
            agent_model: "test".into(),
        }
    }

    #[test]
    fn expire_stale_drafts_removes_old_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = DraftStore::new(dir.path()).unwrap();

        // Draft older than 72 hours
        let old_draft = make_draft(
            DraftStatus::Pending,
            Utc::now() - chrono::Duration::hours(100),
        );
        store.save(&old_draft).unwrap();

        let expired = store.expire_stale_drafts(72).unwrap();
        assert_eq!(expired, 1);
        assert!(store.get(&old_draft.id).unwrap().is_none());
    }

    #[test]
    fn expire_stale_drafts_keeps_recent_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = DraftStore::new(dir.path()).unwrap();

        // Draft only 1 hour old
        let recent_draft = make_draft(
            DraftStatus::Pending,
            Utc::now() - chrono::Duration::hours(1),
        );
        store.save(&recent_draft).unwrap();

        let expired = store.expire_stale_drafts(72).unwrap();
        assert_eq!(expired, 0);
        assert!(store.get(&recent_draft.id).unwrap().is_some());
    }

    #[test]
    fn expire_stale_drafts_skips_non_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = DraftStore::new(dir.path()).unwrap();

        // Adopted draft older than TTL — should NOT be removed
        let adopted = make_draft(
            DraftStatus::Adopted,
            Utc::now() - chrono::Duration::hours(100),
        );
        store.save(&adopted).unwrap();

        let expired = store.expire_stale_drafts(72).unwrap();
        assert_eq!(expired, 0);
        assert!(store.get(&adopted.id).unwrap().is_some());
    }
}
