use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::OnceCell;

use lk_core::event::RawItem;

use crate::atlassian::{AtlassianAuth, JiraPaging, Product};
use crate::{ExtractContext, Source, SourceError};

pub struct JiraSource {
    http: reqwest::Client,
    auth: Arc<AtlassianAuth>,
    /// The authenticated user's ownership key (`accountId` on Cloud, `name` on Data
    /// Center), fetched once and cached for the life of the source — it is invariant for
    /// fixed credentials.
    account_id: OnceCell<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JiraParams {
    jql: String,
    #[serde(default = "default_fields")]
    fields: Vec<String>,
    #[serde(default = "default_max")]
    max_issues: u32,
    /// Jira "start date" custom-field id (instance-specific, e.g. `customfield_10015`).
    /// Unset → start date is simply not shown. Avoids guessing a field id that means
    /// something different on another Jira instance.
    #[serde(default)]
    start_date_field: Option<String>,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<JiraParams>(params).map(|_| ())
}

impl crate::ValidatedParams for JiraParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.max_issues == 0 {
            return Err(SourceError::InvalidParams(
                "jira `max_issues` must be > 0".into(),
            ));
        }
        // A blank `jql` deserializes fine (it's a plain String) but Jira's search API treats an
        // empty query as "match every issue", so a targeted daily snapshot
        // (`assignee = currentUser() AND updated >= -1d`) would silently collapse into a full
        // instance scrape — over-fetching unrelated issues and polluting ownership/work-log.
        if self.jql.trim().is_empty() {
            return Err(SourceError::InvalidParams(
                "jira `jql` must not be blank — an empty JQL matches every issue in the instance, \
                 turning a targeted daily snapshot into a full scrape."
                    .into(),
            ));
        }
        Ok(())
    }
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
    200
}

#[derive(Deserialize)]
struct SearchResult {
    issues: Option<Vec<Issue>>,
    /// Cloud continuation cursor. Absent on Data Center, which pages by offset.
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    /// Data Center reports the full match count so the offset loop knows when it is done.
    /// Absent on Cloud, which signals completion purely by dropping the cursor.
    #[serde(default)]
    total: Option<u64>,
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
    status: Option<StatusField>,
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
struct StatusField {
    name: Option<String>,
    /// Jira's own three-value classification of the status, which every workflow's every
    /// status maps to: `new`, `indeterminate`, `done`.
    ///
    /// Read INSTEAD of the status name, which is per-project and translated — "완료",
    /// "Closed", "Resolved", "배포완료" all mean done in some workflow and none of them in
    /// another. Matching the name would be the pattern-matching this codebase refuses: a
    /// project that renames a status silently turns finished work back into open work, or the
    /// reverse. The category is Jira's, fixed, and the same in every language.
    #[serde(rename = "statusCategory")]
    category: Option<StatusCategory>,
}

#[derive(Deserialize)]
struct StatusCategory {
    key: Option<String>,
}

impl StatusField {
    /// Whether Jira classifies this status as finished.
    ///
    /// An issue whose category is ABSENT is not treated as open: the answer would be a guess,
    /// and a guess here proposes work every morning that may already be finished.
    fn is_open(&self) -> bool {
        matches!(
            self.category.as_ref().and_then(|c| c.key.as_deref()),
            Some("new" | "indeterminate")
        )
    }
}

#[derive(Deserialize)]
struct UserField {
    #[serde(rename = "accountId")]
    account_id: Option<String>,
    /// Data Center's identity field. `accountId` is Cloud-only, so without this every DC
    /// issue would deserialize with no identity at all and silently compare not-self —
    /// erasing a whole deployment's work from the personal log with no signal.
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

impl UserField {
    /// The ownership key this deployment populates. Exactly one of the two is ever present
    /// — Cloud sends `accountId`, Data Center sends `name` — so preferring whichever exists
    /// is unambiguous, and mirrors how the Confluence adapter resolves the same split.
    fn identity(&self) -> Option<&str> {
        self.account_id.as_deref().or(self.name.as_deref())
    }
}

impl JiraSource {
    pub fn new(http: reqwest::Client, auth: Arc<AtlassianAuth>) -> Self {
        Self {
            http,
            auth,
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
                let deployment = self.auth.deployment();
                let url = format!(
                    "{}{}",
                    self.auth.api_base(Product::Jira),
                    deployment.jira_myself_path()
                );
                let header = self.auth.header().await?;
                let resp =
                    crate::retry::send_with_retry(|| header.apply(self.http.get(&url)).send())
                        .await?;
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SourceError::Api {
                        status,
                        message: format!(
                            "Jira /myself failed: {}",
                            self.auth.explain_failure(status, &body)
                        ),
                    });
                }
                // Cloud identifies users by `accountId`, Data Center by `name` — the
                // dialect owns which, so this stays a single lookup.
                let key = deployment.jira_user_key();
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(|e| SourceError::Parse(e.to_string()))?
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or_else(|| {
                        SourceError::Parse(format!("Jira /myself response missing {key}"))
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
        let p: JiraParams = crate::parse_validated(params)?;

        let deployment = self.auth.deployment();
        let url = format!(
            "{}{}",
            self.auth.api_base(Product::Jira),
            deployment.jira_search_path()
        );
        // Append the configured start-date field (if any) so the API returns it; it's
        // extracted by raw id afterward since the id is instance-specific.
        let mut fields = p.fields.clone();
        if let Some(sdf) = &p.start_date_field {
            fields.push(sdf.clone());
        }
        let fields_csv = fields.join(",");

        let my_account_id = self.account_id().await?;
        let header = self.auth.header().await?;

        // Paginate to the end of the JQL result (complete-refetch contract: the daily page
        // is re-rendered from this fetch, so an issue beyond one page is silently lost
        // knowledge). The continuation mechanism is deployment-specific — Cloud's
        // `/search/jql` hands back an opaque `nextPageToken`, Data Center's v2 `/search`
        // uses a `startAt` offset — but termination is `paging::page_step` either way, the
        // rule all listing adapters share.
        const PAGE_SIZE: usize = 100;
        let page_size = PAGE_SIZE.to_string();
        let cap = p.max_issues as usize;

        let mut issues: Vec<Issue> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages_fetched = 0usize;

        loop {
            let start_at = issues.len().to_string();
            let resp = crate::retry::send_with_retry(|| {
                let mut req = header.apply(self.http.get(&url)).query(&[
                    ("jql", p.jql.as_str()),
                    ("maxResults", page_size.as_str()),
                    ("fields", fields_csv.as_str()),
                ]);
                req = match deployment.jira_paging() {
                    JiraPaging::Token => match page_token {
                        Some(ref pt) => req.query(&[("nextPageToken", pt.as_str())]),
                        None => req,
                    },
                    JiraPaging::Offset => req.query(&[("startAt", start_at.as_str())]),
                };
                req.send()
            })
            .await?;

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                return Err(SourceError::Api {
                    status,
                    message: format!(
                        "Jira search failed: {}",
                        self.auth.explain_failure(status, &body)
                    ),
                });
            }

            let result: SearchResult = resp
                .json()
                .await
                .map_err(|e| SourceError::Parse(e.to_string()))?;

            let page_len = result.issues.as_ref().map_or(0, Vec::len);
            issues.extend(result.issues.unwrap_or_default());
            pages_fetched += 1;

            // Each dialect states "more remain" differently: Cloud by handing back a cursor,
            // Data Center by reporting a `total` the collected count hasn't reached. A DC
            // response that returns nothing while claiming more would spin forever on an
            // unadvancing offset, so an empty page also ends the offset loop.
            let has_next = match deployment.jira_paging() {
                JiraPaging::Token => result.next_page_token.is_some(),
                JiraPaging::Offset => {
                    let more_expected = result.total.is_some_and(|t| (issues.len() as u64) < t);
                    // An empty page while `total` says more remain means the offset is not
                    // advancing; continuing would spin. Stopping is right, but it IS a
                    // truncation, and this crate's contract is that truncation is never
                    // silent. (`total` absent entirely is the same story: nothing left to
                    // page against.)
                    if more_expected && page_len == 0 {
                        tracing::warn!(
                            collected = issues.len(),
                            total = result.total,
                            "jira: server returned an empty page before the result set was \
                             exhausted; results may be incomplete"
                        );
                    }
                    page_len > 0 && more_expected
                }
            };

            match crate::paging::page_step(issues.len(), cap, has_next, pages_fetched) {
                crate::paging::PageStep::Continue => page_token = result.next_page_token,
                crate::paging::PageStep::Stop { dropped } => {
                    issues.truncate(cap);
                    if dropped {
                        tracing::warn!(
                            max = p.max_issues,
                            "jira: issue cap hit, some issues may have been dropped; raise max_issues"
                        );
                    }
                    break;
                }
                crate::paging::PageStep::Exhausted => {
                    tracing::warn!(
                        pages = crate::paging::MAX_PAGES,
                        "jira: page budget exhausted before the search completed; results may be incomplete"
                    );
                    break;
                }
            }
        }

        tracing::info!(count = issues.len(), "jira: issues found");

        // Links resolve against the site, never the OAuth gateway (which is not browsable).
        let base = self.auth.browse_base(Product::Jira).unwrap_or_default();
        let items = issues
            .into_iter()
            .filter_map(|issue| {
                map_issue(
                    issue,
                    my_account_id,
                    &base,
                    &ctx.identity.email,
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
    let assignee_aid = assignee.and_then(UserField::identity);
    let is_me = assignee_aid == Some(my_account_id);
    let author = if is_me {
        Some(my_email.to_string())
    } else {
        assignee
            .and_then(|a| a.email_address.as_deref().or(a.display_name.as_deref()))
            .map(String::from)
    };

    // Declared from the provider's own structured fields and nothing else: the issue is
    // assigned to the authenticated account, and Jira's own status category says it is not
    // finished. No text is read, so there is no reading in which this is a false positive.
    let open_work = issue
        .fields
        .status
        .as_ref()
        .filter(|_| is_me)
        .filter(|status| status.is_open())
        .and_then(|_| {
            let url = (!base.is_empty()).then(|| format!("{base}/browse/{}", issue.key))?;
            Some(lk_core::event::OpenWork {
                summary: format!("[{}] {}", issue.key, summary),
                url,
            })
        });

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
        .and_then(|a| a.identity().map(str::to_string));

    Some(RawItem {
        external_id: Some(issue.key.clone()),
        title: format!("[{}] {}", issue.key, summary),
        body,
        url: (!base.is_empty()).then(|| format!("{base}/browse/{}", issue.key)),
        author,
        timestamp: ts,
        is_self: is_me,
        open_work,
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

    /// Jira returns only the fields asked for, and `IssueFields` deserializes with every named
    /// field `Option`al — so a field dropped from the request arrives as `None` and the mapper
    /// renders a page without it. No error, no warning, just an issue with no description or no
    /// status, and the mapper's own tests never notice because they deserialize complete fixtures
    /// they build themselves.
    ///
    /// Pinned by round-tripping a payload that carries ONLY the requested fields: anything
    /// `IssueFields` names and the request omits comes back `None` and fails here.
    #[test]
    fn every_field_the_mapper_reads_is_a_field_the_request_asks_for() {
        let requested = default_fields();
        let sample = |name: &str| -> serde_json::Value {
            match name {
                "labels" => serde_json::json!(["a"]),
                "status" | "priority" => serde_json::json!({"name": "n"}),
                "assignee" => serde_json::json!({"displayName": "d", "accountId": "id"}),
                "description" => serde_json::json!({"type": "doc", "content": []}),
                _ => serde_json::json!("v"),
            }
        };
        let payload: serde_json::Map<String, serde_json::Value> = requested
            .iter()
            .map(|name| (name.clone(), sample(name)))
            .collect();
        let fields: IssueFields = serde_json::from_value(serde_json::Value::Object(payload))
            .expect("the requested fields must deserialize into IssueFields");

        for (name, present) in [
            ("summary", fields.summary.is_some()),
            ("description", fields.description.is_some()),
            ("status", fields.status.is_some()),
            ("priority", fields.priority.is_some()),
            ("labels", fields.labels.is_some()),
            ("updated", fields.updated.is_some()),
            ("duedate", fields.duedate.is_some()),
            ("assignee", fields.assignee.is_some()),
        ] {
            assert!(
                present,
                "`{name}` is read from the issue but not requested from Jira, so it is always \
                 absent — add it to `default_fields`"
            );
        }
        // Requested-but-unread would land in `extra`, which exists for instance-specific custom
        // fields; a static field there is a request nothing consumes.
        assert!(
            fields.extra.is_empty(),
            "requested fields nothing reads: {:?}",
            fields.extra.keys().collect::<Vec<_>>()
        );
    }

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

    fn mapped(assignee: &str, status: serde_json::Value) -> lk_core::event::RawItem {
        let issue = issue_from(serde_json::json!({
            "key": "PROJ-1",
            "fields": {
                "summary": "Ship the thing",
                "status": status,
                "updated": "2026-05-23T09:00:00.000+0000",
                "assignee": {"accountId": assignee, "emailAddress": "other@x.com"}
            }
        }));
        map_issue(
            issue,
            "acc-123",
            "https://x.atlassian.net",
            "me@x.com",
            None,
            lk_core::i18n::Locale::En.strings(),
        )
        .expect("maps")
    }

    /// The status NAME is per-project and translated — "완료", "Closed", "Resolved" and
    /// "배포완료" each mean done in some workflow and none of them in another — so a rule over
    /// it would turn finished work back into open work the day a project renamed a column.
    /// Jira's own category is fixed in every language and every workflow maps to it.
    #[test]
    fn open_work_is_declared_from_the_status_category_never_the_name() {
        for (key, open) in [("new", true), ("indeterminate", true), ("done", false)] {
            let item = mapped(
                "acc-123",
                serde_json::json!({"name": "배포완료", "statusCategory": {"key": key}}),
            );
            assert_eq!(
                item.open_work.is_some(),
                open,
                "category `{key}` under a name that reads finished"
            );
        }

        let open = mapped(
            "acc-123",
            serde_json::json!({"name": "In Progress", "statusCategory": {"key": "indeterminate"}}),
        )
        .open_work
        .expect("declared");
        assert_eq!(open.summary, "[PROJ-1] Ship the thing");
        assert_eq!(open.url, "https://x.atlassian.net/browse/PROJ-1");
    }

    /// Someone else's open issue is not the user's work, and an issue whose category Jira did
    /// not send is not declared open — the answer would be a guess, and a guess proposes work
    /// every morning that may already be finished.
    #[test]
    fn open_work_needs_the_users_own_issue_and_a_category_to_read() {
        assert!(
            mapped(
                "someone-else",
                serde_json::json!({"name": "In Progress", "statusCategory": {"key": "indeterminate"}})
            )
            .open_work
            .is_none()
        );
        assert!(
            mapped("acc-123", serde_json::json!({"name": "In Progress"}))
                .open_work
                .is_none()
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
        let params = serde_json::json!({ "max_issues": 10 });
        assert!(validate_params(&params).is_err());
    }

    #[test]
    fn blank_jql_rejected() {
        // A present-but-blank jql deserializes fine but Jira reads an empty JQL as "every issue";
        // reject it so a daily snapshot can't silently become a full instance scrape.
        assert!(validate_params(&serde_json::json!({ "jql": "" })).is_err());
        assert!(validate_params(&serde_json::json!({ "jql": "   " })).is_err());
        // A real query is accepted.
        assert!(validate_params(&serde_json::json!({ "jql": "assignee = currentUser()" })).is_ok());
    }

    #[test]
    fn wrong_type_rejected() {
        let params = serde_json::json!({ "jql": "x", "max_issues": "fifty" });
        assert!(validate_params(&params).is_err());
    }

    #[test]
    fn typo_key_rejected() {
        // `deny_unknown_fields` catches misspelled params (max_issue vs max_issues).
        let params = serde_json::json!({ "jql": "x", "max_issue": 10 });
        assert!(validate_params(&params).is_err());
    }

    #[test]
    fn max_issues_defaults_and_rejects_zero() {
        let params: JiraParams = serde_json::from_value(serde_json::json!({"jql": "x"})).unwrap();
        assert_eq!(params.max_issues, 200);
        // A zero cap would silently fetch nothing; validation must refuse it up front.
        let params = serde_json::json!({ "jql": "x", "max_issues": 0 });
        assert!(validate_params(&params).is_err());
        let params = serde_json::json!({ "jql": "x", "max_issues": 1 });
        assert!(validate_params(&params).is_ok());
    }
}
