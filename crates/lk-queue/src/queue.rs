use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use lk_core::concept::ExtractedConcept;

use crate::{
    ExtractConceptsRequest, LlmClient, QueueError, SummarizeRequest, TaskRequest, TaskTarget,
    Theme, ThemeRequest,
};

/// LlmClient that defers semantic work to a Claude Code skill. Buffers task records
/// in memory during `summarize`/`extract_concepts` calls; `flush` admits requests not
/// already pending to `<queue_dir>/{run_id}.jsonl` atomically (temp + fsync + rename).
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
    pub input: crate::Identity,
    pub target: TaskTarget,
}

/// The payload key naming a second section the task fills, written by
/// `ConceptSynthesisRequest::task_input` and read back by `QueueTask::anchors`.
///
/// One producer, by convention rather than by a type: `anchors` reads the key whatever the
/// task's kind, so a future request type spending this name on anything but "a heading the
/// drain must find" would silently acquire a second required-heading check. The name is
/// declared here, beside the only reader, so that reuse has to pass through this line.
const RELATED_ANCHOR: &str = "related_anchor";

impl QueueTask {
    fn request_identity(&self) -> (TaskKind, &str, &crate::Identity, &TaskTarget) {
        (self.kind, &self.cache_hash, &self.input, &self.target)
    }

    /// Required policy for the typed input shape.
    pub fn expected_generation_policy(&self) -> &'static str {
        match self.kind {
            TaskKind::ExtractConcepts if self.input.contains_key("rss_original") => {
                lk_core::rss_evidence::EXTRACTION_POLICY
            }
            TaskKind::SynthesizeConcept
                if self
                    .input
                    .get("citations")
                    .and_then(|v| v.as_array())
                    .is_some_and(|citations| {
                        citations.iter().any(|c| c.get("rss_event").is_some())
                    }) =>
            {
                lk_core::rss_evidence::SYNTHESIS_POLICY
            }
            _ => self.kind.generation_policy(),
        }
    }

    /// Missing page-only provenance is legacy/unknown, never an automatic invalidation.
    pub fn generation_policy(&self) -> Result<Option<&str>, String> {
        let expected = self.expected_generation_policy();
        if expected != self.kind.generation_policy() {
            return match self.input.get("generation_policy").and_then(|v| v.as_str()) {
                Some(policy) if policy == expected => Ok(Some(policy)),
                _ => Err("RSS evidence requires its versioned generation policy".into()),
            };
        }
        match self.input.get("generation_policy") {
            None => Ok(None),
            Some(value) if value.as_str() == Some(self.kind.generation_policy()) => {
                Ok(value.as_str())
            }
            Some(_) => Err("unsupported generation policy; use a compatible processor".into()),
        }
    }

    /// Reconstruct the original typed request instead of maintaining another hash projection.
    pub fn validate_retained_input(&self) -> Result<(), String> {
        if self.expected_generation_policy() != self.kind.generation_policy() {
            self.generation_policy()?;
        }
        if self.kind == TaskKind::ExtractConcepts
            && let Some(value) = self.input.get("rss_original")
        {
            let manifest: lk_core::rss_original::RssOriginalManifest =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            manifest.validate()?;
            if self.input.get("source_type").and_then(|v| v.as_str()) != Some("rss")
                || self.input.get("source_id").and_then(|v| v.as_str())
                    != Some(manifest.source_id.as_str())
                || self.input.get("date").and_then(|v| v.as_str())
                    != Some(manifest.date.to_string().as_str())
            {
                return Err("RSS evidence manifest differs from extraction scope".into());
            }
        }
        if self.kind == TaskKind::SynthesizeConcept {
            let citations: Vec<lk_core::concept::Citation> = serde_json::from_value(
                self.input
                    .get("citations")
                    .cloned()
                    .ok_or("missing citations")?,
            )
            .map_err(|e| e.to_string())?;
            for citation in citations {
                if let Some(event) = citation.rss_event {
                    event.validate(&citation.revision)?;
                }
            }
        }
        if !self.kind.supports_target(self.target.kind) {
            return Err("task kind does not match target kind".into());
        }
        let mut input = self.input.clone();
        input.insert(
            "target".into(),
            serde_json::to_value(&self.target).map_err(|e| e.to_string())?,
        );
        let value = serde_json::Value::Object(input);
        let hash = match self.kind {
            TaskKind::Summarize | TaskKind::RefineEvents => {
                serde_json::from_value::<crate::SummarizeRequest>(value).map(|r| r.cache_hash())
            }
            TaskKind::ExtractConcepts => {
                serde_json::from_value::<crate::ExtractConceptsRequest>(value)
                    .map(|r| r.cache_hash())
            }
            TaskKind::IdentifyThemes => {
                serde_json::from_value::<crate::ThemeRequest>(value).map(|r| r.cache_hash())
            }
            TaskKind::SynthesizeConcept => {
                serde_json::from_value::<crate::ConceptSynthesisRequest>(value)
                    .map(|r| r.cache_hash())
            }
        }
        .map_err(|e| format!("original typed input is missing or malformed: {e}"))?;
        if hash != self.cache_hash {
            return Err("retained input does not match its cache hash".into());
        }
        Ok(())
    }

    /// Every section heading this task writes, in the order it writes them.
    ///
    /// Most kinds name one. A `synthesize-concept` task names two — the synthesis heading
    /// and the page's related-concepts heading — because one citation set is what both
    /// answer to. Both must exist on the target page for the task to be work: the drain
    /// locates a section by heading, and a heading the page lacks receives nothing while
    /// reporting success. Asking the task rather than reading its payload at the call site
    /// is what keeps the second heading from being validated in one command and forgotten
    /// in the next.
    pub fn anchors(&self) -> Vec<&str> {
        std::iter::once(self.target.anchor.as_str())
            .chain(
                self.input
                    .get(RELATED_ANCHOR)
                    .and_then(|v| v.as_str())
                    .filter(|a| !a.is_empty()),
            )
            .collect()
    }
}

/// What a drained task produced, for the kinds whose output Lorekeeper materializes itself.
///
/// Most tasks write one section of one page, and the drain writes it directly — there is
/// nothing to decide. Concept extraction is different: its output lands on a SHARED page
/// per concept, under merge rules (preserved `## Synthesis`, aliases, category, citation
/// count) that already exist as tested Rust. Returning the extraction as data lets that
/// code own the merge, which keeps one implementation of it and leaves the extraction
/// itself a pure function of the page it read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: String,
    /// The task's `cache_hash`, carried back so the applier can re-check staleness against
    /// the target page: the page may have been re-rendered while the drain was running.
    pub cache_hash: String,
    pub target_revision: String,
    pub target: TaskTarget,
    pub date: jiff::civil::Date,
    pub concepts: Vec<ReportedConcept>,
}

/// A synthesis draft against one target snapshot and the task's source revisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisResult {
    pub task_id: String,
    pub cache_hash: String,
    pub target_revision: String,
    pub synthesis: String,
    pub related: Option<String>,
}

pub const SYNTHESIS_RESULT_SUFFIX: &str = ".synthesis.json";
pub const SECTION_RESULT_SUFFIX: &str = ".section.json";

/// A single semantic section, published only against its captured target bytes.
/// The pending task owns the destination, heading and completion marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionResult {
    pub task_id: String,
    pub cache_hash: String,
    pub target_revision: String,
    pub body: String,
}

/// One concept an extraction named, with the grounding sentence that makes its page worth
/// having.
///
/// The synthesis travels with the extraction because only the reader of the source page can
/// write it, and a concept page created without one is an empty heading nobody can act on.
/// It applies on CREATION only — an established page's synthesis is its accumulated meaning
/// and outranks any single mention.
#[derive(Debug, Clone, Serialize)]
pub struct ReportedConcept {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rss_evidence: Vec<lk_core::rss_evidence::RssEventSelection>,
    #[serde(flatten)]
    pub concept: ExtractedConcept,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<String>,
}

impl<'de> Deserialize<'de> for ReportedConcept {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            name: String,
            #[serde(default)]
            category: Option<String>,
            #[serde(default)]
            aliases: Vec<String>,
            #[serde(default)]
            synthesis: Option<String>,
            #[serde(default)]
            rss_evidence: Vec<lk_core::rss_evidence::RssEventSelection>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            concept: ExtractedConcept {
                name: wire.name,
                category: wire.category,
                aliases: wire.aliases,
            },
            synthesis: wire.synthesis,
            rss_evidence: wire.rss_evidence,
        })
    }
}

/// Subdirectory a queue file is moved to once every task in it is settled. Written by
/// `/lore-process` when it finishes a run and by `lore queue prune` when it retires one,
/// read by `lore maintenance` — a path three components agree on, so it is named once.
pub const PROCESSED_SUBDIR: &str = "processed";

/// Subdirectory of the queue holding results awaiting `lore queue apply`.
pub const RESULTS_SUBDIR: &str = "results";

/// Subdirectory results that cannot be parsed are moved to, out of the apply path.
pub const CORRUPT_SUBDIR: &str = "corrupt";

/// The file whose advisory lock says who is materializing the queue's results.
const LOCK_FILE: &str = ".lock";

/// What [`claim_queue`] found: this process holds the queue, another one does, or the
/// filesystem will not answer the question.
pub enum QueueClaim {
    /// Held until this value drops or the process dies, whichever comes first.
    Held(std::fs::File),
    /// Another process is materializing results right now.
    Busy,
    /// The lock could not be taken for a reason that is not contention — a network or
    /// synced filesystem that does not carry advisory locks. Said out loud rather than
    /// read as either answer.
    Unavailable(std::io::Error),
}

/// Claim the queue while admitting tasks, materializing results or pruning pending files.
///
/// Applying a result is a read-modify-write across several files that ends in deleting the
/// result: two runs at once each read an origin page before the other writes it, so the
/// second write drops the citations the first added, and both then delete the evidence. The
/// claim is an advisory lock rather than a lock file with a pid, because the kernel releases
/// it when the holder dies — a killed run leaves nothing for the next one to time out on.
///
/// Scoped to that write, never to a drain session: a session spans many processes and hours,
/// and an exclusive claim held that long needs a liveness answer nothing here can give.
pub fn claim_queue(queue_dir: &Path) -> std::io::Result<QueueClaim> {
    std::fs::create_dir_all(queue_dir)?;
    let file = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(queue_dir.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(QueueClaim::Held(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(QueueClaim::Busy),
        Err(std::fs::TryLockError::Error(e)) => Ok(QueueClaim::Unavailable(e)),
    }
}

/// Every result file in `<queue_dir>/results/`, oldest first, split by whether it parses.
pub struct ResultBatch {
    /// Results ready to apply, in filename order.
    pub ready: Vec<(PathBuf, TaskResult)>,
    /// Files that could not be parsed, with the reason.
    pub unreadable: Vec<(PathBuf, String)>,
}

/// Read every result file in `<queue_dir>/results/`, oldest first.
///
/// A malformed file — a drain session killed mid-write — is separated rather than failing
/// the read. Failing it would strand every OTHER pending result behind one truncated file,
/// on every run, forever: nothing prunes results, so only a human noticing a red pipeline
/// would clear it. The caller quarantines it instead, which is safe because the concepts are
/// not lost with the file. The drain writes results only; `queue apply` is what stamps the
/// completion marker — so an unapplied result means its page still carries no marker, and
/// the next ingest re-enqueues the task off the unchanged input hash.
///
/// (`event_log` is strict about a corrupt line for a reason that does not apply here: its
/// caller REWRITES the log from what it parsed, so skipping would destroy the record.)
pub fn read_results(queue_dir: &Path) -> std::io::Result<ResultBatch> {
    let dir = queue_dir.join(RESULTS_SUBDIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ResultBatch {
                ready: Vec::new(),
                unreadable: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && !p.to_string_lossy().ends_with(SYNTHESIS_RESULT_SUFFIX)
                && !p.to_string_lossy().ends_with(SECTION_RESULT_SUFFIX)
        })
        .collect();
    paths.sort();

    let mut batch = ResultBatch {
        ready: Vec::with_capacity(paths.len()),
        unreadable: Vec::new(),
    };
    for path in paths {
        // An I/O failure is NOT a malformed file: the bytes may be perfectly good and
        // unreadable for a reason retrying fixes, so it still fails the read.
        let raw = std::fs::read(&path)?;
        // Encoding errors are permanent payload corruption, not filesystem failures.
        match serde_json::from_slice::<TaskResult>(&raw) {
            Ok(result) => batch.ready.push((path, result)),
            Err(e) => batch.unreadable.push((path, e.to_string())),
        }
    }
    Ok(batch)
}

/// `EnumIter` exists for the skill-contract tests — see [`crate::TargetKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::EnumIter)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    Summarize,
    ExtractConcepts,
    IdentifyThemes,
    RefineEvents,
    SynthesizeConcept,
}

impl TaskKind {
    /// The kebab-case wire name, matching the `#[serde(rename_all = "kebab-case")]`
    /// JSONL encoding (pinned by `as_str_matches_wire_encoding`). The single source
    /// of truth for the textual kind label.
    /// Semantic policy versions are manually advanced, independently of release versions.
    pub fn generation_policy(self) -> &'static str {
        match self {
            Self::Summarize => "summary-v1",
            Self::ExtractConcepts => "concept-extraction-v1",
            Self::IdentifyThemes => "themes-v1",
            Self::RefineEvents => "event-refinement-v1",
            Self::SynthesizeConcept => "concept-synthesis-v1",
        }
    }

    pub fn supports_target(self, target: crate::TargetKind) -> bool {
        use crate::TargetKind as T;
        matches!(
            (self, target),
            (
                Self::Summarize,
                T::DailySummary
                    | T::DocumentSummary
                    | T::WorkLogSynthesis
                    | T::WeeklyReviewNarrative
                    | T::MonthlyReviewNarrative
                    | T::QuarterlyReviewNarrative
                    | T::AnnualReviewNarrative
            ) | (Self::IdentifyThemes, T::WeeklySynthesisThemes)
                | (Self::RefineEvents, T::DailyRefineEvents)
                | (
                    Self::ExtractConcepts,
                    T::DailyConcepts | T::DocumentConcepts
                )
                | (Self::SynthesizeConcept, T::ConceptSynthesis)
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Summarize => "summarize",
            TaskKind::ExtractConcepts => "extract-concepts",
            TaskKind::IdentifyThemes => "identify-themes",
            TaskKind::RefineEvents => "refine-events",
            TaskKind::SynthesizeConcept => "synthesize-concept",
        }
    }
}

/// Serialize `tasks` to JSONL and write them to `final_path` atomically. Used by
/// `QueueLlmClient::flush` and `lore queue prune` for identical durability and encoding.
/// Delegates the temp + fsync + rename +
/// dir-fsync + per-writer-unique-temp mechanics to `lk_core::fs::write_atomic` (the one
/// sync atomic write in the workspace); a `.jsonl` path yields a `*.jsonl.tmp` temp the
/// ingest startup sweep still reaps.
pub fn write_tasks_atomic(final_path: &Path, tasks: &[QueueTask]) -> std::io::Result<()> {
    let mut buf = String::with_capacity(tasks.len() * 256);
    for task in tasks {
        let line = serde_json::to_string(task).map_err(std::io::Error::other)?;
        buf.push_str(&line);
        buf.push('\n');
    }
    lk_core::fs::write_atomic(final_path, buf.as_bytes(), None)
}

pub struct QueueFile {
    pub tasks: Vec<QueueTask>,
    pub unparseable: usize,
}

impl QueueFile {
    pub fn is_rewritable(&self) -> bool {
        self.unparseable == 0
    }
}

/// Decode JSONL without losing malformed lines or treating decode errors as I/O failures.
pub fn read_tasks(path: &Path) -> std::io::Result<QueueFile> {
    let file = std::fs::File::open(path)?;
    let mut tasks = Vec::new();
    let mut unparseable = 0;
    for (index, line) in BufReader::new(file).split(b'\n').enumerate() {
        let line = line?;
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        match serde_json::from_slice(&line) {
            Ok(task) => tasks.push(task),
            Err(error) => {
                unparseable += 1;
                tracing::warn!(file = %path.display(), line = index + 1, error = %error, "unparseable queue line preserved; not eligible for reuse");
            }
        }
    }
    Ok(QueueFile { tasks, unparseable })
}

fn read_pending_tasks(queue_dir: &Path) -> std::io::Result<Vec<QueueTask>> {
    let mut tasks = Vec::new();
    for entry in std::fs::read_dir(queue_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            return Err(std::io::Error::other(format!(
                "pending queue entry is not a regular file: {}",
                path.display()
            )));
        }
        tasks.extend(read_tasks(&path)?.tasks);
    }
    Ok(tasks)
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

    /// Build one typed request as a task without publishing or buffering it.
    /// Callers can include it in a recoverable vault mutation plan.
    ///
    /// The single place a `QueueTask` is built, which is what lets the vault's language be
    /// written into every payload exactly once. `TaskRequest::locale` is the guarantee there
    /// is one to write: a new request kind cannot reach this function without answering it.
    pub fn build_task<R: TaskRequest>(&self, prefix: &str, kind: TaskKind, req: R) -> QueueTask {
        let mut input = req.task_input();
        input.insert("locale".into(), req.locale().tag().into());
        input.insert(
            "generation_policy".into(),
            req.evidence_policy()
                .unwrap_or(kind.generation_policy())
                .into(),
        );
        QueueTask {
            task_id: self.next_id(prefix),
            kind,
            created_at: jiff::Timestamp::now(),
            cache_hash: req.cache_hash(),
            input,
            target: req.into_target(),
        }
    }

    async fn enqueue<R: TaskRequest + Send>(&self, prefix: &str, kind: TaskKind, req: R) {
        let task = self.build_task(prefix, kind, req);
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

    async fn summarize(&self, req: SummarizeRequest) -> Result<String, QueueError> {
        let (kind, prefix) = if req.target.kind == crate::TargetKind::DailyRefineEvents {
            (TaskKind::RefineEvents, "ref")
        } else {
            (TaskKind::Summarize, "sum")
        };
        self.enqueue(prefix, kind, req).await;
        Ok(String::new())
    }

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, QueueError> {
        self.enqueue("ext", TaskKind::ExtractConcepts, req).await;
        Ok(vec![])
    }

    async fn synthesize_concept(
        &self,
        req: crate::ConceptSynthesisRequest,
    ) -> Result<String, QueueError> {
        self.enqueue("syn", TaskKind::SynthesizeConcept, req).await;
        Ok(String::new())
    }

    async fn identify_themes(&self, req: ThemeRequest) -> Result<Vec<Theme>, QueueError> {
        self.enqueue("thm", TaskKind::IdentifyThemes, req).await;
        Ok(vec![])
    }

    async fn flush(&self) -> Result<(), QueueError> {
        let mut buffer = self.buffer.lock().await;
        if buffer.tasks.is_empty() {
            return Ok(());
        }

        std::fs::create_dir_all(&self.queue_dir)
            .map_err(|e| QueueError::QueueIo(format!("create dir: {e}")))?;

        let _claim = match claim_queue(&self.queue_dir)
            .map_err(|e| QueueError::QueueIo(format!("claim queue: {e}")))?
        {
            QueueClaim::Held(file) => file,
            QueueClaim::Busy => return Err(QueueError::QueueIo("queue is busy".into())),
            QueueClaim::Unavailable(e) => {
                return Err(QueueError::QueueIo(format!("queue lock unavailable: {e}")));
            }
        };
        let pending = read_pending_tasks(&self.queue_dir)
            .map_err(|e| QueueError::QueueIo(format!("read pending requests: {e}")))?;
        let mut identities: HashSet<_> = pending.iter().map(QueueTask::request_identity).collect();
        let tasks: Vec<_> = buffer
            .tasks
            .iter()
            .filter(|task| identities.insert(task.request_identity()))
            .cloned()
            .collect();

        if !tasks.is_empty() {
            let final_path = self.queue_path();
            write_tasks_atomic(&final_path, &tasks)
                .map_err(|e| QueueError::QueueIo(format!("write {}: {e}", final_path.display())))?;
        }

        *buffer = Buffer::default();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TargetKind;
    use lk_core::i18n::Locale;
    use tempfile::TempDir;

    fn build_summary_request(text: &str) -> SummarizeRequest {
        SummarizeRequest {
            text: text.into(),
            max_sentences: 5,
            locale: Locale::Ko,
            source_type: None,
            focus: None,
            target: TaskTarget {
                vault_path: "daily/test/2026-05-23.md".into(),
                kind: TargetKind::DailySummary,
                anchor: "## Summary".into(),
            },
        }
    }

    #[tokio::test]
    async fn repeated_requests_reuse_pending_work_but_not_processed_history() {
        let dir = TempDir::new().unwrap();
        let first = QueueLlmClient::new(dir.path().into());
        for _ in 0..2 {
            first
                .summarize(build_summary_request("same"))
                .await
                .unwrap();
        }
        first.flush().await.unwrap();
        assert_eq!(read_pending_tasks(dir.path()).unwrap().len(), 1);

        let repeated = QueueLlmClient::new(dir.path().into());
        repeated
            .summarize(build_summary_request("same"))
            .await
            .unwrap();
        repeated.flush().await.unwrap();
        assert!(!repeated.queue_path().exists());
        repeated.flush().await.unwrap();

        let processed = dir.path().join(PROCESSED_SUBDIR);
        std::fs::create_dir(&processed).unwrap();
        std::fs::rename(first.queue_path(), processed.join("retired.jsonl")).unwrap();
        let fresh = QueueLlmClient::new(dir.path().into());
        fresh
            .summarize(build_summary_request("same"))
            .await
            .unwrap();
        fresh.flush().await.unwrap();
        assert_eq!(read_pending_tasks(dir.path()).unwrap().len(), 1);
        assert!(fresh.queue_path().exists());
    }

    #[tokio::test]
    async fn admission_preserves_payload_and_destination_differences_with_the_same_cache_hash() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().into());
        let base = client.build_task("sum", TaskKind::Summarize, build_summary_request("same"));
        let mut variants = vec![base.clone()];
        let mut target = base.clone();
        target.target.vault_path = "daily/other/2026-05-23.md".into();
        variants.push(target);
        let mut heading = base.clone();
        heading.target.anchor = "## 요약".into();
        variants.push(heading);
        let mut routing = base.clone();
        routing.input.insert("source_type".into(), "manual".into());
        variants.push(routing);
        let mut regeneration = base.clone();
        regeneration
            .input
            .insert("regenerated_from".into(), base.task_id.clone().into());
        variants.push(regeneration);
        for task in &mut variants {
            task.task_id = client.next_id("sum");
        }
        assert!(
            variants
                .iter()
                .all(|task| task.cache_hash == base.cache_hash)
        );
        client.buffer.lock().await.tasks.extend(variants);
        client.flush().await.unwrap();
        assert_eq!(read_pending_tasks(dir.path()).unwrap().len(), 5);
    }

    #[tokio::test]
    async fn failed_admission_preserves_buffer_for_retry() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().into());
        client
            .summarize(build_summary_request("new"))
            .await
            .unwrap();
        let held = claim_queue(dir.path()).unwrap();
        assert!(matches!(held, QueueClaim::Held(_)));
        assert!(client.flush().await.is_err());
        assert!(!client.queue_path().exists());
        drop(held);

        client.flush().await.unwrap();
        assert_eq!(read_pending_tasks(dir.path()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn malformed_pending_lines_are_preserved_without_suppressing_new_work() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().into());
        let retained = client.build_task("sum", TaskKind::Summarize, build_summary_request("same"));
        let mut original = serde_json::to_vec(&retained).unwrap();
        original.extend_from_slice(b"\n{unfinished JSON\n\xff\n");
        let path = dir.path().join("retained.jsonl");
        std::fs::write(&path, &original).unwrap();
        client
            .summarize(build_summary_request("same"))
            .await
            .unwrap();
        client
            .summarize(build_summary_request("new"))
            .await
            .unwrap();
        client.flush().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(read_tasks(&path).unwrap().unparseable, 2);
        assert!(!read_tasks(&path).unwrap().is_rewritable());
        let admitted = std::fs::read_to_string(client.queue_path()).unwrap();
        assert_eq!(admitted.lines().count(), 1);
        let task: QueueTask = serde_json::from_str(admitted.trim()).unwrap();
        assert_eq!(task.input["text"], "new");
        assert_eq!(read_pending_tasks(dir.path()).unwrap().len(), 2);
    }

    #[test]
    fn rss_task_policy_dispatch_is_explicit_and_malformed_v2_never_falls_back() {
        let root = TempDir::new().unwrap();
        let client = QueueLlmClient::new(root.path().into());
        let request = crate::ConceptSynthesisRequest {
            citations: vec![lk_core::concept::Citation {
                path: "daily/feed/2026-10-18.md".into(),
                revision: "a".repeat(64),
                rss_event: Some(lk_core::rss_evidence::RssEventSelector {
                    schema: lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA.into(),
                    event_id: "feed:2026-10-18:0000000000000001".into(),
                }),
            }],
            locale: Locale::En,
            related_anchor: None,
            target: TaskTarget {
                vault_path: "wiki/concepts/alpha.md".into(),
                kind: TargetKind::ConceptSynthesis,
                anchor: "## Synthesis".into(),
            },
        };
        let task = client.build_task("syn", TaskKind::SynthesizeConcept, request);
        task.validate_retained_input().unwrap();
        assert_eq!(
            task.generation_policy().unwrap(),
            Some(lk_core::rss_evidence::SYNTHESIS_POLICY)
        );
        let mut legacy_policy = task.clone();
        legacy_policy.input.insert(
            "generation_policy".into(),
            TaskKind::SynthesizeConcept.generation_policy().into(),
        );
        assert!(legacy_policy.validate_retained_input().is_err());
        for value in [
            serde_json::Value::Null,
            serde_json::json!({"schema":"unknown","event_id":"feed:2026-10-18:0000000000000001"}),
            serde_json::json!({"schema":"rss-event-evidence-v1","event_id":"feed:2026-10-18:"}),
        ] {
            let mut malformed = task.clone();
            malformed.input.get_mut("citations").unwrap()[0]["rss_event"] = value;
            assert!(malformed.validate_retained_input().is_err());
        }
        assert!(
            serde_json::from_value::<ReportedConcept>(
                serde_json::json!({"name":"Alpha","rss_evidenc":[]})
            )
            .is_err()
        );
        let legacy: ReportedConcept = serde_json::from_value(
            serde_json::json!({"name":"Alpha","aliases":[],"synthesis":"Definition"}),
        )
        .unwrap();
        assert!(
            !serde_json::to_value(legacy)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("rss_evidence")
        );
    }

    #[test]
    fn result_discovery_distinguishes_missing_storage_from_invalid_storage() {
        let dir = TempDir::new().unwrap();
        let queue = dir.path().join("queue");
        let empty = read_results(&queue).unwrap();
        assert!(empty.ready.is_empty());
        assert!(empty.unreadable.is_empty());

        std::fs::write(&queue, "not a directory").unwrap();
        assert!(read_results(&queue).is_err());
        assert_eq!(std::fs::read_to_string(&queue).unwrap(), "not a directory");
    }

    /// The claim is what makes applying results one-at-a-time, and it has to answer three
    /// ways: held, taken, and unaskable. The third is a filesystem that carries no advisory
    /// lock, which cannot be produced here — what is pinned is that the first two are
    /// distinct and that releasing needs no cleanup, since a lock file left on disk is
    /// exactly what a pid file would have made permanent.
    #[test]
    fn the_queue_is_claimed_by_one_run_at_a_time() {
        let dir = TempDir::new().unwrap();
        let queue = dir.path().join("queue");

        let first = claim_queue(&queue).unwrap();
        assert!(matches!(first, QueueClaim::Held(_)));
        assert!(
            matches!(claim_queue(&queue).unwrap(), QueueClaim::Busy),
            "a second run must find the queue taken"
        );

        drop(first);
        assert!(
            matches!(claim_queue(&queue).unwrap(), QueueClaim::Held(_)),
            "the claim is released by dropping it, with the file still on disk"
        );
        assert!(queue.join(LOCK_FILE).exists());
    }

    #[test]
    fn one_unreadable_result_does_not_hide_the_readable_ones() {
        // The whole point of the split: a drain killed mid-write leaves a truncated file,
        // and failing the read there stranded every other pending result behind it.
        let dir = TempDir::new().unwrap();
        let results = dir.path().join(RESULTS_SUBDIR);
        std::fs::create_dir_all(&results).unwrap();

        let good = TaskResult {
            target_revision: String::new(),
            task_id: "ext-1".into(),
            cache_hash: "h".into(),
            target: crate::TaskTarget {
                vault_path: "daily/src/2026-05-23.md".into(),
                kind: TargetKind::DailyConcepts,
                anchor: "## Related Concepts".into(),
            },
            date: jiff::civil::date(2026, 5, 23),
            concepts: Vec::new(),
        };
        std::fs::write(
            results.join("a-good.json"),
            serde_json::to_string(&good).unwrap(),
        )
        .unwrap();
        // Truncated mid-write, and sorting BEFORE the good one so a read that failed on it
        // would never reach the good one either.
        std::fs::write(results.join("a-bad.json"), "{\"task_id\": \"ext-2\", \"ca").unwrap();

        let batch = read_results(dir.path()).unwrap();
        assert_eq!(batch.ready.len(), 1, "the readable result must survive");
        assert_eq!(batch.ready[0].1.task_id, "ext-1");
        assert_eq!(batch.unreadable.len(), 1);
        assert!(batch.unreadable[0].0.ends_with("a-bad.json"));
        assert!(
            !batch.unreadable[0].1.is_empty(),
            "the reason must be carried for the operator"
        );
    }

    #[test]
    fn result_file_io_failure_is_not_misclassified_as_corrupt_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(RESULTS_SUBDIR).join("not-a-file.json");
        std::fs::create_dir_all(&path).unwrap();
        assert!(read_results(dir.path()).is_err());
        assert!(path.is_dir());
    }

    #[test]
    fn a_missing_results_dir_is_an_empty_batch() {
        let dir = TempDir::new().unwrap();
        let batch = read_results(dir.path()).unwrap();
        assert!(batch.ready.is_empty() && batch.unreadable.is_empty());
    }

    /// Any stranded `*.jsonl.tmp` left in `dir` (the temp name is per-writer-unique,
    /// so tests assert on the suffix rather than a fixed path).
    fn any_tmp_in(dir: &std::path::Path) -> bool {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".jsonl.tmp"))
    }

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
            locale: Locale::Ko,
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

        for index in 0..3 {
            let req = SummarizeRequest {
                text: format!("input {index}"),
                max_sentences: 5,
                locale: Locale::Ko,
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
                locale: Locale::Ko,
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
            rss_original: None,
            text: "Anthropic releases Opus 4.7".into(),
            source_id: "ai-news".into(),
            source_type: lk_core::config::SourceType::Gmail,
            date: jiff::civil::date(2026, 5, 23),
            focus: None,
            locale: Locale::Ko,
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
                rss_original: None,
                text: "mixed feed".into(),
                source_id: "tech-news".into(),
                source_type: lk_core::config::SourceType::Gmail,
                date: jiff::civil::date(2026, 5, 23),
                focus: Some("software engineering and AI/ML only".into()),
                locale: Locale::Ko,
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
        assert!(!any_tmp_in(dir.path()), "temp file must not linger");
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
            locale: Locale::Ko,
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
        use crate::CategoryReference;
        let date = jiff::civil::date(2026, 5, 23);
        let a = ExtractConceptsRequest {
            rss_original: None,
            text: "x".into(),
            source_id: "ai-news".into(),
            source_type: lk_core::config::SourceType::Gmail,
            date,
            focus: None,
            locale: Locale::Ko,
            target: TaskTarget {
                vault_path: "p".into(),
                kind: TargetKind::DailyConcepts,
                anchor: "## c".into(),
            },
            categories: vec![
                CategoryReference {
                    id: "ai-ml".into(),
                    label: "AI/ML".into(),
                },
                CategoryReference {
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
            locale: Locale::Ko,
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
            locale: Locale::Ko,
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
            locale: Locale::Ko,
            target: target.clone(),
        };
        let en = ThemeRequest {
            locale: Locale::En,
            ..ko.clone()
        };
        assert_ne!(ko.cache_hash(), en.cache_hash());
    }

    #[tokio::test]
    async fn a_locale_switch_leaves_the_concept_kinds_cache_hashes_alone() {
        // The other half of `theme_cache_hash_changes_when_locale_changes`, and the half
        // that is easy to get wrong by making it consistent. What a summary or a week's
        // themes states is re-derived from an input, so a locale switch has to re-derive it.
        // What an extraction and a synthesis state ends on a CONCEPT page — an authored body
        // that accumulates across every source citing it, and that a switch deliberately does
        // not retranslate. Hashing the locale into these two would re-enqueue an extraction
        // for every daily page and document in the vault, and a rewrite for every concept,
        // the first time `vault.locale` moved.
        let extract = ExtractConceptsRequest {
            rss_original: None,
            text: "combined source text".into(),
            source_id: "src".into(),
            source_type: lk_core::config::SourceType::Rss,
            date: jiff::civil::date(2026, 1, 1),
            focus: None,
            locale: Locale::Ko,
            target: TaskTarget {
                vault_path: "daily/src/2026-01-01.md".into(),
                kind: TargetKind::DailyConcepts,
                anchor: "## 관련 개념".into(),
            },
            categories: vec![],
        };
        assert_eq!(
            extract.cache_hash(),
            ExtractConceptsRequest {
                rss_original: None,
                locale: Locale::En,
                ..extract.clone()
            }
            .cache_hash()
        );

        let synthesis = crate::ConceptSynthesisRequest {
            citations: vec![lk_core::concept::Citation {
                rss_event: None,
                path: "daily/src/2026-01-01.md".into(),
                revision: "revision".into(),
            }],
            locale: Locale::Ko,
            related_anchor: None,
            target: TaskTarget {
                vault_path: "wiki/concepts/rag.md".into(),
                kind: TargetKind::ConceptSynthesis,
                anchor: "## 핵심".into(),
            },
        };
        assert_eq!(
            synthesis.cache_hash(),
            crate::ConceptSynthesisRequest {
                locale: Locale::En,
                ..synthesis.clone()
            }
            .cache_hash()
        );
    }

    #[tokio::test]
    async fn a_concept_task_carries_both_headings_and_is_keyed_on_neither() {
        // One task writes the synthesis AND the relations, because both answer to the same
        // citation set, so it carries both headings for the reason `target.anchor` exists: the
        // writer must not resolve a heading from the locale table. Neither belongs in the
        // cache identity — a heading is not part of what the answer would be, and a
        // `vault.locale` switch renaming every heading in the vault must not re-queue every
        // concept in it.
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());
        let req = crate::ConceptSynthesisRequest {
            citations: vec![lk_core::concept::Citation {
                rss_event: None,
                path: "daily/src/2026-01-01.md".into(),
                revision: "revision".into(),
            }],
            locale: Locale::Ko,
            related_anchor: Some("## 관련".into()),
            target: TaskTarget {
                vault_path: "wiki/concepts/rag.md".into(),
                kind: TargetKind::ConceptSynthesis,
                anchor: "## 핵심".into(),
            },
        };
        assert_eq!(
            req.cache_hash(),
            crate::ConceptSynthesisRequest {
                related_anchor: None,
                ..req.clone()
            }
            .cache_hash(),
            "the headings a result lands under cannot change what the answer would be"
        );

        client.synthesize_concept(req).await.unwrap();
        client.flush().await.unwrap();
        let queued = std::fs::read_to_string(client.queue_path()).unwrap();
        let task: QueueTask = serde_json::from_str(queued.trim()).unwrap();
        assert_eq!(task.target.anchor, "## 핵심");
        assert_eq!(
            task.input.get(RELATED_ANCHOR).and_then(|v| v.as_str()),
            Some("## 관련")
        );
    }

    #[tokio::test]
    async fn every_queued_task_states_the_vault_language() {
        // Every task ends as prose on a page, so every one of them has to say which language
        // to write it in. Two kinds shipped without the field and the drain inferred a
        // language from the pages around the target — which reads correctly in an established
        // vault and has nothing to read in a new one, so the first concept page a vault wrote
        // got whatever language its source happened to be in.
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());
        let target = |kind| TaskTarget {
            vault_path: "p.md".into(),
            kind,
            anchor: "## a".into(),
        };
        client
            .summarize(SummarizeRequest {
                text: "t".into(),
                max_sentences: 3,
                focus: None,
                locale: Locale::En,
                source_type: None,
                target: target(TargetKind::DailySummary),
            })
            .await
            .unwrap();
        client
            .extract_concepts(ExtractConceptsRequest {
                rss_original: None,
                text: "t".into(),
                source_id: "src".into(),
                source_type: lk_core::config::SourceType::Rss,
                date: jiff::civil::date(2026, 1, 1),
                focus: None,
                locale: Locale::En,
                target: target(TargetKind::DailyConcepts),
                categories: vec![],
            })
            .await
            .unwrap();
        client
            .synthesize_concept(crate::ConceptSynthesisRequest {
                citations: vec![lk_core::concept::Citation {
                    rss_event: None,
                    path: "daily/src/2026-01-01.md".into(),
                    revision: "revision".into(),
                }],
                locale: Locale::En,
                related_anchor: None,
                target: target(TargetKind::ConceptSynthesis),
            })
            .await
            .unwrap();
        client
            .identify_themes(ThemeRequest {
                text: "t".into(),
                max_themes: 3,
                locale: Locale::En,
                target: target(TargetKind::WeeklySynthesisThemes),
            })
            .await
            .unwrap();
        client
            .summarize(SummarizeRequest {
                text: "event".into(),
                max_sentences: 0,
                focus: None,
                locale: Locale::En,
                source_type: None,
                target: target(TargetKind::DailyRefineEvents),
            })
            .await
            .unwrap();
        client.flush().await.unwrap();

        let queued = std::fs::read_to_string(client.queue_path()).unwrap();
        let tasks: Vec<QueueTask> = queued
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(tasks.len(), 5, "one task per kind enqueued");
        for mut task in tasks {
            assert_eq!(
                task.generation_policy().unwrap(),
                Some(task.kind.generation_policy())
            );
            task.validate_retained_input().unwrap();
            let original_hash = task.cache_hash.clone();
            task.input.remove("generation_policy");
            assert_eq!(task.generation_policy().unwrap(), None);
            task.validate_retained_input().unwrap();
            assert_eq!(task.cache_hash, original_hash);
            assert_eq!(
                task.input.get("locale").and_then(|v| v.as_str()),
                Some("en"),
                "task {} ({:?}) carries no vault language",
                task.task_id,
                task.kind
            );
        }
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
    async fn invalid_queue_entry_preserves_buffer_without_temporary_files() {
        let dir = TempDir::new().unwrap();
        let client = QueueLlmClient::new(dir.path().to_path_buf());
        client
            .summarize(SummarizeRequest {
                text: "x".into(),
                max_sentences: 5,
                locale: Locale::Ko,
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
        assert!(result.is_err(), "a queue entry must be a regular file");
        assert!(
            !any_tmp_in(dir.path()),
            "rejected admission must leave no temporary file"
        );
        std::fs::remove_dir(client.queue_path()).unwrap();
        client.flush().await.unwrap();
        assert_eq!(read_pending_tasks(dir.path()).unwrap().len(), 1);
    }
}
