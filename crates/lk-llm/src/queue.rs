use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use lk_core::concept::ExtractedConcept;

use crate::{ExtractConceptsRequest, LlmClient, LlmError, SummarizeRequest, TaskTarget};

/// LlmClient that defers semantic work to a Claude Code skill. Buffers task records
/// in memory during `summarize`/`extract_concepts` calls; `flush` writes the entire
/// queue to `<queue_dir>/{run_id}.jsonl` atomically (temp + fsync + rename).
///
/// The buffer-then-rename design preserves an invariant `/lore-process` depends on:
/// **a queue file exists only when every task in it points at a page the pipeline
/// also wrote successfully**. A fatal mid-run error drops the in-memory buffer
/// without leaving a half-written JSONL or orphan tasks targeting pages that never
/// got written.
pub struct QueueLlmClient {
    queue_dir: PathBuf,
    run_id: String,
    counter: AtomicU64,
    buffer: tokio::sync::Mutex<Vec<QueueTask>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTask {
    pub task_id: String,
    pub kind: TaskKind,
    pub created_at: jiff::Timestamp,
    pub input: serde_json::Value,
    pub target: TaskTarget,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    Summarize,
    ExtractConcepts,
}

impl QueueLlmClient {
    pub fn new(queue_dir: PathBuf) -> Self {
        // Wall-clock second + PID separates distinct CLI invocations; a process-global
        // sequence additionally separates two clients constructed in the same process and
        // second, so their flushes never rename onto the same final path.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let run_id = format!(
            "{}-pid{}-{}",
            jiff::Zoned::now().strftime("%Y-%m-%dT%H-%M-%SZ"),
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        );
        Self {
            queue_dir,
            run_id,
            counter: AtomicU64::new(0),
            buffer: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn queue_path(&self) -> PathBuf {
        self.queue_dir.join(format!("{}.jsonl", self.run_id))
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}-{}-{:03}", self.run_id, n)
    }

    async fn enqueue(&self, task: QueueTask) {
        self.buffer.lock().await.push(task);
    }
}

#[async_trait]
impl LlmClient for QueueLlmClient {
    async fn summarize(&self, req: SummarizeRequest) -> Result<String, LlmError> {
        let task = QueueTask {
            task_id: self.next_id("sum"),
            kind: TaskKind::Summarize,
            created_at: jiff::Timestamp::now(),
            input: serde_json::json!({
                "text": req.text,
                "max_sentences": req.max_sentences,
            }),
            target: req.target,
        };
        self.enqueue(task).await;
        Ok(String::new())
    }

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError> {
        let task = QueueTask {
            task_id: self.next_id("ext"),
            kind: TaskKind::ExtractConcepts,
            created_at: jiff::Timestamp::now(),
            input: serde_json::json!({
                "text": req.text,
                "source_id": req.source_id,
                "date": req.date.to_string(),
            }),
            target: req.target,
        };
        self.enqueue(task).await;
        Ok(vec![])
    }

    async fn flush(&self) -> Result<(), LlmError> {
        let mut buffer = self.buffer.lock().await;
        if buffer.is_empty() {
            return Ok(());
        }

        tokio::fs::create_dir_all(&self.queue_dir)
            .await
            .map_err(|e| LlmError::QueueIo(format!("create dir: {e}")))?;

        let final_path = self.queue_path();
        let tmp_path = final_path.with_extension("jsonl.tmp");

        // Open the temp file for write (truncate if a previous flush attempt left one).
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .await
            .map_err(|e| LlmError::QueueIo(format!("open {}: {e}", tmp_path.display())))?;

        for task in buffer.iter() {
            let line = serde_json::to_string(task)
                .map_err(|e| LlmError::QueueIo(format!("serialize: {e}")))?;
            file.write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| LlmError::QueueIo(format!("write: {e}")))?;
        }
        file.flush()
            .await
            .map_err(|e| LlmError::QueueIo(format!("flush: {e}")))?;
        file.sync_all()
            .await
            .map_err(|e| LlmError::QueueIo(format!("fsync: {e}")))?;
        drop(file);

        if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
            // Best-effort cleanup so a future run doesn't see a stale `.jsonl.tmp`
            // accumulating in the queue dir. The rename error is the one the caller
            // needs to see — we deliberately discard the unlink error.
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(LlmError::QueueIo(format!(
                "rename {} → {}: {e}",
                tmp_path.display(),
                final_path.display()
            )));
        }

        buffer.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TargetKind;
    use tempfile::TempDir;

    #[tokio::test]
    async fn summarize_buffers_until_flush() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        let req = SummarizeRequest {
            text: "Some content".into(),
            max_sentences: 5,
            target: TaskTarget {
                vault_path: "daily/test/2026-05-23.md".into(),
                kind: TargetKind::DailySummary,
            },
        };
        let result = client.summarize(req).await.unwrap();
        assert!(result.is_empty(), "queue mode returns empty result");

        // Before flush: no file on disk.
        assert!(
            !client.queue_path().exists(),
            "queue file must not exist before flush"
        );

        client.flush().await.unwrap();

        let content = tokio::fs::read_to_string(client.queue_path())
            .await
            .unwrap();
        let task: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert!(matches!(task.kind, TaskKind::Summarize));
        assert_eq!(task.target.vault_path, "daily/test/2026-05-23.md");
        assert!(matches!(task.target.kind, TargetKind::DailySummary));
    }

    #[tokio::test]
    async fn multiple_tasks_one_file() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        for _ in 0..3 {
            let req = SummarizeRequest {
                text: "x".into(),
                max_sentences: 5,
                target: TaskTarget {
                    vault_path: "p".into(),
                    kind: TargetKind::DailySummary,
                },
            };
            client.summarize(req).await.unwrap();
        }
        client.flush().await.unwrap();

        let content = tokio::fs::read_to_string(client.queue_path())
            .await
            .unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[tokio::test]
    async fn extract_concepts_returns_empty_in_queue_mode() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        let req = ExtractConceptsRequest {
            text: "Anthropic releases Opus 4.7".into(),
            source_id: "ai-news".into(),
            date: jiff::civil::date(2026, 5, 23),
            target: TaskTarget {
                vault_path: "daily/ai-news/2026-05-23.md".into(),
                kind: TargetKind::DailyConcepts,
            },
        };
        let concepts = client.extract_concepts(req).await.unwrap();
        assert!(concepts.is_empty(), "queue mode emits task, returns empty");
        client.flush().await.unwrap();

        let content = tokio::fs::read_to_string(client.queue_path())
            .await
            .unwrap();
        let task: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert!(matches!(task.kind, TaskKind::ExtractConcepts));
    }

    #[tokio::test]
    async fn flush_without_tasks_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        client.flush().await.unwrap();

        assert!(
            !client.queue_path().exists(),
            "empty buffer must not produce a queue file"
        );
        let tmp = client.queue_path().with_extension("jsonl.tmp");
        assert!(!tmp.exists(), "temp file must not linger");
    }

    #[tokio::test]
    async fn abort_before_flush_persists_nothing() {
        // Simulates the recovery invariant: a fatal mid-run error must not leave a
        // queue file behind, since the corresponding pages were never written either.
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        let req = SummarizeRequest {
            text: "x".into(),
            max_sentences: 5,
            target: TaskTarget {
                vault_path: "p".into(),
                kind: TargetKind::DailySummary,
            },
        };
        client.summarize(req).await.unwrap();
        // No flush call — drop here.
        drop(client);

        let dir_entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            dir_entries.is_empty(),
            "buffered tasks must not touch the filesystem"
        );
    }

    #[tokio::test]
    async fn flush_rename_failure_leaves_no_tmp() {
        // Force the atomic rename to fail by occupying the final path with a directory,
        // then assert flush errors AND the temp file is cleaned up (no partial .jsonl,
        // no lingering .tmp).
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());
        client
            .summarize(SummarizeRequest {
                text: "x".into(),
                max_sentences: 5,
                target: TaskTarget {
                    vault_path: "p".into(),
                    kind: TargetKind::DailySummary,
                },
            })
            .await
            .unwrap();

        std::fs::create_dir_all(client.queue_path()).unwrap();

        let result = client.flush().await;
        assert!(result.is_err(), "rename onto a directory must fail");
        let tmp = client.queue_path().with_extension("jsonl.tmp");
        assert!(
            !tmp.exists(),
            "temp file must be cleaned up on rename failure"
        );
    }
}
