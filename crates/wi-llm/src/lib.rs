mod claude;
pub mod mock;
mod noop;
mod queue;

pub use claude::ClaudeClient;
pub use mock::MockLlmClient;
pub use noop::NoopLlmClient;
pub use queue::QueueLlmClient;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use wi_core::concept::ExtractedConcept;

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

/// What kind of vault content a semantic task produces. The Claude Code skill uses
/// this to decide how to integrate the LLM result into the target page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// Daily page `## 요약` body.
    DailySummary,
    /// Daily page `## 관련 개념` wiki-links + concept page creation/merge.
    DailyConcepts,
    /// Cross-source weekly synthesis narrative.
    WeeklySynthesisNarrative,
    /// Personal weekly summary narrative.
    WeeklyPersonalNarrative,
    /// Monthly summary narrative.
    MonthlyNarrative,
    /// Quarterly review narrative.
    QuarterlyNarrative,
    /// Annual review narrative.
    AnnualNarrative,
}

/// Where the result of a semantic task should land in the vault. Carried through the
/// LlmClient trait so queue-mode tasks have full target context for skill processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTarget {
    pub vault_path: String,
    pub kind: TargetKind,
}

#[derive(Debug, Clone)]
pub struct SummarizeRequest {
    pub text: String,
    pub max_sentences: usize,
    pub target: TaskTarget,
}

#[derive(Debug, Clone)]
pub struct ClassifyLabelsRequest {
    pub text: String,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExtractConceptsRequest {
    pub text: String,
    pub source_id: String,
    pub date: jiff::civil::Date,
    pub target: TaskTarget,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn summarize(&self, req: SummarizeRequest) -> Result<String, LlmError>;

    async fn classify_labels(&self, req: ClassifyLabelsRequest) -> Result<Vec<String>, LlmError>;

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError>;
}
