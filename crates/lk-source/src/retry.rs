//! Shared transient-failure retry for outbound HTTP.
//!
//! Slack carries its own retry inside `slack_post`; this is the provider-agnostic
//! equivalent for Google and Jira, which would otherwise abort a whole run on a single
//! rate-limit (429) or transient server error (5xx) from the provider — a real
//! resilience gap for unattended cron ingests.

use std::time::Duration;

use tokio::time::sleep;

use crate::SourceError;

const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_SECS: u64 = 2;

/// Send an HTTP request with bounded retries on rate-limit (429) and transient server
/// errors (5xx), plus transient connect/timeout send errors, honoring a numeric
/// `Retry-After` header when present (exponential backoff otherwise).
///
/// `send` rebuilds and sends the request on each attempt — a `reqwest::Response` (and the
/// `RequestBuilder` that produced it) is single-use — so only wrap IDEMPOTENT requests
/// (GETs, token refresh). On exhaustion the last response is returned as-is so the
/// caller's existing status handling produces the final error.
pub(crate) async fn send_with_retry<F, Fut>(send: F) -> Result<reqwest::Response, SourceError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let mut attempt = 0u32;
    loop {
        match send().await {
            Ok(resp) => {
                let status = resp.status();
                let transient = status.as_u16() == 429 || status.is_server_error();
                if transient && attempt < MAX_RETRIES {
                    let wait = parse_retry_after(&resp)
                        .unwrap_or(BASE_BACKOFF_SECS * (attempt as u64 + 1));
                    attempt += 1;
                    tracing::warn!(
                        status = status.as_u16(),
                        wait_secs = wait,
                        attempt,
                        "HTTP transient failure; retrying"
                    );
                    sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if (e.is_timeout() || e.is_connect()) && attempt < MAX_RETRIES {
                    let wait = BASE_BACKOFF_SECS * (attempt as u64 + 1);
                    attempt += 1;
                    tracing::warn!(error = %e, wait_secs = wait, attempt, "HTTP transient send error; retrying");
                    sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                return Err(e.into());
            }
        }
    }
}

/// A numeric `Retry-After` (seconds). The HTTP-date form is intentionally not parsed —
/// providers we hit use the seconds form, matching Slack's handling.
fn parse_retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}
