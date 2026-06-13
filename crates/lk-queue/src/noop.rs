use async_trait::async_trait;

use lk_core::concept::ExtractedConcept;

use crate::{ExtractConceptsRequest, LlmClient, QueueError, SummarizeRequest};

pub struct NoopLlmClient;

#[async_trait]
impl LlmClient for NoopLlmClient {
    async fn summarize(&self, _req: SummarizeRequest) -> Result<String, QueueError> {
        Ok(String::new())
    }

    async fn extract_concepts(
        &self,
        _req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, QueueError> {
        Ok(vec![])
    }
}
