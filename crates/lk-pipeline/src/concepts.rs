use std::collections::BTreeMap;

use lk_core::concept::{ExtractedConcept, slugify};
use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_vault::{TemplateEngine, VaultReader};

use crate::PipelineError;
use crate::render::RenderOutput;

/// In-memory aggregator for concept page state across multiple dates in a single run.
/// Reads existing vault pages on first encounter, then merges further mentions.
pub struct ConceptDrafts {
    drafts: BTreeMap<String, ConceptDraft>,
}

struct ConceptDraft {
    slug: String,
    name: String,
    first_seen: jiff::civil::Date,
    last_seen: jiff::civil::Date,
    reference_count: u64,
    sources: Vec<String>,
}

impl ConceptDrafts {
    pub fn new() -> Self {
        Self {
            drafts: BTreeMap::new(),
        }
    }

    pub async fn merge(
        &mut self,
        concept: &ExtractedConcept,
        source_id: &str,
        date: jiff::civil::Date,
        reader: &VaultReader,
        dirs: &VaultDirs,
    ) -> Result<(), PipelineError> {
        let Some(safe_slug) = canonical_slug(&concept.slug, &concept.name) else {
            tracing::warn!(name = %concept.name, "skipping concept with empty slug");
            return Ok(());
        };

        let source_ref = strip_md_extension(&VaultPath::daily(dirs, source_id, date).to_string());

        if let Some(draft) = self.drafts.get_mut(&safe_slug) {
            draft.add_reference(source_ref, date);
            return Ok(());
        }

        let path = VaultPath::concept(dirs, &safe_slug);
        let existing = reader.read_page(path.as_ref()).await?;

        let mut draft = match existing.as_ref() {
            Some(page) => {
                let reference_count = page
                    .frontmatter
                    .get("reference_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let sources: Vec<String> = page
                    .frontmatter
                    .get("sources")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                // The persisted page stores these as `created`/`updated` (the keys the
                // template and fallback write). Reading `first_seen`/`last_seen` would
                // always miss and reset the origin date to today on every re-ingest.
                let first_seen = page
                    .frontmatter
                    .get("created")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<jiff::civil::Date>().ok())
                    .unwrap_or(date);
                let last_seen = page
                    .frontmatter
                    .get("updated")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<jiff::civil::Date>().ok())
                    .unwrap_or(date);
                // Preserve the established page identity: keep the existing title rather
                // than letting the newest extraction's casing/spelling overwrite it.
                let name = page
                    .frontmatter
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| concept.name.clone());

                ConceptDraft {
                    slug: safe_slug.clone(),
                    name,
                    first_seen,
                    last_seen,
                    reference_count,
                    sources,
                }
            }
            None => ConceptDraft {
                slug: safe_slug.clone(),
                name: concept.name.clone(),
                first_seen: date,
                last_seen: date,
                reference_count: 0,
                sources: vec![],
            },
        };

        draft.add_reference(source_ref, date);
        self.drafts.insert(safe_slug, draft);
        Ok(())
    }

    pub fn render_pages(
        &self,
        engine: &TemplateEngine,
        dirs: &VaultDirs,
        locale: Locale,
    ) -> Result<Vec<RenderOutput>, PipelineError> {
        self.drafts
            .values()
            .map(|d| d.render(engine, dirs, locale))
            .collect()
    }
}

impl Default for ConceptDrafts {
    fn default() -> Self {
        Self::new()
    }
}

impl ConceptDraft {
    fn add_reference(&mut self, source_ref: String, date: jiff::civil::Date) {
        if !self.sources.contains(&source_ref) {
            self.sources.push(source_ref);
            self.reference_count += 1;
        }
        self.first_seen = self.first_seen.min(date);
        self.last_seen = self.last_seen.max(date);
    }

    fn render(
        &self,
        engine: &TemplateEngine,
        dirs: &VaultDirs,
        locale: Locale,
    ) -> Result<RenderOutput, PipelineError> {
        let path = VaultPath::concept(dirs, &self.slug);

        let context = serde_json::json!({
            "slug": self.slug,
            "name": self.name,
            "first_seen": self.first_seen.to_string(),
            "last_seen": self.last_seen.to_string(),
            "reference_count": self.reference_count,
            "sources": self.sources,
            "tags": ["concept"],
            "i18n": locale.strings(),
        });

        // concept.md.jinja is embedded, so it always resolves.
        let content = engine
            .render("concept.md.jinja", &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?;

        Ok(RenderOutput { path, content })
    }
}

pub(crate) fn canonical_slug(provided: &str, name: &str) -> Option<String> {
    slugify(provided).or_else(|| slugify(name))
}

/// Filter that callers use to drop concepts whose slug would be empty before threading
/// them into rendered output. Keeps daily-page wiki links honest.
pub fn is_valid(concept: &ExtractedConcept) -> bool {
    canonical_slug(&concept.slug, &concept.name).is_some()
}

fn strip_md_extension(s: &str) -> String {
    s.strip_suffix(".md").unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_with_slashes_is_normalized() {
        assert_eq!(canonical_slug("foo/bar", "Foo Bar"), Some("foo-bar".into()));
        assert_eq!(canonical_slug("..", "Up Up"), Some("up-up".into()));
        assert_eq!(
            canonical_slug("", "Hello World"),
            Some("hello-world".into())
        );
    }

    #[test]
    fn rendered_frontmatter_escapes_quotes_in_name() {
        let draft = ConceptDraft {
            slug: "rag".into(),
            name: r#"RAG: "Retrieval" Augmented"#.into(),
            first_seen: jiff::civil::date(2026, 5, 1),
            last_seen: jiff::civil::date(2026, 5, 1),
            reference_count: 1,
            sources: vec!["daily/x/2026-05-01".into()],
        };
        let engine = TemplateEngine::new(None).unwrap();
        let page = draft
            .render(&engine, &VaultDirs::default(), Locale::Ko)
            .unwrap();
        // `| tojson` escapes inner quotes so the YAML title stays valid, rather than raw
        // `title: "RAG: "Retrieval"..."` which would break parsing.
        assert!(
            page.content
                .contains(r#"title: "RAG: \"Retrieval\" Augmented""#),
            "title not properly escaped:\n{}",
            page.content
        );
    }
}
