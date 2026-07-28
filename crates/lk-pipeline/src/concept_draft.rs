use std::collections::BTreeMap;

use lk_core::concept::{ExtractedConcept, identity_key, slugify};
use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_vault::{TemplateEngine, VaultStore, replace_section, section_body};

use crate::PipelineError;
use crate::render::RenderResult;

/// In-memory aggregator for concept page state across multiple dates in a single run.
/// Reads existing vault pages on first encounter, then merges further mentions.
/// The page an extraction resolved to: its established title and slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConceptIdentity {
    pub name: String,
    pub slug: String,
}

pub struct ConceptDrafts {
    drafts: BTreeMap<String, ConceptDraft>,
    /// `identity_key(name)` → the page that owns that name: seeded once from the concept
    /// pages already in the vault, then extended by each new name this run resolves, so a
    /// decision made for one spelling is the answer every other spelling of it gets.
    ///
    /// A concept page's id is NOT always `slugify(title)`: a page renamed or merged keeps
    /// its original id and records the other names as aliases, which is what makes every
    /// existing citation to it keep resolving. Extractions arrive under any of those names,
    /// so resolving by slug alone would mint a second page beside the canonical one
    /// — splitting a concept's citations in two and leaving the synthesis on the old page.
    /// The lookup is EXACT (both sides through `identity_key`), never fuzzy: a name either
    /// IS one this vault already answers to, or it is a new concept.
    alias_index: Option<BTreeMap<String, ConceptIdentity>>,
}

/// One extraction with everything the vault could tell us about it already read: the page
/// identity its name resolves to, and the existing page at that slug if there is one.
/// Produced by [`ConceptDrafts::stage`] (fallible, reads) and consumed by
/// [`ConceptDrafts::commit`] (pure, mutates).
pub struct StagedConcept {
    concept: ExtractedConcept,
    slug: String,
    existing: Option<lk_core::frontmatter::VaultPage>,
    synthesis: Option<String>,
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
    /// `retrieval-augmented-generation`) so every citation addresses the one page.
    /// An ingest re-render that re-emitted only `[title]` would silently erase them and
    /// break every link that relied on the alias — so they are preserved exactly like the
    /// title and category. The title seed is dropped here and re-added first at render.
    preserved_aliases: Vec<String>,
}

impl ConceptDrafts {
    /// Resolve a concept name to the page identity that owns it, building the alias index
    /// from disk on first use. Nothing is staged — the run's accumulator is untouched, so
    /// callers may resolve before deciding to commit.
    ///
    /// Lookup is by `identity_key`, so a name reaches its page however its separators fall
    /// (`VectorDB` finds `vector-db.md`); only when NO page owns the name does it become a
    /// new concept, addressed at its own `slugify` slug — and that decision is recorded in
    /// the index, so it is the answer every later resolve of the name gets.
    ///
    /// Recording it here rather than at commit is what makes resolution self-consistent
    /// within a run, and both callers need that. `Pipeline::plan` renders a page's concept
    /// links from resolutions taken BEFORE any merge, so a decision made only at commit
    /// would leave the link pointing at a page the merge then folded away. And
    /// `apply_concept_result` stages a whole extraction before folding any of it, so two
    /// spellings of one name in a single result would each resolve against the pre-batch
    /// index and mint rival pages. The index is a lookup cache, not the accumulator, so a
    /// caller that abandons a resolution leaves nothing half-folded behind — only the slug
    /// that name would have resolved to anyway.
    pub async fn resolve_identity(
        &mut self,
        name: &str,
        reader: &dyn VaultStore,
        dirs: &VaultDirs,
    ) -> Result<ConceptIdentity, PipelineError> {
        let slug =
            slugify(name).expect("concepts are slug-filtered via has_valid_slug before merge");
        let index = match &mut self.alias_index {
            Some(index) => index,
            slot => slot.insert(build_alias_index(reader, dirs).await?),
        };
        let key = identity_key(&slug).expect("a slug always carries an identity");
        Ok(index
            .entry(key)
            .or_insert_with(|| ConceptIdentity {
                name: name.to_string(),
                slug,
            })
            .clone())
    }

    pub fn new() -> Self {
        Self {
            drafts: BTreeMap::new(),
            alias_index: None,
        }
    }

    /// Read everything a fold needs from the vault, without folding it into the drafts.
    ///
    /// Splitting the read from the fold is what lets a caller with several concepts stage
    /// them all before committing any: the reads are the only fallible part, so a failure on
    /// the third concept cannot leave the first two in the run's drafts. That matters because
    /// [`Self::render_pages`] emits the accumulator unconditionally — a half-folded result
    /// would write concept pages whose origin page was never updated to cite them.
    ///
    /// The drafts are what that protects. Resolving a name does record it in the alias index
    /// (see [`Self::resolve_identity`]), so a `stage` that fails afterwards leaves that entry
    /// behind — deliberately: it holds the slug the name would resolve to on any later
    /// attempt, so the record cannot make a subsequent resolution differ from a fresh one.
    pub async fn stage(
        &mut self,
        concept: &ExtractedConcept,
        synthesis: Option<&str>,
        reader: &dyn VaultStore,
        dirs: &VaultDirs,
    ) -> Result<StagedConcept, PipelineError> {
        let identity = self.resolve_identity(&concept.name, reader, dirs).await?;
        // A slug already staged this run needs no read: the draft in hand is newer than the
        // page on disk, and `commit` folds into it.
        let existing = if self.drafts.contains_key(&identity.slug) {
            None
        } else {
            reader
                .read_page(VaultPath::concept(dirs, &identity.slug).as_ref())
                .await?
        };
        Ok(StagedConcept {
            concept: concept.clone(),
            slug: identity.slug,
            existing,
            synthesis: synthesis.map(str::to_string),
        })
    }

    /// Fold a staged concept into the run's drafts and return the page identity it resolved
    /// to — the established title and slug. Pure and infallible: every read it could need
    /// already happened in [`Self::stage`].
    ///
    /// Callers render links from the RETURNED identity, never from the extraction's own
    /// name: an alias resolves to a different slug than it would produce itself, so
    /// re-deriving one would point the citation at a page that does not exist.
    pub fn commit(&mut self, staged: StagedConcept, date: jiff::civil::Date) -> ConceptIdentity {
        let StagedConcept {
            concept,
            slug: safe_slug,
            existing,
            synthesis,
        } = staged;
        let synthesis = synthesis.as_deref();

        if let Some(draft) = self.drafts.get_mut(&safe_slug) {
            draft.observe(date);
            draft.seed_synthesis(synthesis);
            warn_category_conflict(
                &safe_slug,
                draft.category.as_deref(),
                concept.category.as_deref(),
            );
            if draft.category.is_none() {
                draft.category = concept.category.clone();
            }
            return ConceptIdentity {
                name: draft.name.clone(),
                slug: safe_slug,
            };
        }

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
        draft.seed_synthesis(synthesis);
        let identity = ConceptIdentity {
            name: draft.name.clone(),
            slug: safe_slug.clone(),
        };
        self.drafts.insert(safe_slug, draft);
        identity
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
    /// re-deriving it exactly from the link graph.
    fn observe(&mut self, date: jiff::civil::Date) {
        self.first_seen = self.first_seen.min(date);
        self.last_seen = self.last_seen.max(date);
    }

    /// Fill `## Synthesis` from a grounding sentence, but only when there is nothing there
    /// yet — an established synthesis is accumulated meaning across every source that cited
    /// the concept, so one new mention never overwrites it.
    ///
    /// Applied on every merge, not just the one that stages the draft: two results in a run
    /// can name the same new concept and only one of them carry a grounding. Seeding solely
    /// on the staging merge would leave the created page's synthesis empty or filled
    /// depending on which result the run happened to read first.
    fn seed_synthesis(&mut self, synthesis: Option<&str>) {
        if self.preserved_synthesis.is_none()
            && let Some(text) = synthesis.map(str::trim).filter(|t| !t.is_empty())
        {
            self.preserved_synthesis = Some(text.to_string());
        }
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

/// Map every name a concept page answers to — its own slug, its title, and each alias —
/// to that page's identity, so an extraction naming any of them lands on the established
/// page. Keyed by `lk_core::concept::identity_key`, the same rule `lore graph lint` uses to
/// report two pages owning one name: what routes here and what the lint calls a duplicate
/// cannot drift apart.
async fn build_alias_index(
    reader: &dyn VaultStore,
    dirs: &VaultDirs,
) -> Result<BTreeMap<String, ConceptIdentity>, PipelineError> {
    let dir = lk_core::vault_path::concepts_dir(dirs);
    let mut index: BTreeMap<String, ConceptIdentity> = BTreeMap::new();
    for path in reader.list_markdown(&dir).await? {
        let Some(page) = reader.read_page(&path).await? else {
            continue;
        };
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let title = page
            .frontmatter
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(slug)
            .to_string();
        let names = page
            .frontmatter
            .get("title")
            .and_then(|v| v.as_str())
            .into_iter()
            .chain(
                page.frontmatter
                    .get("aliases")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str()),
            );
        let identity = ConceptIdentity {
            name: title.clone(),
            slug: slug.to_string(),
        };
        // A page owns its own address unconditionally, whatever order the pages are read
        // in — so a stale alias elsewhere can never redirect a concept away from its own
        // page. Seeding the stem is what makes that hold for EVERY page: deriving the
        // claim from the names alone protects only pages whose title or an alias happens
        // to reproduce the stem, and a page titled more descriptively than its file
        // (`access-ingress-2axis-model` ← "Access × Ingress 2-Axis Deployment Model") has
        // no name that does — leaving its address free for another page's alias to take.
        // A stem that carries no identity at all has no address to seed; its names still
        // register, so the page keeps answering to them.
        if let Some(own_key) = identity_key(slug) {
            // Two pages whose ADDRESSES claim one identity is the same vault defect the
            // duplicate lint reports, and the router has to pick one. It does so
            // deterministically (last read wins) but arbitrarily, so say which — silence
            // here is what would make a mis-addressed citation impossible to explain.
            // Only when the holder claims the key as its own ADDRESS: a page displaced
            // because it merely aliased this name is the seed guarantee working, and the
            // mirror of that case is deliberately silent a few lines below.
            if let Some(held) = index.get(&own_key)
                && held.slug != slug
                && identity_key(&held.slug).as_deref() == Some(own_key.as_str())
            {
                tracing::warn!(
                    identity = %own_key,
                    now_resolves_to = %slug,
                    displaced = %held.slug,
                    "two concept pages are addressed by the same name"
                );
            }
            index.insert(own_key, identity.clone());
        }
        for name in names {
            let Some(key) = identity_key(name) else {
                continue;
            };
            match index.entry(key) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(identity.clone());
                }
                std::collections::btree_map::Entry::Occupied(held) => {
                    // Two pages registering the same alias is a vault defect, not something
                    // to settle silently. The winner is deterministic (pages are read in
                    // sorted path order) but arbitrary, and every citation naming the alias
                    // lands on one page while the other's meaning stays uncited — so it is
                    // reported like a category conflict and left for a human to merge.
                    // Losing to a page that holds the key as its own ADDRESS is the seed
                    // above working, not a conflict.
                    let held_claims_it_by_name =
                        identity_key(&held.get().slug).as_deref() != Some(held.key());
                    if held.get().slug != slug && held_claims_it_by_name {
                        tracing::warn!(
                            name = %held.key(),
                            resolves_to = %held.get().slug,
                            also_claimed_by = %slug,
                            "two concept pages claim the same name"
                        );
                    }
                }
            }
        }
    }
    Ok(index)
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
            preserved_sources: Some(
                "- [d1](../../daily/x/2026-05-01.md)\n- [d2](../../daily/x/2026-05-02.md)".into(),
            ),
            preserved_related: Some("- [Vector Search](vector-search.md)".into()),
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
            page.content.contains("- [d2](../../daily/x/2026-05-02.md)"),
            "sources body must survive re-render:\n{}",
            page.content
        );
        assert!(
            page.content.contains("- [Vector Search](vector-search.md)"),
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
        // A synonym registered by a human or `/lore-wiki audit` (so the concept registry
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
