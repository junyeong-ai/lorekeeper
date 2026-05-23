use std::path::Path;

use serde::Deserialize;

use crate::SourceError;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub google: Option<GoogleCredentials>,
    #[serde(default)]
    pub slack: Option<SlackCredentials>,
    #[serde(default)]
    pub jira: Option<JiraCredentials>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoogleCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlackCredentials {
    pub bot_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraCredentials {
    pub base_url: String,
    pub email: String,
    pub api_token: String,
}

impl Credentials {
    pub fn load(vault_root: &Path) -> Result<Self, SourceError> {
        let file_path = vault_root.join(".wiki-ingest").join("credentials.json");

        let mut creds = if file_path.exists() {
            let content = std::fs::read_to_string(&file_path)
                .map_err(|e| SourceError::Auth(format!("failed to read credentials: {e}")))?;
            serde_json::from_str(&content)
                .map_err(|e| SourceError::Auth(format!("failed to parse credentials: {e}")))?
        } else {
            Self::default()
        };

        creds.override_from_env();
        Ok(creds)
    }

    fn override_from_env(&mut self) {
        if let (Ok(id), Ok(secret), Ok(refresh)) = (
            std::env::var("WI_GOOGLE_CLIENT_ID"),
            std::env::var("WI_GOOGLE_CLIENT_SECRET"),
            std::env::var("WI_GOOGLE_REFRESH_TOKEN"),
        ) {
            self.google = Some(GoogleCredentials {
                client_id: id,
                client_secret: secret,
                refresh_token: refresh,
            });
        }

        if let Ok(token) = std::env::var("WI_SLACK_TOKEN") {
            self.slack = Some(SlackCredentials { bot_token: token });
        }

        if let (Ok(url), Ok(email), Ok(token)) = (
            std::env::var("WI_JIRA_URL"),
            std::env::var("WI_JIRA_EMAIL"),
            std::env::var("WI_JIRA_TOKEN"),
        ) {
            self.jira = Some(JiraCredentials {
                base_url: url,
                email,
                api_token: token,
            });
        }
    }
}
