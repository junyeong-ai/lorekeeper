use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::OnceCell;

use lk_core::event::RawItem;

use crate::credentials::JiraCredentials;
use crate::{ExtractContext, Source, SourceError};

pub struct JiraSource {
    http: reqwest::Client,
    creds: JiraCredentials,
    /// The authenticated user's `accountId`, fetched once and cached for the life
    /// of the source (it is invariant for fixed credentials).
    account_id: OnceCell<String>,
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
        Self {
            http,
            creds,
            account_id: OnceCell::new(),
        }
    }

    /// The authenticated user's Jira `accountId`, the exact-match ownership key.
    /// Fetched once via `/myself` and cached. A failure is PROPAGATED, never
    /// degraded to "no account id": collapsing an auth/connectivity/schema error
    /// into `None` would silently mark every assigned issue as not-self → not
    /// personal, zeroing a whole batch's contribution record with no signal —
    /// irrecoverable on an append-only review.
    async fn account_id(&self) -> Result<&str, SourceError> {
        self.account_id
            .get_or_try_init(|| async {
                let url = format!(
                    "{}/rest/api/3/myself",
                    self.creds.base_url.trim_end_matches('/')
                );
                let resp = crate::retry::send_with_retry(|| {
                    self.http
                        .get(&url)
                        .basic_auth(&self.creds.email, Some(&self.creds.api_token))
                        .send()
                })
                .await?;
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SourceError::Api {
                        status,
                        message: format!("Jira /myself failed: {body}"),
                    });
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(|e| SourceError::Parse(e.to_string()))?
                    .get("accountId")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or_else(|| {
                        SourceError::Parse("Jira /myself response missing accountId".into())
                    })
            })
            .await
            .map(String::as_str)
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

        let my_account_id = self.account_id().await?;

        let max_results = p.max_results.to_string();
        let resp = crate::retry::send_with_retry(|| {
            self.http
                .get(&url)
                .basic_auth(&self.creds.email, Some(&self.creds.api_token))
                .query(&[
                    ("jql", p.jql.as_str()),
                    ("maxResults", max_results.as_str()),
                    ("fields", fields_csv.as_str()),
                ])
                .send()
        })
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
                map_issue(
                    issue,
                    my_account_id,
                    base,
                    &self.creds.email,
                    p.start_date_field.as_deref(),
                    ctx.locale.strings(),
                )
            })
            .collect();

        Ok(items)
    }
}

/// Map one Jira issue to a `RawItem`, or `None` if it has no parseable `updated`
/// timestamp (it can't be filed onto a day). Pure — no I/O — so the ownership match,
/// ADF→Markdown body, status/period header, and metadata projection are unit-testable
/// against fixtures without a live Jira. `my_account_id` is the authenticated account's
/// id (the exact ownership key); `my_email` stands in as the author when the issue is
/// the user's own.
fn map_issue(
    issue: Issue,
    my_account_id: &str,
    base: &str,
    my_email: &str,
    start_date_field: Option<&str>,
    strings: &lk_core::i18n::Strings,
) -> Option<RawItem> {
    let summary = issue.fields.summary.as_deref().unwrap_or(strings.untitled);

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
    let is_me = assignee_aid == Some(my_account_id);
    let author = if is_me {
        Some(my_email.to_string())
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
    let start_date = start_date_field
        .and_then(|f| issue.fields.extra.get(f))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    // Snapshot the status + planned window as a header. These are the values *as of this
    // run* — the page is a daily record, so a later schedule change doesn't rewrite history.
    let body = with_status_header(
        strings,
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
        is_self: is_me,
        metadata: serde_json::json!({
            "status": status,
            "priority": issue.fields.priority.and_then(|p| p.name),
            "labels": issue.fields.labels,
            "duedate": issue.fields.duedate,
            "start_date": start_date,
            "assignee_account_id": assignee_account_id,
        }),
    })
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

    fn issue_from(json: serde_json::Value) -> Issue {
        serde_json::from_value(json).expect("issue fixture parses")
    }

    #[test]
    fn map_issue_owns_when_assignee_matches_account() {
        let s = lk_core::i18n::Locale::En.strings();
        let issue = issue_from(serde_json::json!({
            "key": "PROJ-1",
            "fields": {
                "summary": "Ship the thing",
                "status": {"name": "In Progress"},
                "updated": "2026-05-23T09:00:00.000+0000",
                "assignee": {"accountId": "acc-123", "emailAddress": "other@x.com"},
                "customfield_10015": "2026-05-18"
            }
        }));
        let item = map_issue(
            issue,
            "acc-123",
            "https://x.atlassian.net",
            "me@x.com",
            Some("customfield_10015"),
            s,
        )
        .expect("maps");
        assert!(item.is_self, "assignee accountId == my account → self");
        assert_eq!(item.author.as_deref(), Some("me@x.com"));
        assert_eq!(item.title, "[PROJ-1] Ship the thing");
        assert_eq!(
            item.url.as_deref(),
            Some("https://x.atlassian.net/browse/PROJ-1")
        );
        assert!(
            item.body.contains("In Progress"),
            "status header present: {}",
            item.body
        );
        assert!(
            item.body.contains("2026-05-18"),
            "start date from custom field: {}",
            item.body
        );
    }

    #[test]
    fn map_issue_not_self_uses_assignee_identity() {
        let s = lk_core::i18n::Locale::En.strings();
        let issue = issue_from(serde_json::json!({
            "key": "PROJ-2",
            "fields": {
                "summary": "Someone else's task",
                "updated": "2026-05-23T09:00:00.000+0000",
                "assignee": {"accountId": "acc-999", "displayName": "Alice"}
            }
        }));
        let item = map_issue(
            issue,
            "acc-123",
            "https://x.atlassian.net",
            "me@x.com",
            None,
            s,
        )
        .expect("maps");
        assert!(!item.is_self);
        assert_eq!(item.author.as_deref(), Some("Alice"));
    }

    #[test]
    fn map_issue_skips_unparseable_timestamp() {
        let s = lk_core::i18n::Locale::En.strings();
        let issue = issue_from(serde_json::json!({
            "key": "PROJ-3",
            "fields": {"summary": "No date", "updated": "not-a-timestamp"}
        }));
        assert!(map_issue(issue, "acc-123", "https://x", "me@x.com", None, s).is_none());
    }

    #[test]
    fn map_issue_unassigned_is_not_self() {
        let s = lk_core::i18n::Locale::En.strings();
        let issue = issue_from(serde_json::json!({
            "key": "PROJ-4",
            "fields": {"summary": "Open", "updated": "2026-05-23T09:00:00.000+0000"}
        }));
        let item = map_issue(issue, "acc-123", "https://x", "me@x.com", None, s).expect("maps");
        assert!(!item.is_self);
        assert!(item.author.is_none());
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
