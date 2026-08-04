use std::collections::BTreeMap;

use lk_core::concept::{ConceptIdentity, ConceptRegistry, ExtractedConcept, Resolution, slugify};
use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_vault::{TemplateEngine, VaultStore, replace_section};

use crate::PipelineError;
use crate::render::RenderResult;

/// In-memory aggregator for concept page state across multiple dates in a single run.
/// Reads existing vault pages on first encounter, then merges further mentions.
pub struct ConceptDrafts {
    drafts: BTreeMap<String, ConceptDraft>,
    /// Every name the vault's concept pages answer to: read once from disk, then extended
    /// by each new name this run resolves, so a decision made for one spelling is the answer
    /// every other spelling of it gets.
    ///
    /// A concept page's id is NOT always `slugify(title)`: a page renamed or merged keeps
    /// its original id and records the other names as aliases, which is what makes every
    /// existing citation to it keep resolving. Extractions arrive under any of those names,
    /// so resolving by slug alone would mint a second page beside the canonical one
    /// — splitting a concept's citations in two and leaving the synthesis on the old page.
    registry: Option<ConceptRegistry>,
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
    /// The page's `llm_inputs` markers, carried verbatim. A concept's synthesis is owed
    /// against the set of pages citing it, which only `lore graph backlinks-sync` derives —
    /// so this render has no way to compute the input and no standing to judge the marker.
    /// Re-emitting both unchanged is what keeps an answered section answered and a section
    /// awaiting a drain still awaiting it; dropping them would re-enqueue every concept the
    /// run touched and, once answered again, freeze the page against an input nothing
    /// recorded.
    preserved_llm_inputs: BTreeMap<String, String>,
    /// Bodies of LLM-authored or graph-maintained sections, captured from the existing
    /// concept page so a re-render can splice them back. Each has a writer of its own —
    /// `/lore-process` for `## Synthesis`, `lore graph backlinks-sync` for `## Sources`,
    /// `/lore-wiki audit` for `## Related` — and none should ever be wiped by an ingest
    /// re-render.
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
    /// from disk on first use. The run's accumulator is untouched, so a caller may resolve
    /// before deciding whether to fold anything.
    ///
    /// Lookup is by `identity_key`, so a name reaches its page however its separators fall
    /// (`VectorDB` finds `vector-db.md`); only when NO page owns the name does it become a
    /// new concept, addressed at its own `slugify` slug — and that decision is recorded in
    /// the index, so it is the answer every later resolve of the name gets.
    ///
    /// Recording it here rather than at commit is what makes resolution self-consistent
    /// within a run, and both callers need that. `Pipeline::plan` renders a page's concept
    /// links from resolutions taken BEFORE anything is folded, so a decision made only at
    /// commit would leave the link pointing at a page the fold then absorbed. And
    /// `apply_concept_result` stages a whole extraction before folding any of it, so two
    /// spellings of one name in a single result would each resolve against the pre-batch
    /// index and mint rival pages. The index is a lookup cache, not the accumulator, so a
    /// caller that abandons a resolution leaves nothing half-folded behind — only the
    /// decision that the FIRST spelling seen is the one this run answers with.
    pub async fn resolve_identity(
        &mut self,
        name: &str,
        reader: &dyn VaultStore,
        dirs: &VaultDirs,
    ) -> Result<ConceptIdentity, PipelineError> {
        let slug =
            slugify(name).expect("concepts are slug-filtered via has_valid_slug before staging");
        let registry = match &mut self.registry {
            Some(registry) => registry,
            slot => slot.insert(build_registry(reader, dirs).await?),
        };
        if let Resolution::Ambiguous { routed, claimants } = registry.resolve(name) {
            // The vault defect `lore graph lint` reports as a duplicate concept. Routing has
            // to pick one, and it does so deterministically — but silence here is what would
            // make a mis-addressed citation impossible to explain afterwards.
            tracing::warn!(
                name,
                resolves_to = %routed.slug,
                claimants = %claimants.iter().map(|c| c.slug.as_str()).collect::<Vec<_>>().join(", "),
                "more than one concept page answers to this name"
            );
        }
        Ok(registry.resolve_or_claim(
            name,
            ConceptIdentity {
                slug,
                title: name.to_string(),
            },
        ))
    }

    pub fn new() -> Self {
        Self {
            drafts: BTreeMap::new(),
            registry: None,
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
    /// behind. Deliberately, but it is not a no-op: the entry fixes the SLUG for the rest of
    /// the run, so a run whose first mention was `VectorDB` writes `vectordb.md` where one
    /// that saw `Vector DB` first would write `vector-db.md`. Only the address is inherited
    /// — the page's title is whichever spelling created the draft, and the display name in a
    /// link is the resolved one only where a caller renders from [`Self::resolve_identity`]
    /// rather than from [`Self::commit`]. So a link may read `[VectorDB]` beside a page
    /// titled `Vector DB`: one concept, one address, deterministic for a given input, and
    /// the next run rebuilds the index from disk where address and title key alike.
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
    /// A link is rendered from a RESOLVED identity, never from the extraction's own name:
    /// an alias resolves to a different slug than it would produce itself, so re-deriving
    /// one would point the citation at a page that does not exist. `apply_concept_result`
    /// takes that identity from here; `Pipeline::plan` and `plan_documents` render a page
    /// before staging anything and so take it from [`Self::resolve_identity`], discarding
    /// this return. The two agree because resolution records its decision — which is what
    /// makes rendering-before-folding safe at all.
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
                slug: safe_slug,
                title: draft.name.clone(),
            };
        }

        let mut draft = match existing.as_ref() {
            Some(page) => {
                // The persisted page stores these as `created`/`updated` (the keys the template
                // writes). Reading `first_seen`/`last_seen` would always miss and reset the
                // origin date to today on every re-ingest.
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
                    preserved_llm_inputs: capture_llm_inputs(page),
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
                preserved_llm_inputs: BTreeMap::new(),
                preserved_synthesis: None,
                preserved_sources: None,
                preserved_related: None,
                preserved_aliases: Vec::new(),
            },
        };

        draft.observe(date);
        draft.seed_synthesis(synthesis);
        let identity = ConceptIdentity {
            slug: safe_slug.clone(),
            title: draft.name.clone(),
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
    /// Applied on every fold, not just the one that creates the draft: two results in a run
    /// can name the same new concept and only one of them carry a grounding. Seeding solely
    /// on the fold that creates it would leave the page's synthesis empty or filled
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
            "llm_inputs": self.preserved_llm_inputs,
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

        Ok(RenderResult::fresh(path, content))
    }
}

/// Capture an existing concept page's `llm_inputs` markers so a re-render re-emits them
/// unchanged. Only string values are carried: the map is a protocol between the writer of a
/// section and the reader deciding whether it is answered, and a value of another shape
/// belongs to neither side of it.
fn capture_llm_inputs(page: &lk_core::frontmatter::VaultPage) -> BTreeMap<String, String> {
    page.frontmatter
        .get(lk_core::frontmatter::field::LLM_INPUTS)
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect()
}

/// Capture the body of a logical concept section from an existing page so a re-render can
/// splice it back in. Returns the body trimmed of section framing, or `None` when the page
/// carries nothing under that section — an empty one has nothing to preserve, and the fresh
/// render already writes an empty section.
fn capture_section(body: &str, section: impl lk_vault::SectionKey) -> Option<String> {
    let found = lk_vault::resolve_section(body, section)?;
    let trimmed = found.body.trim_matches('\n');
    (!trimmed.trim().is_empty()).then(|| trimmed.to_string())
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

/// Read every concept page in the vault into the registry that answers what a name
/// addresses. A page whose stem is not readable as a slug is skipped — it has no address
/// for a citation to name.
async fn build_registry(
    reader: &dyn VaultStore,
    dirs: &VaultDirs,
) -> Result<ConceptRegistry, PipelineError> {
    let dir = lk_core::vault_path::concepts_dir(dirs);
    let mut registry = ConceptRegistry::new();
    for path in reader.list_markdown(&dir).await? {
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // A page whose frontmatter will not parse still answers to its ADDRESS: the name it
        // is reachable by is its filename, which parsing has no say in. Failing the read
        // instead would let one hand-edited page block every concept this run would
        // materialize, on every run — and would have the write plane report absent for a name
        // the read plane calls owned, which is the disagreement the shared registry exists to
        // remove. Its title and aliases are simply unknown until the page is repaired, which
        // `lore graph lint` reports.
        let page = match reader.read_page(&path).await {
            Ok(Some(page)) => Some(page),
            Ok(None) => continue,
            Err(_) => None,
        };
        let frontmatter = page.as_ref().map(|p| &p.frontmatter);
        let title = frontmatter
            .and_then(|f| f.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or(slug);
        let aliases: Vec<String> = frontmatter
            .and_then(|f| f.get("aliases"))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
        registry.register(
            ConceptIdentity {
                slug: slug.to_string(),
                title: title.to_string(),
            },
            &aliases,
        );
    }
    Ok(registry)
}

/// Filter that callers use to drop concepts whose slug would be empty before threading
/// them into rendered output. Keeps daily-page wiki links honest.
pub fn has_valid_slug(concept: &ExtractedConcept) -> bool {
    slugify(&concept.name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vault whose every page is missing, except one path whose read fails outright.
    struct FailsOn(&'static str);

    #[async_trait::async_trait]
    impl VaultStore for FailsOn {
        async fn read_page(
            &self,
            rel_path: &std::path::Path,
        ) -> Result<Option<lk_core::frontmatter::VaultPage>, lk_vault::VaultError> {
            if rel_path.to_string_lossy().contains(self.0) {
                return Err(lk_vault::VaultError::Io(std::io::Error::other(
                    "read failed",
                )));
            }
            Ok(None)
        }

        async fn list_markdown(
            &self,
            _rel_dir: &std::path::Path,
        ) -> Result<Vec<std::path::PathBuf>, lk_vault::VaultError> {
            Ok(Vec::new())
        }
    }

    /// The property every caller that stages a batch before folding it relies on — and the
    /// reason `Pipeline::plan`, `plan_documents` and `apply_concept_result` all read every
    /// concept before committing any. `render_pages` emits the accumulator unconditionally,
    /// so a fold that survived a failed batch would write concept pages for an origin page
    /// the failed run never wrote.
    #[tokio::test]
    async fn a_failed_stage_leaves_the_drafts_untouched() {
        let dirs = VaultDirs::default();
        let reader = FailsOn("second");
        let mut drafts = ConceptDrafts::new();

        let first = ExtractedConcept {
            name: "First".into(),
            category: None,
        };
        let second = ExtractedConcept {
            name: "Second".into(),
            category: None,
        };
        let staged = drafts.stage(&first, None, &reader, &dirs).await.unwrap();
        assert!(drafts.stage(&second, None, &reader, &dirs).await.is_err());
        assert!(
            drafts.drafts.is_empty(),
            "staging alone must fold nothing, so the batch can be abandoned whole"
        );

        let date = jiff::civil::date(2026, 5, 23);
        drafts.commit(staged, date);
        assert_eq!(drafts.drafts.len(), 1, "only what the caller commits lands");
    }

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
            preserved_llm_inputs: BTreeMap::new(),
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
            preserved_llm_inputs: BTreeMap::new(),
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
            preserved_llm_inputs: BTreeMap::new(),
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
            preserved_llm_inputs: BTreeMap::new(),
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
