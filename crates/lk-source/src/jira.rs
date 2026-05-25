use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use crate::credentials::JiraCredentials;
use crate::{ExtractContext, Source, SourceError};

pub struct JiraSource {
    http: reqwest::Client,
    creds: JiraCredentials,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JiraParams {
    jql: String,
    #[serde(default = "default_fields")]
    fields: Vec<String>,
    #[serde(default = "default_max")]
    max_results: u32,
    /// Jira "start date" custom-field id (instance-specific, e.g. `customfield_10015`).
    /// Unset → start date is simply not shown. Avoids guessing a field id that means
    /// something different on another Jira instance.
    #[serde(default)]
    start_date_field: Option<String>,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    serde_json::from_value::<JiraParams>(params.clone())
        .map(|_| ())
        .map_err(|e| SourceError::InvalidParams(e.to_string()))
}

fn default_fields() -> Vec<String> {
    [
        "summary",
        "status",
        "priority",
        "labels",
        "updated",
        "duedate",
        "assignee",
        "description",
    ]
    .into_iter()
    .map(Into::into)
    .collect()
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
    /// Jira Cloud v3 returns rich text as an Atlassian Document Format tree
    /// (`{type:"doc", content:[…]}`), not a plain string — hence `Value`.
    description: Option<serde_json::Value>,
    status: Option<NameField>,
    priority: Option<NameField>,
    labels: Option<Vec<String>>,
    updated: Option<String>,
    duedate: Option<String>,
    assignee: Option<UserField>,
    /// Remaining fields (custom fields like the configured start-date field) by raw id,
    /// extracted dynamically since the id is instance-specific.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct NameField {
    name: Option<String>,
}

#[derive(Deserialize)]
struct UserField {
    #[serde(rename = "accountId")]
    account_id: Option<String>,
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
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: JiraParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let url = format!(
            "{}/rest/api/3/search/jql",
            self.creds.base_url.trim_end_matches('/')
        );
        // Append the configured start-date field (if any) so the API returns it; it's
        // extracted by raw id afterward since the id is instance-specific.
        let mut fields = p.fields.clone();
        if let Some(sdf) = &p.start_date_field {
            fields.push(sdf.clone());
        }
        let fields_csv = fields.join(",");

        let myself_url = format!(
            "{}/rest/api/3/myself",
            self.creds.base_url.trim_end_matches('/')
        );
        let my_account_id: Option<String> = match self
            .http
            .get(&myself_url)
            .basic_auth(&self.creds.email, Some(&self.creds.api_token))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v.get("accountId")?.as_str().map(String::from)),
            _ => None,
        };

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
            .filter_map(|issue| {
                let summary = issue.fields.summary.as_deref().unwrap_or("(no summary)");

                let Some(ts) = issue
                    .fields
                    .updated
                    .as_deref()
                    .and_then(|s| s.parse::<jiff::Timestamp>().ok())
                else {
                    tracing::warn!(issue_key = %issue.key, "jira: skipping issue with unparseable timestamp");
                    return None;
                };

                let assignee = issue.fields.assignee.as_ref();
                let assignee_aid = assignee.and_then(|a| a.account_id.as_deref());
                let is_me = my_account_id
                    .as_deref()
                    .is_some_and(|me| assignee_aid == Some(me));
                let author = if is_me {
                    Some(self.creds.email.clone())
                } else {
                    assignee
                        .and_then(|a| a.email_address.as_deref().or(a.display_name.as_deref()))
                        .map(String::from)
                };

                let status = issue.fields.status.and_then(|s| s.name);
                let description = issue
                    .fields
                    .description
                    .as_ref()
                    .map(crate::markdown::adf_to_markdown)
                    .unwrap_or_default();
                let start_date = p
                    .start_date_field
                    .as_ref()
                    .and_then(|f| issue.fields.extra.get(f))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                // Snapshot the status + planned window as a header. These are the values
                // *as of this run* — the page is a daily record, so a later schedule change
                // doesn't rewrite history.
                let body = with_status_header(
                    ctx.locale.strings(),
                    status.as_deref(),
                    start_date.as_deref(),
                    issue.fields.duedate.as_deref(),
                    &description,
                );

                let assignee_account_id = issue
                    .fields
                    .assignee
                    .as_ref()
                    .and_then(|a| a.account_id.clone());

                Some(RawItem {
                    external_id: Some(issue.key.clone()),
                    title: format!("[{}] {}", issue.key, summary),
                    body,
                    url: Some(format!("{base}/browse/{}", issue.key)),
                    author,
                    timestamp: ts,
                    metadata: serde_json::json!({
                        "status": status,
                        "priority": issue.fields.priority.and_then(|p| p.name),
                        "labels": issue.fields.labels,
                        "duedate": issue.fields.duedate,
                        "start_date": start_date,
                        "assignee_account_id": assignee_account_id,
                        "is_self": is_me,
                    }),
                })
            })
            .collect();

        Ok(items)
    }
}

/// Prefix the description with a one-line status/period snapshot (`**상태**: … · **기간**:
/// start ~ due`). Either part is omitted when absent; with neither, the description is
/// returned unchanged.
fn with_status_header(
    s: &lk_core::i18n::Strings,
    status: Option<&str>,
    start: Option<&str>,
    due: Option<&str>,
    description: &str,
) -> String {
    let mut header = String::new();
    if let Some(st) = status {
        header.push_str(&format!("**{}**: {st}", s.status));
    }
    let period = match (start, due) {
        (Some(a), Some(b)) => Some(format!("{a} ~ {b}")),
        (Some(a), None) => Some(format!("{a} ~")),
        (None, Some(b)) => Some(format!("~ {b}")),
        (None, None) => None,
    };
    if let Some(p) = period {
        if !header.is_empty() {
            header.push_str(" · ");
        }
        header.push_str(&format!("**{}**: {p}", s.period));
    }
    match (header.is_empty(), description.is_empty()) {
        (true, _) => description.to_string(),
        (false, true) => header,
        (false, false) => format!("{header}\n\n{description}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_header_combines_status_and_period() {
        let s = lk_core::i18n::Locale::Ko.strings();
        let b = with_status_header(
            s,
            Some("진행 중"),
            Some("2026-05-18"),
            Some("2026-05-22"),
            "본문",
        );
        assert_eq!(
            b,
            "**상태**: 진행 중 · **기간**: 2026-05-18 ~ 2026-05-22\n\n본문"
        );
    }

    #[test]
    fn status_header_omits_missing_parts() {
        let s = lk_core::i18n::Locale::Ko.strings();
        assert_eq!(with_status_header(s, None, None, None, "본문"), "본문");
        assert_eq!(
            with_status_header(s, Some("완료"), None, None, ""),
            "**상태**: 완료"
        );
        assert_eq!(
            with_status_header(s, None, None, Some("2026-05-22"), "x"),
            "**기간**: ~ 2026-05-22\n\nx"
        );
    }

    #[test]
    fn valid_params_accepted() {
        let params = serde_json::json!({ "jql": "assignee = currentUser()" });
        assert!(validate_params(&params).is_ok());
    }

    #[test]
    fn missing_required_field_rejected() {
        // `jql` is required; omitting it must fail validation, not at runtime.
        let params = serde_json::json!({ "max_results": 10 });
        assert!(validate_params(&params).is_err());
    }

    #[test]
    fn wrong_type_rejected() {
        let params = serde_json::json!({ "jql": "x", "max_results": "fifty" });
        assert!(validate_params(&params).is_err());
    }

    #[test]
    fn typo_key_rejected() {
        // `deny_unknown_fields` catches misspelled params (max_result vs max_results).
        let params = serde_json::json!({ "jql": "x", "max_result": 10 });
        assert!(validate_params(&params).is_err());
    }
}
