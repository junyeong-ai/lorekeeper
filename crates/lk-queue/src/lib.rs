#[cfg(feature = "test-util")]
pub mod mock;
mod noop;
mod queue;

#[cfg(feature = "test-util")]
pub use mock::MockLlmClient;
pub use noop::NoopLlmClient;
pub use queue::{QueueLlmClient, QueueTask, TaskKind};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use lk_core::concept::ExtractedConcept;
use lk_core::config::SourceType;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("{0}")]
    Api(String),
    #[error("queue I/O: {0}")]
    QueueIo(String),
}

impl LlmError {
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

    /// Whether this task fills an initially-empty section (`FillEmpty`) or rewrites
    /// a section that the deterministic render already populates (`InPlace`).
    ///
    /// This distinction is the whole reason the cache has two shapes. A fill-empty
    /// section signals completion by being non-empty, so the pipeline records its
    /// hash in `llm_inputs_key()` at render time. An in-place rewrite (the daily
    /// event list) is structurally non-empty from the first render, so emptiness
    /// can't signal completion: the pipeline still pre-stamps `llm_inputs_key()`
    /// with the *current-input* hash (the stale-task reference point), and
    /// `/lore-process` writes `completion_key()` once it has actually rewritten the
    /// bodies. Cache hit ⟺ the two keys agree.
    pub fn cache_shape(self) -> CacheShape {
        // Exhaustive (no wildcard) so a new kind is compiler-forced to declare its
        // shape — a new in-place rewrite must not silently default to FillEmpty.
        match self {
            TargetKind::DailyRefineEvents => CacheShape::InPlace {
                completion_key: "refine_events_done",
            },
            TargetKind::DailySummary
            | TargetKind::DailyConcepts
            | TargetKind::WeeklySynthesisThemes
            | TargetKind::WeeklyReviewNarrative
            | TargetKind::MonthlyReviewNarrative
            | TargetKind::QuarterlyReviewNarrative
            | TargetKind::AnnualReviewNarrative
            | TargetKind::WorkLogSynthesis
            | TargetKind::DocumentSummary
            | TargetKind::DocumentConcepts => CacheShape::FillEmpty,
        }
    }
}

/// How a task's section reaches "done" — see [`TargetKind::cache_shape`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheShape {
    /// Section starts empty; non-empty body == done. Pipeline pre-stamps the hash.
    FillEmpty,
    /// Section starts populated by the render; a separate `completion_key`
    /// frontmatter field, written by `/lore-process`, marks the rewrite done.
    InPlace { completion_key: &'static str },
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

/// Compact reference to an existing concept, passed to the LLM so it can reuse
/// established names instead of creating duplicates with variant spellings.
/// `aliases` carries the registered synonyms/abbreviations (e.g. `RAG` for
/// `retrieval-augmented-generation`) beyond the title, so a surface form that only
/// matches an alias is recognized as the existing concept rather than re-created under
/// a new slug — without aliases here, the merge/audit alias machinery would resolve old
/// links but not stop a fresh extraction from forking a duplicate page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistingConceptRef {
    pub slug: String,
    pub name: String,
    pub aliases: Vec<String>,
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
    /// Existing concept slugs+names. The LLM should reuse an existing entry when
    /// the extracted entity matches, preventing duplicate concept pages.
    pub existing_concepts: Vec<ExistingConceptRef>,
    /// Valid category IDs the LLM may assign to each concept. Empty = no categorization.
    pub categories: Vec<CategoryRef>,
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
//   field the skill needs to do its work: the cache identity PLUS payload hints
//   that steer HOW the skill works but don't change the output's cache identity
//   (the existing-concepts dedup registry; the originating `source_type` used to
//   pick a synthesis strategy).
// - `cache_identity()` — what `cache_hash` digests. Restricted to the fields that
//   determine WHETHER the output would differ. Per-page-invariant context
//   (`source_type` never changes for a given page) and registry hints are excluded,
//   so they can't cause spurious cache misses (and excluding `source_type` avoids a
//   full re-hash of every page just to record a value that never varies per page).

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
    /// Queue payload: identity fields PLUS the source type and existing-concepts
    /// registry hint the skill needs to apply source-type-aware extraction and reuse
    /// slugs instead of inventing variants.
    pub fn task_input(&self) -> serde_json::Value {
        let mut existing = self.existing_concepts.clone();
        existing.sort_by(|a, b| a.slug.cmp(&b.slug));

        let mut v = match self.cache_identity() {
            serde_json::Value::Object(m) => m,
            _ => unreachable!("cache_identity always returns an object"),
        };
        v.insert(
            "source_type".into(),
            serde_json::to_value(self.source_type).expect("serializable"),
        );
        if !existing.is_empty() {
            v.insert(
                "existing_concepts".into(),
                serde_json::to_value(&existing).expect("serializable"),
            );
        }
        serde_json::Value::Object(v)
    }

    /// Hashable identity. `categories` is sorted by `id` so configuration ordering
    /// can't perturb the hash. `existing_concepts` is excluded by design — it is a
    /// dedup hint, not part of the prompt's identity. Including it would invalidate
    /// every cache hit as soon as ANY new concept appears in the vault.
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
    async fn summarize(&self, req: SummarizeRequest) -> Result<String, LlmError>;

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError>;

    /// Open a per-source transaction boundary. The CLI calls this immediately before it
    /// plans a source. Buffered tasks accumulate across sources for one atomic flush, but
    /// a source whose plan fails PARTWAY has already buffered some tasks pointing at pages
    /// that will never be written — flushing them would break the invariant that a queued
    /// task always targets a written page. Pairing this with [`rollback_source`] lets the
    /// CLI discard exactly that source's tasks while keeping earlier sources' valid ones.
    /// Default no-op: providers that don't buffer (noop/mock) need no boundary.
    async fn begin_source(&self) {}

    /// Discard every task buffered since the last [`begin_source`], used when a source's
    /// plan errors so its half-produced tasks never reach the flushed queue file. Default
    /// no-op.
    async fn rollback_source(&self) {}

    /// Extract structured themes from combined multi-source text. Returns a JSON-parsed
    /// list of themes with titles and descriptions. The default returns an empty vec,
    /// which suffices for noop and mock clients; queue mode emits a deferred task and
    /// returns empty (the skill fills the section later).
    async fn identify_themes(&self, _req: ThemeRequest) -> Result<Vec<Theme>, LlmError> {
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
    async fn flush(&self) -> Result<(), LlmError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concept_req(existing: Vec<ExistingConceptRef>) -> ExtractConceptsRequest {
        ExtractConceptsRequest {
            text: "body".into(),
            source_id: "s".into(),
            source_type: SourceType::Rss,
            date: jiff::civil::date(2026, 5, 23),
            focus: None,
            target: TaskTarget {
                vault_path: "daily/s/2026-05-23.md".into(),
                kind: TargetKind::DailyConcepts,
                anchor: "## Concepts".into(),
            },
            existing_concepts: existing,
            categories: vec![],
        }
    }

    #[test]
    fn existing_concepts_are_payload_only_and_sorted() {
        let with = concept_req(vec![
            ExistingConceptRef {
                slug: "zeta".into(),
                name: "Zeta".into(),
                aliases: vec![],
            },
            ExistingConceptRef {
                slug: "alpha".into(),
                name: "Alpha".into(),
                aliases: vec![],
            },
        ]);
        let without = concept_req(vec![]);

        // Registry hints never shape the cache identity: a growing concept registry
        // must not invalidate unrelated extraction caches.
        assert_eq!(with.cache_hash(), without.cache_hash());
        assert!(with.cache_identity().get("existing_concepts").is_none());

        // …but the skill payload carries them, sorted by slug so the queue file is
        // byte-deterministic regardless of registry scan order.
        let input = with.task_input();
        let slugs: Vec<&str> = input["existing_concepts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, ["alpha", "zeta"]);
    }
}
