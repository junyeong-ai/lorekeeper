use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use lk_core::concept::ExtractedConcept;

use crate::{
    ExtractConceptsRequest, LlmClient, LlmError, SummarizeRequest, TaskTarget, Theme, ThemeRequest,
};

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
    buffer: tokio::sync::Mutex<Buffer>,
}

/// Buffered tasks plus the rollback savepoint, guarded by one lock so the savepoint is
/// always a valid index into the very task list it bounds — the rollback invariant holds
/// by construction rather than by a sequential-use convention.
#[derive(Default)]
struct Buffer {
    tasks: Vec<QueueTask>,
    /// Task count captured at the last `begin_source`; `rollback_source` truncates back to
    /// it to drop a failed source's tasks while keeping every earlier source's.
    source_mark: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTask {
    pub task_id: String,
    pub kind: TaskKind,
    pub created_at: jiff::Timestamp,
    /// BLAKE3-128 of the request's `cache_identity()` — see `cache_hash`.
    /// Identical to the `llm_inputs.<key>` value the pipeline stamped into
    /// `target.vault_path` at queue time. `/lore-process` MUST compare this
    /// against the page's current frontmatter before writing; a mismatch means
    /// the task is stale (the page was re-rendered between enqueue and
    /// processing) and must be dropped without modifying the page. Without this
    /// guard a stale task would overwrite the section with content keyed to an
    /// older input, and the next ingest's cache lookup would freeze that
    /// mismatch forever.
    ///
    /// Note this hashes `cache_identity()`, NOT the `input` field below — `input`
    /// carries the originating `source_type`, which intentionally does not participate
    /// in cache identity.
    pub cache_hash: String,
    pub input: serde_json::Value,
    pub target: TaskTarget,
}

/// `EnumIter` exists for the skill-contract tests — see [`crate::TargetKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumIter)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    Summarize,
    ExtractConcepts,
    IdentifyThemes,
    RefineEvents,
}

impl TaskKind {
    /// The kebab-case wire name, matching the `#[serde(rename_all = "kebab-case")]`
    /// JSONL encoding (pinned by `as_str_matches_wire_encoding`). The single source
    /// of truth for the textual kind label.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Summarize => "summarize",
            TaskKind::ExtractConcepts => "extract-concepts",
            TaskKind::IdentifyThemes => "identify-themes",
            TaskKind::RefineEvents => "refine-events",
        }
    }
}

/// Write `tasks` as JSONL to `final_path` atomically (temp + fsync + rename),
/// cleaning up the temp file if the rename fails. The single writer implementation
/// for queue files: `QueueLlmClient::flush` creates them through it and
/// `lore queue prune` rewrites them through it, so every queue file on disk —
/// fresh or pruned — has identical durability and encoding guarantees.
pub fn write_tasks_atomic(final_path: &Path, tasks: &[QueueTask]) -> std::io::Result<()> {
    use std::io::Write;

    let tmp_path = final_path.with_extension("jsonl.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;
    for task in tasks {
        let line = serde_json::to_string(task).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);

    if let Err(e) = std::fs::rename(&tmp_path, final_path) {
        // The rename error is the one the caller needs to see — deliberately
        // discard the unlink error so a stale `.jsonl.tmp` never accumulates.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // Make the rename itself crash-durable: the file's bytes are fsynced above,
    // but the directory entry needs its own fsync or a power loss can roll the
    // rename back. Unix-only — Windows cannot fsync a directory handle via std.
    #[cfg(unix)]
    if let Some(dir) = final_path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

impl QueueLlmClient {
    pub fn new(queue_dir: PathBuf) -> Self {
        // Wall-clock second + PID separates distinct CLI invocations; a process-global
        // sequence additionally separates two clients constructed in the same process and
        // second, so their flushes never rename onto the same final path. The timestamp
        // is UTC — filenames are sortable across machines and timezones.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let run_id = format!(
            "{}-pid{}-{}",
            jiff::Timestamp::now()
                .to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y-%m-%dT%H-%M-%SZ"),
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        );
        Self {
            queue_dir,
            run_id,
            counter: AtomicU64::new(0),
            buffer: tokio::sync::Mutex::new(Buffer::default()),
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
        self.buffer.lock().await.tasks.push(task);
    }
}

#[async_trait]
impl LlmClient for QueueLlmClient {
    async fn begin_source(&self) {
        let mut buffer = self.buffer.lock().await;
        buffer.source_mark = buffer.tasks.len();
    }

    async fn rollback_source(&self) {
        let mut buffer = self.buffer.lock().await;
        let mark = buffer.source_mark;
        buffer.tasks.truncate(mark);
    }

    async fn summarize(&self, req: SummarizeRequest) -> Result<String, LlmError> {
        let kind = if req.target.kind == crate::TargetKind::DailyRefineEvents {
            TaskKind::RefineEvents
        } else {
            TaskKind::Summarize
        };
        let prefix = if kind == TaskKind::RefineEvents {
            "ref"
        } else {
            "sum"
        };
        let task = QueueTask {
            task_id: self.next_id(prefix),
            kind,
            created_at: jiff::Timestamp::now(),
            cache_hash: req.cache_hash(),
            input: req.task_input(),
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
            cache_hash: req.cache_hash(),
            input: req.task_input(),
            target: req.target,
        };
        self.enqueue(task).await;
        Ok(vec![])
    }

    async fn identify_themes(&self, req: ThemeRequest) -> Result<Vec<Theme>, LlmError> {
        let task = QueueTask {
            task_id: self.next_id("thm"),
            kind: TaskKind::IdentifyThemes,
            created_at: jiff::Timestamp::now(),
            cache_hash: req.cache_hash(),
            input: req.task_input(),
            target: req.target,
        };
        self.enqueue(task).await;
        Ok(vec![])
    }

    async fn flush(&self) -> Result<(), LlmError> {
        let mut buffer = self.buffer.lock().await;
        if buffer.tasks.is_empty() {
            return Ok(());
        }

        std::fs::create_dir_all(&self.queue_dir)
            .map_err(|e| LlmError::QueueIo(format!("create dir: {e}")))?;

        let final_path = self.queue_path();
        write_tasks_atomic(&final_path, &buffer.tasks)
            .map_err(|e| LlmError::QueueIo(format!("write {}: {e}", final_path.display())))?;

        *buffer = Buffer::default();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TargetKind;
    use tempfile::TempDir;

    #[test]
    fn as_str_matches_wire_encoding() {
        // `as_str` documents itself as the serde JSONL encoding; pin the two
        // together so `queue status` labels and on-disk task records can't diverge.
        use strum::IntoEnumIterator;
        for kind in TaskKind::iter() {
            let wire = serde_json::to_value(kind).unwrap();
            assert_eq!(wire.as_str().unwrap(), kind.as_str());
        }
    }

    #[tokio::test]
    async fn summarize_buffers_until_flush() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        let req = SummarizeRequest {
            text: "Some content".into(),
            max_sentences: 5,
            locale: "ko".into(),
            source_type: None,
            focus: None,
            target: TaskTarget {
                vault_path: "daily/test/2026-05-23.md".into(),
                kind: TargetKind::DailySummary,
                anchor: "## Summary".into(),
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
                locale: "ko".into(),
                source_type: None,
                focus: None,
                target: TaskTarget {
                    vault_path: "p".into(),
                    kind: TargetKind::DailySummary,
                    anchor: String::new(),
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
    async fn rollback_source_discards_only_tasks_buffered_since_begin() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        fn req(path: &str) -> SummarizeRequest {
            SummarizeRequest {
                text: "x".into(),
                max_sentences: 5,
                locale: "ko".into(),
                source_type: None,
                focus: None,
                target: TaskTarget {
                    vault_path: path.into(),
                    kind: TargetKind::DailySummary,
                    anchor: String::new(),
                },
            }
        }

        // Source A commits its task (no rollback).
        client.begin_source().await;
        client
            .summarize(req("daily/a/2026-05-23.md"))
            .await
            .unwrap();

        // Source B begins, buffers a task, then its plan "fails" → rollback.
        client.begin_source().await;
        client
            .summarize(req("daily/b/2026-05-23.md"))
            .await
            .unwrap();
        client.rollback_source().await;

        client.flush().await.unwrap();

        let content = tokio::fs::read_to_string(client.queue_path())
            .await
            .unwrap();
        assert_eq!(
            content.lines().count(),
            1,
            "only the committed source's task survives a rollback"
        );
        assert!(content.contains("daily/a/2026-05-23.md"));
        assert!(
            !content.contains("daily/b/2026-05-23.md"),
            "a rolled-back source's tasks must never reach the flushed queue file"
        );
    }

    #[tokio::test]
    async fn extract_concepts_returns_empty_in_queue_mode() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());

        let req = ExtractConceptsRequest {
            text: "Anthropic releases Opus 4.7".into(),
            source_id: "ai-news".into(),
            source_type: lk_core::config::SourceType::Gmail,
            date: jiff::civil::date(2026, 5, 23),
            focus: None,
            target: TaskTarget {
                vault_path: "daily/ai-news/2026-05-23.md".into(),
                kind: TargetKind::DailyConcepts,
                anchor: "## Related Concepts".into(),
            },
            categories: vec![],
        };
        let concepts = client.extract_concepts(req).await.unwrap();
        assert!(concepts.is_empty(), "queue mode emits task, returns empty");
        client.flush().await.unwrap();

        let content = tokio::fs::read_to_string(client.queue_path())
            .await
            .unwrap();
        let task: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert!(matches!(task.kind, TaskKind::ExtractConcepts));
        // No focus set → no `focus` key in the task input.
        assert!(task.input.get("focus").is_none());
    }

    #[tokio::test]
    async fn focus_is_serialized_into_task_input() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());
        client
            .extract_concepts(ExtractConceptsRequest {
                text: "mixed feed".into(),
                source_id: "tech-news".into(),
                source_type: lk_core::config::SourceType::Gmail,
                date: jiff::civil::date(2026, 5, 23),
                focus: Some("software engineering and AI/ML only".into()),
                target: TaskTarget {
                    vault_path: "daily/tech-news/2026-05-23.md".into(),
                    kind: TargetKind::DailyConcepts,
                    anchor: "## Related Concepts".into(),
                },
                categories: vec![],
            })
            .await
            .unwrap();
        client.flush().await.unwrap();

        let content = tokio::fs::read_to_string(client.queue_path())
            .await
            .unwrap();
        let task: QueueTask = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(
            task.input.get("focus").and_then(|v| v.as_str()),
            Some("software engineering and AI/ML only")
        );
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
            locale: "ko".into(),
            source_type: None,
            focus: None,
            target: TaskTarget {
                vault_path: "p".into(),
                kind: TargetKind::DailySummary,
                anchor: String::new(),
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
    async fn cache_hash_is_stable_across_field_order() {
        // The cache identity must produce the same hash regardless of the order
        // categories arrive in — they are sorted before hashing.
        use crate::CategoryRef;
        let date = jiff::civil::date(2026, 5, 23);
        let a = ExtractConceptsRequest {
            text: "x".into(),
            source_id: "ai-news".into(),
            source_type: lk_core::config::SourceType::Gmail,
            date,
            focus: None,
            target: TaskTarget {
                vault_path: "p".into(),
                kind: TargetKind::DailyConcepts,
                anchor: "## c".into(),
            },
            categories: vec![
                CategoryRef {
                    id: "ai-ml".into(),
                    label: "AI/ML".into(),
                },
                CategoryRef {
                    id: "infra".into(),
                    label: "Infra".into(),
                },
            ],
        };
        let mut b = a.clone();
        b.categories.reverse();
        assert_eq!(a.cache_hash(), b.cache_hash());
    }

    #[tokio::test]
    async fn cache_hash_changes_when_input_text_changes() {
        let target = TaskTarget {
            vault_path: "p".into(),
            kind: TargetKind::DailySummary,
            anchor: "## s".into(),
        };
        let a = SummarizeRequest {
            text: "v1".into(),
            max_sentences: 5,
            locale: "ko".into(),
            source_type: None,
            focus: None,
            target: target.clone(),
        };
        let b = SummarizeRequest {
            text: "v2".into(),
            ..a.clone()
        };
        assert_ne!(a.cache_hash(), b.cache_hash());
    }

    #[tokio::test]
    async fn cache_hash_ignores_target() {
        // target.vault_path is the OUTPUT location — it must not perturb the input
        // hash, otherwise renaming a daily file path would invalidate every cache.
        let a = SummarizeRequest {
            text: "x".into(),
            max_sentences: 5,
            locale: "ko".into(),
            source_type: None,
            focus: None,
            target: TaskTarget {
                vault_path: "daily/a.md".into(),
                kind: TargetKind::DailySummary,
                anchor: "## s".into(),
            },
        };
        let b = SummarizeRequest {
            target: TaskTarget {
                vault_path: "daily/b.md".into(),
                kind: TargetKind::DailySummary,
                anchor: "## s".into(),
            },
            ..a.clone()
        };
        assert_eq!(a.cache_hash(), b.cache_hash());
    }

    #[tokio::test]
    async fn theme_cache_hash_changes_when_locale_changes() {
        // Theme titles/descriptions are prose written into a localized synthesis page,
        // so a vault.locale flip MUST invalidate the cache — otherwise the stale-task
        // guard would freeze wrong-language themes under a matching hash.
        let target = TaskTarget {
            vault_path: "synthesis/weekly/2026-W01.md".into(),
            kind: TargetKind::WeeklySynthesisThemes,
            anchor: "## Themes".into(),
        };
        let ko = ThemeRequest {
            text: "combined source text".into(),
            max_themes: 5,
            locale: "ko".into(),
            target: target.clone(),
        };
        let en = ThemeRequest {
            locale: "en".into(),
            ..ko.clone()
        };
        assert_ne!(ko.cache_hash(), en.cache_hash());
    }

    #[tokio::test]
    async fn run_id_timestamp_is_utc() {
        // The run_id prefix is `YYYY-MM-DDTHH-MM-SSZ`; the literal `Z` must reflect
        // actual UTC, not local time. Verify by reparsing as UTC and confirming the
        // delta from `Timestamp::now()` is small.
        let dir = TempDir::new().unwrap();
        let before = jiff::Timestamp::now();
        let client = QueueLlmClient::new(dir.path().to_path_buf());
        let after = jiff::Timestamp::now();

        let ts_str = client
            .run_id()
            .split("-pid")
            .next()
            .expect("run_id has -pid separator");
        // Convert `YYYY-MM-DDTHH-MM-SSZ` back to RFC3339 by turning the last three
        // hyphens (the time-component separators) into colons. The first two
        // hyphens (date separators) stay.
        let mut rfc = String::with_capacity(ts_str.len());
        let mut dash_count = 0;
        for b in ts_str.bytes() {
            if b == b'-' {
                dash_count += 1;
                if dash_count > 2 {
                    rfc.push(':');
                    continue;
                }
            }
            rfc.push(b as char);
        }
        let parsed: jiff::Timestamp = rfc.parse().expect("run_id timestamp must parse as UTC");

        // The run_id is truncated to seconds; allow a 2-second window around now.
        let lower = before.duration_since(parsed);
        let upper = parsed.duration_since(after);
        assert!(
            lower.as_secs().abs() <= 2 && upper.as_secs().abs() <= 2,
            "run_id timestamp {parsed} is not close to now [{before}, {after}]",
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
                locale: "ko".into(),
                source_type: None,
                focus: None,
                target: TaskTarget {
                    vault_path: "p".into(),
                    kind: TargetKind::DailySummary,
                    anchor: String::new(),
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
