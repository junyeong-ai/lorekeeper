//! Confluence Cloud source. Projects pages the user touched in the target day's window
//! into [`RawItem`]s via CQL.
//!
//! A Confluence page is a LIVING document, unlike the immutable events every other adapter
//! reads (a sent mail, a posted message). Two consequences shape this adapter:
//!
//! - **The version is part of the identity.** `external_id` is `confluence:{id}:v{version}`,
//!   so re-fetching an unchanged page reproduces the same event (dedup absorbs it, and the
//!   `llm_inputs` hash keeps the LLM idle), while an edit mints a distinct event that flows
//!   through summarize/concept extraction again. Freshness falls out of the existing
//!   materialized-view machinery instead of needing a separate reconciliation pass.
//! - **Ownership is last-writer, not contributor.** `is_self` compares the *current
//!   version's* author to the authenticated account, so a page someone else last edited is
//!   knowledge to read, never the user's own work-log entry — even though CQL's
//!   `contributor = currentUser()` matches it.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::OnceCell;

use lk_core::event::RawItem;

use crate::atlassian::{AtlassianAuth, Product};
use crate::{ExtractContext, Source, SourceError};

pub struct ConfluenceSource {
    http: reqwest::Client,
    auth: Arc<AtlassianAuth>,
    /// The authenticated user's `accountId` — the exact ownership key. Invariant for a
    /// fixed grant, so it is fetched once and cached.
    account_id: OnceCell<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfluenceParams {
    /// CQL selecting WHICH pages matter, with NO time clause — the adapter appends the
    /// window from `day_window` so `lore ingest --date <past>` backfills the right day.
    /// A time clause here would pin the query to the wall clock and break that.
    cql: String,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    #[serde(default = "default_max_pages")]
    max_pages: usize,
    /// Keep only pages whose current version the authenticated user wrote. The default —
    /// documents I authored are my work; documents I merely once touched are not.
    #[serde(default = "default_only_mine")]
    only_my_edits: bool,
}

fn default_lookback() -> u32 {
    24
}

fn default_max_pages() -> usize {
    // Deliberately above the 50-item request size so the `_links.next` follow path is
    // exercised by a normal configuration rather than only by a tuned one.
    200
}

fn default_only_mine() -> bool {
    true
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<ConfluenceParams>(params).map(|_| ())
}

impl crate::ValidatedParams for ConfluenceParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.max_pages == 0 {
            return Err(SourceError::InvalidParams(
                "confluence `max_pages` must be > 0".into(),
            ));
        }
        // A blank CQL is not "match nothing" — Confluence treats it as an unbounded query,
        // so a targeted daily snapshot would collapse into a whole-site scrape, dragging
        // every space's pages into the vault and the work-log.
        if self.cql.trim().is_empty() {
            return Err(SourceError::InvalidParams(
                "confluence `cql` must not be blank — an empty query matches every page in \
                 the site, turning a targeted daily snapshot into a full scrape."
                    .into(),
            ));
        }
        // The adapter owns the time window. A user-supplied one would either fight it or
        // silently anchor the query to `now`, breaking `--date` backfill.
        let lowered = self.cql.to_lowercase();
        if lowered.contains("lastmodified") || lowered.contains("created >") {
            return Err(SourceError::InvalidParams(
                "confluence `cql` must not carry a time clause (`lastModified`/`created`) — \
                 the adapter appends the target day's window, so a hand-written one would \
                 anchor the query to the wall clock and break `--date` backfill. Use \
                 `lookback_hours` instead."
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Page>,
    #[serde(default, rename = "_links")]
    links: Links,
}

#[derive(Default, Deserialize)]
struct Links {
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct Page {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    version: Option<Version>,
    #[serde(default)]
    space: Option<Space>,
    #[serde(default)]
    body: Option<Body>,
    #[serde(default, rename = "_links")]
    links: PageLinks,
}

#[derive(Deserialize)]
struct Version {
    #[serde(default)]
    number: Option<u64>,
    /// ISO-8601 instant of this edit — the event's timestamp, so the page lands on the day
    /// it was actually edited.
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    by: Option<User>,
}

#[derive(Deserialize)]
struct User {
    #[serde(default, rename = "accountId")]
    account_id: Option<String>,
    /// Data Center's identity field — `accountId` is Cloud-only.
    #[serde(default)]
    username: Option<String>,
    #[serde(default, rename = "publicName")]
    public_name: Option<String>,
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct Space {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    storage: Option<BodyValue>,
}

#[derive(Deserialize)]
struct BodyValue {
    #[serde(default)]
    value: Option<String>,
}

#[derive(Default, Deserialize)]
struct PageLinks {
    /// Path relative to the site's wiki root (e.g. `/spaces/KEY/pages/123/Title`).
    #[serde(default)]
    webui: Option<String>,
}

impl ConfluenceSource {
    pub fn new(http: reqwest::Client, auth: Arc<AtlassianAuth>) -> Self {
        Self {
            http,
            auth,
            account_id: OnceCell::new(),
        }
    }

    /// The authenticated user's `accountId`. A failure is PROPAGATED, never degraded to
    /// "no account id" — collapsing an auth error into `None` would mark every page
    /// not-self, silently erasing a batch's authorship from the work-log with no signal
    /// (the same rule the Jira adapter follows).
    async fn account_id(&self) -> Result<&str, SourceError> {
        self.account_id
            .get_or_try_init(|| async {
                let url = format!(
                    "{}/rest/api/user/current",
                    self.auth.api_base(Product::Confluence)
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
                            "Confluence /user/current failed: {}",
                            self.auth.explain_failure(status, &body)
                        ),
                    });
                }
                let key = self.auth.deployment().confluence_user_key();
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(|e| SourceError::Parse(e.to_string()))?
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .ok_or_else(|| {
                        SourceError::Parse(format!(
                            "Confluence /user/current response missing {key}"
                        ))
                    })
            })
            .await
            .map(String::as_str)
    }
}

#[async_trait]
impl Source for ConfluenceSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: ConfluenceParams = crate::parse_validated(params)?;

        let (min, max) = ctx.day_window(p.lookback_hours, 0)?;
        let cql = build_windowed_cql(&p.cql, min, max);
        tracing::debug!(cql = %cql, "confluence: query");

        let my_account_id = self.account_id().await?.to_string();
        let header = self.auth.header().await?;
        let search_url = format!(
            "{}/rest/api/content/search",
            self.auth.api_base(Product::Confluence)
        );

        // Paginate to the end of the result set (complete-refetch contract). Confluence v1
        // search hands back a ready-made `_links.next` path rather than a bare token, so the
        // loop follows that; termination is still `paging::page_step`, the rule every
        // listing adapter shares.
        const PAGE_SIZE: usize = 50;
        let mut pages: Vec<Page> = Vec::new();
        let mut next_path: Option<String> = None;
        let mut pages_fetched = 0usize;

        loop {
            let request_url = match &next_path {
                // `_links.next` is site-root-relative (`/rest/api/...`); re-anchor it on the
                // API gateway, which is a different host than the one that produced it.
                Some(path) => format!(
                    "{}{}",
                    self.auth.api_base(Product::Confluence),
                    path.strip_prefix("/wiki").unwrap_or(path)
                ),
                None => search_url.clone(),
            };
            let is_first = next_path.is_none();

            let resp = crate::retry::send_with_retry(|| {
                let mut req = header
                    .apply(self.http.get(&request_url))
                    .header("Accept", "application/json");
                if is_first {
                    req = req.query(&[
                        ("cql", cql.as_str()),
                        ("limit", PAGE_SIZE.to_string().as_str()),
                        ("expand", "version,space,body.storage"),
                    ]);
                }
                req.send()
            })
            .await?;

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                return Err(SourceError::Api {
                    status,
                    message: format!(
                        "Confluence search failed: {}",
                        self.auth.explain_failure(status, &body)
                    ),
                });
            }

            let page: SearchResponse = resp.json().await?;
            let has_next = page.links.next.is_some();
            next_path = page.links.next.clone();
            pages.extend(page.results);
            pages_fetched += 1;

            match crate::paging::page_step(pages.len(), p.max_pages, has_next, pages_fetched) {
                crate::paging::PageStep::Continue => continue,
                crate::paging::PageStep::Stop { dropped } => {
                    if dropped {
                        tracing::warn!(
                            cap = p.max_pages,
                            "confluence: max_pages reached; some pages may have been dropped"
                        );
                    }
                    pages.truncate(p.max_pages);
                    break;
                }
                crate::paging::PageStep::Exhausted => {
                    tracing::warn!(
                        pages = crate::paging::MAX_PAGES,
                        "confluence: page budget exhausted before the search completed; \
                         results may be incomplete"
                    );
                    break;
                }
            }
        }

        tracing::info!(count = pages.len(), "confluence: pages found");

        let site = self
            .auth
            .browse_base(Product::Confluence)
            .unwrap_or_default();
        let items = pages
            .into_iter()
            .filter_map(|page| map_page(page, &my_account_id, &site, p.only_my_edits))
            // The authoritative window. The CQL bound is a coarse superset; this cuts it
            // to the exact instants `day_window` asked for, so the batch holds only the
            // whole days the pipeline is entitled to re-render.
            .filter(|item| item.timestamp >= min && item.timestamp < max)
            .collect();
        Ok(items)
    }
}

/// Append a date-granular prefilter to the user's CQL.
///
/// CQL date literals carry no UTC offset and no time — Confluence resolves them in the
/// querying user's profile timezone, which no API exposes to a client. So the query asks for
/// a deliberate SUPERSET of whole dates, and the precise cut happens in `extract`, against
/// each version's `when` — an ISO-8601 instant that needs no assumption about anyone's clock.
///
/// The padding is asymmetric because a date literal resolves to the START of its day. That
/// errs earlier at both ends, which widens the lower bound (safe) but TIGHTENS the upper one:
/// a single day of slack there resolves before `max` whenever the profile timezone sits east
/// of the vault's, silently dropping the tail of the target day. Two days of slack covers the
/// worst inhabited offset with hours to spare, and under any looser resolution it is simply a
/// wider superset — so the bound holds without depending on which reading is right.
///
/// Exactness matters beyond accuracy here. `day_window` returns bounds on day boundaries, so
/// a correctly-cut batch holds only WHOLE days; Confluence is non-streaming, and the pipeline
/// re-renders a daily page for every date it sees, from the fetch alone. A batch carrying a
/// PARTIAL day would overwrite that date's page with whatever fragment fell inside the window.
fn build_windowed_cql(base: &str, min: jiff::Timestamp, max: jiff::Timestamp) -> String {
    let back = jiff::SignedDuration::from_hours(24);
    let forward = jiff::SignedDuration::from_hours(48);
    format!(
        "({base}) AND lastModified >= \"{}\" AND lastModified <= \"{}\" \
         ORDER BY lastModified DESC",
        format_cql_date(min.checked_sub(back).unwrap_or(min)),
        format_cql_date(max.checked_add(forward).unwrap_or(max)),
    )
}

/// `yyyy/MM/dd` — CQL's date-only literal. Rendered in UTC because this is only a coarse
/// bound; no downstream decision depends on which calendar day it names.
fn format_cql_date(ts: jiff::Timestamp) -> String {
    let z = ts.to_zoned(jiff::tz::TimeZone::UTC);
    format!("{:04}/{:02}/{:02}", z.year(), z.month(), z.day())
}

/// Map one Confluence page to a `RawItem`, or `None` when it can't be filed onto a day
/// (no parseable version timestamp) or when `only_my_edits` excludes it. Pure — no I/O —
/// so ownership, identity, and storage→Markdown conversion are testable against fixtures.
fn map_page(page: Page, my_account_id: &str, site: &str, only_my_edits: bool) -> Option<RawItem> {
    let Some(version) = page.version else {
        tracing::warn!(page = %page.id, "confluence: page has no version; skipping");
        return None;
    };
    let version_number = version.number.unwrap_or(1);

    let editor = version.by.as_ref();
    // Whichever field this deployment populates IS the identity; `account_id` is Cloud's,
    // `username` is Data Center's, and exactly one is ever present.
    let editor_account_id = editor.and_then(|u| u.account_id.as_deref().or(u.username.as_deref()));
    let is_self = editor_account_id.is_some_and(|id| id == my_account_id);
    if only_my_edits && !is_self {
        return None;
    }

    let Some(ts) = version
        .when
        .as_deref()
        .and_then(|w| w.parse::<jiff::Timestamp>().ok())
    else {
        tracing::warn!(
            page = %page.id,
            when = ?version.when,
            "confluence: unparseable version timestamp; cannot file onto a day, skipping"
        );
        return None;
    };

    let title = page
        .title
        .unwrap_or_else(|| format!("Confluence {}", page.id));
    let storage = page
        .body
        .and_then(|b| b.storage)
        .and_then(|s| s.value)
        .unwrap_or_default();
    // Confluence "storage format" is XHTML with custom macro elements; the shared HTML
    // converter degrades unmapped constructs to their text rather than leaking markup.
    let body = crate::markdown::html_to_markdown(&storage);

    let space_key = page.space.as_ref().and_then(|s| s.key.clone());
    let author = editor.and_then(|u| {
        u.display_name
            .clone()
            .or_else(|| u.public_name.clone())
            .or_else(|| u.email.clone())
    });

    // `site` is already the browsable root for this deployment (Cloud includes `/wiki`;
    // Data Center's context path is part of the configured site URL), so `webui` — which is
    // relative to that root — appends directly.
    let url = page
        .links
        .webui
        .filter(|_| !site.is_empty())
        .map(|webui| format!("{site}{webui}"));

    Some(RawItem {
        // The version is part of the identity: an unchanged page re-fetched tomorrow yields
        // the same id (dedup absorbs it, no LLM work), while an edit yields a new one and
        // flows through summarize/concepts again. This is the freshness mechanism.
        external_id: Some(format!("confluence:{}:v{version_number}", page.id)),
        title,
        body,
        url,
        author,
        timestamp: ts,
        is_self,
        metadata: serde_json::json!({
            "page_id": page.id,
            "version": version_number,
            "space_key": space_key,
            "space_name": page.space.and_then(|s| s.name),
            "editor_account_id": editor_account_id,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_json(account_id: &str, version: u64) -> Page {
        serde_json::from_value(serde_json::json!({
            "id": "12345",
            "title": "설계 문서",
            "version": {
                "number": version,
                "when": "2026-07-27T15:07:12.746Z",
                "by": { "accountId": account_id, "displayName": "엄준영" }
            },
            "space": { "key": "OYAISTRAT", "name": "AI 전략" },
            "body": { "storage": { "value": "<p>본문 <strong>강조</strong></p>" } },
            "_links": { "webui": "/spaces/OYAISTRAT/pages/12345/Design" }
        }))
        .unwrap()
    }

    #[test]
    fn external_id_carries_the_version_so_an_edit_is_a_new_event() {
        let site = "https://acme.atlassian.net";
        let v3 = map_page(page_json("me", 3), "me", site, true).unwrap();
        let v4 = map_page(page_json("me", 4), "me", site, true).unwrap();
        assert_eq!(v3.external_id.as_deref(), Some("confluence:12345:v3"));
        assert_ne!(
            v3.external_id, v4.external_id,
            "an edit must mint a distinct event so it is re-summarized"
        );
    }

    #[test]
    fn unchanged_page_reproduces_a_stable_id() {
        let site = "https://acme.atlassian.net";
        let a = map_page(page_json("me", 7), "me", site, true).unwrap();
        let b = map_page(page_json("me", 7), "me", site, true).unwrap();
        assert_eq!(
            a.external_id, b.external_id,
            "a re-fetch of the same version must dedup away, leaving the LLM idle"
        );
    }

    #[test]
    fn ownership_is_the_current_versions_author() {
        let site = "https://acme.atlassian.net";
        let mine = map_page(page_json("me", 2), "me", site, false).unwrap();
        assert!(mine.is_self);

        // CQL `contributor = currentUser()` also matches pages someone else last edited;
        // those are knowledge to read, never the user's own work-log entry.
        let theirs = map_page(page_json("someone-else", 2), "me", site, false).unwrap();
        assert!(!theirs.is_self);
    }

    #[test]
    fn only_my_edits_drops_pages_last_touched_by_others() {
        let site = "https://acme.atlassian.net";
        assert!(map_page(page_json("someone-else", 2), "me", site, true).is_none());
        assert!(map_page(page_json("me", 2), "me", site, true).is_some());
    }

    #[test]
    fn storage_format_is_converted_to_markdown() {
        let item = map_page(page_json("me", 1), "me", "https://a.net", true).unwrap();
        assert!(item.body.contains("**강조**"), "got: {}", item.body);
        assert!(!item.body.contains("<p>"), "raw markup must not leak");
    }

    #[test]
    fn browse_url_is_absolute_and_omitted_without_a_site() {
        let with_site = map_page(
            page_json("me", 1),
            "me",
            "https://acme.atlassian.net/wiki",
            true,
        )
        .unwrap()
        .url;
        assert_eq!(
            with_site.as_deref(),
            Some("https://acme.atlassian.net/wiki/spaces/OYAISTRAT/pages/12345/Design")
        );
        assert!(
            map_page(page_json("me", 1), "me", "", true)
                .unwrap()
                .url
                .is_none(),
            "no site root means no browsable link, not a broken relative one"
        );
    }

    #[test]
    fn cql_prefilter_is_a_date_granular_superset() {
        let min: jiff::Timestamp = "2026-07-26T15:00:00Z".parse().unwrap();
        let max: jiff::Timestamp = "2026-07-27T15:00:00Z".parse().unwrap();
        let cql = build_windowed_cql("type = page AND contributor = currentUser()", min, max);
        assert!(cql.starts_with("(type = page AND contributor = currentUser())"));
        assert!(cql.contains(r#"lastModified >= "2026/07/25""#), "{cql}");
        assert!(cql.contains(r#"lastModified <= "2026/07/29""#), "{cql}");
        assert!(cql.ends_with("ORDER BY lastModified DESC"));
        // A wall clock in the literal would re-introduce the timezone assumption.
        assert!(!cql.contains(':'), "date-only literal expected: {cql}");
    }

    /// The two `yyyy/MM/dd` literals the query actually carries, so the coverage test below
    /// measures what is sent rather than re-deriving it.
    fn literal_dates(cql: &str) -> (jiff::civil::Date, jiff::civil::Date) {
        let mut found = cql.split('"').skip(1).step_by(2).map(|lit| {
            let parts: Vec<i16> = lit.split('/').map(|p| p.parse().unwrap()).collect();
            jiff::civil::date(parts[0], parts[1] as i8, parts[2] as i8)
        });
        (found.next().unwrap(), found.next().unwrap())
    }

    #[test]
    fn prefilter_covers_the_window_under_start_of_day_resolution() {
        // A date literal carries no time, so it resolves to the START of its day in the
        // querying user's profile timezone — the pessimistic reading, and the one that can
        // pull the upper bound BEFORE `max`. Proving coverage under it proves coverage
        // under any looser reading too, so the window holds without the code depending on
        // which resolution Atlassian actually applies.
        let min: jiff::Timestamp = "2026-07-26T15:00:00Z".parse().unwrap();
        let max: jiff::Timestamp = "2026-07-27T15:00:00Z".parse().unwrap();
        let (lower, upper) = literal_dates(&build_windowed_cql("type = page", min, max));

        // Baker Island (UTC-12) through Line Islands (UTC+14) bound the inhabited range.
        for offset in -12..=14 {
            let start_of_day = |d: jiff::civil::Date| {
                d.to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp()
                    - jiff::SignedDuration::from_hours(offset)
            };
            assert!(
                start_of_day(lower) <= min,
                "UTC{offset:+} lower {lower} resolves after {min}"
            );
            assert!(
                start_of_day(upper) >= max,
                "UTC{offset:+} upper {upper} resolves before {max} — the tail of the \
                 target day would be dropped"
            );
        }
    }

    #[test]
    fn params_reject_a_blank_or_time_bearing_cql() {
        use crate::ValidatedParams;
        let parse = |v: serde_json::Value| serde_json::from_value::<ConfluenceParams>(v).unwrap();

        assert!(
            parse(serde_json::json!({ "cql": "   " }))
                .validate()
                .is_err()
        );
        assert!(
            parse(serde_json::json!({ "cql": "type = page AND lastModified >= now(\"-1d\")" }))
                .validate()
                .is_err(),
            "a hand-written window would break --date backfill"
        );
        assert!(
            parse(serde_json::json!({ "cql": "type = page", "max_pages": 0 }))
                .validate()
                .is_err()
        );
        assert!(
            parse(serde_json::json!({ "cql": "type = page" }))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn only_my_edits_defaults_on() {
        let p: ConfluenceParams =
            serde_json::from_value(serde_json::json!({ "cql": "type = page" })).unwrap();
        assert!(p.only_my_edits);
        assert_eq!(p.lookback_hours, 24);
    }
}
