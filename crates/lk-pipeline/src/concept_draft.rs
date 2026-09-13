use std::collections::{BTreeMap, BTreeSet};

use lk_core::concept::{ConceptIdentity, ConceptRegistry, ExtractedConcept, Resolution, slugify};
use lk_core::config::VaultDirs;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_vault::{TemplateEngine, VaultError, VaultStore, replace_section};

use crate::PipelineError;
use crate::render::RenderResult;

/// In-memory aggregator for concept page state across multiple dates in a single run.
/// Reads existing vault pages on first encounter, then merges further mentions.
/// An extraction that named a concept an established page already answers to, minted beside
/// it anyway. The convergence contract forbids the draft: an agent asks the resolver for every
/// form the material writes, so a hit on any of them routes the extraction to the owner. When
/// one arrives regardless, the vault gains a rival that no later check can see — the two pages
/// share no name, so `lore graph lint` reports nothing — and this is the only moment it is
/// knowable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedAlias {
    /// The name the extraction claimed, which the owner already answers to.
    pub alias: String,
    /// The page that already answers to it.
    pub owner: String,
    /// The page the extraction minted instead.
    pub minted: String,
}

pub struct ConceptDrafts {
    drafts: BTreeMap<String, ConceptDraft>,
    /// Recorded at [`Self::commit`] rather than where the refusal happens, so a batch
    /// abandoned before it commits names no page. `lore queue apply` reports them after its
    /// writes land, which is where the extraction that produces them runs; the ingest path
    /// accumulates them too and says nothing, because a queue-backed ingest extracts no
    /// concepts of its own.
    refused: Vec<RefusedAlias>,
    /// Slugs a read found on disk. A refusal is only a rival where this run created the page
    /// the extraction's own name resolved to — a source that conflates two ESTABLISHED
    /// concepts minted nothing, and offering to merge them would delete one on the strength
    /// of one source's confusion.
    established: BTreeSet<String>,
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
    /// The extraction's aliases this vault can actually adopt, decided in [`ConceptDrafts::stage`]
    /// and already registered there.
    aliases: Vec<String>,
    /// Names this extraction claimed that another page already answers to, carried to
    /// [`ConceptDrafts::commit`] so an abandoned batch records none.
    refused: Vec<(String, String)>,
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
    /// Every name this page answers to beyond its own title: the ones already on disk, plus
    /// the ones this run's extractions contributed. Aliases are established identity, not
    /// regenerated content — a re-render that re-emitted only `[title]` would erase a synonym
    /// a human registered and break every citation relying on it. Only ever added to, never
    /// dropped. The title seed is kept out here and re-added first at render.
    aliases: Vec<String>,
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
            refused: Vec::new(),
            established: BTreeSet::new(),
            registry: None,
        }
    }

    /// Every extraction this run minted beside a page that already answered to one of its
    /// names.
    pub fn refused_aliases(&self) -> &[RefusedAlias] {
        &self.refused
    }

    /// Read everything a fold needs from the vault, without folding it into the drafts.
    ///
    /// Splitting the read from the fold is what lets a caller with several concepts stage
    /// them all before committing any: the reads are the only fallible part, so a failure on
    /// the third concept cannot leave the first two in the run's drafts. That matters because
    /// [`Self::render_pages`] emits the accumulator unconditionally — a half-folded result
    /// would write concept pages whose origin page was never updated to cite them.
    ///
    /// The drafts are what that protects. Resolving a name — and adopting the extraction's
    /// other names — does record them in the alias index (see [`Self::resolve_identity`]), so
    /// a `stage` that fails afterwards leaves those entries behind. Deliberately, but it is not a no-op: the entry fixes the SLUG for the rest of
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
        let (aliases, refused) = self.adopt_aliases(&identity, &concept.aliases);
        // A slug already staged this run needs no read: the draft in hand is newer than the
        // page on disk, and `commit` folds into it.
        let existing = if self.drafts.contains_key(&identity.slug) {
            None
        } else {
            reader
                .read_page(VaultPath::concept(dirs, &identity.slug).as_ref())
                .await?
        };
        if existing.is_some() {
            self.established.insert(identity.slug.clone());
        }
        Ok(StagedConcept {
            concept: concept.clone(),
            slug: identity.slug,
            existing,
            synthesis: synthesis.map(str::to_string),
            aliases,
            refused,
        })
    }

    /// Register the aliases this vault can answer with, and report which they were.
    ///
    /// An alias is adopted only where nothing else claims it. A name another page already
    /// answers to is the vault's duplicate-concept defect if registered here, and the
    /// extraction is the weaker claim: the established page earned the name by being cited
    /// under it, while this is one source's reading of what a term also means. So the
    /// conflict is reported and the alias dropped — never routed by preference, and never
    /// matched approximately, since an alias exists precisely to make an exact lookup succeed.
    ///
    /// Within ONE extraction the outcome therefore depends on the order it reports concepts in:
    /// `{name: X, aliases: [Y]}` ahead of `{name: Y}` converges both on X, while the reverse
    /// order mints `y.md` first and then refuses the alias. Deterministic for a given result
    /// and the same rule either way — a name is kept by whoever answered to it first — but it
    /// means two spellings of one concept converge or split by where the extraction happened
    /// to put them, which is what a reader looking at a split concept has to know.
    fn adopt_aliases(
        &mut self,
        identity: &ConceptIdentity,
        proposed: &[String],
    ) -> (Vec<String>, Vec<(String, String)>) {
        let registry = self
            .registry
            .as_mut()
            .expect("stage resolves the name first, which builds the registry");
        let mut adopted = Vec::new();
        let mut refused = Vec::new();
        for alias in proposed {
            let alias = alias.trim();
            if alias.is_empty() || slugify(alias).is_none() {
                continue;
            }
            match registry.resolve(alias) {
                Resolution::Absent => {}
                held if held.routed().is_some_and(|r| r.slug == identity.slug) => continue,
                held => {
                    let owner = held
                        .routed()
                        .map(|r| r.slug.as_str())
                        .unwrap_or_default()
                        .to_string();
                    tracing::warn!(
                        alias,
                        proposed_for = %identity.slug,
                        answered_by = %owner,
                        "alias already answers to another concept page; not adopted"
                    );
                    refused.push((alias.to_string(), owner));
                    continue;
                }
            }
            let alias = alias.to_string();
            registry.register(identity.clone(), std::slice::from_ref(&alias));
            adopted.push(alias);
        }
        (adopted, refused)
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
            aliases,
            refused,
        } = staged;
        let synthesis = synthesis.as_deref();

        // Only where this run created the page the extraction's own name resolved to. A
        // source that conflates two established concepts minted nothing, and the same record
        // there would offer to delete one of them.
        if !self.established.contains(&safe_slug) {
            for (alias, owner) in refused {
                let record = RefusedAlias {
                    alias,
                    owner,
                    minted: safe_slug.clone(),
                };
                if !self.refused.contains(&record) {
                    self.refused.push(record);
                }
            }
        }

        if let Some(draft) = self.drafts.get_mut(&safe_slug) {
            draft.observe(date);
            draft.seed_synthesis(synthesis);
            draft.adopt_aliases(aliases);
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
                let aliases = page
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
                    aliases,
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
                aliases: Vec::new(),
            },
        };

        draft.observe(date);
        draft.seed_synthesis(synthesis);
        draft.adopt_aliases(aliases);
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

    /// Add names the page did not already answer to. A name is only ever added, because a
    /// citation somewhere may already address the page through it.
    fn adopt_aliases(&mut self, aliases: Vec<String>) {
        for alias in aliases {
            if alias != self.name && !self.aliases.contains(&alias) {
                self.aliases.push(alias);
            }
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
        for a in &self.aliases {
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
        // remove.
        //
        // ONLY that failure. A page this cannot READ answers to nothing knowable — the
        // filename is a guess about a file whose content was never seen — and continuing past
        // it mints a page beside one whose alias would have caught the name, which no lint
        // reports: the two slugs reduce to different identities, so there is no pair to
        // compare. `lore resolve` propagates an I/O error for the same reason, and the two
        // planes answering one question differently is what this registry exists to prevent.
        //
        // Losing a broken page's aliases splits the concept the same silent way, and that
        // cost is real — it is simply smaller than the run this used to block, and repairing
        // the page is what recovers it. Routing is all this tolerance buys: `stage`'s own
        // read of the page it is about to re-render stays strict, because a page rendered
        // from the template without the frontmatter it carries loses everything on it.
        let page = match reader.read_page(&path).await {
            Ok(Some(page)) => Some(page),
            Ok(None) => continue,
            Err(VaultError::Frontmatter(_)) => None,
            Err(e) => return Err(e.into()),
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

    /// A vault holding one concept page whose read fails with the given error.
    struct OneBadPage(lk_vault::VaultError);

    #[async_trait::async_trait]
    impl VaultStore for OneBadPage {
        async fn read_page(
            &self,
            _rel_path: &std::path::Path,
        ) -> Result<Option<lk_core::frontmatter::VaultPage>, lk_vault::VaultError> {
            Err(match &self.0 {
                lk_vault::VaultError::Frontmatter(m) => {
                    lk_vault::VaultError::Frontmatter(m.clone())
                }
                _ => lk_vault::VaultError::Io(std::io::Error::other("read failed")),
            })
        }

        async fn list_markdown(
            &self,
            rel_dir: &std::path::Path,
        ) -> Result<Vec<std::path::PathBuf>, lk_vault::VaultError> {
            Ok(vec![rel_dir.join("vector-db.md")])
        }
    }

    /// The two halves of what a page the registry cannot read means, and they are opposite
    /// answers. Frontmatter that will not parse leaves the page's ADDRESS knowable, so it
    /// still claims it and an extraction naming it routes there instead of minting a rival
    /// beside it. A page that cannot be READ leaves nothing knowable — the filename is a
    /// guess about content never seen — so the run fails. Tolerating both would put the write
    /// plane's answer at odds with `lore resolve`, which propagates I/O and only ever
    /// swallows a parse.
    ///
    /// Routing is the only question this tolerance answers. Reading the page to MERGE into is
    /// stricter and stays that way: an unparseable page re-rendered from the template loses
    /// the synthesis, aliases, category and count it carries, so `stage` fails rather than
    /// overwrite what it could not read.
    #[tokio::test]
    async fn a_page_that_will_not_parse_claims_its_address_and_one_that_cannot_be_read_fails() {
        let dirs = VaultDirs::default();

        let unparseable = OneBadPage(lk_vault::VaultError::Frontmatter("bad".into()));
        let identity = ConceptDrafts::new()
            .resolve_identity("VectorDB", &unparseable, &dirs)
            .await
            .expect("a page that will not parse never fails the routing");
        assert_eq!(
            identity.slug, "vector-db",
            "the extraction routes to the page already holding that address"
        );
        assert!(
            ConceptDrafts::new()
                .stage(
                    &ExtractedConcept {
                        name: "VectorDB".into(),
                        category: None,
                        aliases: Vec::new(),
                    },
                    None,
                    &unparseable,
                    &dirs,
                )
                .await
                .is_err(),
            "a page this cannot read is not a page to re-render from the template"
        );

        let unreadable = OneBadPage(lk_vault::VaultError::Io(std::io::Error::other("x")));
        assert!(
            ConceptDrafts::new()
                .resolve_identity("VectorDB", &unreadable, &dirs)
                .await
                .is_err(),
            "a page nothing could read is not a page with no names"
        );
    }

    /// What the alias field on an extraction buys: a concept named differently by a second
    /// source lands on the page the first one created, instead of a rival page carrying half
    /// the citations and a synthesis of its own.
    ///
    /// The rejection half is the same property from the other side. An alias is a claim to
    /// answer to a name, and a name an established page already answers to is not a claim an
    /// extraction gets to make — granting it would be the duplicate-concept defect written
    /// deliberately, with citations of that name routed by read order afterwards.
    #[tokio::test]
    async fn an_alias_routes_a_later_name_and_never_takes_one_already_answered() {
        let dirs = VaultDirs::default();
        let empty = FailsOn("nothing reads this");
        let date = jiff::civil::date(2026, 1, 1);
        let mut drafts = ConceptDrafts::new();

        let staged = drafts
            .stage(
                &ExtractedConcept {
                    name: "Agent Capability".into(),
                    category: None,
                    aliases: vec!["에이전트 호출 가능 애플리케이션 단위".into()],
                },
                None,
                &empty,
                &dirs,
            )
            .await
            .expect("an empty vault stages cleanly");
        drafts.commit(staged, date);

        let by_alias = drafts
            .resolve_identity("에이전트 호출 가능 애플리케이션 단위", &empty, &dirs)
            .await
            .expect("resolving a registered alias reads nothing new");
        assert_eq!(
            by_alias.slug, "agent-capability",
            "the alias addresses the page the name created"
        );

        let staged = drafts
            .stage(
                &ExtractedConcept {
                    name: "Tool Exposure".into(),
                    category: None,
                    aliases: vec!["Agent Capability".into()],
                },
                None,
                &empty,
                &dirs,
            )
            .await
            .expect("an empty vault stages cleanly");
        assert!(
            staged.aliases.is_empty(),
            "a name an established page answers to is not an alias this may take"
        );
        drafts.commit(staged, date);
        assert_eq!(
            drafts
                .resolve_identity("Agent Capability", &empty, &dirs)
                .await
                .expect("resolving reads nothing new")
                .slug,
            "agent-capability",
            "the established page keeps the name"
        );
        // The refusal is correct and it is also a page minted beside one that already held
        // the subject. Nothing downstream can see that afterwards — the two share no name —
        // so it is recorded here for whoever runs the drafts to report.
        assert_eq!(
            drafts.refused_aliases(),
            [RefusedAlias {
                alias: "Agent Capability".into(),
                owner: "agent-capability".into(),
                minted: "tool-exposure".into(),
            }]
        );
    }

    /// A batch that never commits wrote nothing, so it names nothing. `apply_concept_result`
    /// stages every concept before folding any, and a later read that fails abandons the
    /// whole batch — recording the refusal where it happens would print a merge for a page
    /// the run did not create.
    #[tokio::test]
    async fn a_batch_that_never_commits_names_no_rival() {
        let dirs = VaultDirs::default();
        let date = jiff::civil::date(2026, 1, 1);
        let mut drafts = ConceptDrafts::new();

        let staged = drafts
            .stage(
                &ExtractedConcept {
                    name: "Agent Capability".into(),
                    category: None,
                    aliases: vec![],
                },
                None,
                &FailsOn("nothing"),
                &dirs,
            )
            .await
            .expect("stages cleanly");
        drafts.commit(staged, date);

        let refused_but_abandoned = drafts
            .stage(
                &ExtractedConcept {
                    name: "Tool Exposure".into(),
                    category: None,
                    aliases: vec!["Agent Capability".into()],
                },
                None,
                &FailsOn("nothing"),
                &dirs,
            )
            .await
            .expect("stages cleanly");
        assert!(
            drafts.refused_aliases().is_empty(),
            "staging is not creating; nothing is reportable until the fold"
        );
        drop(refused_but_abandoned);
        assert!(drafts.refused_aliases().is_empty());
    }

    /// A page on disk is not a rival this run created, and the record is what a person is
    /// told to delete. A source that conflates two ESTABLISHED concepts refuses an alias for
    /// the same reason a new page does — and offering to merge them there would fold one
    /// away on the strength of one source's confusion.
    #[tokio::test]
    async fn an_extraction_that_conflates_two_established_pages_names_no_rival() {
        struct Holds(&'static str);
        #[async_trait::async_trait]
        impl VaultStore for Holds {
            async fn read_page(
                &self,
                rel_path: &std::path::Path,
            ) -> Result<Option<lk_core::frontmatter::VaultPage>, lk_vault::VaultError> {
                if !rel_path.to_string_lossy().contains(self.0) {
                    return Ok(None);
                }
                Ok(Some(
                    lk_core::frontmatter::parse_page(
                        "---\ntype: concept\ntitle: \"RAG\"\naliases: [\"RAG\"]\n---\n\n# RAG\n",
                    )
                    .expect("a page this test wrote"),
                ))
            }
            async fn list_markdown(
                &self,
                _rel_dir: &std::path::Path,
            ) -> Result<Vec<std::path::PathBuf>, lk_vault::VaultError> {
                Ok(Vec::new())
            }
        }

        let dirs = VaultDirs::default();
        let date = jiff::civil::date(2026, 1, 1);
        let mut drafts = ConceptDrafts::new();

        let staged = drafts
            .stage(
                &ExtractedConcept {
                    name: "Vector DB".into(),
                    category: None,
                    aliases: vec![],
                },
                None,
                &FailsOn("nothing"),
                &dirs,
            )
            .await
            .expect("stages cleanly");
        drafts.commit(staged, date);

        // `rag` is on disk, so the extraction's own name routes to it and nothing is minted;
        // the alias it also claimed belongs to the page created above.
        let staged = drafts
            .stage(
                &ExtractedConcept {
                    name: "RAG".into(),
                    category: None,
                    aliases: vec!["Vector DB".into()],
                },
                None,
                &Holds("rag"),
                &dirs,
            )
            .await
            .expect("stages cleanly");
        drafts.commit(staged, date);

        assert!(
            drafts.refused_aliases().is_empty(),
            "an established page is not a rival this run minted: {:?}",
            drafts.refused_aliases()
        );
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
            aliases: Vec::new(),
        };
        let second = ExtractedConcept {
            name: "Second".into(),
            category: None,
            aliases: Vec::new(),
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
            aliases: Vec::new(),
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
            aliases: Vec::new(),
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
            aliases: Vec::new(),
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
    fn aliases_survive_render() {
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
            aliases: vec!["RAG".into()],
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
