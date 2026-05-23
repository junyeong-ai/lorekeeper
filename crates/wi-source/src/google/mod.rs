pub mod calendar;
pub mod drive;
pub mod gmail;

use std::sync::Mutex;

use serde::Deserialize;

use crate::SourceError;
use crate::credentials::GoogleCredentials;

pub struct GoogleAuth {
    http: reqwest::Client,
    creds: GoogleCredentials,
    cache: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    access_token: String,
    expires_at: jiff::Timestamp,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
}

impl GoogleAuth {
    pub fn new(http: reqwest::Client, creds: GoogleCredentials) -> Self {
        Self {
            http,
            creds,
            cache: Mutex::new(None),
        }
    }

    pub async fn access_token(&self) -> Result<String, SourceError> {
        {
            let cache = self.cache.lock().unwrap_or_else(|poisoned| {
                tracing::warn!("Google token cache mutex poisoned; recovering");
                poisoned.into_inner()
            });
            if let Some(ref cached) = *cache
                && cached.expires_at > jiff::Timestamp::now()
            {
                return Ok(cached.access_token.clone());
            }
        }
        self.refresh().await
    }

    async fn refresh(&self) -> Result<String, SourceError> {
        tracing::debug!("refreshing Google access token");

        let resp = self
            .http
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", self.creds.client_secret.as_str()),
                ("refresh_token", self.creds.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SourceError::Api {
                status,
                message: format!("Google token refresh failed: {body}"),
            });
        }

        let tr: TokenResponse = resp.json().await?;

        let margin_secs = 60;
        let expires_at = jiff::Timestamp::now()
            .checked_add(jiff::SignedDuration::from_secs(tr.expires_in - margin_secs))
            .unwrap_or_else(|_| jiff::Timestamp::now());

        let access_token = tr.access_token.clone();
        {
            let mut cache = self.cache.lock().unwrap_or_else(|poisoned| {
                tracing::warn!("Google token cache mutex poisoned; recovering");
                poisoned.into_inner()
            });
            *cache = Some(CachedToken {
                access_token: tr.access_token,
                expires_at,
            });
        }

        Ok(access_token)
    }
}

async fn check_response(resp: reqwest::Response) -> Result<reqwest::Response, SourceError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(SourceError::Api {
        status,
        message: body,
    })
}
