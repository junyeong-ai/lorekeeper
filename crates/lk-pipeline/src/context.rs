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
    pub(crate) llm_provider: lk_core::config::LlmProvider,
    pub(crate) dirs: VaultDirs,
    /// The personal-productivity module config, or `None` for a domain-neutral engine.
    /// When `None`, no work-log, reviews, or performance categorization run.
    pub(crate) personal: Option<PersonalConfig>,
    pub(crate) timezone: jiff::tz::TimeZone,
    pub(crate) locale: Locale,
    pub(crate) concept_categories: Vec<lk_queue::CategoryReference>,
    pub(crate) manual_projection_limit: usize,
    pub(crate) input_limits: lk_core::config::IngestInputLimits,
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
            llm_provider: config.llm.provider,
            dirs: config.vault.dirs.clone(),
            personal: config.personal.clone(),
            timezone: config.vault.timezone(),
            locale: config.vault.locale(),
            concept_categories,
            input_limits: config.ingest.inputs.clone(),
            manual_projection_limit: config
                .ingest
                .documents
                .as_ref()
                .map(|documents| documents.max_output_bytes.get())
                .unwrap_or(lk_core::manual_original::MAX_ORIGINAL_ENTRY_BYTES),
        })
    }
}
