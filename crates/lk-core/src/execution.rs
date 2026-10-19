use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentEngine {
    ClaudeCode,
    Codex,
}

impl AgentEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn skills_directory(self) -> &'static str {
        match self {
            Self::ClaudeCode => ".claude",
            Self::Codex => ".agents",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionOperation {
    Daily,
    Process,
    Knowledge,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    pub default_profile: Option<String>,
    pub profiles: BTreeMap<String, ExecutionProfile>,
    pub operations: BTreeMap<ExecutionOperation, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "engine", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExecutionProfile {
    ClaudeCode {
        #[serde(default = "claude_executable")]
        executable: PathBuf,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        config_directory: Option<PathBuf>,
        timeout_seconds: NonZeroU64,
    },
    Codex {
        #[serde(default = "codex_executable")]
        executable: PathBuf,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        profile: Option<String>,
        timeout_seconds: NonZeroU64,
    },
}

fn claude_executable() -> PathBuf {
    "claude".into()
}

fn codex_executable() -> PathBuf {
    "codex".into()
}

impl ExecutionConfig {
    pub fn resolve_profile(
        &self,
        operation: ExecutionOperation,
        explicit: Option<&str>,
    ) -> Result<(&str, &ExecutionProfile), ConfigError> {
        let name = explicit
            .or_else(|| self.operations.get(&operation).map(String::as_str))
            .or(self.default_profile.as_deref())
            .ok_or_else(|| {
                ConfigError::Validation(
                    "semantic execution needs execution.default_profile or an explicit --profile"
                        .into(),
                )
            })?;
        self.profiles
            .get_key_value(name)
            .map(|(name, profile)| (name.as_str(), profile))
            .ok_or_else(|| ConfigError::Validation(format!("unknown execution profile '{name}'")))
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        for name in self.profiles.keys() {
            validate_text("execution profile name", name)?;
        }
        for name in self.default_profile.iter().chain(self.operations.values()) {
            if !self.profiles.contains_key(name) {
                return Err(ConfigError::Validation(format!(
                    "unknown execution profile '{name}'"
                )));
            }
        }
        for profile in self.profiles.values() {
            validate_text(
                "execution executable",
                &profile.executable().to_string_lossy(),
            )?;
            if let Some(model) = profile.model() {
                validate_text("execution model", model)?;
            }
            match profile {
                ExecutionProfile::ClaudeCode {
                    config_directory: Some(path),
                    ..
                } => {
                    validate_text("execution config_directory", &path.to_string_lossy())?;
                }
                ExecutionProfile::Codex {
                    profile: Some(name),
                    ..
                } => {
                    validate_text("Codex profile", name)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn resolve_relative_to(&mut self, parent: &Path) {
        for profile in self.profiles.values_mut() {
            let executable = match profile {
                ExecutionProfile::ClaudeCode {
                    executable,
                    config_directory,
                    ..
                } => {
                    if let Some(path) = config_directory {
                        *path = crate::config::resolve_vault_root(&path.to_string_lossy(), parent);
                    }
                    executable
                }
                ExecutionProfile::Codex { executable, .. } => executable,
            };
            if executable.components().count() > 1 {
                *executable =
                    crate::config::resolve_vault_root(&executable.to_string_lossy(), parent);
            }
        }
    }
}

fn validate_text(field: &str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(ConfigError::Validation(format!(
            "{field} must be nonblank text without surrounding whitespace or control characters"
        )));
    }
    Ok(())
}

impl ExecutionProfile {
    pub fn engine(&self) -> AgentEngine {
        match self {
            Self::ClaudeCode { .. } => AgentEngine::ClaudeCode,
            Self::Codex { .. } => AgentEngine::Codex,
        }
    }

    pub fn executable(&self) -> &Path {
        match self {
            Self::ClaudeCode { executable, .. } | Self::Codex { executable, .. } => executable,
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            Self::ClaudeCode { model, .. } | Self::Codex { model, .. } => model.as_deref(),
        }
    }

    pub fn timeout(&self) -> std::time::Duration {
        let seconds = match self {
            Self::ClaudeCode {
                timeout_seconds, ..
            }
            | Self::Codex {
                timeout_seconds, ..
            } => timeout_seconds.get(),
        };
        std::time::Duration::from_secs(seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ExecutionConfig {
        crate::yaml::from_str("default_profile: main\nprofiles:\n  main: {engine: claude-code, timeout_seconds: 3600}\n  review: {engine: codex, timeout_seconds: 1800}\noperations: {knowledge: review}\n").unwrap()
    }

    #[test]
    fn selection_is_explicit_then_operation_then_default() {
        let config = config();
        config.validate().unwrap();
        assert_eq!(
            config
                .resolve_profile(ExecutionOperation::Daily, None)
                .unwrap()
                .0,
            "main"
        );
        assert_eq!(
            config
                .resolve_profile(ExecutionOperation::Knowledge, None)
                .unwrap()
                .0,
            "review"
        );
        assert_eq!(
            config
                .resolve_profile(ExecutionOperation::Knowledge, Some("main"))
                .unwrap()
                .0,
            "main"
        );
        assert!(
            config
                .resolve_profile(ExecutionOperation::Daily, Some("missing"))
                .is_err()
        );
        assert!(
            ExecutionConfig::default()
                .resolve_profile(ExecutionOperation::Process, None)
                .is_err()
        );
    }

    #[test]
    fn rejects_unknown_hosts_fields_operations_and_unbounded_time() {
        for body in [
            "profiles: {x: {engine: other, timeout_seconds: 1}}",
            "profiles: {x: {engine: codex, timeout_seconds: 0}}",
            "profiles: {x: {engine: codex}}",
            "profiles: {x: {engine: codex, timeout_seconds: 1, config_directory: /tmp}}",
            "operations: {unknown: x}",
        ] {
            assert!(
                crate::yaml::from_str::<ExecutionConfig>(body).is_err(),
                "{body}"
            );
        }
        let mut config = config();
        config.default_profile = Some("missing".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn command_names_keep_path_lookup_and_paths_follow_the_config() {
        let mut config: ExecutionConfig = crate::yaml::from_str("profiles:\n  c: {engine: claude-code, executable: bin/claude, config_directory: auth, timeout_seconds: 1}\n  x: {engine: codex, timeout_seconds: 1}\n").unwrap();
        config.resolve_relative_to(Path::new("/settings"));
        assert_eq!(
            config.profiles["c"].executable(),
            Path::new("/settings/bin/claude")
        );
        assert_eq!(config.profiles["x"].executable(), Path::new("codex"));
    }
}
