use std::path::Path;
use std::sync::Arc;

use lk_core::config::{Config, Identity, PerformanceConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_queue::LlmClient;
use lk_vault::TemplateEngine;

use crate::PipelineError;

pub struct PipelineContext {
    pub(crate) engine: TemplateEngine,
    pub(crate) llm: Arc<dyn LlmClient>,
    pub(crate) dirs: VaultDirs,
    pub(crate) perf: PerformanceConfig,
    pub(crate) identity: Identity,
    pub(crate) timezone: jiff::tz::TimeZone,
    pub(crate) locale: Locale,
    pub(crate) concept_categories: Vec<lk_queue::CategoryRef>,
}

impl PipelineContext {
    pub fn new(
        template_dir: Option<&Path>,
        llm: Arc<dyn LlmClient>,
        config: &Config,
    ) -> Result<Self, PipelineError> {
        let engine = TemplateEngine::new(template_dir)?;
        let concept_categories = config
            .concepts
            .categories
            .iter()
            .map(|c| lk_queue::CategoryRef {
                id: c.id.clone(),
                label: c.label.clone(),
            })
            .collect();
        Ok(Self {
            engine,
            llm,
            dirs: config.vault.dirs.clone(),
            perf: config.performance.clone(),
            identity: config.identity.clone(),
            timezone: config.vault.timezone(),
            locale: config.vault.locale(),
            concept_categories,
        })
    }
}
