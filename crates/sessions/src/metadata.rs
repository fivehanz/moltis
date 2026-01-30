use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A single session entry in the metadata index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    pub key: String,
    pub label: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: u32,
}

/// JSON file-backed index mapping session key → SessionEntry.
pub struct SessionMetadata {
    path: PathBuf,
    entries: HashMap<String, SessionEntry>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl SessionMetadata {
    /// Load metadata from disk, or create an empty index.
    pub fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            let data = fs::read_to_string(&path)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            HashMap::new()
        };
        Ok(Self { path, entries })
    }

    /// Persist metadata to disk.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&self.entries)?;
        fs::write(&self.path, data)?;
        Ok(())
    }

    /// Get an entry by key.
    pub fn get(&self, key: &str) -> Option<&SessionEntry> {
        self.entries.get(key)
    }

    /// Insert or update an entry. If key doesn't exist, creates a new entry.
    pub fn upsert(&mut self, key: &str, label: Option<String>) -> &SessionEntry {
        let now = now_ms();
        self.entries
            .entry(key.to_string())
            .and_modify(|e| {
                if label.is_some() {
                    e.label = label.clone();
                }
                e.updated_at = now;
            })
            .or_insert_with(|| SessionEntry {
                id: uuid::Uuid::new_v4().to_string(),
                key: key.to_string(),
                label,
                created_at: now,
                updated_at: now,
                message_count: 0,
            })
    }

    /// Update message count and updated_at timestamp.
    pub fn touch(&mut self, key: &str, message_count: u32) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.message_count = message_count;
            entry.updated_at = now_ms();
        }
    }

    /// Remove an entry by key. Returns the removed entry if found.
    pub fn remove(&mut self, key: &str) -> Option<SessionEntry> {
        self.entries.remove(key)
    }

    /// List all entries sorted by updated_at descending.
    pub fn list(&self) -> Vec<SessionEntry> {
        let mut entries: Vec<_> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upsert_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        let mut meta = SessionMetadata::load(path.clone()).unwrap();

        meta.upsert("main", None);
        meta.upsert("session:abc", Some("My Chat".to_string()));

        let list = meta.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].key, "session:abc"); // more recent
    }

    #[test]
    fn test_save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");

        {
            let mut meta = SessionMetadata::load(path.clone()).unwrap();
            meta.upsert("main", Some("Main".to_string()));
            meta.save().unwrap();
        }

        let meta = SessionMetadata::load(path).unwrap();
        let entry = meta.get("main").unwrap();
        assert_eq!(entry.label.as_deref(), Some("Main"));
    }

    #[test]
    fn test_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        let mut meta = SessionMetadata::load(path).unwrap();

        meta.upsert("main", None);
        assert!(meta.get("main").is_some());
        meta.remove("main");
        assert!(meta.get("main").is_none());
    }

    #[test]
    fn test_touch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        let mut meta = SessionMetadata::load(path).unwrap();

        meta.upsert("main", None);
        meta.touch("main", 5);
        assert_eq!(meta.get("main").unwrap().message_count, 5);
    }
}
