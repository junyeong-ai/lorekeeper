use async_trait::async_trait;
use serde::Deserialize;

use lk_core::concept::ExtractedConcept;

use crate::{
    ClassifyRequest, ExtractConceptsRequest, LlmClient, LlmError, SummarizeRequest, Theme,
    ThemeRequest,
};

pub struct AnthropicClient {
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

impl AnthropicClient {
    pub fn new(config: &lk_core::config::LlmConfig) -> Result<Self, LlmError> {
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
                .header("anthropic-version", "2024-10-22")
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
                let status = resp.status();
                let text = resp
                    .text()
                    .await
                    .unwrap_or_else(|e| format!("{status}: <body read failed: {e}>"));
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
impl LlmClient for AnthropicClient {
    async fn summarize(&self, req: SummarizeRequest) -> Result<String, LlmError> {
        self.call(
            "You are a concise summarizer. Output only the summary, no preamble.",
            &format!(
                "Summarize in at most {} bullet points.{}\n\n{}",
                req.max_sentences,
                focus_clause(&req.focus),
                req.text
            ),
        )
        .await
    }

    async fn extract_concepts(
        &self,
        req: ExtractConceptsRequest,
    ) -> Result<Vec<ExtractedConcept>, LlmError> {
        let mut system = format!(
            "Extract the key named entities, topics, and concepts.{}",
            focus_clause(&req.focus)
        );

        if !req.existing_concepts.is_empty() {
            system.push_str(&existing_clause(&req.existing_concepts));
        }

        if !req.categories.is_empty() {
            let cat_list: String = req
                .categories
                .iter()
                .map(|c| format!("{}: {}", c.id, c.label))
                .collect::<Vec<_>>()
                .join(", ");
            system.push_str(&format!(
                r#" Assign exactly one category from [{cat_list}]. Output JSON array: [{{"name":"...","slug":"...","category":"..."}}]. ONLY the JSON array."#
            ));
        } else {
            system.push_str(
                r#" Output JSON array: [{"name":"...","slug":"..."}]. ONLY the JSON array."#,
            );
        }

        let resp = self.call(&system, &req.text).await?;
        serde_json::from_str(strip_code_fences(&resp))
            .map_err(|e| LlmError::Api(format!("concept parse: {e}")))
    }

    async fn identify_themes(&self, req: ThemeRequest) -> Result<Vec<Theme>, LlmError> {
        let system = format!(
            r#"Identify the top themes from the text below. Output a JSON array of at most {} objects: [{{"title":"...","description":"..."}}]. ONLY the JSON array."#,
            req.max_themes
        );
        let resp = self.call(&system, &req.text).await?;
        serde_json::from_str(strip_code_fences(&resp))
            .map_err(|e| LlmError::Api(format!("themes parse: {e}")))
    }

    async fn classify(&self, req: ClassifyRequest) -> Result<Option<String>, LlmError> {
        let categories = req.categories.join(", ");
        let system = format!(
            "Classify the following into exactly one of these categories: [{categories}]. \
             Output ONLY the category name, nothing else. If none fit, output \"null\"."
        );
        let text = format!("Title: {}\n\n{}", req.title, req.excerpt);
        let result = self.call(&system, &text).await?;
        let cat = result.trim().to_string();
        if cat == "null" || !req.categories.contains(&cat) {
            Ok(None)
        } else {
            Ok(Some(cat))
        }
    }
}

fn existing_clause(existing: &[crate::ExistingConceptRef]) -> String {
    let names: Vec<String> = existing
        .iter()
        .map(|c| format!("{} ({})", c.name, c.slug))
        .collect();
    format!(
        " Existing concepts (reuse exact name+slug when the entity matches, do NOT create duplicates): [{}].",
        names.join(", ")
    )
}

/// Relevance filter clause appended to the system prompt. Normalized by
/// `SourceConfig::normalized_focus` (never blank), so no trimming is needed here.
fn focus_clause(focus: &Option<String>) -> String {
    match focus {
        Some(f) => {
            format!(" Limit to content matching this focus: {f}; ignore anything off-topic.")
        }
        None => String::new(),
    }
}
