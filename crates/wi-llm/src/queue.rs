use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use wi_core::concept::ExtractedConcept;

use crate::{
    ClassifyLabelsRequest, ExtractConceptsRequest, LlmClient, LlmError, SummarizeRequest,
    TaskTarget,
};

/// LlmClient that defers semantic work to a Claude Code skill by appending JSONL
/// task records to `<queue_dir>/{run_id}.jsonl`. All semantic methods return empty
/// results — Pipeline templates handle the empty case, and `/wi-process` later edits
/// the target pages via Obsidian MCP.
pub struct QueueLlmClient {
    queue_dir: PathBuf,
    run_id: String,
    counter: AtomicU64,
}

#[derive(Debug, Serialize, Deserialize)]
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
        // Combine second-resolution wall time + process id so two CLI invocations that
        // start in the same second land in different queue files. Each Rust process is
        // the sole writer of its file — no inter-process append locking required.
        let run_id = format!(
            "{}-pid{}",
            jiff::Zoned::now().strftime("%Y-%m-%dT%H-%M-%SZ"),
            std::process::id()
        );
        Self {
            queue_dir,
            run_id,
            counter: AtomicU64::new(0),
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

    async fn append(&self, task: &QueueTask) -> Result<(), LlmError> {
        tokio::fs::create_dir_all(&self.queue_dir)
            .await
            .map_err(|e| LlmError::QueueIo(format!("create dir: {e}")))?;
        let line = serde_json::to_string(task)
            .map_err(|e| LlmError::QueueIo(format!("serialize: {e}")))?;

        let path = self.queue_path();
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| LlmError::QueueIo(format!("open {}: {e}", path.display())))?;
        file.write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| LlmError::QueueIo(format!("write: {e}")))?;
        Ok(())
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
        self.append(&task).await?;
        Ok(String::new())
    }

    async fn classify_labels(&self, _req: ClassifyLabelsRequest) -> Result<Vec<String>, LlmError> {
        // Classification feeds in-memory event labels, not a vault page. Queue mode
        // currently delegates only page-producing tasks; classification is left to
        // Rust's keyword-based `classify_by_keywords` stage.
        Ok(vec![])
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
        self.append(&task).await?;
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TargetKind;
    use tempfile::TempDir;

    #[tokio::test]
    async fn summarize_appends_jsonl_task() {
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

        let content = tokio::fs::read_to_string(client.queue_path())
            .await
            .unwrap();
        let task: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert!(matches!(task.kind, TaskKind::ExtractConcepts));
    }
}
