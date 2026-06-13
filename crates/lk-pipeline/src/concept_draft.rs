use std::collections::BTreeMap;

use lk_core::concept::{ExtractedConcept, slugify};
use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_vault::{TemplateEngine, VaultStore, replace_section, section_body};

use crate::PipelineError;
use crate::render::RenderResult;

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
    /// Last `source_count` written to the page, preserved verbatim across this
    /// ingest re-render. `lore graph backlinks-sync` is the sole *computer* of the
    /// citation count; ingest must not reset it to 0 (that would blank an
    /// established count until the next sync), so it carries the on-disk value
    /// through unchanged. A brand-new page starts at 0.
    source_count: u64,
    /// Bodies of LLM-authored or graph-maintained sections, captured from the
    /// existing concept page so a re-render can splice them back. Concept pages
    /// have no `llm_inputs` hash because their semantic content is monotonically
    /// additive — the skill writes `## Synthesis` on creation, `lore graph
    /// backlinks-sync` derives `## Sources` from real incoming citations, and `## Related`
    /// is curated via `lore-wiki audit` (community-grounded, LLM-confirmed links).
    /// None of those should ever be wiped by an ingest re-render.
    preserved_synthesis: Option<String>,
    preserved_sources: Option<String>,
    preserved_related: Option<String>,
    /// Extra `aliases` (beyond the page title itself) carried verbatim from the existing
    /// page. Aliases are established identity, not regenerated content: a human or
    /// `/lore-wiki audit` registers a synonym/abbreviation (e.g. `RAG` →
    /// `retrieval-augmented-generation`) so a bare `[[RAG]]` resolves to the one page.
    /// An ingest re-render that re-emitted only `[title]` would silently erase them and
    /// break every link that relied on the alias — so they are preserved exactly like the
    /// title and category. The title seed is dropped here and re-added first at render.
    preserved_aliases: Vec<String>,
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
        date: jiff::civil::Date,
        reader: &dyn VaultStore,
        dirs: &VaultDirs,
    ) -> Result<(), PipelineError> {
        let safe_slug = slugify(&concept.name)
            .expect("concepts are slug-filtered via has_valid_slug before merge");

        if let Some(draft) = self.drafts.get_mut(&safe_slug) {
            draft.observe(date);
            warn_category_conflict(
                &safe_slug,
                draft.category.as_deref(),
                concept.category.as_deref(),
            );
            if draft.category.is_none() {
                draft.category = concept.category.clone();
            }
            return Ok(());
        }

        let path = VaultPath::concept(dirs, &safe_slug);
        let existing = reader.read_page(path.as_ref()).await?;

        let mut draft = match existing.as_ref() {
            Some(page) => {
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
                let existing_category = page
                    .frontmatter
                    .get("category")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                warn_category_conflict(
                    &safe_slug,
                    existing_category.as_deref(),
                    concept.category.as_deref(),
                );
                let category = existing_category.or_else(|| concept.category.clone());
                let source_count = page.frontmatter.source_count().unwrap_or(0);
                // Keep every alias except the title seed (`render` re-adds the title first),
                // so a synonym a human/audit registered survives this re-render.
                let preserved_aliases = page
                    .frontmatter
                    .get("aliases")
                    .and_then(|v| v.as_array())
                    .map(|seq| {
                        seq.iter()
                            .filter_map(|x| x.as_str())
                            .filter(|a| *a != name.as_str())
                            .map(String::from)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                ConceptDraft {
                    slug: safe_slug.clone(),
                    name,
                    category,
                    first_seen,
                    last_seen,
                    source_count,
                    preserved_synthesis: capture_section(&page.body, |s| s.concept_synthesis),
                    preserved_sources: capture_section(&page.body, |s| s.concept_sources),
                    preserved_related: capture_section(&page.body, |s| s.related),
                    preserved_aliases,
                }
            }
            None => ConceptDraft {
                slug: safe_slug.clone(),
                name: concept.name.clone(),
                category: concept.category.clone(),
                first_seen: date,
                last_seen: date,
                source_count: 0,
                preserved_synthesis: None,
                preserved_sources: None,
                preserved_related: None,
                preserved_aliases: Vec::new(),
            },
        };

        draft.observe(date);
        self.drafts.insert(safe_slug, draft);
        Ok(())
    }

    pub fn render_pages(
        &self,
        engine: &TemplateEngine,
        dirs: &VaultDirs,
        locale: Locale,
    ) -> Result<Vec<RenderResult>, PipelineError> {
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
    /// Widen the observed [first_seen, last_seen] window. Citation counting is not
    /// done here — `lore graph backlinks-sync` is the sole owner of `source_count`,
    /// re-deriving it exactly from the wikilink graph.
    fn observe(&mut self, date: jiff::civil::Date) {
        self.first_seen = self.first_seen.min(date);
        self.last_seen = self.last_seen.max(date);
    }

    fn render(
        &self,
        engine: &TemplateEngine,
        dirs: &VaultDirs,
        locale: Locale,
    ) -> Result<RenderResult, PipelineError> {
        let path = VaultPath::concept(dirs, &self.slug);
        let strings = locale.strings();

        // The title is always the first alias (Obsidian convention); preserved synonyms
        // follow, deduped. Single list, so the template never hardcodes `[name]` and a
        // re-render can't drop a registered alias.
        let mut aliases = vec![self.name.clone()];
        for a in &self.preserved_aliases {
            if !aliases.contains(a) {
                aliases.push(a.clone());
            }
        }

        let context = serde_json::json!({
            "slug": self.slug,
            "name": self.name,
            "aliases": aliases,
            "category": self.category.as_deref().unwrap_or(""),
            "first_seen": self.first_seen.to_string(),
            "last_seen": self.last_seen.to_string(),
            // Preserved verbatim — backlinks-sync owns the real count; ingest never
            // recomputes or resets it (new pages start at 0).
            "source_count": self.source_count,
            // Tag with the category id when set, else the literal "concept" — the same
            // invariant `/lore-process` writes, so every concept page carries at least
            // one tag for Obsidian filtering.
            "tags": match self.category.as_deref().filter(|c| !c.is_empty()) {
                Some(cat) => vec![cat],
                None => vec!["concept"],
            },
            "i18n": strings,
        });

        // concept.md.jinja is embedded, so it always resolves.
        let mut content = engine
            .render("concept.md.jinja", &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?;

        // Splice the previously-captured bodies back into the freshly rendered
        // page. These sections are owned by other writers (`/lore-process` for
        // synthesis, `lore graph backlinks-sync` for `## Sources`, `lore-wiki audit`
        // for `## Related`) and re-rendering must NEVER wipe them.
        for (heading, body) in [
            (strings.concept_synthesis, &self.preserved_synthesis),
            (strings.concept_sources, &self.preserved_sources),
            (strings.related, &self.preserved_related),
        ] {
            if let Some(body) = body {
                content = replace_section(&content, heading, body);
            }
        }

        Ok(RenderResult { path, content })
    }
}

/// Capture the body of a logical concept section from an existing page so a
/// re-render can splice it back in. `section` selects the heading from a locale's
/// `Strings` (e.g. `|s| s.concept_synthesis`); the page is searched under EVERY
/// locale's heading for that section, so a page authored before a `vault.locale`
/// switch is still found — body content is never translated (i18n invariant), only
/// the structural heading changes. Returns the body verbatim (trimmed of section
/// framing newlines) for the first locale heading that yields non-empty content.
fn capture_section(
    body: &str,
    section: fn(&lk_core::i18n::Strings) -> &'static str,
) -> Option<String> {
    let mut tried: Vec<&str> = Vec::new();
    for locale in Locale::ALL {
        let heading = section(locale.strings());
        if tried.contains(&heading) {
            continue;
        }
        tried.push(heading);
        if let Some(raw) = section_body(body, heading) {
            let trimmed = raw.trim_matches('\n');
            if !trimmed.trim().is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Surface a genuine category conflict — an established category that a fresh
/// extraction disagrees with. Identity is first-writer (the established one is kept),
/// but a silent divergence would calcify a possibly-wrong assignment, so make it
/// observable. Fires only when both sides are present and differ. Used for BOTH the
/// in-memory-draft and on-disk merge paths so a same-run conflict isn't missed.
fn warn_category_conflict(slug: &str, established: Option<&str>, incoming: Option<&str>) {
    if let (Some(established), Some(incoming)) = (established, incoming)
        && established != incoming
    {
        tracing::warn!(
            concept = %slug,
            established = %established,
            extracted = %incoming,
            "concept category conflict; keeping established category"
        );
    }
}

/// Filter that callers use to drop concepts whose slug would be empty before threading
/// them into rendered output. Keeps daily-page wiki links honest.
pub fn has_valid_slug(concept: &ExtractedConcept) -> bool {
    slugify(&concept.name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_section_finds_body_under_any_locale_heading() {
        // A page authored under Ko has `## 핵심`. After a locale switch to En the
        // capture must still find it (searched across all locale headings), so the
        // LLM-authored body is preserved rather than silently wiped.
        let ko_page = "# RAG\n\n## 핵심\n\nKorean-authored synthesis body.\n\n## 출처\n";
        let captured = capture_section(ko_page, |s| s.concept_synthesis);
        assert_eq!(
            captured.as_deref(),
            Some("Korean-authored synthesis body."),
            "synthesis body authored under Ko must be found regardless of current locale"
        );

        // And the En heading on an En-authored page is found too.
        let en_page = "# RAG\n\n## Synthesis\n\nEnglish body.\n\n## Sources\n";
        assert_eq!(
            capture_section(en_page, |s| s.concept_synthesis).as_deref(),
            Some("English body.")
        );

        // Empty section → None (so a re-render doesn't splice a blank body).
        let empty = "# RAG\n\n## 핵심\n\n\n## 출처\n";
        assert!(capture_section(empty, |s| s.concept_synthesis).is_none());
    }

    #[test]
    fn rendered_frontmatter_escapes_quotes_in_name() {
        let draft = ConceptDraft {
            slug: "rag".into(),
            name: r#"RAG: "Retrieval" Augmented"#.into(),
            category: Some("ai-ml".into()),
            first_seen: jiff::civil::date(2026, 5, 1),
            last_seen: jiff::civil::date(2026, 5, 1),
            source_count: 0,
            preserved_synthesis: None,
            preserved_sources: None,
            preserved_related: None,
            preserved_aliases: Vec::new(),
        };
        let engine = TemplateEngine::build(None).unwrap();
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
            page.content.contains(r#"category: "ai-ml""#),
            "category should appear in frontmatter as a JSON-quoted string:\n{}",
            page.content
        );
    }

    #[test]
    fn preserved_sections_are_spliced_back_into_rendered_page() {
        let draft = ConceptDraft {
            slug: "rag".into(),
            name: "RAG".into(),
            category: Some("ai-ml".into()),
            first_seen: jiff::civil::date(2026, 5, 1),
            last_seen: jiff::civil::date(2026, 5, 1),
            source_count: 3,
            preserved_synthesis: Some(
                "Retrieval-Augmented Generation enriches an LLM prompt with retrieved context."
                    .into(),
            ),
            preserved_sources: Some("- [[daily/x/2026-05-01]]\n- [[daily/x/2026-05-02]]".into()),
            preserved_related: Some("- [[vector-search]]".into()),
            preserved_aliases: Vec::new(),
        };
        let engine = TemplateEngine::build(None).unwrap();
        let page = draft
            .render(&engine, &VaultDirs::default(), Locale::Ko)
            .unwrap();
        assert!(
            page.content
                .contains("Retrieval-Augmented Generation enriches an LLM prompt"),
            "synthesis body must survive re-render:\n{}",
            page.content
        );
        assert!(
            page.content.contains("- [[daily/x/2026-05-02]]"),
            "sources body must survive re-render:\n{}",
            page.content
        );
        assert!(
            page.content.contains("- [[vector-search]]"),
            "related body must survive re-render:\n{}",
            page.content
        );
        assert!(
            page.content.contains("source_count: 3"),
            "an established source_count must survive ingest re-render, not reset to 0 \
             (backlinks-sync owns the value):\n{}",
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
            preserved_synthesis: None,
            preserved_sources: None,
            preserved_related: None,
            preserved_aliases: Vec::new(),
        };
        let engine = TemplateEngine::build(None).unwrap();
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

    #[test]
    fn preserved_aliases_survive_render() {
        // A synonym registered by a human or `/lore-wiki audit` (so a bare `[[RAG]]`
        // resolves to the canonical page) must NOT be wiped when a later ingest re-renders
        // the concept. The title is always the first alias; preserved synonyms follow.
        let draft = ConceptDraft {
            slug: "retrieval-augmented-generation".into(),
            name: "Retrieval Augmented Generation".into(),
            category: None,
            first_seen: jiff::civil::date(2026, 5, 1),
            last_seen: jiff::civil::date(2026, 5, 1),
            source_count: 0,
            preserved_synthesis: None,
            preserved_sources: None,
            preserved_related: None,
            preserved_aliases: vec!["RAG".into()],
        };
        let engine = TemplateEngine::build(None).unwrap();
        let page = draft
            .render(&engine, &VaultDirs::default(), Locale::Ko)
            .unwrap();
        assert!(
            page.content
                .contains(r#"aliases: ["Retrieval Augmented Generation","RAG"]"#),
            "registered alias must survive the re-render (title first, then synonyms):\n{}",
            page.content
        );
    }
}
