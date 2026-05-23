use async_trait::async_trait;
use serde::Deserialize;

use wi_core::config::SourceType;
use wi_core::event::RawItem;

use crate::credentials::JiraCredentials;
use crate::{ExtractContext, Source, SourceError};

pub struct JiraSource {
    http: reqwest::Client,
    creds: JiraCredentials,
}

#[derive(Debug, Deserialize)]
struct JiraParams {
    jql: String,
    #[serde(default = "default_fields")]
    fields: Vec<String>,
    #[serde(default = "default_max")]
    max_results: u32,
}

fn default_fields() -> Vec<String> {
    vec![
        "summary".into(),
        "status".into(),
        "priority".into(),
        "labels".into(),
        "updated".into(),
        "assignee".into(),
        "description".into(),
    ]
}

fn default_max() -> u32 {
    50
}

#[derive(Deserialize)]
struct SearchResult {
    issues: Option<Vec<Issue>>,
}

#[derive(Deserialize)]
struct Issue {
    key: String,
    fields: IssueFields,
}

#[derive(Deserialize)]
struct IssueFields {
    summary: Option<String>,
    description: Option<String>,
    status: Option<NameField>,
    priority: Option<NameField>,
    labels: Option<Vec<String>>,
    updated: Option<String>,
    assignee: Option<UserField>,
}

#[derive(Deserialize)]
struct NameField {
    name: Option<String>,
}

#[derive(Deserialize)]
struct UserField {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

impl JiraSource {
    pub fn new(http: reqwest::Client, creds: JiraCredentials) -> Self {
        Self { http, creds }
    }
}

#[async_trait]
impl Source for JiraSource {
    fn source_type(&self) -> SourceType {
        SourceType::Jira
    }

    async fn extract(
        &self,
        params: &serde_json::Value,
        _ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: JiraParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let url = format!(
            "{}/rest/api/3/search",
            self.creds.base_url.trim_end_matches('/')
        );
        let fields_csv = p.fields.join(",");

        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.creds.email, Some(&self.creds.api_token))
            .query(&[
                ("jql", p.jql.as_str()),
                ("maxResults", &p.max_results.to_string()),
                ("fields", &fields_csv),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SourceError::Api {
                status,
                message: format!("Jira search failed: {body}"),
            });
        }

        let result: SearchResult = resp
            .json()
            .await
            .map_err(|e| SourceError::Parse(e.to_string()))?;

        let issues = result.issues.unwrap_or_default();
        tracing::info!(count = issues.len(), "jira: issues found");

        let base = self.creds.base_url.trim_end_matches('/');
        let items = issues
            .into_iter()
            .map(|issue| {
                let summary = issue.fields.summary.as_deref().unwrap_or("(no summary)");

                let ts = issue
                    .fields
                    .updated
                    .as_deref()
                    .and_then(|s| s.parse::<jiff::Timestamp>().ok())
                    .unwrap_or_else(jiff::Timestamp::now);

                let author = issue
                    .fields
                    .assignee
                    .as_ref()
                    .and_then(|a| a.display_name.as_deref().or(a.email_address.as_deref()))
                    .map(String::from);

                RawItem {
                    external_id: Some(issue.key.clone()),
                    title: format!("[{}] {}", issue.key, summary),
                    body: issue.fields.description.unwrap_or_default(),
                    url: Some(format!("{base}/browse/{}", issue.key)),
                    author,
                    timestamp: ts,
                    metadata: serde_json::json!({
                        "status": issue.fields.status.and_then(|s| s.name),
                        "priority": issue.fields.priority.and_then(|p| p.name),
                        "labels": issue.fields.labels,
                    }),
                }
            })
            .collect();

        Ok(items)
    }
}
