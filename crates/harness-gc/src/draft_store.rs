use harness_core::{Draft, DraftId};
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

    fn draft_path(&self, id: &DraftId) -> PathBuf {
        self.data_dir.join(format!("{}.json", id))
    }
}
