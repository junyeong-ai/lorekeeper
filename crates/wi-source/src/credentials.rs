use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SourceError;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google: Option<GoogleCredentials>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackCredentials>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jira: Option<JiraCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

/// Slack accepts either a bot token (`xoxb-`) or a user token (`xoxp-`) — or both.
/// `conversations.history` works with either; `search.messages` requires a user token,
/// so the slack-search adapter needs `user_token`. At least one must be set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_token: Option<String>,
}

impl SlackCredentials {
    /// Token for `conversations.history` (channel reader): bot token preferred, else user.
    pub fn history_token(&self) -> Option<&str> {
        self.bot_token.as_deref().or(self.user_token.as_deref())
    }

    /// Token for `search.messages`: a user token is mandatory — bot tokens cannot search.
    pub fn search_token(&self) -> Option<&str> {
        self.user_token.as_deref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraCredentials {
    pub base_url: String,
    pub email: String,
    pub api_token: String,
}

impl Credentials {
    /// Canonical on-disk location: `<vault>/.wiki-ingest/credentials.json`.
    pub fn path(vault_root: &Path) -> PathBuf {
        vault_root.join(".wiki-ingest").join("credentials.json")
    }

    /// Read the credentials file only (no env overlay). Missing file → all-None default.
    /// Used by the `wi init credentials` wizard to seed defaults from existing values.
    pub fn from_file(vault_root: &Path) -> Result<Self, SourceError> {
        let path = Self::path(vault_root);
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| SourceError::Auth(format!("failed to read credentials: {e}")))?;
            serde_json::from_str(&content)
                .map_err(|e| SourceError::Auth(format!("failed to parse credentials: {e}")))
        } else {
            Ok(Self::default())
        }
    }

    /// File values with environment variables overlaid (env wins per service). This is
    /// what the pipeline uses at runtime.
    pub fn load(vault_root: &Path) -> Result<Self, SourceError> {
        let mut creds = Self::from_file(vault_root)?;
        creds.override_from_env();
        Ok(creds)
    }

    /// Persist to `<vault>/.wiki-ingest/credentials.json` atomically (temp + rename) with
    /// owner-only permissions (`0600` on Unix). Returns the written path. Providers left
    /// `None` are omitted from the file.
    pub fn save(&self, vault_root: &Path) -> Result<PathBuf, SourceError> {
        let path = Self::path(vault_root);
        let dir = path
            .parent()
            .ok_or_else(|| SourceError::Auth("invalid credentials path".into()))?;
        std::fs::create_dir_all(dir)
            .map_err(|e| SourceError::Auth(format!("create .wiki-ingest dir: {e}")))?;

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| SourceError::Auth(format!("serialize credentials: {e}")))?;

        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|e| SourceError::Auth(format!("write credentials: {e}")))?;
        // Restrict to the owner before publishing — a credentials file is secret.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| SourceError::Auth(format!("chmod credentials: {e}")))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            SourceError::Auth(format!("publish credentials: {e}"))
        })?;
        Ok(path)
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

        let bot = std::env::var("WI_SLACK_TOKEN").ok();
        let user = std::env::var("WI_SLACK_USER_TOKEN").ok();
        if bot.is_some() || user.is_some() {
            let mut slack = self.slack.clone().unwrap_or_default();
            if let Some(b) = bot {
                slack.bot_token = Some(b);
            }
            if let Some(u) = user {
                slack.user_token = Some(u);
            }
            self.slack = Some(slack);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn save_round_trips_and_omits_unset_providers() {
        let dir = TempDir::new().unwrap();
        let creds = Credentials {
            slack: Some(SlackCredentials {
                user_token: Some("xoxp-abc".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let path = creds.save(dir.path()).unwrap();
        assert_eq!(path, Credentials::path(dir.path()));

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("user_token"), "configured token present");
        assert!(!raw.contains("bot_token"), "unset token omitted");
        assert!(!raw.contains("google"), "unset provider omitted");

        let back = Credentials::from_file(dir.path()).unwrap();
        let slack = back.slack.unwrap();
        assert_eq!(slack.search_token(), Some("xoxp-abc"));
        assert_eq!(slack.history_token(), Some("xoxp-abc")); // falls back to user token
        assert!(back.google.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = Credentials::default().save(dir.path()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
