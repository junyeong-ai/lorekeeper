//! Shared Atlassian plumbing: authentication, API routing, and REST dialect.
//!
//! Jira and Confluence sit on one instance behind one credential, so both adapters go
//! through a single [`AtlassianAuth`]. Adapters ask it three questions — *what base URL*,
//! *what `Authorization` header*, *which REST dialect* — and never branch on the auth
//! method themselves. Adding a method (or a deployment) is a change here alone.
//!
//! # Two orthogonal axes
//!
//! **How we authenticate** ([`AtlassianAuthMethod`]) and **which API dialect the instance
//! speaks** ([`Deployment`]) are separate concerns, but they are not independently
//! configurable: each credential form exists on exactly one deployment (OAuth and both
//! account-token forms are Cloud-only; personal access tokens are Data Center/Server-only).
//! So the deployment is DERIVED from the method rather than configured, which makes an
//! impossible pairing — a PAT against the Cloud gateway — unrepresentable instead of merely
//! discouraged.
//!
//! # Two hosts, and why the method chooses one
//!
//! Cloud answers at two addresses and each honors a different set of credentials: the site
//! (`{org}.atlassian.net`) takes a classic account token, while the gateway
//! (`api.atlassian.com`) takes an OAuth grant or a SCOPED account token. Neither accepts
//! the other's credential, and the site does not reject a scoped token so much as ignore
//! it — an anonymous 401 that names nothing. The method therefore fixes the host, and
//! [`AtlassianAuth::explain_failure`] is where that shows up as a sentence.
//!
//! An **IP allowlist** cuts across this, and it is about the PRINCIPAL rather than the
//! host: it answers `403 "your IP address is not listed in the IP allowlist"` to an account
//! token from an unlisted address at EITHER host, while admitting an org-approved OAuth app
//! at the same moment from the same address. So on such an instance `oauth` is the only
//! method that reaches the API from an address the list does not name; from one it does,
//! and on an instance with no list, every method works.

pub mod oauth;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;

use crate::SourceError;
use crate::credentials::{AtlassianAuthMethod, AtlassianCredentials, CredentialStore, Credentials};

/// Atlassian's OAuth token endpoint (the auth server, not the API gateway).
const DEFAULT_TOKEN_ENDPOINT: &str = "https://auth.atlassian.com/oauth/token";

/// The token endpoint, overridable via `LORE_ATLASSIAN_TOKEN_ENDPOINT`.
///
/// The override exists so the exact bytes of a token request can be captured against a
/// local server. An OAuth exchange that fails on the provider side is otherwise a black
/// box — the error text names a policy, never the field that tripped it — and guessing at
/// request shape costs a browser round-trip per attempt.
pub fn token_endpoint() -> String {
    std::env::var("LORE_ATLASSIAN_TOKEN_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_TOKEN_ENDPOINT.to_string())
}

/// Cloud API gateway. OAuth traffic addresses tenants here by id, never by hostname.
const GATEWAY: &str = "https://api.atlassian.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    Jira,
    Confluence,
}

impl Product {
    const fn segment(self) -> &'static str {
        match self {
            Product::Jira => "jira",
            Product::Confluence => "confluence",
        }
    }
}

/// Which Atlassian deployment an instance is, and therefore which REST dialect it speaks.
///
/// This is the one place the Cloud/Server API divergence is named; adapters consult it
/// instead of sniffing URLs or guessing from credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deployment {
    /// Atlassian Cloud. Jira REST v3 with token-cursor search; Confluence under `/wiki`.
    Cloud,
    /// Self-hosted Data Center / Server. Jira REST v2 with offset search; Confluence at the
    /// site root (its context path is already part of `site_url`).
    DataCenter,
}

impl Deployment {
    /// Jira's search endpoint path and pagination style differ per deployment: Cloud
    /// replaced the offset-paged `/search` with token-paged `/search/jql` (the old one now
    /// returns 410), while Data Center still serves offset-paged v2 `/search`.
    pub fn jira_search_path(self) -> &'static str {
        match self {
            Deployment::Cloud => "/rest/api/3/search/jql",
            Deployment::DataCenter => "/rest/api/2/search",
        }
    }

    pub fn jira_myself_path(self) -> &'static str {
        match self {
            Deployment::Cloud => "/rest/api/3/myself",
            Deployment::DataCenter => "/rest/api/2/myself",
        }
    }

    /// Cloud pages a JQL search by opaque `nextPageToken`; Data Center by `startAt` offset.
    pub fn jira_paging(self) -> JiraPaging {
        match self {
            Deployment::Cloud => JiraPaging::Token,
            Deployment::DataCenter => JiraPaging::Offset,
        }
    }

    /// The ownership key. Cloud identifies users by `accountId`; Data Center has no such
    /// field and identifies them by `name` (the username).
    pub fn jira_user_key(self) -> &'static str {
        match self {
            Deployment::Cloud => "accountId",
            Deployment::DataCenter => "name",
        }
    }

    /// Confluence's equivalent ownership key — `accountId` on Cloud, `username` on Data
    /// Center. Kept separate from [`Self::jira_user_key`] because the two products spell
    /// the Data Center field differently (`name` vs `username`).
    pub fn confluence_user_key(self) -> &'static str {
        match self {
            Deployment::Cloud => "accountId",
            Deployment::DataCenter => "username",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JiraPaging {
    Token,
    Offset,
}

/// A resolved credential for one batch of requests.
///
/// Resolved before a request and reused for its retries: `send_with_retry`'s closure
/// returns a future typed on `reqwest::Error`, which cannot carry an auth failure, so the
/// header has to be a plain value by then. Callers that paginate re-resolve per page, which
/// bounds how long one value is held to a single page and its retries.
#[derive(Clone)]
pub enum AuthHeader {
    Bearer(String),
    Basic { user: String, secret: String },
}

impl AuthHeader {
    pub fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            AuthHeader::Bearer(token) => rb.bearer_auth(token),
            AuthHeader::Basic { user, secret } => rb.basic_auth(user, Some(secret)),
        }
    }
}

struct CachedToken {
    access_token: String,
    expires_at: jiff::Timestamp,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    /// Atlassian issues a NEW refresh token on every refresh and invalidates the one just
    /// used ("rotating refresh tokens"). Absent only on non-rotating grants.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Authenticated access to one Atlassian instance.
///
/// Share one owner per instance to reuse access tokens. Separate owners and processes
/// coordinate refresh through the canonical credential-store lock and reload disk state.
pub struct AtlassianAuth {
    http: reqwest::Client,
    /// Instance key in `credentials.atlassian`, so a rotated token is written back to the
    /// right entry.
    instance: String,
    site_url: String,
    method: Method,
    credential_path: PathBuf,
    token_endpoint: String,
}

/// Runtime form of [`AtlassianAuthMethod`] — the OAuth arm carries mutable rotation state,
/// so it cannot simply be the deserialized config value.
enum Method {
    Oauth {
        client_id: String,
        client_secret: String,
        cloud_id: String,
        /// Mutated by rotation: the token that authenticated a refresh is dead by the time
        /// the response arrives, so the successor replaces it in memory and on disk.
        refresh_token: Mutex<String>,
        cache: Mutex<Option<CachedToken>>,
    },
    ApiToken {
        email: String,
        api_token: String,
    },
    ScopedToken {
        email: String,
        api_token: String,
        cloud_id: String,
    },
    PersonalAccessToken {
        token: String,
    },
}

impl AtlassianAuth {
    pub fn new(
        http: reqwest::Client,
        instance: &str,
        creds: &AtlassianCredentials,
        vault_root: &Path,
    ) -> Self {
        let method = match &creds.auth {
            AtlassianAuthMethod::Oauth {
                client_id,
                client_secret,
                refresh_token,
                cloud_id,
            } => Method::Oauth {
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                cloud_id: cloud_id.clone(),
                refresh_token: Mutex::new(refresh_token.clone()),
                cache: Mutex::new(None),
            },
            AtlassianAuthMethod::ApiToken { email, api_token } => Method::ApiToken {
                email: email.clone(),
                api_token: api_token.clone(),
            },
            AtlassianAuthMethod::ScopedToken {
                email,
                api_token,
                cloud_id,
            } => Method::ScopedToken {
                email: email.clone(),
                api_token: api_token.clone(),
                cloud_id: cloud_id.clone(),
            },
            AtlassianAuthMethod::PersonalAccessToken { token } => Method::PersonalAccessToken {
                token: token.clone(),
            },
        };
        Self {
            http,
            instance: instance.to_string(),
            site_url: creds.site_url.trim_end_matches('/').to_string(),
            method,
            credential_path: Credentials::path(vault_root),
            token_endpoint: token_endpoint(),
        }
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Derived, never configured — see the module docs on the two axes.
    pub fn deployment(&self) -> Deployment {
        match self.method {
            Method::Oauth { .. } | Method::ApiToken { .. } | Method::ScopedToken { .. } => {
                Deployment::Cloud
            }
            Method::PersonalAccessToken { .. } => Deployment::DataCenter,
        }
    }

    /// Root the REST paths hang off.
    ///
    /// The host is a property of the credential, not a preference: a grant and a scoped
    /// token are honored only by the gateway, which addresses tenants by id, while a
    /// classic token and a PAT are honored only by the site. Confluence Cloud additionally
    /// lives under `/wiki`, while a Data Center instance's context path is already part of
    /// `site_url`.
    ///
    /// `/wiki` is the SITE's context path, so it belongs to the site base and not to the
    /// gateway one, which already names the product. The Confluence adapter addresses v1
    /// (`/rest/api/…`); a v2 path spells the prefix itself (`/wiki/api/v2/…`) and would have
    /// to carry it here.
    pub fn api_base(&self, product: Product) -> String {
        match &self.method {
            Method::Oauth { cloud_id, .. } | Method::ScopedToken { cloud_id, .. } => {
                format!("{GATEWAY}/ex/{}/{cloud_id}", product.segment())
            }
            Method::ApiToken { .. } => match product {
                Product::Jira => self.site_url.clone(),
                Product::Confluence => format!("{}/wiki", self.site_url),
            },
            Method::PersonalAccessToken { .. } => self.site_url.clone(),
        }
    }

    /// Root for human-facing links. Always the site — the OAuth gateway serves the API but
    /// is not browsable.
    pub fn browse_base(&self, product: Product) -> Option<String> {
        if self.site_url.is_empty() {
            return None;
        }
        Some(match (product, self.deployment()) {
            (Product::Confluence, Deployment::Cloud) => format!("{}/wiki", self.site_url),
            _ => self.site_url.clone(),
        })
    }

    /// Resolve the `Authorization` header, refreshing an OAuth access token if needed.
    pub async fn header(&self) -> Result<AuthHeader, SourceError> {
        match &self.method {
            Method::Oauth { .. } => Ok(AuthHeader::Bearer(self.access_token().await?)),
            Method::ApiToken { email, api_token }
            | Method::ScopedToken {
                email, api_token, ..
            } => Ok(AuthHeader::Basic {
                user: email.clone(),
                secret: api_token.clone(),
            }),
            // Data Center PATs authenticate as Bearer — NOT Basic. Sending one as a Basic
            // password is the classic misconfiguration and yields an opaque 401.
            Method::PersonalAccessToken { token } => Ok(AuthHeader::Bearer(token.clone())),
        }
    }

    pub(crate) fn set_credential_path(&mut self, path: &Path) {
        self.credential_path = path.to_path_buf();
    }

    async fn access_token(&self) -> Result<String, SourceError> {
        let Method::Oauth {
            cache,
            refresh_token,
            client_id,
            client_secret,
            ..
        } = &self.method
        else {
            return Err(SourceError::Auth(
                "access_token is only meaningful for an OAuth instance".into(),
            ));
        };

        {
            let guard = lock(cache, "token cache");
            if let Some(ref cached) = *guard
                && cached.expires_at > jiff::Timestamp::now()
            {
                return Ok(cached.access_token.clone());
            }
        }

        let store = CredentialStore::new(&self.credential_path)?;
        let store_guard = store.lock_async().await?;
        // Recheck after waiting: another call on this auth owner may have refreshed.
        {
            let guard = lock(cache, "token cache");
            if let Some(cached) = guard.as_ref()
                && cached.expires_at > jiff::Timestamp::now()
            {
                return Ok(cached.access_token.clone());
            }
        }
        let mut on_disk = store_guard.load_file()?;
        let entry = on_disk.atlassian.get(&self.instance).ok_or_else(|| {
            SourceError::Auth(
                "OAuth instance is missing from the credential store; refusing refresh".into(),
            )
        })?;
        let AtlassianAuthMethod::Oauth {
            client_id: latest_id,
            client_secret: latest_secret,
            cloud_id: latest_cloud,
            refresh_token: current,
        } = &entry.auth
        else {
            return Err(SourceError::Auth(
                "OAuth instance changed authentication method; restart the command".into(),
            ));
        };
        let Method::Oauth { cloud_id, .. } = &self.method else {
            unreachable!()
        };
        if latest_id != client_id
            || latest_secret != client_secret
            || latest_cloud != cloud_id
            || entry.site_url.trim_end_matches('/') != self.site_url
        {
            return Err(SourceError::Auth(
                "OAuth instance configuration changed; restart the command".into(),
            ));
        }
        let current = current.clone();

        // No blind retry: a lost response cannot tell us whether the provider spent
        // the rotating predecessor. A transport failure remains an explicit auth failure.
        let resp = self
            .http
            .post(&self.token_endpoint)
            // Client credentials in the `Authorization: Basic` header, NOT the body —
            // RFC 6749 §2.3.1 makes Basic the required method and body params only optional,
            // and Atlassian refuses body-carried credentials on some apps (see the
            // authorization-code exchange in `oauth.rs` for the failure it produces).
            .basic_auth(client_id, Some(client_secret))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", current.as_str()),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            // `invalid_grant` covers a refresh token that is spent, revoked, expired or
            // issued to another client; the status alone does not identify which cause.
            let hint = if body.contains("invalid_grant") {
                " — the stored refresh token was not accepted: spent, revoked, expired, or \
                 issued to a different client. A client outside this shared credential store can also spend the grant; re-authorize with `lore init credentials` if needed."
            } else {
                ""
            };
            return Err(SourceError::Auth(format!(
                "Atlassian token refresh failed ({status}) for instance `{}`{hint}",
                self.instance
            )));
        }

        let tr: TokenResponse = resp.json().await?;

        // Persist under the same store-wide guard BEFORE exposing the access token.
        // Another instance's rotation cannot be overwritten by an old file snapshot.
        if let Some(rotated) = tr.refresh_token {
            let entry = on_disk
                .atlassian
                .get_mut(&self.instance)
                .expect("validated instance");
            let AtlassianAuthMethod::Oauth {
                refresh_token: stored,
                ..
            } = &mut entry.auth
            else {
                unreachable!()
            };
            *stored = rotated.clone();
            store_guard.save(&on_disk).map_err(|e| SourceError::Auth(format!(
                "Atlassian successor could not be persisted for instance `{}`: {e}; access token will not be used. Re-authorize if the stored grant is no longer accepted.", self.instance
            )))?;
            *lock(refresh_token, "refresh token") = rotated;
        }

        // Refresh a minute early; never cache a token with a non-positive lifetime.
        const MARGIN_SECS: i64 = 60;
        const FALLBACK_LIFETIME_SECS: i64 = 3300; // standard ~1h Atlassian token, minus margin
        let lifetime = (tr.expires_in - MARGIN_SECS).max(1);
        let expires_at = jiff::Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(lifetime))
            .unwrap_or_else(|_| {
                jiff::Timestamp::now() + jiff::SignedDuration::from_secs(FALLBACK_LIFETIME_SECS)
            });

        let access_token = tr.access_token.clone();
        *lock(cache, "token cache") = Some(CachedToken {
            access_token: tr.access_token,
            expires_at,
        });
        Ok(access_token)
    }
}

fn lock<'a, T>(m: &'a Mutex<T>, what: &str) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("Atlassian {what} mutex poisoned; recovering");
        poisoned.into_inner()
    })
}

impl AtlassianAuth {
    /// Annotate a failed response with the remedy this instance's auth method admits.
    ///
    /// The annotation is derived from what is KNOWN — the configured method and the HTTP
    /// status — never from matching text in the provider's body, which would make every
    /// `403` carrying an HTML doctype an allowlist problem. Those two facts narrow the
    /// causes without choosing between them, so the annotation NAMES them: a token's `403`
    /// is an allowlist, a missing scope or an account without access, and only the operator
    /// can tell which. The body is passed through verbatim either way, so nothing the
    /// provider said is lost.
    pub fn explain_failure(&self, status: u16, body: &str) -> String {
        let remedy = match (&self.method, status) {
            // 403 and 401 divide on WHAT was refused. A 403 means the credential was
            // understood and the request was not permitted — the address, the token's
            // scopes, or the account's access — and only the scopes are repaired by a new
            // token, since a token's scopes are fixed when it is minted. A 401 means the
            // credential itself was not accepted, where reissuing is usually the fix.
            (Method::ApiToken { .. }, 403) => Some(
                "The site refused this resource. An IP allowlist produces exactly this, \
                 and no reissued token answers one: from an address such a list does not \
                 name it admits an org-approved OAuth app and refuses an account token at \
                 either host, so `scoped-token` is not the remedy either — `lore init \
                 credentials` authorizes a grant. The account may instead lack access to the \
                 product or space, which no change of method repairs.",
            ),
            (Method::ApiToken { .. }, 401) => Some(
                "This instance rejected the API token itself — expired, revoked, or paired \
                 with a different account than `email`. Reissue it at \
                 id.atlassian.com/manage-profile/security/api-tokens. One further cause \
                 looks identical: the site host IGNORES a token carrying scopes rather than \
                 refusing it, so a scoped token fails here exactly like a bad one — such a \
                 token belongs on `scoped-token`.",
            ),
            (Method::ScopedToken { .. }, 401) => Some(
                "The gateway did not accept this token. It honors only a token carrying \
                 scopes, so the first thing to check is the SHAPE: a classic token is \
                 refused here and belongs on `api-token`, addressed at the site. Otherwise \
                 the token is expired or revoked, its scopes do not admit this endpoint, or \
                 the account behind it has no access to the product — the last needs an \
                 admin rather than a new token.",
            ),
            (Method::ScopedToken { .. }, 403) => Some(
                "The gateway refused this resource. Three causes look alike here: the \
                 token's scopes may not cover it, the account behind it may lack access to \
                 the product or space, or an IP allowlist may not name this address — such \
                 a list admits an org-approved OAuth app where it refuses an account token, \
                 and reaching it through api.atlassian.com does not change that.",
            ),
            (Method::PersonalAccessToken { .. }, 401 | 403) => Some(
                "This instance refused a personal access token request. Data Center expects \
                 a PAT as `Authorization: Bearer` (which Lorekeeper sends), so confirm the \
                 token is current — and, for a 403, that the account behind it can reach \
                 this project or space, which no reissued token changes.",
            ),
            (Method::Oauth { .. }, 403) => Some(
                "The grant was refused this resource. Its app's registered scopes most \
                 likely do not cover it — re-run `lore init credentials` and match the app's \
                 Permissions page — though an account without access to the product or space \
                 is refused the same way, and no scope repairs that.",
            ),
            _ => None,
        };
        match remedy {
            Some(r) => format!("{body}\n\n{r}"),
            None => body.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refresh_server(
        count: usize,
        fail_save: Option<PathBuf>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for n in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                bodies.push(String::from_utf8(body).unwrap());
                if let Some(path) = &fail_save {
                    std::fs::remove_file(path).unwrap();
                    std::fs::create_dir(path).unwrap();
                }
                let body = format!(
                    r#"{{"access_token":"access-{n}","expires_in":3600,"refresh_token":"successor-{n}"}}"#
                );
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
            bodies
        });
        (endpoint, thread)
    }

    fn shared_oauth(tmp: &Path) -> (CredentialStore, AtlassianCredentials) {
        let store = CredentialStore::new(&tmp.join("credentials.json")).unwrap();
        let entry = AtlassianCredentials {
            site_url: "https://acme.atlassian.net".into(),
            auth: oauth(),
        };
        let mut creds = Credentials::default();
        creds.atlassian.insert("default".into(), entry.clone());
        creds.atlassian.insert("sibling".into(), entry.clone());
        store.lock().unwrap().save(&creds).unwrap();
        (store, entry)
    }

    #[tokio::test]
    async fn separate_auth_owners_reload_successors_under_shared_store_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, entry) = shared_oauth(tmp.path());
        let (endpoint, server) = refresh_server(2, None);
        let mut first = AtlassianAuth::new(reqwest::Client::new(), "default", &entry, tmp.path());
        let mut second = AtlassianAuth::new(reqwest::Client::new(), "default", &entry, tmp.path());
        first.set_credential_path(store.path());
        second.set_credential_path(store.path());
        first.token_endpoint = endpoint.clone();
        second.token_endpoint = endpoint;
        let (a, b) = tokio::join!(first.access_token(), second.access_token());
        assert!(a.is_ok() && b.is_ok());
        let requests = server.join().unwrap();
        assert!(requests[0].contains("refresh_token=REFRESH"));
        assert!(requests[1].contains("refresh_token=successor-0"));
        let creds = store.load_file().unwrap();
        assert!(matches!(&creds.atlassian["default"].auth,
            AtlassianAuthMethod::Oauth { refresh_token, .. } if refresh_token == "successor-1"));
        assert!(matches!(&creds.atlassian["sibling"].auth,
            AtlassianAuthMethod::Oauth { refresh_token, .. } if refresh_token == "REFRESH"));
    }

    #[tokio::test]
    async fn concurrent_instance_rotations_preserve_both_successors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, entry) = shared_oauth(tmp.path());
        let (endpoint, server) = refresh_server(2, None);
        let mut first = AtlassianAuth::new(reqwest::Client::new(), "default", &entry, tmp.path());
        let mut second = AtlassianAuth::new(reqwest::Client::new(), "sibling", &entry, tmp.path());
        first.set_credential_path(store.path());
        second.set_credential_path(store.path());
        first.token_endpoint = endpoint.clone();
        second.token_endpoint = endpoint;
        let (a, b) = tokio::join!(first.access_token(), second.access_token());
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_ne!(a, b);
        server.join().unwrap();
        let creds = store.load_file().unwrap();
        for (name, access) in [("default", a), ("sibling", b)] {
            let expected = access.replace("access-", "successor-");
            assert!(matches!(&creds.atlassian[name].auth,
                AtlassianAuthMethod::Oauth { refresh_token, .. } if refresh_token == &expected));
        }
    }

    #[tokio::test]
    async fn persistence_failure_never_exposes_or_caches_access_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, entry) = shared_oauth(tmp.path());
        let (endpoint, server) = refresh_server(1, Some(store.path().to_path_buf()));
        let mut auth = AtlassianAuth::new(reqwest::Client::new(), "default", &entry, tmp.path());
        auth.set_credential_path(store.path());
        auth.token_endpoint = endpoint;
        let error = auth.access_token().await.unwrap_err().to_string();
        assert!(error.contains("could not be persisted"));
        let Method::Oauth { cache, .. } = &auth.method else {
            unreachable!()
        };
        assert!(lock(cache, "test").is_none());
        assert_eq!(server.join().unwrap().len(), 1);
    }

    fn auth(site: &str, method: AtlassianAuthMethod) -> AtlassianAuth {
        AtlassianAuth::new(
            reqwest::Client::new(),
            "default",
            &AtlassianCredentials {
                site_url: site.into(),
                auth: method,
            },
            Path::new("/vault"),
        )
    }

    fn oauth() -> AtlassianAuthMethod {
        AtlassianAuthMethod::Oauth {
            client_id: "cid".into(),
            client_secret: "SECRET".into(),
            refresh_token: "REFRESH".into(),
            cloud_id: "cloud-1".into(),
        }
    }

    fn pat() -> AtlassianAuthMethod {
        AtlassianAuthMethod::PersonalAccessToken {
            token: "PAT123".into(),
        }
    }

    fn api_token() -> AtlassianAuthMethod {
        AtlassianAuthMethod::ApiToken {
            email: "me@corp.example".into(),
            api_token: "TOKEN".into(),
        }
    }

    fn scoped_token() -> AtlassianAuthMethod {
        AtlassianAuthMethod::ScopedToken {
            email: "me@corp.example".into(),
            api_token: "SCOPED".into(),
            cloud_id: "cloud-1".into(),
        }
    }

    #[test]
    fn deployment_is_derived_from_the_credential_form() {
        assert_eq!(
            auth("https://acme.atlassian.net", oauth()).deployment(),
            Deployment::Cloud
        );
        assert_eq!(
            auth("https://acme.atlassian.net", api_token()).deployment(),
            Deployment::Cloud
        );
        assert_eq!(
            auth("https://acme.atlassian.net", scoped_token()).deployment(),
            Deployment::Cloud
        );
        assert_eq!(
            auth("https://wiki.corp/confluence", pat()).deployment(),
            Deployment::DataCenter
        );
    }

    #[test]
    fn a_scoped_token_routes_through_the_gateway_like_a_grant() {
        // The site host ignores a scoped token rather than rejecting it, so addressing it
        // there costs an anonymous 401 and no explanation. The gateway is the only host
        // that honors one, which is what separates this method from `api-token`.
        let a = auth("https://acme.atlassian.net", scoped_token());
        assert_eq!(
            a.api_base(Product::Jira),
            "https://api.atlassian.com/ex/jira/cloud-1"
        );
        assert_eq!(
            a.api_base(Product::Confluence),
            "https://api.atlassian.com/ex/confluence/cloud-1"
        );
        // Browse links still address the site — the gateway serves the API, not pages.
        assert_eq!(
            a.browse_base(Product::Confluence).as_deref(),
            Some("https://acme.atlassian.net/wiki")
        );
    }

    #[tokio::test]
    async fn a_scoped_token_authenticates_as_basic_not_bearer() {
        // It is an account token like the classic one, and differs only in where it is
        // honored — sending it as a Bearer is the mistake the gateway path invites.
        let header = auth("https://acme.atlassian.net", scoped_token())
            .header()
            .await
            .unwrap();
        assert!(matches!(header, AuthHeader::Basic { ref user, ref secret }
                if user == "me@corp.example" && secret == "SCOPED"));
    }

    #[test]
    fn each_token_shape_is_told_where_the_other_one_belongs() {
        // The two shapes fail in each other's place without saying why, so the remedy has
        // to name the sibling method rather than advise reissuing the token.
        let classic = auth("https://acme.atlassian.net", api_token());
        assert!(
            classic
                .explain_failure(401, "body")
                .contains("scoped-token")
        );
        assert!(
            classic
                .explain_failure(403, "body")
                .contains("scoped-token")
        );

        let scoped = auth("https://acme.atlassian.net", scoped_token());
        assert!(scoped.explain_failure(401, "body").contains("api-token"));
    }

    #[test]
    fn an_allowlist_stays_a_candidate_for_an_account_token_at_either_host() {
        // From an address the list does not name it admits an org-approved app and turns
        // away the account at either host, so reaching the gateway is not an exemption and
        // neither 403 may rule the address out. Both name the causes rather than pick one.
        for method in [api_token(), scoped_token()] {
            let refused = auth("https://acme.atlassian.net", method).explain_failure(403, "body");
            assert!(refused.contains("allowlist"), "{refused}");
        }
        assert!(
            auth("https://acme.atlassian.net", scoped_token())
                .explain_failure(403, "body")
                .contains("scopes")
        );
    }

    #[test]
    fn oauth_routes_through_the_gateway_not_the_site() {
        let a = auth("https://acme.atlassian.net", oauth());
        assert_eq!(
            a.api_base(Product::Jira),
            "https://api.atlassian.com/ex/jira/cloud-1"
        );
        assert_eq!(
            a.api_base(Product::Confluence),
            "https://api.atlassian.com/ex/confluence/cloud-1"
        );
    }

    #[test]
    fn api_token_talks_to_the_site_with_confluence_under_wiki() {
        let a = auth("https://acme.atlassian.net/", api_token());
        assert_eq!(a.api_base(Product::Jira), "https://acme.atlassian.net");
        assert_eq!(
            a.api_base(Product::Confluence),
            "https://acme.atlassian.net/wiki"
        );
    }

    #[test]
    fn data_center_keeps_its_context_path_and_adds_no_wiki_segment() {
        // The context path is already part of site_url; appending `/wiki` would 404.
        let a = auth("https://wiki.corp/confluence", pat());
        assert_eq!(
            a.api_base(Product::Confluence),
            "https://wiki.corp/confluence"
        );
        assert_eq!(
            a.browse_base(Product::Confluence).as_deref(),
            Some("https://wiki.corp/confluence")
        );
    }

    #[tokio::test]
    async fn pat_authenticates_as_bearer_not_basic() {
        // A PAT sent as a Basic password is the classic Data Center misconfiguration.
        let header = auth("https://wiki.corp", pat()).header().await.unwrap();
        assert!(matches!(header, AuthHeader::Bearer(ref t) if t == "PAT123"));
    }

    #[tokio::test]
    async fn api_token_authenticates_as_basic_with_the_account_email() {
        let header = auth("https://acme.atlassian.net", api_token())
            .header()
            .await
            .unwrap();
        assert!(matches!(header, AuthHeader::Basic { ref user, ref secret }
                     if user == "me@corp.example" && secret == "TOKEN"));
    }

    #[test]
    fn dialect_differs_per_deployment() {
        assert_eq!(
            Deployment::Cloud.jira_search_path(),
            "/rest/api/3/search/jql"
        );
        assert_eq!(
            Deployment::DataCenter.jira_search_path(),
            "/rest/api/2/search"
        );
        assert_eq!(Deployment::Cloud.jira_paging(), JiraPaging::Token);
        assert_eq!(Deployment::DataCenter.jira_paging(), JiraPaging::Offset);
        assert_eq!(Deployment::Cloud.jira_user_key(), "accountId");
        assert_eq!(Deployment::DataCenter.jira_user_key(), "name");
    }

    #[test]
    fn browse_base_is_the_site_even_when_the_api_is_the_gateway() {
        let a = auth("https://acme.atlassian.net", oauth());
        assert_eq!(
            a.browse_base(Product::Jira).as_deref(),
            Some("https://acme.atlassian.net")
        );
        assert_eq!(
            a.browse_base(Product::Confluence).as_deref(),
            Some("https://acme.atlassian.net/wiki")
        );
    }

    #[test]
    fn failure_advice_follows_the_configured_method_not_the_response_text() {
        let body = r#"{"errorMessages":["nope"]}"#;

        // An API token meeting 403 may be the allowlist, and that remedy is a different
        // auth method — worth naming beside the alternatives.
        let token_403 = auth("https://acme.atlassian.net", api_token()).explain_failure(403, body);
        assert!(token_403.contains("IP allowlist"));
        assert!(token_403.contains("lore init credentials"));

        // 401 is the credential being rejected, not the address — reissuing IS the fix, so
        // the 403 advice ("reissuing cannot help") must not reach it.
        let token_401 = auth("https://acme.atlassian.net", api_token()).explain_failure(401, body);
        assert!(token_401.contains("Reissue it"), "{token_401}");
        assert!(!token_401.contains("IP allowlist"), "{token_401}");

        // The same body under OAuth is NOT an allowlist problem, and must not be described
        // as one — the old text-matching version could not tell these apart.
        let oauth_403 = auth("https://acme.atlassian.net", oauth()).explain_failure(403, body);
        assert!(!oauth_403.contains("IP allowlist"));
        assert!(oauth_403.contains("scopes"));

        let pat_401 = auth("https://wiki.corp", pat()).explain_failure(401, body);
        assert!(pat_401.contains("Bearer"));

        // A status that carries no auth meaning is passed through untouched.
        let plain = auth("https://acme.atlassian.net", api_token()).explain_failure(500, body);
        assert_eq!(plain, body);

        // The provider's own words survive in every case.
        for msg in [token_403, token_401, oauth_403, pat_401] {
            assert!(msg.starts_with(body));
        }
    }

    #[test]
    fn secrets_never_reach_debug_output() {
        for method in [oauth(), api_token(), pat()] {
            let rendered = format!("{method:?}");
            assert!(!rendered.contains("SECRET"), "{rendered}");
            assert!(!rendered.contains("REFRESH"), "{rendered}");
            assert!(!rendered.contains("TOKEN"), "{rendered}");
            assert!(!rendered.contains("PAT123"), "{rendered}");
        }
        // Principal identifiers stay visible — they name who acts, not how they prove it.
        assert!(format!("{:?}", oauth()).contains("cloud-1"));
        assert!(format!("{:?}", api_token()).contains("me@corp.example"));
    }
}
