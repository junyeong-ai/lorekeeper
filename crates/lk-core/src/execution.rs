use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkflowId(String);

impl WorkflowId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(format!("{:032x}", u128::from_be_bytes(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for WorkflowId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("workflow ID must contain exactly 32 hexadecimal digits".into());
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

impl From<WorkflowId> for String {
    fn from(value: WorkflowId) -> Self {
        value.0
    }
}

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
    pub work: SemanticWorkConfig,
    #[serde(skip)]
    selected_profile: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExtractionContext {
    Progressive,
    FullContext,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticWorkConfig {
    pub max_input_bytes: usize,
    pub max_result_bytes: usize,
    pub max_calls: usize,
    pub max_parallel: usize,
    pub max_consecutive_calls: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_screening_batches: Option<NonZeroUsize>,
    pub extraction_context: ExtractionContext,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticWorkOverrides {
    pub max_input_bytes: Option<usize>,
    pub max_result_bytes: Option<usize>,
    pub max_calls: Option<usize>,
    pub max_parallel: Option<usize>,
    pub max_consecutive_calls: Option<usize>,
    pub max_screening_batches: Option<NonZeroUsize>,
    pub extraction_context: Option<ExtractionContext>,
}

impl SemanticWorkOverrides {
    fn resolve(&self, defaults: SemanticWorkConfig) -> SemanticWorkConfig {
        SemanticWorkConfig {
            max_input_bytes: self.max_input_bytes.unwrap_or(defaults.max_input_bytes),
            max_result_bytes: self.max_result_bytes.unwrap_or(defaults.max_result_bytes),
            max_calls: self.max_calls.unwrap_or(defaults.max_calls),
            max_parallel: self.max_parallel.unwrap_or(defaults.max_parallel),
            max_consecutive_calls: self
                .max_consecutive_calls
                .unwrap_or(defaults.max_consecutive_calls),
            max_screening_batches: self
                .max_screening_batches
                .or(defaults.max_screening_batches),
            extraction_context: self
                .extraction_context
                .unwrap_or(defaults.extraction_context),
        }
    }
}

impl SemanticWorkConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_input_bytes < 4096
            || self.max_result_bytes < 1024
            || self.max_result_bytes > self.max_input_bytes / 4
            || self.max_calls == 0
            || self.max_parallel == 0
            || self.max_consecutive_calls == 0
        {
            return Err(ConfigError::Validation(
                "semantic work requires max_input_bytes >= 4096, max_result_bytes >= 1024 and <= max_input_bytes/4, max_calls > 0, max_parallel > 0, and max_consecutive_calls > 0".into(),
            ));
        }
        Ok(())
    }
}

impl Default for SemanticWorkConfig {
    fn default() -> Self {
        Self {
            max_input_bytes: 262_144,
            max_result_bytes: 32_768,
            max_calls: 64,
            max_parallel: 1,
            max_consecutive_calls: 4,
            max_screening_batches: None,
            extraction_context: ExtractionContext::Progressive,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaudeCodeEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ClaudeCodeEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "engine", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExecutionProfile {
    ClaudeCode {
        #[serde(default)]
        work: SemanticWorkOverrides,
        #[serde(default = "claude_executable")]
        executable: PathBuf,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<ClaudeCodeEffort>,
        #[serde(default)]
        config_directory: Option<PathBuf>,
        timeout_seconds: NonZeroU64,
    },
    Codex {
        #[serde(default)]
        work: SemanticWorkOverrides,
        #[serde(default = "codex_executable")]
        executable: PathBuf,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        model_reasoning_effort: Option<String>,
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
    pub fn select_profile(&mut self, name: &str) -> Result<(), ConfigError> {
        self.work_for(name)?;
        self.selected_profile = Some(name.to_owned());
        Ok(())
    }

    pub fn selected_profile(&self) -> Option<&str> {
        self.selected_profile.as_deref()
    }

    pub fn work_for(&self, name: &str) -> Result<SemanticWorkConfig, ConfigError> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            ConfigError::Validation(format!("unknown execution profile '{name}'"))
        })?;
        let overrides = match profile {
            ExecutionProfile::ClaudeCode { work, .. } | ExecutionProfile::Codex { work, .. } => {
                work
            }
        };
        let work = overrides.resolve(self.work);
        work.validate()?;
        Ok(work)
    }

    pub fn effective_work(&self) -> Result<SemanticWorkConfig, ConfigError> {
        match self.selected_profile() {
            Some(name) => self.work_for(name),
            None => Ok(self.work),
        }
    }

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
        self.work.validate()?;
        for name in self.profiles.keys() {
            validate_text("execution profile name", name)?;
            self.work_for(name)?;
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
            if let ExecutionProfile::Codex {
                model_reasoning_effort: Some(effort),
                ..
            } = profile
            {
                validate_text("Codex model_reasoning_effort", effort)?;
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

    pub fn set_executable(&mut self, path: std::path::PathBuf) {
        match self {
            Self::ClaudeCode { executable, .. } | Self::Codex { executable, .. } => {
                *executable = path
            }
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

    #[test]
    fn workflow_context_is_validated_and_roundtrips_without_changing_identity() {
        let id = WorkflowId::from_bytes([0xab; 16]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<WorkflowId>(&json).unwrap(), id);
        assert_eq!(WorkflowId::try_from("AB".repeat(16)).unwrap(), id);
        for value in ["", "short", "gggggggggggggggggggggggggggggggg"] {
            assert!(WorkflowId::try_from(value.to_owned()).is_err());
        }
    }

    fn config() -> ExecutionConfig {
        crate::yaml::from_str("default_profile: main\nprofiles:\n  main: {engine: claude-code, timeout_seconds: 3600}\n  review: {engine: codex, timeout_seconds: 1800}\noperations: {knowledge: review}\n").unwrap()
    }

    #[test]
    fn semantic_work_defaults_and_valid_boundaries_are_explicit() {
        for body in ["{}", "work: {}", "work: null"] {
            let config: ExecutionConfig = crate::yaml::from_str(body).unwrap();
            config.validate().unwrap();
            assert_eq!(config.work.max_input_bytes, 262_144);
            assert_eq!(config.work.max_result_bytes, 32_768);
            assert_eq!(config.work.max_calls, 64);
            assert_eq!(config.work.max_consecutive_calls, 4);
            assert_eq!(
                config.work.extraction_context,
                ExtractionContext::Progressive
            );
        }
        for (input, result, calls) in [
            (4096, 1024, 1),
            (4099, 1024, 1),
            (8192, 2048, 64),
            (131_072, 32_768, 1),
            (262_144, 32_768, 64),
        ] {
            let body = format!(
                "work: {{max_input_bytes: {input}, max_result_bytes: {result}, max_calls: {calls}}}"
            );
            let config: ExecutionConfig = crate::yaml::from_str(&body).unwrap();
            config.validate().unwrap();
        }
    }

    #[test]
    fn semantic_work_rejects_unbounded_and_inconsistent_limits() {
        for (input, result, calls) in [
            (0, 1024, 1),
            (4095, 1024, 1),
            (4096, 0, 1),
            (4096, 1023, 1),
            (4096, 1025, 1),
            (8191, 2048, 1),
            (4096, 1024, 0),
        ] {
            let body = format!(
                "work: {{max_input_bytes: {input}, max_result_bytes: {result}, max_calls: {calls}}}"
            );
            let config: ExecutionConfig = crate::yaml::from_str(&body).unwrap();
            assert!(
                matches!(config.validate(), Err(ConfigError::Validation(_))),
                "{body}"
            );
        }
    }

    #[test]
    fn semantic_work_rejects_unknown_fields_and_invalid_scalar_types() {
        for body in [
            "work: {max_input_byte: 262144}",
            "work: {max_calls: 64, unknown: true}",
            "work: {max_calls: -1}",
            "work: {max_calls: 1.5}",
            "work: {max_parallel: -1}",
            "work: {max_parallel: true}",
            "work: {max_consecutive_calls: -1}",
            "work: {max_consecutive_calls: true}",
            "work: {max_input_bytes: true}",
            "work: {max_result_bytes: many}",
            "work: {extraction_context: automatic}",
            "work: {extraction_context: true}",
        ] {
            assert!(
                crate::yaml::from_str::<ExecutionConfig>(body).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn parallel_work_limits_are_positive_inherited_and_profile_scoped() {
        let mut config: ExecutionConfig = crate::yaml::from_str(
            "work: {max_parallel: 10}\nprofiles:\n  inherited: {engine: codex, timeout_seconds: 60}\n  serial: {engine: codex, timeout_seconds: 60, work: {max_parallel: 1}}\n",
        )
        .unwrap();
        config.validate().unwrap();
        config.select_profile("inherited").unwrap();
        assert_eq!(config.effective_work().unwrap().max_parallel, 10);
        config.select_profile("serial").unwrap();
        assert_eq!(config.effective_work().unwrap().max_parallel, 1);
        for body in [
            "work: {max_parallel: 0}",
            "profiles:\n  broken: {engine: codex, timeout_seconds: 60, work: {max_parallel: 0}}",
        ] {
            let config: ExecutionConfig = crate::yaml::from_str(body).unwrap();
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn consecutive_work_calls_are_positive_and_profile_scoped() {
        let config: ExecutionConfig = crate::yaml::from_str(
            "work: {max_consecutive_calls: 6}\nprofiles:\n  inherited: {engine: codex, timeout_seconds: 60}\n  alternate: {engine: codex, timeout_seconds: 60, work: {max_consecutive_calls: 1}}\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.work_for("inherited").unwrap().max_consecutive_calls,
            6
        );
        assert_eq!(
            config.work_for("alternate").unwrap().max_consecutive_calls,
            1
        );
        for body in [
            "work: {max_consecutive_calls: 0}",
            "profiles:\n  broken: {engine: codex, timeout_seconds: 60, work: {max_consecutive_calls: 0}}",
        ] {
            assert!(
                crate::yaml::from_str::<ExecutionConfig>(body)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn screening_capacity_is_optional_positive_and_inherits_per_profile() {
        let config:ExecutionConfig=crate::yaml::from_str("work: {max_screening_batches: 3}\nprofiles:\n  inherited: {engine: codex, timeout_seconds: 60}\n  bounded: {engine: codex, timeout_seconds: 60, work: {max_screening_batches: 1}}\n").unwrap();
        assert_eq!(
            config
                .work_for("inherited")
                .unwrap()
                .max_screening_batches
                .unwrap()
                .get(),
            3
        );
        assert_eq!(
            config
                .work_for("bounded")
                .unwrap()
                .max_screening_batches
                .unwrap()
                .get(),
            1
        );
        assert!(
            crate::yaml::from_str::<ExecutionConfig>("work: {max_screening_batches: 0}").is_err()
        );
        assert!(
            ExecutionConfig::default()
                .work
                .max_screening_batches
                .is_none()
        );
    }

    #[test]
    fn profile_work_overrides_inherit_and_validate_the_effective_budget() {
        let mut config: ExecutionConfig = crate::yaml::from_str(
            "work: {max_calls: 12}\nprofiles:\n  main: {engine: claude-code, timeout_seconds: 60}\n  small: {engine: codex, timeout_seconds: 60, work: {max_input_bytes: 16384, max_result_bytes: 4096, max_calls: 2}}\n"
        ).unwrap();
        config.validate().unwrap();
        assert_eq!(config.effective_work().unwrap().max_calls, 12);
        config.select_profile("small").unwrap();
        assert_eq!(
            config.effective_work().unwrap(),
            SemanticWorkConfig {
                max_screening_batches: None,
                max_input_bytes: 16384,
                max_result_bytes: 4096,
                max_calls: 2,
                max_parallel: 1,
                max_consecutive_calls: 4,
                extraction_context: ExtractionContext::Progressive,
            }
        );
        assert!(config.select_profile("missing").is_err());
        assert_eq!(config.selected_profile(), Some("small"));
        config.select_profile("main").unwrap();
        assert_eq!(config.effective_work().unwrap(), config.work);
        let mut scopes: ExecutionConfig = crate::yaml::from_str(
            "work: {extraction_context: full-context}\nprofiles:\n  inherited: {engine: codex, timeout_seconds: 60}\n  selected: {engine: codex, timeout_seconds: 60, work: {extraction_context: progressive}}\n"
        ).unwrap();
        scopes.select_profile("inherited").unwrap();
        assert_eq!(
            scopes.effective_work().unwrap().extraction_context,
            ExtractionContext::FullContext
        );
        scopes.select_profile("selected").unwrap();
        assert_eq!(
            scopes.effective_work().unwrap().extraction_context,
            ExtractionContext::Progressive
        );
        for fields in ["max_input_bytes: 8192", "max_calls: 0"] {
            let body = format!(
                "profiles: {{x: {{engine: codex, timeout_seconds: 1, work: {{{fields}}}}}}}"
            );
            let config: ExecutionConfig = crate::yaml::from_str(&body).unwrap();
            assert!(config.validate().is_err(), "{body}");
        }
        assert!(
            crate::yaml::from_str::<ExecutionConfig>(
                "profiles: {x: {engine: codex, timeout_seconds: 1, work: {max_call: 2}}}"
            )
            .is_err()
        );
        assert!(crate::yaml::from_str::<ExecutionConfig>("selected_profile: x").is_err());
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
    fn engine_options_are_optional_and_keep_the_hosts_own_value_contracts() {
        let config = config();
        assert!(matches!(
            &config.profiles["main"],
            ExecutionProfile::ClaudeCode { effort: None, .. }
        ));
        assert!(matches!(
            &config.profiles["review"],
            ExecutionProfile::Codex {
                model_reasoning_effort: None,
                ..
            }
        ));
        for (engine, field) in [
            ("claude-code", "effort"),
            ("codex", "model_reasoning_effort"),
        ] {
            let config: ExecutionConfig = crate::yaml::from_str(&format!(
                "profiles: {{x: {{engine: {engine}, {field}: null, timeout_seconds: 1}}}}"
            ))
            .unwrap();
            config.validate().unwrap();
            match &config.profiles["x"] {
                ExecutionProfile::ClaudeCode { effort, .. } => assert!(effort.is_none()),
                ExecutionProfile::Codex {
                    model_reasoning_effort,
                    ..
                } => assert!(model_reasoning_effort.is_none()),
            }
        }
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let config: ExecutionConfig = crate::yaml::from_str(&format!(
                "profiles: {{x: {{engine: claude-code, effort: {effort}, timeout_seconds: 1}}}}"
            ))
            .unwrap();
            config.validate().unwrap();
            let ExecutionProfile::ClaudeCode {
                effort: Some(actual),
                ..
            } = config.profiles["x"]
            else {
                panic!("missing Claude effort");
            };
            assert_eq!(actual.as_str(), effort);
        }
        for effort in [
            "none",
            "minimal",
            "low",
            "max",
            "ultra",
            "future-model-level",
        ] {
            let config: ExecutionConfig = crate::yaml::from_str(&format!(
                "profiles: {{x: {{engine: codex, model_reasoning_effort: {effort}, timeout_seconds: 1}}}}"
            ))
            .unwrap();
            config.validate().unwrap();
            let ExecutionProfile::Codex {
                model_reasoning_effort: Some(actual),
                ..
            } = &config.profiles["x"]
            else {
                panic!("missing Codex reasoning effort");
            };
            assert_eq!(actual, effort);
        }
    }

    #[test]
    fn engine_options_refuse_cross_engine_fields_and_invalid_values() {
        for body in [
            "profiles: {x: {engine: codex, effort: low, timeout_seconds: 1}}",
            "profiles: {x: {engine: claude-code, model_reasoning_effort: low, timeout_seconds: 1}}",
            "profiles: {x: {engine: claude-code, effort: ultracode, timeout_seconds: 1}}",
            "profiles: {x: {engine: claude-code, effort: automatic, timeout_seconds: 1}}",
            "effort: low",
            "profiles: {x: {engine: codex, model_reasoning_effort: 1, timeout_seconds: 1}}",
        ] {
            assert!(
                crate::yaml::from_str::<ExecutionConfig>(body).is_err(),
                "{body}"
            );
        }
        for effort in ["", " low", "low ", "low\n", "low\0"] {
            let body = format!(
                "profiles: {{x: {{engine: codex, model_reasoning_effort: {}, timeout_seconds: 1}}}}",
                serde_json::to_string(effort).unwrap()
            );
            let config: ExecutionConfig = crate::yaml::from_str(&body).unwrap();
            assert!(config.validate().is_err(), "{effort:?}");
        }
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
