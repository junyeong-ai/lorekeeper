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
    pub event_count: usize,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStatus {
    Success,
    Failed,
    Skipped,
}

impl LogStatus {
    /// Whether this entry records a source whose window was actually OBSERVED. `Skipped`
    /// is written at exactly one place — a fetch that succeeded and yielded no pages — so
    /// it reports an answer ("nothing happened"), not an absence of one. Only `Failed`
    /// leaves the window unobserved.
    ///
    /// The distinction is what keeps freshness reporting honest: a quiet source (a Jira
    /// filter matching nothing today) is collected on schedule and must not read as
    /// overdue, or the warning fires forever and trains its reader to ignore a real
    /// outage. Exhaustive on purpose — a new status has to answer this.
    pub fn is_collected(self) -> bool {
        match self {
            LogStatus::Success | LogStatus::Skipped => true,
            LogStatus::Failed => false,
        }
    }
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
        // `tokio::fs::File` buffers, and dropping one only schedules a best-effort
        // background flush — so without this the bytes may not have reached the OS when
        // `record` returns, and the next reader sees a log missing its newest entry. That
        // entry is not history: `find_last_collection` reads it as the state `lore health`
        // reports, so losing it makes a live source read stale. It surfaced as a test that
        // wrote and immediately read passing on one platform and failing on another, which
        // is what an unflushed buffer looks like from the outside.
        //
        // Only `flush`, not `sync_all`: reaching the OS is what makes the entry visible to
        // every reader, and the log is append-only, so a machine crash losing the last
        // entry self-heals on the next run rather than corrupting anything.
        file.flush().await?;
        Ok(())
    }

    /// The most recent entry in which `source_id` was collected — see
    /// [`LogStatus::is_collected`] for why an empty collection counts and a failure
    /// does not. `None` means the source has genuinely never been collected.
    pub async fn find_last_collection(
        &self,
        source_id: &str,
    ) -> Result<Option<LogEntry>, VaultError> {
        let content = match tokio::fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        for line in content.lines().rev() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LogEntry>(line) {
                Ok(e) if e.source_id == source_id && e.status.is_collected() => {
                    return Ok(Some(e));
                }
                Ok(_) => {}
                // A corrupt line is observable, not silently treated as "no history".
                Err(e) => tracing::warn!(error = %e, "skipping malformed ingest-log line"),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source_id: &str, status: LogStatus, secs: i64) -> LogEntry {
        LogEntry {
            timestamp: jiff::Timestamp::from_second(secs).unwrap(),
            source_id: source_id.into(),
            status,
            event_count: 0,
            duration_ms: 1,
            error: None,
        }
    }

    async fn log_with(entries: &[LogEntry]) -> (tempfile::TempDir, IngestLog) {
        let dir = tempfile::TempDir::new().unwrap();
        let log = IngestLog::new(dir.path().join("ingest.jsonl"));
        for e in entries {
            log.record(e).await.unwrap();
        }
        (dir, log)
    }

    /// A source whose window was fetched and found empty was COLLECTED. Reading it as
    /// "never since the last non-empty day" is what made `lore health` report a quiet
    /// Jira filter STALE for 49 days while it ran correctly every morning — a warning
    /// that fires forever is one its reader stops seeing.
    #[tokio::test]
    async fn an_empty_collection_is_still_a_collection() {
        let (_dir, log) = log_with(&[
            entry("jira", LogStatus::Success, 1_000),
            entry("jira", LogStatus::Skipped, 2_000),
        ])
        .await;
        let found = log.find_last_collection("jira").await.unwrap().unwrap();
        assert_eq!(
            found.timestamp.as_second(),
            2_000,
            "the empty run is the source's last collection"
        );
    }

    /// A failure leaves the window unobserved, so it must not refresh the answer.
    #[tokio::test]
    async fn a_failure_does_not_count_as_a_collection() {
        let (_dir, log) = log_with(&[
            entry("jira", LogStatus::Success, 1_000),
            entry("jira", LogStatus::Failed, 2_000),
        ])
        .await;
        let found = log.find_last_collection("jira").await.unwrap().unwrap();
        assert_eq!(found.timestamp.as_second(), 1_000);
    }

    #[tokio::test]
    async fn entries_are_scoped_to_their_source() {
        let (_dir, log) = log_with(&[
            entry("jira", LogStatus::Success, 1_000),
            entry("gmail", LogStatus::Skipped, 2_000),
        ])
        .await;
        assert_eq!(
            log.find_last_collection("jira")
                .await
                .unwrap()
                .unwrap()
                .timestamp
                .as_second(),
            1_000
        );
        assert!(log.find_last_collection("slack").await.unwrap().is_none());
    }

    /// A missing log is "never ingested", never an error — the first run has no log yet.
    #[tokio::test]
    async fn a_missing_log_reads_as_no_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = IngestLog::new(dir.path().join("nope.jsonl"));
        assert!(log.find_last_collection("jira").await.unwrap().is_none());
    }

    /// An entry must be on disk when `record` returns. `tokio::fs::File` buffers and its
    /// drop only schedules a best-effort flush, so without an explicit one the newest entry
    /// can be invisible to the next reader — and that entry is the state `lore health`
    /// reports, not history, so losing it makes a live source read stale.
    ///
    /// Losing the race is what the defect looks like from outside, so a single round observes
    /// it only sometimes: on macOS the unflushed write usually wins anyway, which is why this
    /// first surfaced as a Linux-only CI failure and why one round caught it once in thirty
    /// tries here. A thousand caught it every time, for under half a second. Repetition can
    /// never make this fail against correct code — every round asserts a property that holds
    /// unconditionally once the write is flushed — so it only ever buys back the regressions
    /// a single round would have waved through.
    #[tokio::test]
    async fn a_recorded_entry_is_readable_the_moment_record_returns() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ingest.jsonl");
        let log = IngestLog::new(path.clone());
        for round in 1..=1_000 {
            log.record(&entry("jira", LogStatus::Success, round))
                .await
                .unwrap();
            // Read through the filesystem rather than through `log`, which is the position
            // every other process — and every other handle — is in.
            let written = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                written.lines().count(),
                round as usize,
                "round {round}: the entry must have reached the OS, not just tokio's buffer"
            );
        }
    }

    /// Corruption stays observable without blanking the history behind it.
    #[tokio::test]
    async fn a_malformed_line_is_skipped_not_treated_as_no_history() {
        let (dir, log) = log_with(&[entry("jira", LogStatus::Success, 1_000)]).await;
        let path = dir.path().join("ingest.jsonl");
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("{not json\n");
        std::fs::write(&path, content).unwrap();
        assert_eq!(
            log.find_last_collection("jira")
                .await
                .unwrap()
                .unwrap()
                .timestamp
                .as_second(),
            1_000
        );
    }
}
