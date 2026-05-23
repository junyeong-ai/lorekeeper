use std::collections::BTreeMap;

use wi_core::concept::{Confidence, ExtractedConcept, slugify};
use wi_core::config::VaultDirs;
use wi_core::vault_path::VaultPath;
use wi_vault::{TemplateEngine, VaultReader};

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
    confidence: Confidence,
    first_seen: jiff::civil::Date,
    last_seen: jiff::civil::Date,
    mention_count: u64,
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
        let safe_slug = canonical_slug(&concept.slug, &concept.name);
        if safe_slug.is_empty() {
            tracing::warn!(name = %concept.name, "skipping concept with empty slug");
            return Ok(());
        }

        let source_ref = strip_md_extension(&VaultPath::daily(dirs, source_id, date).to_string());

        if let Some(draft) = self.drafts.get_mut(&safe_slug) {
            draft.add_mention(source_ref, date);
            return Ok(());
        }

        let path = VaultPath::concept(dirs, &safe_slug);
        let existing = reader.read_page(path.as_ref()).await?;

        let mut draft = match existing.as_ref() {
            Some(page) => {
                let mention_count = page
                    .frontmatter
                    .get("mention_count")
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
                let first_seen = page
                    .frontmatter
                    .get("first_seen")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<jiff::civil::Date>().ok())
                    .unwrap_or(date);
                let last_seen = page
                    .frontmatter
                    .get("last_seen")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<jiff::civil::Date>().ok())
                    .unwrap_or(date);

                ConceptDraft {
                    slug: safe_slug.clone(),
                    name: concept.name.clone(),
                    confidence: concept.confidence,
                    first_seen,
                    last_seen,
                    mention_count,
                    sources,
                }
            }
            None => ConceptDraft {
                slug: safe_slug.clone(),
                name: concept.name.clone(),
                confidence: concept.confidence,
                first_seen: date,
                last_seen: date,
                mention_count: 0,
                sources: vec![],
            },
        };

        draft.add_mention(source_ref, date);
        self.drafts.insert(safe_slug, draft);
        Ok(())
    }

    pub fn render(
        &self,
        engine: &TemplateEngine,
        dirs: &VaultDirs,
    ) -> Result<Vec<RenderOutput>, PipelineError> {
        self.drafts
            .values()
            .map(|d| d.render(engine, dirs))
            .collect()
    }
}

impl Default for ConceptDrafts {
    fn default() -> Self {
        Self::new()
    }
}

impl ConceptDraft {
    fn add_mention(&mut self, source_ref: String, date: jiff::civil::Date) {
        if !self.sources.contains(&source_ref) {
            self.sources.push(source_ref);
            self.mention_count += 1;
        }
        self.first_seen = self.first_seen.min(date);
        self.last_seen = self.last_seen.max(date);
    }

    fn render(
        &self,
        engine: &TemplateEngine,
        dirs: &VaultDirs,
    ) -> Result<RenderOutput, PipelineError> {
        let path = VaultPath::concept(dirs, &self.slug);

        let context = serde_json::json!({
            "slug": self.slug,
            "name": self.name,
            "confidence": self.confidence.to_string(),
            "first_seen": self.first_seen.to_string(),
            "last_seen": self.last_seen.to_string(),
            "mention_count": self.mention_count,
            "sources": self.sources,
            "tags": ["concept"],
        });

        let content = if engine.available("concept.md.jinja") {
            engine
                .render("concept.md.jinja", &context)
                .map_err(|e| PipelineError::Render(e.to_string()))?
        } else {
            self.fallback(&context)
        };

        Ok(RenderOutput { path, content })
    }

    fn fallback(&self, ctx: &serde_json::Value) -> String {
        let sources_json = serde_json::to_string(&ctx["sources"]).unwrap_or_else(|_| "[]".into());
        format!(
            "---\nid: {}\ntitle: \"{}\"\ncreated: {}\nupdated: {}\nconfidence: {}\nmention_count: {}\nsources: {}\ntags: [\"concept\"]\n---\n\n# {}\n",
            self.slug,
            self.name,
            self.first_seen,
            self.last_seen,
            self.confidence,
            self.mention_count,
            sources_json,
            self.name,
        )
    }
}

pub(crate) fn canonical_slug(provided: &str, name: &str) -> String {
    let normalized = slugify(provided);
    if normalized.is_empty() {
        slugify(name)
    } else {
        normalized
    }
}

/// Filter that callers use to drop concepts whose slug would be empty before threading
/// them into rendered output. Keeps daily-page wiki links honest.
pub fn is_valid(concept: &ExtractedConcept) -> bool {
    !canonical_slug(&concept.slug, &concept.name).is_empty()
}

fn strip_md_extension(s: &str) -> String {
    s.strip_suffix(".md").unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_with_slashes_is_normalized() {
        assert_eq!(canonical_slug("foo/bar", "Foo Bar"), "foobar");
        assert_eq!(canonical_slug("..", "Up Up"), "up-up");
        assert_eq!(canonical_slug("", "Hello World"), "hello-world");
    }
}
