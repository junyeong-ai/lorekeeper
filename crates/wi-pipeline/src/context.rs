use std::path::Path;
use std::sync::Arc;

use wi_core::config::{Config, Identity, PerformanceConfig, VaultDirs};
use wi_core::i18n::Locale;
use wi_llm::LlmClient;
use wi_vault::TemplateEngine;

use crate::PipelineError;

pub struct PipelineContext {
    pub(crate) engine: TemplateEngine,
    pub(crate) llm: Arc<dyn LlmClient>,
    pub(crate) dirs: VaultDirs,
    pub(crate) perf: PerformanceConfig,
    pub(crate) identity: Identity,
    pub(crate) timezone: jiff::tz::TimeZone,
    pub(crate) locale: Locale,
}

impl PipelineContext {
    pub fn new(
        template_dir: Option<&Path>,
        llm: Arc<dyn LlmClient>,
        config: &Config,
    ) -> Result<Self, PipelineError> {
        let engine = TemplateEngine::new(template_dir)?;
        Ok(Self {
            engine,
            llm,
            dirs: config.vault.dirs.clone(),
            perf: config.performance.clone(),
            identity: config.identity.clone(),
            timezone: config.vault.timezone(),
            locale: config.vault.locale(),
        })
    }
}
