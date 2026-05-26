use std::collections::BTreeMap;

use lk_core::concept::{ExtractedConcept, slugify};
use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_vault::{TemplateEngine, VaultReader};

use crate::PipelineError;
use crate::render::RenderOutput;

/// Identifies where a concept was mentioned — the vault page path and the date
/// it was observed. Bundled so callers always pass the pair together.
pub struct ConceptSource {
    pub ref_path: String,
    pub date: jiff::civil::Date,
}

/// In-memory aggregator for concept page state across multiple dates in a single run.
/// Reads existing vault pages on first encounter, then merges further mentions.
pub struct ConceptDrafts {
    drafts: BTreeMap<String, ConceptDraft>,
}

struct ConceptDraft {
    slug: String,
    name: String,
    category: Option<String>,
    first_seen: jiff::civil::Date,
    last_seen: jiff::civil::Date,
    source_count: u64,
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
        source: &ConceptSource,
        reader: &VaultReader,
        dirs: &VaultDirs,
    ) -> Result<(), PipelineError> {
        let Some(safe_slug) = canonical_slug(&concept.slug, &concept.name) else {
            tracing::warn!(name = %concept.name, "skipping concept with empty slug");
            return Ok(());
        };

        let source_ref = source.ref_path.clone();
        let date = source.date;

        if let Some(draft) = self.drafts.get_mut(&safe_slug) {
            draft.add_reference(source_ref, date);
            if draft.category.is_none() {
                draft.category = concept.category.clone();
            }
            return Ok(());
        }

        let path = VaultPath::concept(dirs, &safe_slug);
        let existing = reader.read_page(path.as_ref()).await?;

        let mut draft = match existing.as_ref() {
            Some(page) => {
                let source_count = page
                    .frontmatter
                    .get("source_count")
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
                let category = page
                    .frontmatter
                    .get("category")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .or_else(|| concept.category.clone());

                ConceptDraft {
                    slug: safe_slug.clone(),
                    name,
                    category,
                    first_seen,
                    last_seen,
                    source_count,
                    sources,
                }
            }
            None => ConceptDraft {
                slug: safe_slug.clone(),
                name: concept.name.clone(),
                category: concept.category.clone(),
                first_seen: date,
                last_seen: date,
                source_count: 0,
                sources: vec![],
            },
        };

        draft.add_reference(source_ref, date);
        self.drafts.insert(safe_slug, draft);
        Ok(())
    }

    pub fn known_slugs_and_names(&self) -> Vec<(String, String)> {
        self.drafts
            .values()
            .map(|d| (d.slug.clone(), d.name.clone()))
            .collect()
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
            self.source_count += 1;
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
            "category": self.category.as_deref().unwrap_or(""),
            "first_seen": self.first_seen.to_string(),
            "last_seen": self.last_seen.to_string(),
            "source_count": self.source_count,
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

pub(crate) fn strip_md_extension(s: &str) -> String {
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
            category: Some("ai-ml".into()),
            first_seen: jiff::civil::date(2026, 5, 1),
            last_seen: jiff::civil::date(2026, 5, 1),
            source_count: 1,
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
        assert!(
            page.content.contains("category: ai-ml"),
            "category should appear in frontmatter:\n{}",
            page.content
        );
    }

    #[test]
    fn category_omitted_when_none() {
        let draft = ConceptDraft {
            slug: "test".into(),
            name: "Test".into(),
            category: None,
            first_seen: jiff::civil::date(2026, 5, 1),
            last_seen: jiff::civil::date(2026, 5, 1),
            source_count: 0,
            sources: vec![],
        };
        let engine = TemplateEngine::new(None).unwrap();
        let page = draft
            .render(&engine, &VaultDirs::default(), Locale::Ko)
            .unwrap();
        assert!(
            !page.content.contains("category"),
            "category field must be absent when None:\n{}",
            page.content
        );
        assert!(
            page.content.contains("source_count: 0"),
            "source_count must still render correctly:\n{}",
            page.content
        );
    }
}
