use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};

use anyhow::Result;
use fd_lock::RwLock;

/// Append-only JSONL session storage with file locking.
pub struct SessionStore {
    pub base_dir: PathBuf,
}

impl SessionStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Sanitize a session key for use as a filename.
    fn key_to_filename(key: &str) -> String {
        key.replace(':', "_")
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.base_dir.join(format!("{}.jsonl", Self::key_to_filename(key)))
    }

    /// Append a message (JSON value) as a single line to the session file.
    pub async fn append(&self, key: &str, message: &serde_json::Value) -> Result<()> {
        let path = self.path_for(key);
        let line = serde_json::to_string(message)?;

        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            let mut lock = RwLock::new(file);
            let mut guard = lock.try_write().map_err(|e| anyhow::anyhow!("lock failed: {e}"))?;
            writeln!(*guard, "{line}")?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Read all messages from a session file.
    pub async fn read(&self, key: &str) -> Result<Vec<serde_json::Value>> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
            if !path.exists() {
                return Ok(vec![]);
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut messages = Vec::new();
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str(trimmed) {
                    Ok(val) => messages.push(val),
                    Err(e) => {
                        tracing::warn!("skipping malformed JSONL line: {e}");
                    }
                }
            }
            Ok(messages)
        })
        .await?
    }

    /// Read the last N messages from a session file.
    pub async fn read_last_n(&self, key: &str, n: usize) -> Result<Vec<serde_json::Value>> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
            if !path.exists() {
                return Ok(vec![]);
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let mut all: Vec<serde_json::Value> = Vec::new();
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str(trimmed) {
                    all.push(val);
                }
            }
            let start = all.len().saturating_sub(n);
            Ok(all[start..].to_vec())
        })
        .await?
    }

    /// Delete the session file.
    pub async fn clear(&self, key: &str) -> Result<()> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<()> {
            if path.exists() {
                fs::remove_file(&path)?;
            }
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Count messages in a session file without parsing them.
    pub async fn count(&self, key: &str) -> Result<u32> {
        let path = self.path_for(key);

        tokio::task::spawn_blocking(move || -> Result<u32> {
            if !path.exists() {
                return Ok(0);
            }
            let file = File::open(&path)?;
            let reader = BufReader::new(file);
            let count = reader
                .lines()
                .filter_map(|l| l.ok())
                .filter(|l| !l.trim().is_empty())
                .count();
            Ok(count as u32)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[tokio::test]
    async fn test_append_and_read() {
        let (store, _dir) = temp_store();

        store.append("main", &json!({"role": "user", "content": "hello"})).await.unwrap();
        store.append("main", &json!({"role": "assistant", "content": "hi"})).await.unwrap();

        let msgs = store.read("main").await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[tokio::test]
    async fn test_read_empty() {
        let (store, _dir) = temp_store();
        let msgs = store.read("nonexistent").await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_read_last_n() {
        let (store, _dir) = temp_store();

        for i in 0..10 {
            store.append("test", &json!({"i": i})).await.unwrap();
        }

        let last3 = store.read_last_n("test", 3).await.unwrap();
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0]["i"], 7);
        assert_eq!(last3[2]["i"], 9);
    }

    #[tokio::test]
    async fn test_clear() {
        let (store, _dir) = temp_store();

        store.append("main", &json!({"role": "user", "content": "hello"})).await.unwrap();
        assert_eq!(store.read("main").await.unwrap().len(), 1);

        store.clear("main").await.unwrap();
        assert!(store.read("main").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_count() {
        let (store, _dir) = temp_store();

        assert_eq!(store.count("main").await.unwrap(), 0);
        store.append("main", &json!({"role": "user"})).await.unwrap();
        store.append("main", &json!({"role": "assistant"})).await.unwrap();
        assert_eq!(store.count("main").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_key_sanitization() {
        let (store, _dir) = temp_store();

        store.append("session:abc-123", &json!({"role": "user"})).await.unwrap();
        let msgs = store.read("session:abc-123").await.unwrap();
        assert_eq!(msgs.len(), 1);
    }
}
