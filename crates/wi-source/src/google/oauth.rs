//! One-time OAuth 2.0 loopback (installed-app) flow to mint a Google refresh token.
//!
//! A refresh token isn't visible in the Cloud Console — it's issued when the user
//! completes the consent flow with `access_type=offline`. This runs that flow locally:
//! opens the consent page, captures the redirect on an ephemeral `127.0.0.1` port, and
//! exchanges the authorization code for a refresh token. Requires a "Desktop app" OAuth
//! client (Google auto-allows `http://127.0.0.1` redirects for those).

use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::SourceError;

/// Read-only scopes covering every Google source. One consent grants all three; the
/// resulting refresh token is shared across the Gmail/Drive/Calendar adapters.
const SCOPES: [&str; 3] = [
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/drive.readonly",
    "https://www.googleapis.com/auth/calendar.readonly",
];

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Run the loopback authorization-code flow and return a refresh token.
pub async fn obtain_refresh_token(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
) -> Result<String, SourceError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| SourceError::Auth(format!("bind loopback: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| SourceError::Auth(format!("local addr: {e}")))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let state = csrf_state();
    let auth_url = build_auth_url(client_id, &redirect_uri, &SCOPES, &state);

    eprintln!("\nAuthorize wiki-ingest (Google sign-in + consent) at:");
    eprintln!("  {auth_url}\n");
    if open_in_browser(&auth_url).is_ok() {
        eprintln!("(opened in your browser; complete the consent there)");
    }
    eprintln!("Waiting for the authorization redirect…");

    let code = tokio::time::timeout(CALLBACK_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| {
            SourceError::Auth("authorization timed out (no redirect received)".into())
        })??;

    let resp = http
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", &code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", &redirect_uri),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(SourceError::Auth(format!("token exchange failed: {body}")));
    }
    let tokens: TokenResponse = resp.json().await?;
    tokens.refresh_token.ok_or_else(|| {
        SourceError::Auth(
            "Google returned no refresh token. Revoke this app at \
             https://myaccount.google.com/permissions and retry (the flow forces consent)."
                .into(),
        )
    })
}

fn build_auth_url(client_id: &str, redirect_uri: &str, scopes: &[&str], state: &str) -> String {
    let mut url = reqwest::Url::parse(AUTH_ENDPOINT).expect("valid auth endpoint");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes.join(" "))
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("state", state);
    url.to_string()
}

/// Accept connections until the OAuth redirect arrives, ignoring unrelated hits (favicon,
/// etc.) so a stray browser request doesn't abort the flow.
async fn wait_for_code(
    listener: &TcpListener,
    expected_state: &str,
) -> Result<String, SourceError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| SourceError::Auth(format!("accept: {e}")))?;
        let mut buf = vec![0u8; 8192];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| SourceError::Auth(format!("read request: {e}")))?;
        let request = String::from_utf8_lossy(&buf[..n]);

        match request_target(&request).and_then(|t| parse_callback(t, expected_state)) {
            Some(Ok(code)) => {
                respond(&mut stream, "✓ Authorized — you can close this tab.").await;
                return Ok(code);
            }
            Some(Err(e)) => {
                respond(&mut stream, "Authorization failed; return to the terminal.").await;
                return Err(e);
            }
            None => respond(&mut stream, "Waiting for Google authorization…").await,
        }
    }
}

/// Extract the request-target from the HTTP request line `GET <target> HTTP/1.1`.
fn request_target(request: &str) -> Option<&str> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    parts.next()?; // method
    parts.next()
}

/// Parse the callback query: `Some(Ok(code))` on a matching-state code, `Some(Err)` on an
/// `error=` param or state mismatch, `None` when it isn't the OAuth callback at all.
fn parse_callback(target: &str, expected_state: &str) -> Option<Result<String, SourceError>> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    let (mut code, mut state, mut error) = (None, None, None);
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    if let Some(err) = error {
        return Some(Err(SourceError::Auth(format!(
            "authorization denied: {err}"
        ))));
    }
    let code = code?;
    if state.as_deref() != Some(expected_state) {
        return Some(Err(SourceError::Auth(
            "state mismatch (possible CSRF); aborted".into(),
        )));
    }
    Some(Ok(code))
}

async fn respond(stream: &mut TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8>\
         <body style=\"font-family:system-ui;padding:2rem\">{message}</body>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Loopback redirects can't be intercepted off-host, so `state` is defense-in-depth;
/// derive an unguessable-enough token from high-resolution time + pid.
fn csrf_state() -> String {
    let nanos = jiff::Timestamp::now().as_nanosecond() as u128;
    format!("{nanos:x}{:x}", std::process::id())
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let (program, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (program, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no browser opener for this platform",
    ));

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_has_offline_consent_and_scopes() {
        let url = build_auth_url("cid", "http://127.0.0.1:1234", &SCOPES, "st8");
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=st8"));
        assert!(url.contains("gmail.readonly"));
        assert!(url.contains("calendar.readonly"));
    }

    #[test]
    fn request_target_extracts_path_query() {
        let req = "GET /?code=abc&state=s HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(request_target(req), Some("/?code=abc&state=s"));
    }

    #[test]
    fn callback_returns_code_on_matching_state() {
        let r = parse_callback("/?code=A.B%2FC&state=s", "s");
        assert!(matches!(r, Some(Ok(ref c)) if c == "A.B/C"));
    }

    #[test]
    fn callback_rejects_state_mismatch() {
        assert!(matches!(
            parse_callback("/?code=x&state=bad", "s"),
            Some(Err(_))
        ));
    }

    #[test]
    fn callback_surfaces_error_param() {
        assert!(matches!(
            parse_callback("/?error=access_denied&state=s", "s"),
            Some(Err(_))
        ));
    }

    #[test]
    fn callback_ignores_unrelated_request() {
        assert!(parse_callback("/favicon.ico", "s").is_none());
    }
}
