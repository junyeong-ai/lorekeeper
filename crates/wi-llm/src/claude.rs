use async_trait::async_trait;
use serde::Deserialize;

use wi_core::concept::ExtractedConcept;

use crate::{ExtractConceptsRequest, LlmClient, LlmError, SummarizeRequest};

pub struct ClaudeClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: Option<String>,
}

impl ClaudeClient {
    pub fn new(config: &wi_core::config::LlmConfig) -> Result<Self, LlmError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| LlmError::Api("ANTHROPIC_API_KEY not set".into()))?;
        Ok(Self {
            http: reqwest::Client::new(),
            api_key,
            model: config.model.clone(),
            max_tokens: config.max_tokens,
        })
    }

    async fn call(&self, system: &str, user: &str) -> Result<String, LlmError> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "messages": [{"role": "user", "content": user}]
        });

        let mut retries = 0u32;
        loop {
            let resp = self
                .http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await?;

            if resp.status().as_u16() == 429 {
                retries += 1;
                if retries > 3 {
                    return Err(LlmError::RateLimited {
                        retry_after_secs: 60,
                    });
                }
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2u64.pow(retries));
                tracing::warn!(retries, wait_secs = wait, "rate limited, backing off");
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                continue;
            }

            if !resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(LlmError::Api(text));
            }

            let api: ApiResponse = resp.json().await?;
            return Ok(api
                .content
                .into_iter()
                .filter_map(|b| b.text)
                .collect::<Vec<_>>()
                .join(""));
        }
    }
}

fn strip_code_fences(text: &str) -> &str {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        return rest.strip_suffix("```").unwrap_or(rest).trim();
    }
    if let Some(rest) = t.strip_prefix("```") {
        return rest.strip_suffix("```").unwrap_or(rest).trim();
    }
    t
}

#[async_trait]
impl LlmClient for ClaudeClient {
    async fn summarize(&self, req: SummarizeRequest) -> Result<String, LlmError> {
        self.call(
            "You are a concise summarizer. Output only the summary, no preamble.",
            &format!(
                "Summarize in at most {} bullet points:\n\n{}",
                req.max_sentences, req.text
            ),
        )
        .await
    }

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError> {
        let resp = self
            .call(
                r#"Extract named entities, technologies, key topics. Output JSON array: [{"name":"...","slug":"...","confidence":"extracted"|"inferred"}]. ONLY the JSON array."#,
                &req.text,
            )
            .await?;
        serde_json::from_str(strip_code_fences(&resp))
            .map_err(|e| LlmError::Api(format!("concept parse: {e}")))
    }
}
