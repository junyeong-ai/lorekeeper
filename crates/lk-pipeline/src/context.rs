use std::path::Path;
use std::sync::Arc;

use lk_core::config::{Config, PersonalConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_queue::LlmClient;
use lk_vault::TemplateEngine;

use crate::PipelineError;

pub struct PipelineContext {
    pub(crate) engine: TemplateEngine,
    pub(crate) llm: Arc<dyn LlmClient>,
    pub(crate) dirs: VaultDirs,
    /// The personal-productivity module config, or `None` for a domain-neutral engine.
    /// When `None`, no work-log, reviews, or performance categorization run.
    pub(crate) personal: Option<PersonalConfig>,
    pub(crate) timezone: jiff::tz::TimeZone,
    pub(crate) locale: Locale,
    pub(crate) concept_categories: Vec<lk_queue::CategoryReference>,
}

impl PipelineContext {
    pub fn build(
        template_dir: Option<&Path>,
        llm: Arc<dyn LlmClient>,
        config: &Config,
    ) -> Result<Self, PipelineError> {
        let engine = TemplateEngine::build(template_dir)?;
        let concept_categories = config
            .concepts
            .categories
            .iter()
            .map(|c| lk_queue::CategoryReference {
                id: c.id.clone(),
                label: c.label.clone(),
            })
            .collect();
        Ok(Self {
            engine,
            llm,
            dirs: config.vault.dirs.clone(),
            personal: config.personal.clone(),
            timezone: config.vault.timezone(),
            locale: config.vault.locale(),
            concept_categories,
        })
    }
}
