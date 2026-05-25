mod anthropic;
pub mod mock;
mod noop;
mod queue;

pub use anthropic::AnthropicClient;
pub use mock::MockLlmClient;
pub use noop::NoopLlmClient;
pub use queue::QueueLlmClient;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use lk_core::concept::ExtractedConcept;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP: {0}")]
    Request(#[from] reqwest::Error),
    #[error("{0}")]
    Api(String),
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("queue I/O: {0}")]
    QueueIo(String),
}

impl LlmError {
    /// True for errors that must abort the pipeline run rather than fall back to an
    /// empty result. Persistence failures (queue write, dedup commit, etc.) are fatal
    /// because silently swallowing them would lose work — the daily page would render
    /// without semantic content AND the events would be marked seen in dedup, so re-runs
    /// would skip them and no queue task would exist for `/lore-process` to repair.
    ///
    /// Transient LLM failures (network, rate limit, API errors) are NOT fatal: the run
    /// continues with an empty result, the page renders without summary, but a future
    /// `lore ingest --force` can retry.
    pub fn is_fatal(&self) -> bool {
        matches!(self, LlmError::QueueIo(_))
    }
}

/// What kind of vault content a semantic task produces. Used for classification/logging;
/// the actual section heading is carried in `TaskTarget.anchor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// Daily page summary body.
    DailySummary,
    /// Daily page concept wiki-links + concept page creation/merge.
    DailyConcepts,
    /// Cross-source weekly synthesis narrative.
    WeeklySynthesisNarrative,
    /// Personal weekly review narrative.
    WeeklyPersonalNarrative,
    /// Personal monthly review narrative.
    MonthlyPersonalNarrative,
    /// Personal quarterly review narrative.
    QuarterlyPersonalNarrative,
    /// Personal annual review narrative.
    AnnualPersonalNarrative,
    /// Cross-source topic synthesis for a work-log page.
    WorkLogSynthesis,
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
    pub target: TaskTarget,
}

/// A single theme extracted by `identify_themes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub title: String,
    pub description: String,
}

/// Classification of a single event into one of the given categories.
/// Used as an LLM fallback when deterministic keyword matching produces no match.
/// Does NOT carry a `TaskTarget` because the result is an in-memory judgment applied
/// to `Event.work_category`, not a vault write.
#[derive(Debug, Clone)]
pub struct ClassifyRequest {
    pub title: String,
    pub excerpt: String,
    pub categories: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SummarizeRequest {
    pub text: String,
    pub max_sentences: usize,
    /// Optional natural-language relevance criterion (the source's `focus`). When
    /// set, the summary covers only content matching it and ignores off-topic
    /// items — so a broad source (e.g. a news aggregator) yields a focused digest.
    pub focus: Option<String>,
    pub target: TaskTarget,
}

/// Compact reference to an existing concept, passed to the LLM so it can reuse
/// established names instead of creating duplicates with variant spellings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistingConceptRef {
    pub slug: String,
    pub name: String,
}

/// Category definition passed to the LLM for concept classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryRef {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct ExtractConceptsRequest {
    pub text: String,
    pub source_id: String,
    pub date: jiff::civil::Date,
    /// Optional natural-language relevance criterion (the source's `focus`). When
    /// set, concepts are extracted only from content matching it, so off-topic
    /// items in a broad source never pollute the knowledge graph.
    pub focus: Option<String>,
    pub target: TaskTarget,
    /// Existing concept slugs+names. The LLM should reuse an existing entry when
    /// the extracted entity matches, preventing duplicate concept pages.
    pub existing_concepts: Vec<ExistingConceptRef>,
    /// Valid category IDs the LLM may assign to each concept. Empty = no categorization.
    pub categories: Vec<CategoryRef>,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn summarize(&self, req: SummarizeRequest) -> Result<String, LlmError>;

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError>;

    /// Extract structured themes from combined multi-source text. Returns a JSON-parsed
    /// list of themes with titles and descriptions. The default returns an empty vec,
    /// which suffices for noop and mock clients; queue mode emits a deferred task and
    /// returns empty (the skill fills the section later).
    async fn identify_themes(&self, _req: ThemeRequest) -> Result<Vec<Theme>, LlmError> {
        Ok(vec![])
    }

    /// Classify a single event into one of the given categories. Used as an LLM fallback
    /// when deterministic keyword matching produces no match. Returns `None` when the LLM
    /// declines to classify or the provider doesn't support synchronous inference (queue
    /// mode). Only `anthropic` mode performs an actual call.
    async fn classify(&self, _req: ClassifyRequest) -> Result<Option<String>, LlmError> {
        Ok(None)
    }

    /// Commit any buffered side-effects. The CLI calls this once at the end of a
    /// successful ingest run, AFTER all vault writes have succeeded. Clients that
    /// perform synchronous remote calls (anthropic, noop) leave this as the default
    /// no-op. The queue client buffers tasks in memory and writes the JSONL file
    /// atomically here (temp + fsync + rename) so a mid-run abort drops orphan tasks
    /// instead of persisting them ahead of their target pages.
    async fn flush(&self) -> Result<(), LlmError> {
        Ok(())
    }
}
