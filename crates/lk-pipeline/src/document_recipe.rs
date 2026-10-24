use lk_core::concept::file_revision;
use lk_core::config::SourceConfig;
use lk_vault::TemplateInputs;
use serde::{Deserialize, Serialize};

use crate::{Pipeline, PipelineError};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRenderRecipe {
    version: u8,
    materialization: String,
    templates: TemplateInputs,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentProcessingRecipe {
    render: DocumentRenderRecipe,
    decoder: String,
}

impl DocumentRenderRecipe {
    pub fn with_decoder(self, decoder: String) -> DocumentProcessingRecipe {
        DocumentProcessingRecipe {
            render: self,
            decoder,
        }
    }
}

impl DocumentProcessingRecipe {
    pub fn decode(value: serde_json::Value) -> Result<Self, PipelineError> {
        if !value.is_object() || value.get("render").is_none_or(|value| !value.is_object()) {
            return Err(PipelineError::Render(
                "document processing recipe requires named fields".into(),
            ));
        }
        let recipe: Self = serde_json::from_value(value)
            .map_err(|error| PipelineError::Render(error.to_string()))?;
        if recipe.render.version != 1
            || !recipe.render.templates.contains_key("document.md.jinja")
            || recipe.render.templates["document.md.jinja"].is_none()
            || !valid_revision(&recipe.decoder)
            || !valid_revision(&recipe.render.materialization)
            || recipe
                .render
                .templates
                .values()
                .flatten()
                .any(|value| !valid_revision(value))
        {
            return Err(PipelineError::Render(
                "invalid document processing recipe".into(),
            ));
        }
        Ok(recipe)
    }
}

fn valid_revision(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Pipeline {
    fn document_materialization_revision(
        &self,
        source_id: &str,
        source: &SourceConfig,
    ) -> Result<String, PipelineError> {
        // These artifacts own normalization, document rendering and section cache
        // compatibility. Source bytes and decoder identity have separate bindings.
        let implementation = [
            include_str!("lib.rs"),
            include_str!("normalize.rs"),
            include_str!("classify.rs"),
            include_str!("render.rs"),
            include_str!("llm_cache.rs"),
            include_str!("document_identity.rs"),
            include_str!("../../lk-vault/src/template.rs"),
            include_str!("../../lk-vault/src/fragment.rs"),
            include_str!("../../lk-core/src/markdown/fragment.rs"),
            include_str!("../../lk-core/src/i18n.rs"),
            include_str!("../../lk-core/src/document.rs"),
            include_str!("../../lk-core/src/event.rs"),
            include_str!("../../lk-queue/src/lib.rs"),
        ]
        .map(file_revision);
        let recipe = serde_json::json!({
            "protocol": "document-materialization-v1",
            "implementation": implementation,
            "source": source_id,
            "type": source.source_type,
            "labels": source.labels,
            "classify": source.classify,
            "focus": source.normalized_focus(),
            "concept_policy": source.concept_policy,
            "semantic_publication": self.ctx.llm_provider,
            "locale": self.ctx.locale.tag(),
            "timezone": format!("{:?}", self.ctx.timezone),
            "wiki_directory": self.ctx.dirs.wiki,
            "concept_categories": self.ctx.concept_categories,
        });
        let text = serde_json::to_string(&recipe)
            .map_err(|error| PipelineError::Render(error.to_string()))?;
        Ok(file_revision(&text))
    }

    pub(crate) fn document_render_recipe(
        &self,
        source_id: &str,
        source: &SourceConfig,
        templates: TemplateInputs,
    ) -> Result<DocumentRenderRecipe, PipelineError> {
        Ok(DocumentRenderRecipe {
            version: 1,
            materialization: self.document_materialization_revision(source_id, source)?,
            templates,
        })
    }

    pub fn document_recipe_current(
        &self,
        source_id: &str,
        source: &SourceConfig,
        decoder: &str,
        recipe: &DocumentProcessingRecipe,
    ) -> Result<bool, PipelineError> {
        Ok(recipe.decoder == decoder
            && recipe.render.materialization
                == self.document_materialization_revision(source_id, source)?
            && self.ctx.engine.inputs_current(&recipe.render.templates)?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::{IngestOptions, PipelineContext};
    use lk_core::config::Config;

    fn config(root: &Path) -> Config {
        let mut config: Config =
            lk_core::yaml::from_str(include_str!("../../../config.starter.yaml")).unwrap();
        config.vault.root = root.to_string_lossy().into();
        config
    }

    fn pipeline(root: &Path, templates: Option<&Path>, config: &Config) -> Pipeline {
        let context =
            PipelineContext::build(templates, Arc::new(lk_queue::NoopLlmClient), config).unwrap();
        Pipeline::new(root, Arc::new(context))
    }

    #[test]
    fn recipes_ignore_operational_settings_but_detect_semantic_and_template_changes() {
        let root = tempfile::tempdir().unwrap();
        let templates = tempfile::tempdir().unwrap();
        std::fs::write(
            templates.path().join("document.md.jinja"),
            "{% include 'body' %}",
        )
        .unwrap();
        std::fs::write(templates.path().join("body"), "document").unwrap();
        let config = config(root.path());
        let first = pipeline(root.path(), Some(templates.path()), &config);
        let (_, inputs) = first
            .ctx
            .engine
            .render_with_inputs("document.md.jinja", &serde_json::json!({}))
            .unwrap();
        let decoder = file_revision("decoder");
        let recipe = first
            .document_render_recipe("manual-inbox", &config.sources["manual-inbox"], inputs)
            .unwrap()
            .with_decoder(decoder.clone());
        let mut operational = config.clone();
        operational.ingest.schedule = Some("0 1 * * *".into());
        operational.execution.default_profile = Some("different-account".into());
        operational.vault.dirs.daily = "different-daily-output".into();
        operational.sources.get_mut("manual-inbox").unwrap().params["max_entries_per_run"] =
            1.into();
        operational.sources.insert(
            "other-source".into(),
            operational.sources["manual-inbox"].clone(),
        );
        let current = pipeline(root.path(), Some(templates.path()), &operational);
        assert!(
            current
                .document_recipe_current(
                    "manual-inbox",
                    &operational.sources["manual-inbox"],
                    &decoder,
                    &recipe
                )
                .unwrap()
        );
        let mut publication_mode = operational.clone();
        publication_mode.llm.provider = match operational.llm.provider {
            lk_core::config::LlmProvider::Queue => lk_core::config::LlmProvider::Noop,
            lk_core::config::LlmProvider::Noop => lk_core::config::LlmProvider::Queue,
        };
        assert!(
            !pipeline(root.path(), Some(templates.path()), &publication_mode)
                .document_recipe_current(
                    "manual-inbox",
                    &publication_mode.sources["manual-inbox"],
                    &decoder,
                    &recipe
                )
                .unwrap()
        );
        assert!(
            !current
                .document_recipe_current(
                    "manual-inbox",
                    &operational.sources["manual-inbox"],
                    &file_revision("changed decoder"),
                    &recipe
                )
                .unwrap()
        );
        let mut changed = operational.sources["manual-inbox"].clone();
        changed.focus = Some("different scope".into());
        assert!(
            !current
                .document_recipe_current("manual-inbox", &changed, &decoder, &recipe)
                .unwrap()
        );
        changed = operational.sources["manual-inbox"].clone();
        changed.concept_policy = lk_core::config::ConceptPolicy::Disabled;
        assert!(
            !current
                .document_recipe_current("manual-inbox", &changed, &decoder, &recipe)
                .unwrap()
        );
        std::fs::write(templates.path().join("unrelated"), "does not participate").unwrap();
        assert!(
            current
                .document_recipe_current(
                    "manual-inbox",
                    &operational.sources["manual-inbox"],
                    &decoder,
                    &recipe
                )
                .unwrap()
        );
        std::fs::write(templates.path().join("body"), "changed").unwrap();
        assert!(
            !current
                .document_recipe_current(
                    "manual-inbox",
                    &operational.sources["manual-inbox"],
                    &decoder,
                    &recipe
                )
                .unwrap()
        );
    }

    #[tokio::test]
    async fn only_materialized_document_outputs_receive_render_recipes() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let mut pipeline = pipeline(root.path(), None, &config);
        let item = lk_core::event::RawItem {
            external_id: Some("manual:a".into()),
            title: "Materialized document".into(),
            body: "Source body".into(),
            url: None,
            author: None,
            timestamp: "2026-10-20T10:00:00Z".parse().unwrap(),
            is_self: false,
            work: None,
            labels: Vec::new(),
            metadata: serde_json::json!({"source_file": "/inbox/a.md"}),
        };
        let options = IngestOptions {
            target_date: None,
            today: jiff::civil::date(2026, 10, 22),
            dry_run: true,
        };
        let result = pipeline
            .plan(
                "manual-inbox",
                &config.sources["manual-inbox"],
                vec![item],
                &options,
            )
            .await
            .unwrap();
        assert_eq!(result.document_pages.len(), 1);
        assert_eq!(result.document_recipes.len(), 1);
        assert!(
            result
                .document_recipes
                .contains_key(&result.document_pages[0].path.to_string())
        );
        let empty = pipeline
            .plan(
                "manual-inbox",
                &config.sources["manual-inbox"],
                Vec::new(),
                &options,
            )
            .await
            .unwrap();
        assert!(empty.document_pages.is_empty());
        assert!(empty.document_recipes.is_empty());
    }
}
