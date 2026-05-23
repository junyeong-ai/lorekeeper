use async_trait::async_trait;

use wi_core::concept::ExtractedConcept;

use crate::{ExtractConceptsRequest, LlmClient, LlmError, SummarizeRequest};

#[derive(Default)]
pub struct MockLlmClient {
    pub summary: String,
    pub concepts: Vec<ExtractedConcept>,
    pub fail: bool,
}

impl MockLlmClient {
    pub fn with_concepts(concepts: Vec<ExtractedConcept>) -> Self {
        Self {
            concepts,
            ..Default::default()
        }
    }

    pub fn failing() -> Self {
        Self {
            fail: true,
            ..Default::default()
        }
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn summarize(&self, _req: SummarizeRequest) -> Result<String, LlmError> {
        if self.fail {
            return Err(LlmError::Api("mock failure".into()));
        }
        Ok(self.summary.clone())
    }

    async fn extract_concepts(
        &self,
        _req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError> {
        if self.fail {
            return Err(LlmError::Api("mock failure".into()));
        }
        Ok(self.concepts.clone())
    }
}
