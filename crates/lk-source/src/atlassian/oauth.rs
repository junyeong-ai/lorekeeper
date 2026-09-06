//! One-time OAuth 2.0 (3LO) authorization-code flow to mint an Atlassian refresh token.
//!
//! Differs from the Google loopback flow in two ways that matter:
//!
//! 1. **The redirect URI must be registered.** Atlassian does not auto-allow arbitrary
//!    `127.0.0.1` ports, so the callback port is FIXED and must match the "Callback URL"
//!    configured on the OAuth app — an ephemeral port would simply be rejected.
//! 2. **The resulting refresh token rotates.** Every later refresh invalidates the previous
//!    token, so this grant must belong to Lorekeeper alone; pointing it at an app another
//!    tool also refreshes makes the two clients invalidate each other on every run.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::SourceError;

/// `offline_access` is what makes a grant durable — without it Atlassian issues only a
/// short-lived access token and no refresh token at all, so every scope set includes it.
const OFFLINE_ACCESS: &str = "offline_access";

/// Read-only scopes the Jira adapter needs.
const JIRA_SCOPES: [&str; 2] = ["read:jira-user", "read:jira-work"];

/// Read-only scopes the Confluence adapter needs — the classic granular set, which is what
/// the v1 CQL search endpoint (`/rest/api/content/search`) authorizes against.
const CONFLUENCE_SCOPES: [&str; 3] = [
    "read:confluence-content.all",
    "read:confluence-space.summary",
    "search:confluence",
];

/// Which products one OAuth app covers.
///
/// This is a per-app fact, not a preference: an app authorizes only the scopes registered
/// on it, so requesting Confluence scopes from a Jira-only app fails the whole consent.
/// Organizations commonly register one app per product, which is exactly why a Lorekeeper
/// instance is scoped to a credential rather than to a site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Products {
    Jira,
    Confluence,
    Both,
}

impl Products {
    /// The least-privilege set Lorekeeper actually needs: read plus `offline_access`.
    ///
    /// It is only a DEFAULT. An app authorizes exactly the scopes registered on it, and some
    /// providers refuse a request that does not match that registration — so the caller may
    /// substitute the app's own list. Lorekeeper never issues a write regardless of what the
    /// grant permits.
    pub fn default_scopes(self) -> Vec<&'static str> {
        let mut scopes = vec![OFFLINE_ACCESS];
        if matches!(self, Products::Jira | Products::Both) {
            scopes.extend(JIRA_SCOPES);
        }
        if matches!(self, Products::Confluence | Products::Both) {
            scopes.extend(CONFLUENCE_SCOPES);
        }
        scopes
    }
}

const AUTH_ENDPOINT: &str = "https://auth.atlassian.com/authorize";
const ACCESSIBLE_RESOURCES: &str = "https://api.atlassian.com/oauth/token/accessible-resources";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Default loopback port for the callback. Register
/// `http://127.0.0.1:9123/callback` as the app's Callback URL, or pass a port matching
/// whatever is registered.
pub const DEFAULT_REDIRECT_PORT: u16 = 9123;

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct AccessibleResource {
    id: String,
    #[serde(default)]
    url: String,
}

/// One site this grant can address.
pub struct AtlassianSite {
    pub cloud_id: String,
    pub site_url: String,
}

/// A completed authorization: the durable refresh token plus every tenant it can reach.
///
/// `sites` is a LIST because one account commonly reaches several tenants, and picking one
/// silently would bind the vault to whichever the API happened to return first. The caller
/// chooses.
pub struct AtlassianGrant {
    pub refresh_token: String,
    pub sites: Vec<AtlassianSite>,
}

/// Run the authorization-code flow and return a refresh token plus the sites it can address.
///
/// `port` must match the OAuth app's registered callback URL — Atlassian rejects any
/// redirect URI that is not registered, so this cannot be an ephemeral port.
pub async fn build_grant(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    port: u16,
    scopes: &[String],
) -> Result<AtlassianGrant, SourceError> {
    if scopes.iter().all(|s| s != OFFLINE_ACCESS) {
        return Err(SourceError::Auth(format!(
            "the scope set must include `{OFFLINE_ACCESS}` — without it Atlassian issues no \
             refresh token and the grant cannot outlive one hour."
        )));
    }
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        SourceError::Auth(format!(
            "bind loopback port {port}: {e}. Atlassian requires the callback URL to be \
             registered on the app, so this exact port must be free."
        ))
    })?;

    let state = csrf_state();
    let pkce = Pkce::build();
    let auth_url = build_auth_url(client_id, &redirect_uri, scopes, &state, &pkce.challenge);

    eprintln!("\nAuthorize Lorekeeper (Atlassian sign-in + consent) at:");
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

    // Client credentials go in the `Authorization: Basic` header, NOT the body.
    //
    // RFC 6749 §2.3.1 makes Basic the REQUIRED client-authentication method and body
    // parameters merely OPTIONAL, and Atlassian enforces that: an exchange carrying the
    // secret in the body is treated as an unauthenticated client and refused with
    // `access_denied: Unauthorized` — but only once the code itself validates, so probing
    // with a throwaway code hides the problem behind `invalid_request` and makes this look
    // like a bad-code or bad-scope failure instead.
    let resp = http
        .post(super::token_endpoint())
        .basic_auth(client_id, Some(client_secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", pkce.verifier.as_str()),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        // Consent succeeded (we hold a code) but the exchange was refused, and the two
        // errors that reach here mean opposite things. `invalid_grant` is the code itself
        // being rejected — it is minted for one exact (client, redirect_uri, verifier)
        // triple and is single-use and short-lived — while `access_denied` is a code the
        // server accepted and a policy that refused the result.
        let hint = if body.contains("invalid_grant") {
            "\n\nThe authorization code was not accepted. It is single-use and expires in \
             minutes, and it is bound to one exact client id, callback URL and PKCE verifier \
             — so a retried, reused or stale code fails here, as does a callback URL that \
             differs from the app's registration by so much as a trailing slash. Re-run \
             `lore init credentials` and complete the consent without pausing."
        } else if body.contains("access_denied") {
            "\n\nThe code was accepted and the exchange was refused on policy. The usual \
             cause is a scope set that does not match the app's registration: some apps grant \
             only their exact registered list, not a subset. Re-run `lore init credentials` \
             and paste the app's full scope list at the scope prompt (developer.atlassian.com \
             → your app → Permissions shows it)."
        } else {
            ""
        };
        return Err(SourceError::Auth(format!(
            "token exchange failed: {body}{hint}"
        )));
    }
    let tokens: TokenResponse = resp.json().await?;
    let refresh_token = tokens.refresh_token.ok_or_else(|| {
        SourceError::Auth(
            "Atlassian returned no refresh token. Ensure `offline_access` is among the app's \
             scopes — without it only a short-lived access token is issued."
                .into(),
        )
    })?;
    let access_token = tokens.access_token.ok_or_else(|| {
        SourceError::Auth("Atlassian returned no access token to resolve the site id".into())
    })?;

    let sites = resolve_sites(http, &access_token).await?;
    Ok(AtlassianGrant {
        refresh_token,
        sites,
    })
}

/// List the tenants this grant can address. API calls route through
/// `api.atlassian.com/ex/{product}/{cloud_id}`, so the id — not the hostname — is what the
/// adapters need.
async fn resolve_sites(
    http: &reqwest::Client,
    access_token: &str,
) -> Result<Vec<AtlassianSite>, SourceError> {
    let resp = http
        .get(ACCESSIBLE_RESOURCES)
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(SourceError::Api {
            status,
            message: format!("accessible-resources failed: {body}"),
        });
    }
    let resources: Vec<AccessibleResource> = resp.json().await?;
    if resources.is_empty() {
        return Err(SourceError::Auth(
            "the authorized account can reach no Atlassian sites — check that the app is \
             approved for this organization"
                .into(),
        ));
    }
    Ok(resources
        .into_iter()
        .map(|r| AtlassianSite {
            cloud_id: r.id,
            site_url: r.url.trim_end_matches('/').to_string(),
        })
        .collect())
}

/// PKCE (RFC 7636) parameters for one authorization.
///
/// Sent unconditionally. An app configured to REQUIRE PKCE accepts consent and then refuses
/// the token exchange when `code_verifier` is missing — a failure that looks like bad
/// credentials and is miserable to diagnose. For an app that merely permits PKCE this costs
/// nothing, so there is no case for making it conditional.
struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    fn build() -> Self {
        // RFC 7636 requires 43–128 unreserved characters; 32 random bytes base64url-encode
        // to exactly 43.
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        // S256, not `plain`: the challenge must be SHA-256 of the verifier — the one digest
        // the spec defines, so blake3 (used elsewhere in this workspace) is not substitutable.
        let digest = Sha256::digest(verifier.as_bytes());
        Self {
            challenge: URL_SAFE_NO_PAD.encode(digest),
            verifier,
        }
    }
}

fn build_auth_url(
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    code_challenge: &str,
) -> String {
    let mut url = reqwest::Url::parse(AUTH_ENDPOINT).expect("valid auth endpoint");
    url.query_pairs_mut()
        .append_pair("audience", "api.atlassian.com")
        .append_pair("client_id", client_id)
        .append_pair("scope", &scopes.join(" "))
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("response_type", "code")
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        // Without this Atlassian may reuse a prior consent and skip issuing a refresh token.
        .append_pair("prompt", "consent");
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
            None => respond(&mut stream, "Waiting for Atlassian authorization…").await,
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

/// Loopback redirects can't be intercepted off-host, so `state` is defense-in-depth — but
/// a CSPRNG is already on hand for PKCE, and the full auth URL is briefly visible in `ps`
/// to local users, so there is no reason to settle for a guessable time+pid value.
fn csrf_state() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
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
    fn scope_sets_are_per_app_and_always_durable() {
        // An app registered for one product rejects the other's scopes outright, so the
        // sets must be selectable rather than unioned.
        let jira = Products::Jira.default_scopes();
        assert!(jira.contains(&"read:jira-work"));
        assert!(!jira.iter().any(|s| s.contains("confluence")));

        let conf = Products::Confluence.default_scopes();
        assert!(conf.contains(&"search:confluence"));
        assert!(!conf.iter().any(|s| s.contains("jira")));

        // Without offline_access there is no refresh token, so no set may omit it.
        for products in [Products::Jira, Products::Confluence, Products::Both] {
            assert!(
                products.default_scopes().contains(&OFFLINE_ACCESS),
                "{products:?}"
            );
        }
        // Asserted as exact sets rather than by representative member: a scope dropped or
        // narrowed still leaves the grant working, and only the API read fails — with a 403 at
        // ingest time rather than anything the consent flow could show. Counting them was the
        // whole check before, so any substitution of the same size passed.
        assert_eq!(
            Products::Jira.default_scopes(),
            vec![OFFLINE_ACCESS, "read:jira-user", "read:jira-work"]
        );
        assert_eq!(
            Products::Confluence.default_scopes(),
            vec![
                OFFLINE_ACCESS,
                "read:confluence-content.all",
                "read:confluence-space.summary",
                "search:confluence",
            ]
        );
        assert_eq!(
            Products::Both.default_scopes(),
            vec![
                OFFLINE_ACCESS,
                "read:jira-user",
                "read:jira-work",
                "read:confluence-content.all",
                "read:confluence-space.summary",
                "search:confluence",
            ]
        );
        for products in [Products::Jira, Products::Confluence, Products::Both] {
            for scope in products.default_scopes() {
                assert!(
                    scope == OFFLINE_ACCESS
                        || scope.starts_with("read:")
                        || scope.starts_with("search:"),
                    "{scope} is not read-only, and Lorekeeper never issues an Atlassian write"
                );
            }
        }
    }

    #[test]
    fn auth_url_requests_offline_access_and_both_products() {
        let scopes: Vec<String> = Products::Both
            .default_scopes()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let url = build_auth_url(
            "cid",
            "http://127.0.0.1:9123/callback",
            &scopes,
            "st8",
            "chal",
        );
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("audience=api.atlassian.com"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=st8"));
        // offline_access is what makes the grant durable; without it there is no refresh token.
        assert!(url.contains("offline_access"));
        assert!(url.contains("read%3Ajira-work"));
        assert!(url.contains("search%3Aconfluence"));
    }

    #[test]
    fn pkce_challenge_is_sha256_of_the_verifier_in_the_spec_length_range() {
        let pkce = Pkce::build();
        // RFC 7636: verifier is 43–128 unreserved chars.
        assert!(
            (43..=128).contains(&pkce.verifier.len()),
            "len {}",
            pkce.verifier.len()
        );
        assert!(
            pkce.verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "verifier must be url-safe unreserved: {}",
            pkce.verifier
        );
        // S256 — the server recomputes exactly this, so the digest is not interchangeable.
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        assert!(!pkce.challenge.contains('='), "base64url must be unpadded");
    }

    #[test]
    fn pkce_is_fresh_per_authorization() {
        assert_ne!(Pkce::build().verifier, Pkce::build().verifier);
    }

    #[test]
    fn auth_url_carries_the_s256_challenge() {
        let scopes: Vec<String> = Products::Both
            .default_scopes()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let url = build_auth_url(
            "cid",
            "http://127.0.0.1:9123/callback",
            &scopes,
            "s",
            "CHAL",
        );
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn auth_url_registers_the_exact_callback_path() {
        let scopes: Vec<String> = Products::Both
            .default_scopes()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let url = build_auth_url(
            "cid",
            "http://127.0.0.1:9123/callback",
            &scopes,
            "s",
            "chal",
        );
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9123%2Fcallback"));
    }

    #[test]
    fn request_target_extracts_path_query() {
        let req = "GET /callback?code=abc&state=s HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(request_target(req), Some("/callback?code=abc&state=s"));
    }

    #[test]
    fn callback_returns_code_on_matching_state() {
        let r = parse_callback("/callback?code=A.B%2FC&state=s", "s");
        assert!(matches!(r, Some(Ok(ref c)) if c == "A.B/C"));
    }

    #[test]
    fn callback_rejects_state_mismatch() {
        assert!(matches!(
            parse_callback("/callback?code=x&state=bad", "s"),
            Some(Err(_))
        ));
    }

    #[test]
    fn callback_surfaces_error_param() {
        assert!(matches!(
            parse_callback("/callback?error=access_denied&state=s", "s"),
            Some(Err(_))
        ));
    }

    #[test]
    fn callback_ignores_unrelated_request() {
        assert!(parse_callback("/favicon.ico", "s").is_none());
    }
}
