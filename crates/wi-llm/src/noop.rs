use async_trait::async_trait;

use wi_core::concept::ExtractedConcept;

use crate::{ClassifyLabelsRequest, ExtractConceptsRequest, LlmClient, LlmError, SummarizeRequest};

pub struct NoopLlmClient;

#[async_trait]
impl LlmClient for NoopLlmClient {
    async fn summarize(&self, _req: SummarizeRequest) -> Result<String, LlmError> {
        Ok(String::new())
    }

    async fn classify_labels(&self, _req: ClassifyLabelsRequest) -> Result<Vec<String>, LlmError> {
        Ok(vec![])
    }

    async fn extract_concepts(
        &self,
        _req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError> {
        Ok(vec![])
    }
}
