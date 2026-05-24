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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackCredentials {
    pub bot_token: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn save_round_trips_and_omits_unset_providers() {
        let dir = TempDir::new().unwrap();
        let creds = Credentials {
            slack: Some(SlackCredentials {
                bot_token: "xoxb-abc".into(),
            }),
            ..Default::default()
        };
        let path = creds.save(dir.path()).unwrap();
        assert_eq!(path, Credentials::path(dir.path()));

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("slack"), "configured provider present");
        assert!(!raw.contains("google"), "unset provider omitted");

        let back = Credentials::from_file(dir.path()).unwrap();
        assert_eq!(back.slack.unwrap().bot_token, "xoxb-abc");
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
