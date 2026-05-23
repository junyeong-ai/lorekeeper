pub mod credentials;
mod google;
mod jira;
mod slack;

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use wi_core::config::SourceType;
use wi_core::event::RawItem;

use credentials::Credentials;
use google::GoogleAuth;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth: {0}")]
    Auth(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("API {status}: {message}")]
    Api { status: u16, message: String },
    #[error("parse: {0}")]
    Parse(String),
    #[error("source '{0}' not found")]
    NotFound(String),
    #[error("source '{0}' is disabled")]
    Disabled(String),
}

#[derive(Debug, Clone)]
pub struct ExtractContext {
    pub target_date: jiff::civil::Date,
    pub timezone: jiff::tz::TimeZone,
}

#[async_trait]
pub trait Source: Send + Sync {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError>;
}

/// Validate a source's `params` against its adapter's typed schema without any
/// network access or credentials. Used by `wi validate` to surface config errors
/// (missing required keys, wrong types, unknown/typo'd keys) before runtime.
pub fn validate_params(
    source_type: SourceType,
    params: &serde_json::Value,
) -> Result<(), SourceError> {
    match source_type {
        SourceType::Gmail => google::gmail::validate_params(params),
        SourceType::GoogleDrive => google::drive::validate_params(params),
        SourceType::GoogleCalendar => google::calendar::validate_params(params),
        SourceType::SlackChannel => slack::channel::validate_params(params),
        SourceType::SlackSearch => slack::search::validate_params(params),
        SourceType::Jira => jira::validate_params(params),
    }
}

pub fn create_source(
    source_type: SourceType,
    http: reqwest::Client,
    creds: &Credentials,
) -> Result<Box<dyn Source>, SourceError> {
    match source_type {
        SourceType::Gmail | SourceType::GoogleDrive | SourceType::GoogleCalendar => {
            let gc = creds.google.as_ref().ok_or_else(|| {
                SourceError::Auth(
                    "Google credentials not configured. \
                     Set WI_GOOGLE_CLIENT_ID, WI_GOOGLE_CLIENT_SECRET, \
                     WI_GOOGLE_REFRESH_TOKEN or add to .wiki-ingest/credentials.json"
                        .into(),
                )
            })?;
            let auth = Arc::new(GoogleAuth::new(http.clone(), gc.clone()));
            match source_type {
                SourceType::Gmail => Ok(Box::new(google::gmail::GmailSource::new(http, auth))),
                SourceType::GoogleDrive => {
                    Ok(Box::new(google::drive::DriveSource::new(http, auth)))
                }
                SourceType::GoogleCalendar => {
                    Ok(Box::new(google::calendar::CalendarSource::new(http, auth)))
                }
                _ => unreachable!(),
            }
        }
        SourceType::SlackChannel | SourceType::SlackSearch => {
            let sc = creds.slack.as_ref().ok_or_else(|| {
                SourceError::Auth(
                    "Slack credentials not configured. \
                     Set WI_SLACK_TOKEN or add to .wiki-ingest/credentials.json"
                        .into(),
                )
            })?;
            let token = sc.bot_token.clone();
            match source_type {
                SourceType::SlackChannel => Ok(Box::new(slack::channel::SlackChannelSource::new(
                    http, token,
                ))),
                SourceType::SlackSearch => {
                    Ok(Box::new(slack::search::SlackSearchSource::new(http, token)))
                }
                _ => unreachable!(),
            }
        }
        SourceType::Jira => {
            let jc = creds.jira.as_ref().ok_or_else(|| {
                SourceError::Auth(
                    "Jira credentials not configured. \
                     Set WI_JIRA_URL, WI_JIRA_EMAIL, WI_JIRA_TOKEN \
                     or add to .wiki-ingest/credentials.json"
                        .into(),
                )
            })?;
            Ok(Box::new(jira::JiraSource::new(http, jc.clone())))
        }
    }
}
