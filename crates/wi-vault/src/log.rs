use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::VaultError;

pub struct IngestLog {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: jiff::Timestamp,
    pub source_id: String,
    pub status: LogStatus,
    pub events_count: usize,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStatus {
    Success,
    Partial,
    Failed,
    Skipped,
}

impl IngestLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub async fn record(&self, entry: &LogEntry) -> Result<(), VaultError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let line =
            serde_json::to_string(entry).map_err(|e| VaultError::Serialization(e.to_string()))?;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        file.write_all(format!("{line}\n").as_bytes()).await?;
        Ok(())
    }

    pub async fn last_success(&self, source_id: &str) -> Result<Option<LogEntry>, VaultError> {
        let content = match tokio::fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let entry = content
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<LogEntry>(line).ok())
            .find(|e| e.source_id == source_id && e.status == LogStatus::Success);

        Ok(entry)
    }

    pub async fn all_entries(&self) -> Result<Vec<LogEntry>, VaultError> {
        let content = match tokio::fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };

        Ok(content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }
}
