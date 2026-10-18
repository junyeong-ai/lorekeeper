use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SourceError;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google: Option<GoogleCredentials>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackCredentials>,
    /// Atlassian instances by name (`default`, `cloud`, `onprem`, …). One entry serves both
    /// the Jira and Confluence adapters on that instance; a source selects one with its
    /// `instance` field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub atlassian: BTreeMap<String, AtlassianCredentials>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GoogleCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

// Hand-written Debug so a secret never reaches a log/error string verbatim. `client_id` is
// not a secret (it identifies the OAuth app, not its bearer); `client_secret`/`refresh_token`
// are, so they redact. Keeps `?creds` debugging safe by construction, not by remembering not to.
impl std::fmt::Debug for GoogleCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

/// Slack accepts either a bot token (`xoxb-`) or a user token (`xoxp-`) — or both.
/// `conversations.history` works with either; `search.messages` requires a user token,
/// so the slack-search adapter needs `user_token`. At least one must be set.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SlackCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_token: Option<String>,
}

// Redact token values but keep their presence visible (`Some("<redacted>")` / `None`), so a
// "which token is set?" debug stays useful without leaking the secret itself.
impl std::fmt::Debug for SlackCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackCredentials")
            .field("bot_token", &self.bot_token.as_ref().map(|_| "<redacted>"))
            .field(
                "user_token",
                &self.user_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
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

/// One authenticated Atlassian instance. Jira and Confluence live on the same instance and
/// share its credential, so one entry serves both adapters.
///
/// Instances are NAMED (`credentials.atlassian` is a map) because a single organization
/// routinely runs more than one — a Cloud tenant plus an on-prem Data Center wiki, or
/// separate production and sandbox sites. A source names the instance it reads.
#[derive(Clone, Serialize, Deserialize)]
pub struct AtlassianCredentials {
    /// Site root as a human would type it: `https://acme.atlassian.net` (Cloud) or
    /// `https://wiki.corp.example/confluence` (Data Center, context path included).
    /// Always the basis for browse links; also the API host for the methods addressed at
    /// the site, while `oauth` and `scoped-token` route through Atlassian's gateway instead.
    pub site_url: String,
    #[serde(flatten)]
    pub auth: AtlassianAuthMethod,
}

/// How requests to an instance are authenticated.
///
/// Each variant carries exactly the fields its method needs and no others, so an
/// unusable combination — a PAT with a `cloud_id`, an OAuth grant with an `email` —
/// cannot be expressed. The variant also determines the deployment (and therefore the
/// REST dialect): OAuth and both account-token forms exist only on Cloud, personal access
/// tokens only on Data Center/Server.
///
/// Cloud issues an account token in two shapes, and each is honored by exactly one host —
/// a classic token at the site, a scoped one at the gateway. Sending either to the other
/// place fails without saying so, which is why they are separate variants rather than one
/// with a routing flag.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum AtlassianAuthMethod {
    /// OAuth 2.0 (3LO) — Cloud, through the gateway. The only method an IP allowlist
    /// admits from an unlisted address: such a list turns away an account token wherever it
    /// is sent and lets an org-approved app through.
    ///
    /// **Give Lorekeeper its own OAuth app.** Atlassian ROTATES refresh tokens: each
    /// refresh can replace the predecessor with a successor. Cooperating Lorekeeper
    /// processes must share one locked store rather than copy a grant into separate files.
    Oauth {
        client_id: String,
        client_secret: String,
        /// Rotating — the successor is written back to this file after each refresh.
        refresh_token: String,
        /// Tenant id from `/oauth/token/accessible-resources`; selects the gateway path
        /// `https://api.atlassian.com/ex/{product}/{cloud_id}`.
        cloud_id: String,
    },
    /// Classic (unscoped) account API token over HTTP Basic — Cloud, addressed at the site
    /// host. Simple to set up, but an instance with an IP allowlist refuses it from any
    /// unlisted address — and so it does the scoped form, so the remedy is `oauth`.
    ApiToken { email: String, api_token: String },
    /// Scoped account API token over HTTP Basic — Cloud, addressed through the gateway.
    ///
    /// A token carrying scopes is honored only at `api.atlassian.com`; the site host
    /// ignores it rather than rejecting it, which surfaces as an anonymous 401 naming
    /// nothing. So this is the same credential *kind* as [`Self::ApiToken`] and a different
    /// route, and the two are not interchangeable in either direction.
    ///
    /// It reaches the gateway the way OAuth does but is still the ACCOUNT asking, so an IP
    /// allowlist refuses it from an unlisted address exactly as it refuses the classic form.
    /// What it buys over that form is the scoped token itself, not an exemption.
    ScopedToken {
        email: String,
        api_token: String,
        /// Tenant id, as for OAuth — it selects the gateway path
        /// `https://api.atlassian.com/ex/{product}/{cloud_id}`.
        cloud_id: String,
    },
    /// Personal access token over HTTP Bearer — Data Center / Server. These instances have
    /// no OAuth gateway and no account API tokens; a PAT issued from the user's profile is
    /// the supported programmatic credential.
    #[serde(rename = "pat")]
    PersonalAccessToken { token: String },
}

impl AtlassianAuthMethod {
    /// Stable name for diagnostics and config errors.
    pub fn label(&self) -> &'static str {
        match self {
            AtlassianAuthMethod::Oauth { .. } => "oauth",
            AtlassianAuthMethod::ApiToken { .. } => "api-token",
            AtlassianAuthMethod::ScopedToken { .. } => "scoped-token",
            AtlassianAuthMethod::PersonalAccessToken { .. } => "pat",
        }
    }
}

// Identifiers (app id, tenant id, account email) stay visible because they name the
// principal rather than authenticate it; every bearer secret redacts.
impl std::fmt::Debug for AtlassianAuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AtlassianAuthMethod::Oauth {
                client_id,
                cloud_id,
                ..
            } => f
                .debug_struct("Oauth")
                .field("client_id", client_id)
                .field("client_secret", &"<redacted>")
                .field("refresh_token", &"<redacted>")
                .field("cloud_id", cloud_id)
                .finish(),
            AtlassianAuthMethod::ApiToken { email, .. } => f
                .debug_struct("ApiToken")
                .field("email", email)
                .field("api_token", &"<redacted>")
                .finish(),
            AtlassianAuthMethod::ScopedToken {
                email, cloud_id, ..
            } => f
                .debug_struct("ScopedToken")
                .field("email", email)
                .field("api_token", &"<redacted>")
                .field("cloud_id", cloud_id)
                .finish(),
            AtlassianAuthMethod::PersonalAccessToken { .. } => f
                .debug_struct("PersonalAccessToken")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

/// A credential from the environment, where a variable that is set but blank is absent.
///
/// A wrapper exporting `LORE_SLACK_TOKEN="$MAYBE_TOKEN"` unconditionally sets the variable
/// whether or not it holds anything, and an empty string taken as a credential overwrites a
/// working file entry with one that cannot authenticate — surfacing as a provider rejecting
/// the account rather than as configuration.
fn env_credential(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl std::fmt::Debug for AtlassianCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtlassianCredentials")
            .field("site_url", &self.site_url)
            .field("auth", &self.auth)
            .finish()
    }
}

/// One canonical credential address. A companion lock survives atomic file replacement.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

pub struct CredentialGuard {
    store: CredentialStore,
    _lock: std::fs::File,
}

impl CredentialStore {
    pub fn new(path: &Path) -> Result<Self, SourceError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_err(auth_io)?.join(path)
        };
        // Follow an existing final symlink so atomic replacement updates its target,
        // rather than replacing the alias and splitting the rotation chain.
        let path = match std::fs::symlink_metadata(&absolute) {
            Ok(_) => {
                let canonical = absolute.canonicalize().map_err(auth_io)?;
                if !std::fs::metadata(&canonical).map_err(auth_io)?.is_file() {
                    return Err(SourceError::Auth(
                        "credential store must be a regular file".into(),
                    ));
                }
                canonical
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                lk_core::fs::canonical_prefix(&absolute)
            }
            Err(e) => return Err(auth_io(e)),
        };
        if path.file_name().is_none() || path.is_dir() {
            return Err(SourceError::Auth(
                "credential store must name a file".into(),
            ));
        }
        Ok(Self { path })
    }

    pub fn for_vault(vault_root: &Path) -> Result<Self, SourceError> {
        Self::new(&Credentials::path(vault_root))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_file(&self) -> Result<Credentials, SourceError> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Credentials::default());
            }
            Err(e) => return Err(auth_io(e)),
        };
        let metadata = file.metadata().map_err(auth_io)?;
        if !metadata.is_file() {
            return Err(SourceError::Auth(
                "credential store must be a regular file".into(),
            ));
        }
        require_private_permissions(&metadata, &self.path)?;
        let mut content = String::new();
        file.read_to_string(&mut content).map_err(auth_io)?;
        serde_json::from_str(&content).map_err(|e| {
            SourceError::Auth(format!(
                "failed to parse credentials at line {}, column {}",
                e.line(),
                e.column()
            ))
        })
    }

    pub fn load(&self) -> Result<Credentials, SourceError> {
        let mut creds = self.load_file()?;
        creds.override_from_env();
        Ok(creds)
    }

    /// OS advisory lock; no stale-PID or age-based unlocking. All store writers cooperate.
    pub fn lock(&self) -> Result<CredentialGuard, SourceError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| SourceError::Auth("invalid credential path".into()))?;
        std::fs::create_dir_all(parent).map_err(auth_io)?;
        let mut name = self.path.file_name().unwrap().to_os_string();
        name.push(".lock");
        let lock_path = parent.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(meta) = std::fs::symlink_metadata(&lock_path)
            && !meta.file_type().is_file()
        {
            return Err(SourceError::Auth(
                "credential lock must be a regular file, not a symlink".into(),
            ));
        }
        let file = options.open(&lock_path).map_err(auth_io)?;
        let named = std::fs::symlink_metadata(&lock_path).map_err(auth_io)?;
        let opened = file.metadata().map_err(auth_io)?;
        if !named.file_type().is_file() || !opened.is_file() {
            return Err(SourceError::Auth(
                "credential lock must be a regular file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if named.dev() != opened.dev() || named.ino() != opened.ino() {
                return Err(SourceError::Auth(
                    "credential lock changed while opening".into(),
                ));
            }
        }
        require_private_permissions(&opened, &lock_path)?;
        file.lock().map_err(auth_io)?;
        Ok(CredentialGuard {
            store: self.clone(),
            _lock: file,
        })
    }

    pub async fn lock_async(&self) -> Result<CredentialGuard, SourceError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.lock())
            .await
            .map_err(|e| SourceError::Auth(format!("credential lock task failed: {e}")))?
    }
}

impl CredentialGuard {
    pub fn load_file(&self) -> Result<Credentials, SourceError> {
        self.store.load_file()
    }

    /// Caller holds the store lock across read/modify/write. Do not persist env overlays.
    pub fn save(&self, creds: &Credentials) -> Result<PathBuf, SourceError> {
        let json = serde_json::to_string_pretty(creds)
            .map_err(|e| SourceError::Auth(format!("serialize credentials: {e}")))?;
        lk_core::fs::write_atomic(&self.store.path, json.as_bytes(), Some(0o600))
            .map_err(auth_io)?;
        Ok(self.store.path.clone())
    }
}

fn auth_io(error: std::io::Error) -> SourceError {
    SourceError::Auth(format!("credential store I/O failed: {error}"))
}

fn require_private_permissions(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), SourceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(SourceError::Auth(format!(
                "credential file {} permits group or other-user access; set its permissions to 0600 with chmod 600",
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, path);
    Ok(())
}

impl Credentials {
    /// Canonical on-disk location: `<vault>/.lorekeeper/credentials.json`.
    pub fn path(vault_root: &Path) -> PathBuf {
        vault_root.join(".lorekeeper").join("credentials.json")
    }

    /// Read the credentials file only (no env overlay). Missing file → all-None default.
    /// Used by the `lore init credentials` wizard to seed defaults from existing values.
    pub fn load_file(vault_root: &Path) -> Result<Self, SourceError> {
        CredentialStore::for_vault(vault_root)?.load_file()
    }

    /// File values with stable environment credentials overlaid.
    pub fn load(vault_root: &Path) -> Result<Self, SourceError> {
        CredentialStore::for_vault(vault_root)?.load()
    }

    /// Serialized replacement of a complete snapshot. Read-modify-write callers must
    /// hold CredentialGuard from their initial read, rather than save a stale snapshot.
    pub fn save(&self, vault_root: &Path) -> Result<PathBuf, SourceError> {
        CredentialStore::for_vault(vault_root)?.lock()?.save(self)
    }

    /// Resolve the Atlassian instance a source asked for.
    ///
    /// An explicit name wins. With no name, a lone configured instance is unambiguous and is
    /// used regardless of its key; more than one is a genuine ambiguity, so the error names
    /// the available instances rather than guessing.
    pub fn resolve_atlassian_instance(
        &self,
        name: Option<&str>,
    ) -> Result<(&str, &AtlassianCredentials), SourceError> {
        if let Some(name) = name {
            return self
                .atlassian
                .get_key_value(name)
                .map(|(k, v)| (k.as_str(), v))
                .ok_or_else(|| {
                    SourceError::Auth(format!(
                        "no Atlassian instance named `{name}` in credentials.json \
                         (configured: {})",
                        self.atlassian_instance_names()
                    ))
                });
        }
        let mut iter = self.atlassian.iter();
        match (iter.next(), iter.next()) {
            (Some((k, v)), None) => Ok((k.as_str(), v)),
            (None, _) => Err(SourceError::Auth(
                "no Atlassian instance configured. Run `lore init credentials` to authorize \
                 one (OAuth for Cloud, or a personal access token for Data Center)."
                    .into(),
            )),
            _ => Err(SourceError::Auth(format!(
                "several Atlassian instances are configured ({}); set the source's \
                 `instance` param to name one.",
                self.atlassian_instance_names()
            ))),
        }
    }

    fn atlassian_instance_names(&self) -> String {
        if self.atlassian.is_empty() {
            return "none".into();
        }
        self.atlassian
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Compatibility helper for a default per-vault store. Runtime refresh holds
    /// the same store guard across loading the predecessor, POST, and successor write.
    pub fn persist_atlassian_refresh_token(
        vault_root: &Path,
        instance: &str,
        refresh_token: &str,
    ) -> Result<bool, SourceError> {
        let guard = CredentialStore::for_vault(vault_root)?.lock()?;
        let mut on_disk = guard.load_file()?;
        let Some(entry) = on_disk.atlassian.get_mut(instance) else {
            return Ok(false);
        };
        let AtlassianAuthMethod::Oauth {
            refresh_token: stored,
            ..
        } = &mut entry.auth
        else {
            return Ok(false);
        };
        *stored = refresh_token.to_string();
        guard.save(&on_disk)?;
        Ok(true)
    }

    fn override_from_env(&mut self) {
        if let (Some(id), Some(secret), Some(refresh)) = (
            env_credential("LORE_GOOGLE_CLIENT_ID"),
            env_credential("LORE_GOOGLE_CLIENT_SECRET"),
            env_credential("LORE_GOOGLE_REFRESH_TOKEN"),
        ) {
            self.google = Some(GoogleCredentials {
                client_id: id,
                client_secret: secret,
                refresh_token: refresh,
            });
        }

        let bot = env_credential("LORE_SLACK_TOKEN");
        let user = env_credential("LORE_SLACK_USER_TOKEN");
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

        // Env overlay targets the `default` instance, and covers the methods whose
        // credentials are stable strings: a Data Center PAT, and a Cloud email + account
        // token in either shape. Which variables are set is what selects the method, since
        // each variant needs exactly its own fields — `LORE_ATLASSIAN_CLOUD_ID` is the
        // scoped token's, and asks for the gateway.
        //
        // OAuth is deliberately absent — its refresh token ROTATES, and
        // `persist_atlassian_refresh_token` has no file entry to write the successor to, so
        // an env-supplied grant would work for exactly one run. Grants live in the file,
        // where their rotation can be recorded.
        if let Some(site_url) = env_credential("LORE_ATLASSIAN_SITE_URL") {
            let auth = match (
                env_credential("LORE_ATLASSIAN_PAT"),
                env_credential("LORE_ATLASSIAN_EMAIL"),
                env_credential("LORE_ATLASSIAN_API_TOKEN"),
                env_credential("LORE_ATLASSIAN_CLOUD_ID"),
            ) {
                (Some(token), ..) => Some(AtlassianAuthMethod::PersonalAccessToken { token }),
                (_, Some(email), Some(api_token), Some(cloud_id)) => {
                    Some(AtlassianAuthMethod::ScopedToken {
                        email,
                        api_token,
                        cloud_id,
                    })
                }
                (_, Some(email), Some(api_token), None) => {
                    Some(AtlassianAuthMethod::ApiToken { email, api_token })
                }
                _ => None,
            };
            if let Some(auth) = auth {
                self.atlassian
                    .insert("default".into(), AtlassianCredentials { site_url, auth });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn instance(site: &str, auth: AtlassianAuthMethod) -> AtlassianCredentials {
        AtlassianCredentials {
            site_url: site.into(),
            auth,
        }
    }

    fn oauth_method() -> AtlassianAuthMethod {
        AtlassianAuthMethod::Oauth {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            refresh_token: "rt-1".into(),
            cloud_id: "cloud-1".into(),
        }
    }

    #[test]
    fn shared_store_creates_missing_parents_and_rejects_invalid_utf8() {
        let tmp = TempDir::new().unwrap();
        let store = CredentialStore::new(&tmp.path().join("new/한글/credentials.json")).unwrap();
        let guard = store.lock().unwrap();
        guard.save(&Credentials::default()).unwrap();
        drop(guard);
        assert!(store.load_file().is_ok());
        std::fs::write(store.path(), [0xff, 0xfe]).unwrap();
        assert!(store.load_file().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn credential_reads_require_owner_only_permissions_before_parsing() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let store = CredentialStore::new(&tmp.path().join("credentials.json")).unwrap();
        store.lock().unwrap().save(&Credentials::default()).unwrap();
        for mode in [0o600, 0o400] {
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(store.load_file().is_ok(), "mode {mode:o}");
        }
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(store.path(), b"not JSON").unwrap();
        for mode in [0o640, 0o604, 0o620, 0o602, 0o660, 0o644, 0o666, 0o777] {
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            let error = store.load_file().unwrap_err().to_string();
            assert!(error.contains("chmod 600"), "mode {mode:o}: {error}");
            assert!(!error.contains("parse"), "mode {mode:o}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn credential_writes_keep_store_and_lock_private_and_reject_a_widened_lock() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let store = CredentialStore::new(&tmp.path().join("credentials.json")).unwrap();
        store.lock().unwrap().save(&Credentials::default()).unwrap();
        let lock_path = tmp.path().join("credentials.json.lock");
        for path in [store.path(), lock_path.as_path()] {
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let original = std::fs::read(store.path()).unwrap();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = store
            .lock()
            .err()
            .expect("insecure lock refused")
            .to_string();
        assert!(error.contains("chmod 600"), "{error}");
        assert_eq!(std::fs::read(store.path()).unwrap(), original);
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        store.lock().unwrap().load_file().unwrap();
    }

    #[test]
    fn malformed_credentials_report_location_without_echoing_secret_values() {
        let tmp = TempDir::new().unwrap();
        let store = CredentialStore::new(&tmp.path().join("credentials.json")).unwrap();
        store.lock().unwrap().save(&Credentials::default()).unwrap();
        let content = r#"{"atlassian":{"default":{"site_url":"https://example.test","method":"sensitive-canary-value"}}}"#;
        std::fs::write(store.path(), content).unwrap();
        let error = store.load_file().unwrap_err().to_string();
        assert!(error.contains("line 1, column"), "{error}");
        assert!(!error.contains("sensitive-canary-value"), "{error}");
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), content);
    }

    #[cfg(unix)]
    #[test]
    fn aliases_share_target_and_lock_across_atomic_replacement() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("shared.json");
        let store = CredentialStore::new(&target).unwrap();
        store.lock().unwrap().save(&Credentials::default()).unwrap();
        let alias = tmp.path().join("alias.json");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let alias_store = CredentialStore::new(&alias).unwrap();
        assert_eq!(alias_store.path(), store.path());
        let guard = store.lock().unwrap();
        guard.save(&Credentials::default()).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path().join("shared.json.lock"))
            .unwrap();
        assert!(
            lock.try_lock().is_err(),
            "replacement must not release companion lock"
        );
        drop(guard);
        lock.try_lock().unwrap();
        assert!(
            std::fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_rejects_symlinks_and_nonregular_entries() {
        let tmp = TempDir::new().unwrap();
        let store = CredentialStore::new(&tmp.path().join("shared.json")).unwrap();
        let lock_path = tmp.path().join("shared.json.lock");
        let other = tmp.path().join("other");
        std::fs::write(&other, b"untouched").unwrap();
        std::os::unix::fs::symlink(&other, &lock_path).unwrap();
        assert!(store.lock().is_err());
        assert_eq!(std::fs::read(&other).unwrap(), b"untouched");
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::create_dir(lock_path).unwrap();
        assert!(store.lock().is_err());
    }

    #[test]
    fn a_lone_instance_needs_no_name_whatever_its_key() {
        let mut creds = Credentials::default();
        creds
            .atlassian
            .insert("whatever".into(), instance("https://a.net", oauth_method()));
        let (name, _) = creds.resolve_atlassian_instance(None).unwrap();
        assert_eq!(name, "whatever");
    }

    #[test]
    fn several_instances_require_naming_and_the_error_lists_them() {
        let mut creds = Credentials::default();
        creds
            .atlassian
            .insert("cloud".into(), instance("https://a.net", oauth_method()));
        creds.atlassian.insert(
            "onprem".into(),
            instance(
                "https://wiki.corp/confluence",
                AtlassianAuthMethod::PersonalAccessToken { token: "p".into() },
            ),
        );

        let err = creds
            .resolve_atlassian_instance(None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cloud") && err.contains("onprem"), "{err}");

        assert_eq!(
            creds.resolve_atlassian_instance(Some("onprem")).unwrap().0,
            "onprem"
        );
        let missing = creds
            .resolve_atlassian_instance(Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("cloud, onprem"), "{missing}");
    }

    #[test]
    fn no_instance_configured_points_at_the_setup_command() {
        let err = Credentials::default()
            .resolve_atlassian_instance(None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("lore init credentials"), "{err}");
    }

    #[test]
    fn rotated_refresh_token_is_persisted_for_the_named_instance_only() {
        let dir = TempDir::new().unwrap();
        let mut creds = Credentials::default();
        creds
            .atlassian
            .insert("cloud".into(), instance("https://a.net", oauth_method()));
        creds
            .atlassian
            .insert("other".into(), instance("https://b.net", oauth_method()));
        creds.save(dir.path()).unwrap();

        let updated =
            Credentials::persist_atlassian_refresh_token(dir.path(), "cloud", "rt-2").unwrap();
        assert!(updated);

        let back = Credentials::load_file(dir.path()).unwrap();
        let rt = |c: &Credentials, k: &str| match &c.atlassian[k].auth {
            AtlassianAuthMethod::Oauth { refresh_token, .. } => refresh_token.clone(),
            _ => unreachable!(),
        };
        assert_eq!(rt(&back, "cloud"), "rt-2");
        assert_eq!(rt(&back, "other"), "rt-1", "sibling instance untouched");
    }

    #[test]
    fn persisting_reports_false_when_there_is_no_file_entry_to_update() {
        // Compatibility helper reports an absent file entry; runtime refresh refuses
        // a missing OAuth entry before sending a token request.
        let dir = TempDir::new().unwrap();
        Credentials::default().save(dir.path()).unwrap();
        assert!(
            !Credentials::persist_atlassian_refresh_token(dir.path(), "cloud", "rt-2").unwrap()
        );
    }

    #[test]
    fn a_pat_instance_has_no_refresh_token_to_rotate() {
        let dir = TempDir::new().unwrap();
        let mut creds = Credentials::default();
        creds.atlassian.insert(
            "onprem".into(),
            instance(
                "https://wiki.corp/confluence",
                AtlassianAuthMethod::PersonalAccessToken {
                    token: "pat".into(),
                },
            ),
        );
        creds.save(dir.path()).unwrap();
        assert!(
            !Credentials::persist_atlassian_refresh_token(dir.path(), "onprem", "rt").unwrap(),
            "rotation is an OAuth concept; a PAT entry must be left alone"
        );
    }

    #[test]
    fn auth_method_round_trips_through_its_tag() {
        let dir = TempDir::new().unwrap();
        let mut creds = Credentials::default();
        creds.atlassian.insert(
            "onprem".into(),
            instance(
                "https://wiki.corp/confluence",
                AtlassianAuthMethod::PersonalAccessToken {
                    token: "pat".into(),
                },
            ),
        );
        creds.save(dir.path()).unwrap();

        let raw = std::fs::read_to_string(Credentials::path(dir.path())).unwrap();
        assert!(raw.contains(r#""method": "pat""#), "{raw}");

        let back = Credentials::load_file(dir.path()).unwrap();
        assert!(matches!(
            back.atlassian["onprem"].auth,
            AtlassianAuthMethod::PersonalAccessToken { .. }
        ));
    }

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
        assert_eq!(path, Credentials::path(dir.path()).canonicalize().unwrap());

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("user_token"), "configured token present");
        assert!(!raw.contains("bot_token"), "unset token omitted");
        assert!(!raw.contains("google"), "unset provider omitted");

        let back = Credentials::load_file(dir.path()).unwrap();
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

    #[test]
    fn debug_redacts_secrets_but_keeps_identifiers() {
        let google = GoogleCredentials {
            client_id: "app-123.apps".into(),
            client_secret: "SHHH-secret".into(),
            refresh_token: "1//refresh-shh".into(),
        };
        let g = format!("{google:?}");
        assert!(
            g.contains("app-123.apps"),
            "non-secret client_id shown: {g}"
        );
        assert!(!g.contains("SHHH-secret"), "client_secret redacted: {g}");
        assert!(!g.contains("1//refresh-shh"), "refresh_token redacted: {g}");

        let atlassian = instance(
            "https://x.atlassian.net",
            AtlassianAuthMethod::ApiToken {
                email: "me@x.com".into(),
                api_token: "JIRA-shh".into(),
            },
        );
        let a = format!("{atlassian:?}");
        assert!(a.contains("https://x.atlassian.net") && a.contains("me@x.com"));
        assert!(!a.contains("JIRA-shh"), "api_token redacted: {a}");

        let slack = SlackCredentials {
            bot_token: Some("xoxb-shh".into()),
            user_token: None,
        };
        let s = format!("{slack:?}");
        assert!(!s.contains("xoxb-shh"), "bot_token value redacted: {s}");
        // Presence is still observable (Some vs None) without leaking the value.
        assert!(s.contains("bot_token: Some") && s.contains("user_token: None"));
    }
}
