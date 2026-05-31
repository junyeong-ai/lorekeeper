pub mod channel;
pub mod search;

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::sleep;

use crate::SourceError;

const API: &str = "https://slack.com/api";
const MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_AFTER: u64 = 5;

#[derive(Deserialize)]
struct SlackResponse<T> {
    ok: bool,
    error: Option<String>,
    #[serde(flatten)]
    data: T,
}

/// POST to a Slack Web API method with `application/x-www-form-urlencoded` params. Every
/// method accepts form encoding, whereas JSON bodies are rejected by read methods such as
/// `search.messages` — so form is the one encoding that works across all of them.
async fn slack_post<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    method: &str,
    params: &[(&str, &str)],
) -> Result<T, SourceError> {
    let mut attempts = 0u32;

    loop {
        let resp = http
            .post(format!("{API}/{method}"))
            .bearer_auth(token)
            .form(params)
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            attempts += 1;
            if attempts > MAX_RETRIES {
                let text = resp.text().await.unwrap_or_default();
                return Err(SourceError::Api {
                    status: 429,
                    message: text,
                });
            }
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_RETRY_AFTER);
            sleep(Duration::from_secs(retry_after)).await;
            continue;
        }

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(SourceError::Api {
                status,
                message: text,
            });
        }

        let wrapper: SlackResponse<T> = resp
            .json()
            .await
            .map_err(|e| SourceError::Parse(e.to_string()))?;

        if !wrapper.ok {
            return Err(SourceError::Api {
                status: 200,
                message: wrapper
                    .error
                    .unwrap_or_else(|| "unknown Slack error".into()),
            });
        }

        return Ok(wrapper.data);
    }
}

/// The `next_cursor` envelope returned by every paginated Slack read method
/// (`users.list`, `conversations.list`/`history`/`replies`). Single-sourced so
/// the four call sites can't drift on the field name or its default.
#[derive(Deserialize)]
pub(crate) struct ResponseMetadata {
    #[serde(default)]
    next_cursor: String,
}

impl ResponseMetadata {
    /// The next-page cursor, or empty when the response carried no metadata.
    fn cursor(opt: Option<Self>) -> String {
        opt.map(|r| r.next_cursor).unwrap_or_default()
    }
}

/// Drive Slack cursor pagination for a read `method`, collecting every item up to
/// `max_total`. `decode` maps one decoded page to `(items, next_cursor, has_more)`.
/// The loop owns the termination logic — empty page, cap reached, empty cursor,
/// `has_more == false` — so the collect-all call sites can never disagree on it.
pub(crate) async fn paginate<Page, Item, F>(
    http: &reqwest::Client,
    token: &str,
    method: &str,
    base_params: &[(&str, &str)],
    max_total: usize,
    mut decode: F,
) -> Result<Vec<Item>, SourceError>
where
    Page: serde::de::DeserializeOwned,
    F: FnMut(Page) -> (Vec<Item>, String, bool),
{
    let mut out: Vec<Item> = Vec::new();
    let mut cursor = String::new();
    loop {
        let mut params: Vec<(&str, &str)> = base_params.to_vec();
        if !cursor.is_empty() {
            params.push(("cursor", cursor.as_str()));
        }
        let page: Page = slack_post(http, token, method, &params).await?;
        let (items, next, has_more) = decode(page);
        let page_empty = items.is_empty();
        out.extend(items);
        if out.len() >= max_total {
            // Hit the safety cap with more pages available — make the silent
            // truncation observable so a very busy channel/thread/workspace is
            // visible rather than quietly losing items.
            out.truncate(max_total);
            if !next.is_empty() && has_more {
                tracing::warn!(
                    method,
                    max_total,
                    "Slack pagination hit cap; results truncated"
                );
            }
            break;
        }
        if page_empty {
            break;
        }
        if next.is_empty() || !has_more {
            break;
        }
        cursor = next;
    }
    Ok(out)
}

/// Split a converted Slack body into `(title, body)`.
///
/// Promotes the first line to a title and removes it from the body ONLY when it
/// reads like one: a single short line that is not a code fence, a list/quote
/// marker, a bare mention, or a bare URL. Otherwise the body is kept intact and
/// the title is a non-destructive preview — so a message that opens with a code
/// block, a greeting, or a link never loses content to the heading or leaves an
/// unbalanced fence behind.
pub(crate) fn split_first_line(body: &str) -> (String, String) {
    let first = body.lines().next().unwrap_or_default().trim();
    let title_like = !first.is_empty()
        && first.chars().count() <= 120
        && !first.starts_with("```")
        && !first.starts_with("~~~")
        && !first.starts_with("> ")
        && !first.starts_with("- ")
        && !first.starts_with("* ")
        && !first.starts_with("<@")
        && !first.starts_with("http://")
        && !first.starts_with("https://");

    if title_like {
        let rest = body
            .split_once('\n')
            .map(|x| x.1)
            .unwrap_or("")
            .trim_start()
            .to_string();
        (first.to_string(), rest)
    } else {
        (preview_title(body), body.trim_start().to_string())
    }
}

/// A one-line, length-bounded preview of a body, used as a title when the first
/// line is not itself title-like. Skips fence-marker lines so a code-block opener
/// doesn't leak backticks into the title, and collapses whitespace so a multi-line
/// opener reads as a single label.
fn preview_title(body: &str) -> String {
    const MAX: usize = 80;
    let text = body
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("```") || t.starts_with("~~~"))
        })
        .collect::<Vec<_>>()
        .join(" ");
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        flat
    } else {
        let truncated: String = flat.chars().take(MAX).collect();
        format!("{}…", truncated.trim_end())
    }
}

#[derive(Debug, Deserialize)]
struct ChannelInfo {
    id: String,
    name: String,
}

/// A bare Slack channel id (`C…`/`G…`/`D…` + uppercase alphanumerics) — config may give
/// either an id or a `#name`. Ids are used directly; only names need a lookup.
fn looks_like_channel_id(s: &str) -> bool {
    matches!(s.chars().next(), Some('C' | 'G' | 'D'))
        && s.len() >= 9
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Resolve all workspace user IDs to display names via `users.list`. Returns an empty map
/// on any error so callers degrade gracefully to raw IDs.
pub(crate) async fn resolve_users(http: &reqwest::Client, token: &str) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Profile {
        display_name: Option<String>,
        real_name: Option<String>,
    }

    #[derive(Deserialize)]
    struct Member {
        id: String,
        name: Option<String>,
        profile: Option<Profile>,
    }

    #[derive(Deserialize)]
    struct MembersPage {
        #[serde(default)]
        members: Vec<Member>,
        #[serde(default)]
        response_metadata: Option<ResponseMetadata>,
    }

    fn display_name(m: &Member) -> String {
        m.profile
            .as_ref()
            .and_then(|p| {
                p.display_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or(p.real_name.as_deref().filter(|s| !s.is_empty()))
            })
            .or(m.name.as_deref())
            .unwrap_or_default()
            .to_string()
    }

    // Paginate the full member list (large workspaces exceed 1000); cap at 10,000.
    // This loop is intentionally NOT the shared fail-fast `paginate`: user resolution is
    // best-effort — a mid-pagination failure must KEEP the names already collected (the
    // rest fall back to raw IDs) rather than discard everything. `slack-channel` history
    // and `conversations.replies` use `paginate` precisely because there the opposite
    // policy is correct (a partial read should abort loudly).
    const MAX_PAGES: usize = 50; // 50 × 200 = 10,000 users
    let mut map = HashMap::new();
    let mut cursor = String::new();
    for _ in 0..MAX_PAGES {
        let mut params: Vec<(&str, &str)> = vec![("limit", "200")];
        if !cursor.is_empty() {
            params.push(("cursor", cursor.as_str()));
        }
        match slack_post::<MembersPage>(http, token, "users.list", &params).await {
            Ok(page) => {
                if page.members.is_empty() {
                    break;
                }
                for m in &page.members {
                    map.insert(m.id.clone(), display_name(m));
                }
                cursor = ResponseMetadata::cursor(page.response_metadata);
                if cursor.is_empty() {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, resolved = map.len(), "failed to resolve some Slack users; remaining fall back to raw IDs");
                break;
            }
        }
    }
    map
}

async fn resolve_channel_id(
    http: &reqwest::Client,
    token: &str,
    channel_ref: &str,
) -> Result<String, SourceError> {
    if looks_like_channel_id(channel_ref) {
        return Ok(channel_ref.to_string());
    }
    let name = channel_ref.strip_prefix('#').unwrap_or(channel_ref);

    #[derive(Deserialize)]
    struct ChannelsPage {
        #[serde(default)]
        channels: Vec<ChannelInfo>,
        #[serde(default)]
        response_metadata: Option<ResponseMetadata>,
    }

    // Paginate through the full channel list (large workspaces exceed 1000).
    const MAX_PAGES: usize = 25; // 25 × 200 = 5,000 channels
    let mut cursor = String::new();
    for _ in 0..MAX_PAGES {
        let mut params: Vec<(&str, &str)> = vec![
            ("types", "public_channel,private_channel"),
            ("limit", "200"),
            ("exclude_archived", "true"),
        ];
        if !cursor.is_empty() {
            params.push(("cursor", cursor.as_str()));
        }
        let page: ChannelsPage = slack_post(http, token, "conversations.list", &params).await?;

        if page.channels.is_empty() {
            break;
        }
        if let Some(ch) = page.channels.into_iter().find(|c| c.name == name) {
            return Ok(ch.id);
        }

        cursor = ResponseMetadata::cursor(page.response_metadata);
        if cursor.is_empty() {
            break;
        }
    }

    Err(SourceError::Parse(format!(
        "channel not found: {channel_ref}"
    )))
}

#[cfg(test)]
mod tests {
    use super::split_first_line;

    #[test]
    fn promotes_short_first_line_and_strips_it() {
        let (title, body) = split_first_line("Deploy completed\n\ndetails here");
        assert_eq!(title, "Deploy completed");
        assert_eq!(body, "details here");
    }

    #[test]
    fn code_fence_opener_keeps_body_whole_and_title_free_of_backticks() {
        // Promoting a ``` line would title the page "```" and leave an unbalanced
        // fence in the body. The body must stay intact and the title must carry no
        // fence markers.
        let md = "```\nfn main() {}\n```";
        let (title, body) = split_first_line(md);
        assert!(
            !title.contains("```"),
            "fence markers must not leak into the title: {title:?}"
        );
        assert!(
            body.contains("```\nfn main() {}\n```"),
            "body must keep the full fence"
        );
    }

    #[test]
    fn bare_url_first_line_keeps_body_whole() {
        // A bare URL opener must not be stripped into the title and deleted from the
        // body — the body keeps the URL; the title is a preview.
        let md = "https://example.com/x\n\nsee link";
        let (_title, body) = split_first_line(md);
        assert!(
            body.contains("https://example.com/x"),
            "body must keep the URL"
        );
    }

    #[test]
    fn mention_first_line_keeps_body_whole() {
        let md = "<@U123> please review\n\nbody";
        let (_title, body) = split_first_line(md);
        assert!(
            body.contains("<@U123> please review"),
            "body must keep the mention line"
        );
    }

    #[test]
    fn preview_title_truncates_long_unstructured_body() {
        let long = "a ".repeat(100);
        let (title, body) = split_first_line(&long);
        assert!(
            title.chars().count() <= 81,
            "preview must be bounded: {}",
            title.chars().count()
        );
        assert_eq!(
            body.trim(),
            long.trim(),
            "non-title body must be preserved whole"
        );
    }
}
