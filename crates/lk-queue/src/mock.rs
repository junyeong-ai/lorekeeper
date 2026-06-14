use async_trait::async_trait;

use lk_core::concept::ExtractedConcept;

use crate::{ExtractConceptsRequest, LlmClient, QueueError, SummarizeRequest, Theme, ThemeRequest};

#[derive(Default)]
pub struct MockLlmClient {
    pub summary: String,
    pub concepts: Vec<ExtractedConcept>,
    pub themes: Vec<Theme>,
    pub fail: bool,
}

impl MockLlmClient {
    pub fn build_with_concepts(concepts: Vec<ExtractedConcept>) -> Self {
        Self {
            concepts,
            ..Default::default()
        }
    }

    pub fn build_failing() -> Self {
        Self {
            fail: true,
            ..Default::default()
        }
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn summarize(&self, _req: SummarizeRequest) -> Result<String, QueueError> {
        if self.fail {
            return Err(QueueError::Api("mock failure".into()));
        }
        Ok(self.summary.clone())
    }

    async fn extract_concepts(
        &self,
        _req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, QueueError> {
        if self.fail {
            return Err(QueueError::Api("mock failure".into()));
        }
        Ok(self.concepts.clone())
    }

    async fn identify_themes(&self, _req: ThemeRequest) -> Result<Vec<Theme>, QueueError> {
        if self.fail {
            return Err(QueueError::Api("mock failure".into()));
        }
        Ok(self.themes.clone())
    }
}
