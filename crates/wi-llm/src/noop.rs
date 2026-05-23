use async_trait::async_trait;

use wi_core::concept::ExtractedConcept;

use crate::{LlmClient, LlmError};

pub struct NoopLlmClient;

#[async_trait]
impl LlmClient for NoopLlmClient {
    async fn summarize(&self, _text: &str, _max: usize) -> Result<String, LlmError> {
        Ok(String::new())
    }

    async fn classify_labels(
        &self,
        _text: &str,
        _candidates: &[String],
    ) -> Result<Vec<String>, LlmError> {
        Ok(vec![])
    }

    async fn extract_concepts(&self, _text: &str) -> Result<Vec<ExtractedConcept>, LlmError> {
        Ok(vec![])
    }
}
