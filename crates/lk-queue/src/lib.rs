#[cfg(feature = "test-util")]
pub mod mock;
mod noop;
mod queue;

#[cfg(feature = "test-util")]
pub use mock::MockLlmClient;
pub use noop::NoopLlmClient;
pub use queue::{
    CORRUPT_SUBDIR, PROCESSED_SUBDIR, QueueLlmClient, QueueTask, RESULTS_SUBDIR, ReportedConcept,
    ResultBatch, TaskKind, TaskResult, read_results, write_tasks_atomic,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use lk_core::concept::ExtractedConcept;
use lk_core::config::SourceType;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("{0}")]
    Api(String),
    #[error("queue I/O: {0}")]
    QueueIo(String),
}

impl QueueError {
    pub fn is_fatal(&self) -> bool {
        matches!(self, QueueError::QueueIo(_))
    }
}

/// What kind of vault content a semantic task produces. Used for classification/logging;
/// the actual section heading is carried in `TaskTarget.anchor`.
///
/// `EnumIter` exists for the skill-contract tests: the iterator is macro-generated
/// from the variant list itself, so iterating the full kind space can never drift
/// from the enum — no hand-maintained variant array to forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumIter)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// Daily page summary body.
    DailySummary,
    /// Daily page concept wiki-links + concept page creation/merge.
    DailyConcepts,
    /// Cross-source weekly synthesis narrative.
    WeeklySynthesisThemes,
    /// Personal weekly review narrative.
    WeeklyReviewNarrative,
    /// Personal monthly review narrative.
    MonthlyReviewNarrative,
    /// Personal quarterly review narrative.
    QuarterlyReviewNarrative,
    /// Personal annual review narrative.
    AnnualReviewNarrative,
    /// Cross-source topic synthesis for a work-log page.
    WorkLogSynthesis,
    /// Refine event bodies in-place: translate to locale language, distill raw
    /// content to knowledge summaries, remove noise, keep source links.
    DailyRefineEvents,
    /// Document page summary.
    DocumentSummary,
    /// Document page concept wiki-links.
    DocumentConcepts,
}

impl TargetKind {
    /// The `llm_inputs.<key>` frontmatter field this task's result is cached under.
    /// One key per logical page section: the pipeline stamps the cache hash here at
    /// render time and the `/lore-process` skill compares against it (stale-task
    /// guard). This is the single source of truth for the mapping — the skill's key
    /// table mirrors it, and adding a `TargetKind` forces choosing its key here
    /// (compiler-checked exhaustiveness).
    pub fn llm_inputs_key(self) -> &'static str {
        match self {
            TargetKind::DailySummary | TargetKind::DocumentSummary => "summary",
            TargetKind::DailyRefineEvents => "refine_events",
            TargetKind::DailyConcepts | TargetKind::DocumentConcepts => "concepts",
            TargetKind::WorkLogSynthesis => "topic_summary",
            TargetKind::WeeklySynthesisThemes => "themes",
            TargetKind::WeeklyReviewNarrative
            | TargetKind::MonthlyReviewNarrative
            | TargetKind::QuarterlyReviewNarrative
            | TargetKind::AnnualReviewNarrative => "narrative",
        }
    }

    /// The `llm_inputs.<key>_done` frontmatter field that marks this section finished —
    /// always `llm_inputs_key()` + `_done`, DERIVED so the two can never drift (adding a
    /// `TargetKind` only ever touches `llm_inputs_key`).
    ///
    /// Completion is uniformly marker-signalled: the pipeline pre-stamps `llm_inputs_key()`
    /// with the current input hash (the stale-task reference), and `/lore-process` stamps
    /// this companion `*_done` key once it has finished — **even when the result is empty**.
    /// A cache hit is `*_done == llm_inputs_key()`, never inferred from the body being
    /// non-empty. This is the single completion model precisely because emptiness is not a
    /// reliable "not done" signal: an extraction (concepts, themes) can find nothing, a
    /// focus-filtered summary can match nothing, the work-log skips trivial events, a
    /// review narrative can be empty when its inputs are, and the event refine is non-empty
    /// from the first render. Tying completion to a body would re-enqueue every such empty
    /// result forever, so no kind does.
    pub fn completion_key(self) -> String {
        format!("{}_done", self.llm_inputs_key())
    }
}

/// Where the result of a semantic task should land in the vault. Carried through the
/// LlmClient trait so queue-mode tasks have full target context for skill processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTarget {
    pub vault_path: String,
    pub kind: TargetKind,
    /// The exact section heading the pipeline wrote (e.g. `"## Summary"` or `"## 요약"`),
    /// resolved from i18n at queue time. The `/lore-process` skill uses this as the
    /// locate key instead of a hardcoded kind→heading table, so locale changes never
    /// break the semantic plane.
    pub anchor: String,
}

/// Structured theme extraction from combined multi-source text. Used by weekly
/// synthesis to replace free-text parsing with reliable JSON output.
#[derive(Debug, Clone)]
pub struct ThemeRequest {
    pub text: String,
    pub max_themes: usize,
    /// Output language for the theme titles/descriptions (e.g. "ko", "en"). Derived
    /// from vault.locale. Part of the cache identity because the themes are prose
    /// written into a localized synthesis page — a locale switch must invalidate the
    /// cache, or the stale-task guard would freeze wrong-language themes forever.
    pub locale: String,
    pub target: TaskTarget,
}

/// A single theme extracted by `identify_themes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub title: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SummarizeRequest {
    pub text: String,
    pub max_sentences: usize,
    /// Optional natural-language relevance criterion (the source's `focus`). When
    /// set, the summary covers only content matching it and ignores off-topic
    /// items — so a broad source (e.g. a news aggregator) yields a focused digest.
    pub focus: Option<String>,
    /// Output language for the summary (e.g. "ko", "en"). Derived from vault.locale.
    pub locale: String,
    /// Originating source type, when the task summarizes a single source's content
    /// (daily / document). `None` for cross-source or derived tasks (work-log topic
    /// synthesis, period synthesis) where no single type applies. Lets the skill
    /// pick a source-type-aware synthesis strategy from the task instead of guessing
    /// from the vault path — so a config-driven source id like `eng-chat` is still
    /// recognized as Slack.
    pub source_type: Option<SourceType>,
    pub target: TaskTarget,
}

/// Category definition passed to the LLM for concept classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryReference {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct ExtractConceptsRequest {
    pub text: String,
    pub source_id: String,
    /// Originating source type. Concept extraction always runs against a single
    /// source (daily / document), so this is never absent. Lets the skill apply
    /// source-type-aware extraction without inferring from the vault path.
    pub source_type: SourceType,
    pub date: jiff::civil::Date,
    /// Optional natural-language relevance criterion (the source's `focus`). When
    /// set, concepts are extracted only from content matching it, so off-topic
    /// items in a broad source never pollute the knowledge graph.
    pub focus: Option<String>,
    pub target: TaskTarget,
    /// Valid category IDs the LLM may assign to each concept. Empty = no categorization.
    pub categories: Vec<CategoryReference>,
}

/// Content-addressable hash of an LLM task's cache identity. The page-side
/// frontmatter (`llm_inputs.<key>`) records the same value, so a re-ingest whose
/// input is unchanged matches exactly and the LLM call is skipped. 32 hex chars
/// of BLAKE3 — 128 bits.
///
/// 128 bits is overkill for per-page collision avoidance, but a single false
/// positive preserves stale LLM content forever under a colliding new hash, so
/// the extra 16 bytes per cache key is trivial insurance against a severe failure.
pub fn cache_hash(identity: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(identity).expect("serde_json::Value always serializes");
    let hex = blake3::hash(&bytes).to_hex();
    hex[..32].to_string()
}

// Each request type exposes two JSON projections:
//
// - `task_input()` — what the queue serializes for `/lore-process`. Carries every
//   field the skill needs to do its work: the cache identity PLUS the originating
//   `source_type` — a payload field that steers HOW the skill extracts (per-source-type
//   scoping) but doesn't change the output's cache identity.
// - `cache_identity()` — what `cache_hash` digests. Restricted to the fields that
//   determine WHETHER the output would differ. Per-page-invariant context
//   (`source_type` never changes for a given page) is excluded, so it can't cause
//   spurious cache misses and never triggers a full re-hash to record a value that
//   never varies per page.

impl SummarizeRequest {
    pub fn task_input(&self) -> serde_json::Value {
        let mut v = match self.cache_identity() {
            serde_json::Value::Object(m) => m,
            _ => unreachable!("cache_identity always returns an object"),
        };
        if let Some(st) = self.source_type {
            v.insert(
                "source_type".into(),
                serde_json::to_value(st).expect("serializable"),
            );
        }
        serde_json::Value::Object(v)
    }

    pub fn cache_identity(&self) -> serde_json::Value {
        let mut v = serde_json::Map::new();
        v.insert("text".into(), self.text.clone().into());
        v.insert("max_sentences".into(), self.max_sentences.into());
        v.insert("locale".into(), self.locale.clone().into());
        if let Some(focus) = &self.focus {
            v.insert("focus".into(), focus.clone().into());
        }
        serde_json::Value::Object(v)
    }

    pub fn cache_hash(&self) -> String {
        cache_hash(&self.cache_identity())
    }
}

impl ExtractConceptsRequest {
    /// Queue payload: identity fields PLUS the source type the skill needs to apply
    /// source-type-aware extraction.
    pub fn task_input(&self) -> serde_json::Value {
        let mut v = match self.cache_identity() {
            serde_json::Value::Object(m) => m,
            _ => unreachable!("cache_identity always returns an object"),
        };
        v.insert(
            "source_type".into(),
            serde_json::to_value(self.source_type).expect("serializable"),
        );
        serde_json::Value::Object(v)
    }

    /// Hashable identity. `categories` is sorted by `id` so configuration ordering
    /// can't perturb the hash.
    pub fn cache_identity(&self) -> serde_json::Value {
        let mut categories = self.categories.clone();
        categories.sort_by(|a, b| a.id.cmp(&b.id));

        let mut v = serde_json::Map::new();
        v.insert("text".into(), self.text.clone().into());
        v.insert("source_id".into(), self.source_id.clone().into());
        v.insert("date".into(), self.date.to_string().into());
        if let Some(focus) = &self.focus {
            v.insert("focus".into(), focus.clone().into());
        }
        if !categories.is_empty() {
            v.insert(
                "categories".into(),
                serde_json::to_value(&categories).expect("serializable"),
            );
        }
        serde_json::Value::Object(v)
    }

    pub fn cache_hash(&self) -> String {
        cache_hash(&self.cache_identity())
    }
}

impl ThemeRequest {
    pub fn task_input(&self) -> serde_json::Value {
        self.cache_identity()
    }

    pub fn cache_identity(&self) -> serde_json::Value {
        let mut v = serde_json::Map::new();
        v.insert("text".into(), self.text.clone().into());
        v.insert("max_themes".into(), self.max_themes.into());
        v.insert("locale".into(), self.locale.clone().into());
        serde_json::Value::Object(v)
    }

    pub fn cache_hash(&self) -> String {
        cache_hash(&self.cache_identity())
    }
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn summarize(&self, req: SummarizeRequest) -> Result<String, QueueError>;

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, QueueError>;

    /// Open a per-source transaction boundary. The CLI calls this immediately before it
    /// plans a source. Buffered tasks accumulate across sources for one atomic flush, but
    /// a source whose plan fails PARTWAY has already buffered some tasks pointing at pages
    /// that will never be written — flushing them would break the invariant that a queued
    /// task always targets a written page. Pairing this with [`Self::rollback_source`] lets
    /// the CLI discard exactly that source's tasks while keeping earlier sources' valid ones.
    /// Default no-op: providers that don't buffer (noop/mock) need no boundary.
    async fn begin_source(&self) {}

    /// Discard every task buffered since the last [`Self::begin_source`], used when a source's
    /// plan errors so its half-produced tasks never reach the flushed queue file. Default
    /// no-op.
    async fn rollback_source(&self) {}

    /// Extract structured themes from combined multi-source text. Returns a JSON-parsed
    /// list of themes with titles and descriptions. The default returns an empty vec,
    /// which suffices for noop and mock clients; queue mode emits a deferred task and
    /// returns empty (the skill fills the section later).
    async fn identify_themes(&self, _req: ThemeRequest) -> Result<Vec<Theme>, QueueError> {
        Ok(vec![])
    }

    /// Commit any buffered side-effects. The CLI MUST call this exactly once at the
    /// end of a successful ingest run, AFTER all vault writes have succeeded — and
    /// MUST NOT enqueue further tasks after calling. The queue client buffers tasks
    /// in memory and writes the JSONL file atomically here (temp + fsync + rename)
    /// so a mid-run abort drops orphan tasks instead of persisting them ahead of
    /// their target pages. A second `flush` after additional enqueues would truncate
    /// the on-disk file and silently lose the first batch — the trait deliberately
    /// requires single-call semantics rather than guarding internally, because there
    /// is no legitimate use case for a multi-flush ingest. Noop and mock clients
    /// leave this as the default no-op.
    async fn flush(&self) -> Result<(), QueueError> {
        Ok(())
    }
}
