mod claude;
pub mod mock;
mod noop;

pub use claude::ClaudeClient;
pub use mock::MockLlmClient;
pub use noop::NoopLlmClient;

use async_trait::async_trait;
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
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn summarize(&self, text: &str, max_sentences: usize) -> Result<String, LlmError>;

    async fn classify_labels(
        &self,
        text: &str,
        candidates: &[String],
    ) -> Result<Vec<String>, LlmError>;

    async fn extract_concepts(&self, text: &str) -> Result<Vec<ExtractedConcept>, LlmError>;
}
