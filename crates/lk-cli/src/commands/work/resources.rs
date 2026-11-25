use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::{Citation, file_revision, source_revision};
use lk_core::config::Config;
use lk_core::semantic::{SemanticAuthority, SemanticResource};
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Subject {
    Queue {
        task_id: String,
        cache_hash: String,
        target: String,
        target_revision: String,
    },
    Audit {
        path: String,
        file_revision: String,
        input_revision: String,
    },
}

/// How a preparation presents the context pages it read. Every context resource of one
/// preparation follows its reading, and either reading is rebuilt from the captured pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ContextReading {
    /// Parsed frontmatter without bookkeeping (`lk_core::concept::semantic_text`), so a
    /// citation or task-state update leaves the reading unchanged. A page whose frontmatter
    /// does not parse reads as its exact bytes. Every new Work reads this way.
    Semantic,
    /// The captured page's exact bytes, as Work planned before 0.144.0 read context pages.
    Exact,
}

impl ContextReading {
    fn text(self, raw: &str) -> String {
        match self {
            Self::Semantic => lk_vault::parse_page(raw).map_or_else(
                |_| raw.to_owned(),
                |page| lk_core::concept::semantic_text(&page),
            ),
            Self::Exact => raw.to_owned(),
        }
    }
}

#[derive(Clone)]
pub(super) struct Prepared {
    read: lk_vault::ReadControl,
    reading: ContextReading,
    semantic_config_revision: Option<String>,
    pub subject: Subject,
    pub context: Value,
    pub resources: Vec<SemanticResource>,
    pub pages: BTreeMap<PathBuf, String>,
    /// Addresses read under an authority other than context: a subject, citation or review
    /// evidence stays a current read even where the same page is also context.
    strict_reads: BTreeSet<String>,
    /// A synthesis request's context snapshot, which its context-only pages are read from.
    snapshot: Option<super::snapshot::ContextSnapshot>,
    audit_reviews: Option<Vec<lk_graph::audit::AuditReviewDependency>>,
    pub(super) selected_owners: Option<BTreeSet<String>>,
    /// The current candidate roster that admits this screened evidence. Result completion
    /// needs it; reading the evidence does not, so it is absent from a plain refresh.
    pub(super) roster: Option<super::selection::RosterBinding>,
    /// Digest of a synthesis target's graph citations when this context was captured.
    pub(super) graph_citations: Option<String>,
}

/// What Rust writes into a result beside the model's reply: an extraction's discovery recipes
/// and screening proof, and the open evidence-gap reviews a synthesis references.
pub(super) struct ResultBinding {
    pub discovery: Vec<lk_core::discovery::DiscoveryQuery>,
    pub selection: Option<lk_queue::ExtractionSelection>,
    pub evidence_gaps: Vec<super::gap::EvidenceGap>,
}

impl ResultBinding {
    /// The bound roster's recipes and proof for screened evidence, the context's own
    /// recipes otherwise, and the evidence-gap reviews the synthesis context captured.
    pub(super) fn build(
        context: &Value,
        roster: Option<&super::selection::RosterBinding>,
    ) -> miette::Result<Self> {
        let evidence_gaps = super::gap::from_context(context)?;
        match roster {
            Some(roster) => Ok(Self {
                discovery: roster.queries(),
                selection: Some(roster.selection.clone()),
                evidence_gaps,
            }),
            None if is_screened(context) => Err(miette::miette!(
                "screened extraction evidence is not bound to its current candidate roster"
            )),
            None => Ok(Self {
                discovery: match context["purpose"].get("discovery") {
                    Some(discovery) => {
                        serde_json::from_value(discovery["queries"].clone()).into_diagnostic()?
                    }
                    None => Vec::new(),
                },
                selection: None,
                evidence_gaps,
            }),
        }
    }
}

/// Whether a context names screened extraction evidence.
pub(super) fn is_screened(context: &Value) -> bool {
    context["purpose"]["extraction_scope"]["policy"] == super::selection::POLICY
}

/// One extraction task's candidate roster beside its source: the retained input, the
/// target and the target's scoped reviews. Candidate pages are not guarded here; a
/// screening decision binds the card it judged.
pub(super) struct SelectionInventory {
    pub capture: lk_vault::DiscoveryCapture,
    pub source: Prepared,
}

impl Prepared {
    /// A synthesis request answers the citation set it was made for. Refuse drafting or
    /// publishing it once the concept's graph citations captured here differ, whatever order
    /// reconciliation and application run in. The refusal is a rejection, never a discard:
    /// its scope carries these citations, so they returning releases the held result.
    pub(super) fn require_requested_citations(&self) -> miette::Result<()> {
        let (Some(current), Subject::Queue { cache_hash, .. }) =
            (&self.graph_citations, &self.subject)
        else {
            return Ok(());
        };
        if current != cache_hash {
            return Err(super::publication_rejection(
                super::RejectionKind::CitationsChanged,
                "concept citations changed since this synthesis was requested; `lore graph backlinks-sync` records the current set and requests its synthesis",
            ));
        }
        Ok(())
    }

    /// Whether `address` is a reusable owner captured at the exact file revision `revision`.
    /// Reuse publication writes the owner, so it binds the file, not the context read.
    pub(super) fn is_reuse_owner(&self, address: &str, revision: &str) -> bool {
        self.resources.iter().any(|resource| {
            self.selected_owners
                .as_ref()
                .map_or(resource.authority == SemanticAuthority::Context, |owners| {
                    owners.contains(address)
                })
                && resource.address == address
        }) && self
            .page_file_revision(address)
            .is_some_and(|current| current == revision)
    }

    pub(super) fn page_file_revision(&self, address: &str) -> Option<String> {
        self.captured_page(address).map(file_revision)
    }

    /// The bytes a context resource is read from: its snapshot bytes when the request holds a
    /// snapshot of it, otherwise the captured page.
    fn context_page(&self, address: &str) -> Option<&str> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.page(address))
            .or_else(|| self.captured_page(address))
    }

    /// Reads every context-only page from `snapshot`, which binds them as captured now
    /// unless it already holds them.
    fn with_snapshot(
        mut self,
        mut snapshot: super::snapshot::ContextSnapshot,
    ) -> miette::Result<Self> {
        let mut captured = BTreeMap::new();
        for resource in &self.resources {
            if resource.authority != SemanticAuthority::Context
                || self.strict_reads.contains(&resource.address)
            {
                continue;
            }
            let raw = self.captured_page(&resource.address).ok_or_else(|| {
                miette::miette!("context page was not captured: {}", resource.address)
            })?;
            captured.insert(resource.address.clone(), raw.to_owned());
        }
        snapshot.bind(captured);
        self.snapshot = Some(snapshot);
        let reading = self.reading;
        for resource in &mut self.resources {
            if let Some(raw) = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.page(&resource.address))
            {
                resource.text = reading.text(raw);
                resource.revision = file_revision(&resource.text);
            }
        }
        Ok(self)
    }

    /// Records this request's context snapshot unless it already holds these pages.
    pub(super) fn plan_snapshot(
        &mut self,
        root: &Path,
        plan: &mut lk_vault::MutationPlan,
    ) -> miette::Result<bool> {
        match &mut self.snapshot {
            Some(snapshot) => snapshot.plan_record(root, plan),
            None => Ok(false),
        }
    }

    /// The exact bytes captured for a page, whatever reading its resource presents.
    pub(super) fn captured_page(&self, address: &str) -> Option<&str> {
        self.pages
            .iter()
            .find(|(path, _)| lk_core::vault_path::address(path) == address)
            .map(|(_, raw)| raw.as_str())
    }

    pub(super) fn reading(&self) -> ContextReading {
        self.reading
    }

    /// This preparation with its context resources presented in `reading`, rebuilt from the
    /// pages it captured.
    pub(super) fn read_as(&self, reading: ContextReading) -> miette::Result<Self> {
        let mut read = self.clone();
        if reading == self.reading {
            return Ok(read);
        }
        read.reading = reading;
        for resource in &mut read.resources {
            if resource.authority != SemanticAuthority::Context {
                continue;
            }
            let raw = self.context_page(&resource.address).ok_or_else(|| {
                miette::miette!("context page was not captured: {}", resource.address)
            })?;
            resource.text = reading.text(raw);
            resource.revision = file_revision(&resource.text);
        }
        Ok(read)
    }

    pub fn validate_audit_result(
        &self,
        result: &Value,
    ) -> miette::Result<lk_graph::audit::AuditAssessment> {
        let Subject::Audit {
            path,
            file_revision,
            input_revision,
        } = &self.subject
        else {
            return Err(miette::miette!("audit result requires an audit subject"));
        };
        let mut results: lk_graph::audit::AuditResults =
            serde_json::from_value(result.clone()).into_diagnostic()?;
        let [assessment] = results.assessments.as_slice() else {
            return Err(miette::miette!(
                "completed audit Work requires exactly one assessment"
            ));
        };
        if assessment.path != *path
            || assessment.file_revision != *file_revision
            || assessment.input_revision != *input_revision
            || assessment.reason.trim().is_empty()
        {
            return Err(miette::miette!(
                "audit assessment differs from its captured subject"
            ));
        }
        let dependencies: Vec<String> =
            serde_json::from_value(self.context["purpose"]["review_dependencies"].clone())
                .into_diagnostic()?;
        if assessment.reviews != dependencies {
            return Err(miette::miette!(
                "final assessment must list all exact current scoped review dependencies"
            ));
        }
        let reviews = self
            .audit_reviews
            .as_deref()
            .ok_or_else(|| miette::miette!("audit capture has no scoped review inventory"))?;
        lk_graph::audit::validate_assessment_reviews(reviews, assessment)
            .map_err(lk_graph::GraphError::from)
            .map_err(miette::Report::msg)?;
        Ok(results.assessments.remove(0))
    }

    pub fn read_control(&self) -> &lk_vault::ReadControl {
        &self.read
    }

    pub(super) fn result_binding(&self) -> miette::Result<ResultBinding> {
        ResultBinding::build(&self.context, self.roster.as_ref())
    }

    pub fn validate_context(self, context: &Value) -> miette::Result<Self> {
        let subject: Subject =
            serde_json::from_value(context["subject"].clone()).into_diagnostic()?;
        if self.subject != subject || self.context != *context {
            return Err(super::publication_rejection(
                super::RejectionKind::SemanticContextChanged,
                "semantic work inputs changed; replan and reread changed evidence",
            ));
        }
        Ok(self)
    }

    pub fn revision(&self, config_revision: &str) -> miette::Result<String> {
        self.legacy_revision(
            self.semantic_config_revision
                .as_deref()
                .unwrap_or(config_revision),
        )
    }

    pub fn legacy_revision(&self, config_revision: &str) -> miette::Result<String> {
        let resources = self
            .resources
            .iter()
            .map(|resource| (&resource.address, &resource.revision, resource.authority))
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&(config_revision, &self.subject, &self.context, resources))
            .into_diagnostic()?;
        Ok(lk_core::rss_original::digest(&bytes))
    }

    /// Guards every captured page and the context snapshot the preparation read.
    pub fn guard_pages(&self, plan: &mut lk_vault::MutationPlan) {
        for (path, content) in &self.pages {
            plan.require_unchanged(path, content.clone());
        }
        self.guard_snapshot(plan);
    }

    pub(super) fn guard_snapshot(&self, plan: &mut lk_vault::MutationPlan) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.guard(plan);
        }
    }
}

struct Capture<'a> {
    rss: Option<lk_vault::RssEvidenceCapture>,
    read: lk_vault::ReadControl,
    reading: ContextReading,
    root: &'a Path,
    pages: BTreeMap<PathBuf, String>,
    resources: BTreeMap<String, SemanticResource>,
    strict_reads: BTreeSet<String>,
}

impl<'a> Capture<'a> {
    fn new(root: &'a Path, read: &lk_vault::ReadControl) -> Self {
        Self {
            root,
            rss: None,
            read: read.clone(),
            reading: ContextReading::Semantic,
            pages: BTreeMap::new(),
            resources: BTreeMap::new(),
            strict_reads: BTreeSet::new(),
        }
    }

    /// Continue a finished capture with its pages and resources as the starting state.
    fn resume(root: &'a Path, prepared: &Prepared) -> Self {
        Self {
            root,
            rss: None,
            read: prepared.read.clone(),
            reading: prepared.reading,
            pages: prepared.pages.clone(),
            resources: prepared
                .resources
                .iter()
                .cloned()
                .map(|resource| (resource.address.clone(), resource))
                .collect(),
            strict_reads: prepared.strict_reads.clone(),
        }
    }

    fn page(
        &mut self,
        path: &str,
        expected_file: Option<&str>,
        expected_authored: Option<&str>,
        authority: SemanticAuthority,
    ) -> miette::Result<String> {
        self.read.check().into_diagnostic()?;
        let absolute = lk_vault::page_path(self.root, Path::new(path)).into_diagnostic()?;
        let canonical = absolute.canonicalize().into_diagnostic()?;
        let relative = canonical
            .strip_prefix(self.root)
            .into_diagnostic()?
            .to_path_buf();
        let raw = self.read.read_to_string(&absolute).into_diagnostic()?;
        let exact = file_revision(&raw);
        if expected_file.is_some_and(|expected| expected != exact) {
            return Err(miette::miette!("semantic source changed: {path}"));
        }
        if let Some(expected) = expected_authored {
            let parsed = lk_vault::parse_page(&raw).map_err(|e| miette::miette!("{path}: {e}"))?;
            if source_revision(&parsed) != expected {
                return Err(miette::miette!("semantic authored source changed: {path}"));
            }
        }
        self.insert_page(relative, raw.clone(), authority)?;
        Ok(raw)
    }

    fn insert_page(
        &mut self,
        relative: PathBuf,
        raw: String,
        authority: SemanticAuthority,
    ) -> miette::Result<()> {
        if self.pages.get(&relative).is_some_and(|prior| prior != &raw) {
            return Err(miette::miette!(
                "semantic source changed during capture: {}",
                relative.display()
            ));
        }
        let address = lk_core::vault_path::address(&relative);
        if authority != SemanticAuthority::Context {
            self.strict_reads.insert(address.clone());
        }
        let text = match authority {
            SemanticAuthority::Context => self.reading.text(&raw),
            _ => raw.clone(),
        };
        self.pages.insert(relative, raw);
        self.resources
            .entry(address.clone())
            .or_insert_with(|| SemanticResource {
                address,
                revision: file_revision(&text),
                authority,
                text,
            });
        Ok(())
    }

    fn citation(&mut self, subject: &str, citation: &Citation) -> miette::Result<()> {
        self.read.check().into_diagnostic()?;
        if citation.rss_event.is_some() {
            if self.rss.is_none() {
                self.rss = Some(
                    lk_vault::RssEvidenceCapture::build(self.root, &self.read).into_diagnostic()?,
                );
            }
            let input = self
                .rss
                .as_mut()
                .expect("RSS capture initialized")
                .load_citation(Path::new(subject), citation)
                .into_diagnostic()?;
            self.insert_page(
                input.origin_owner,
                input.origin.as_str().to_owned(),
                SemanticAuthority::Page,
            )?;
            self.insert_page(
                input.payload_path,
                input.payload.as_str().to_owned(),
                SemanticAuthority::RssEvent,
            )?;
        } else {
            self.page(
                &citation.path,
                None,
                Some(&citation.revision),
                SemanticAuthority::Page,
            )?;
        }
        Ok(())
    }

    fn review(&mut self, review: &lk_graph::audit::AuditReviewDependency) -> miette::Result<()> {
        let expected = review
            .file_revision
            .as_deref()
            .ok_or_else(|| miette::miette!("unreadable scoped review: {}", review.path))?;
        self.page(
            &review.path,
            Some(expected),
            review.source_revision.as_deref(),
            SemanticAuthority::Review,
        )?;
        for evidence in &review.evidence {
            for current in evidence.current.iter().chain(&evidence.archived) {
                let authority = match evidence.archived.as_ref() {
                    Some(archive) if archive.path == current.path => {
                        if evidence.recorded.rss_event.is_some() {
                            SemanticAuthority::RssEvent
                        } else {
                            SemanticAuthority::Archive
                        }
                    }
                    _ => SemanticAuthority::Page,
                };
                self.page(
                    &current.path,
                    Some(&current.file_revision),
                    current.source_revision.as_deref(),
                    authority,
                )?;
            }
        }
        Ok(())
    }

    fn finish(self, subject: Subject, context: Value, config: &Config) -> Prepared {
        Prepared {
            read: self.read,
            reading: self.reading,
            semantic_config_revision: config.semantic_work_revision().map(str::to_owned),
            subject,
            context,
            resources: self.resources.into_values().collect(),
            pages: self.pages,
            strict_reads: self.strict_reads,
            snapshot: None,
            audit_reviews: None,
            selected_owners: None,
            roster: None,
            graph_citations: None,
        }
    }
}

fn capture_reviews(
    captured: &mut Capture<'_>,
    context: &mut Value,
    reviews: &BTreeMap<String, Vec<lk_graph::audit::AuditReviewDependency>>,
) -> miette::Result<()> {
    let mut paths = BTreeSet::new();
    for review in reviews.values().flatten() {
        if paths.insert(review.path.clone()) {
            captured.review(review)?;
        }
    }
    context["purpose"]["review_dependencies"] = json!({
        "subject_count":reviews.len(),
        "review_count":paths.len(),
        "revision":file_revision(&serde_json::to_string(reviews).into_diagnostic()?),
    });
    Ok(())
}

pub(super) fn task(root: &Path, config: &Config, task_id: &str) -> miette::Result<Prepared> {
    task_with_audit(root, config, task_id, None)
}

pub(super) fn task_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    task_with_audit_controlled(root, config, task_id, None, read)
}

pub(super) fn section_task_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let mut prepared = task_controlled(root, config, task_id, read)?;
    let Subject::Queue { target, .. } = &prepared.subject else {
        return Err(miette::miette!("section requires a queue subject"));
    };
    let owner = lk_vault::resolve_page_owner(root, Path::new(target)).into_diagnostic()?;
    let raw = prepared
        .pages
        .get(&owner)
        .ok_or_else(|| miette::miette!("section owner is absent from its scope"))?;
    let subjects = BTreeMap::from([(
        target.clone(),
        lk_vault::parse_page(raw).map_err(|error| miette::miette!("{target}: {error}"))?,
    )]);
    let reviews =
        lk_graph::audit::review_dependencies_controlled(root, &config.vault.dirs, &subjects, read)
            .into_diagnostic()?;
    let mut captured = Capture::resume(root, &prepared);
    capture_reviews(&mut captured, &mut prepared.context, &reviews)?;
    Ok(captured.finish(prepared.subject, prepared.context, config))
}

pub(super) fn task_with_audit(
    root: &Path,
    config: &Config,
    task_id: &str,
    audit: Option<&lk_graph::audit::AuditCandidate>,
) -> miette::Result<Prepared> {
    task_with_audit_controlled(
        root,
        config,
        task_id,
        audit,
        &lk_vault::ReadControl::default(),
    )
}

pub(super) fn task_with_audit_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    audit: Option<&lk_graph::audit::AuditCandidate>,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    read.check().into_diagnostic()?;
    let task = super::super::queue::select_task_controlled(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
        read,
    )?;
    let extraction = task.kind == lk_queue::TaskKind::ExtractConcepts;
    let queries = if extraction {
        super::discovery::current(root, &task)?.unwrap_or_default()
    } else {
        Vec::new()
    };
    let prepared = capture_task_with_audit_controlled(root, config, task, audit, read)?;
    prepared.require_requested_citations()?;
    if extraction {
        with_discovery_controlled(root, config, prepared, &queries, read)
    } else {
        Ok(prepared)
    }
}

pub(super) fn task_with_discovery(
    root: &Path,
    config: &Config,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
) -> miette::Result<Prepared> {
    task_with_discovery_controlled(
        root,
        config,
        task_id,
        queries,
        &lk_vault::ReadControl::default(),
    )
}

pub(super) fn task_with_discovery_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let task = super::super::queue::select_task_controlled(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
        read,
    )?;
    with_discovery_controlled(
        root,
        config,
        capture_task_with_audit_controlled(root, config, task, None, read)?,
        queries,
        read,
    )
}

pub(super) fn source_task(root: &Path, config: &Config, task_id: &str) -> miette::Result<Prepared> {
    source_task_controlled(root, config, task_id, &lk_vault::ReadControl::default())
}

pub(super) fn source_task_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    read.check().into_diagnostic()?;
    let mut reviews = lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
        .into_diagnostic()?;
    extraction_source_controlled(root, config, task_id, &mut reviews, read)
}

#[cfg(test)]
pub(super) fn selection_inventory(
    root: &Path,
    config: &Config,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
) -> miette::Result<SelectionInventory> {
    selection_inventory_controlled(
        root,
        config,
        task_id,
        queries,
        &lk_vault::ReadControl::default(),
    )
}

/// The ranked candidate roster one extraction task screens: each query contributes its
/// identity matches and the configured number of best other pages, never the target.
pub(super) fn extraction_recipe(
    config: &Config,
    task: &lk_queue::QueueTask,
    queries: Vec<lk_core::discovery::DiscoveryQuery>,
) -> miette::Result<lk_vault::DiscoveryRecipe> {
    Ok(lk_vault::DiscoveryRecipe {
        queries,
        roster: lk_vault::DiscoveryRoster::Ranked(
            config
                .execution
                .effective_work()
                .into_diagnostic()?
                .max_query_candidates,
        ),
        subject: Some(PathBuf::from(&task.target.vault_path)),
    })
}

pub(super) fn selection_inventory_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
    read: &lk_vault::ReadControl,
) -> miette::Result<SelectionInventory> {
    read.check().into_diagnostic()?;
    let task = super::super::queue::select_task_controlled(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
        read,
    )?;
    let mut reviews = lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
        .into_diagnostic()?;
    let (mut captures, _) = lk_vault::capture_discovery_batch_with_inventory(
        root,
        &config.vault.dirs,
        &[extraction_recipe(config, &task, queries.to_vec())?],
        &mut reviews,
        read,
    )
    .into_diagnostic()?;
    let capture = captures.remove(0);
    read.check().into_diagnostic()?;
    selection_inventory_with_capture(root, config, task_id, capture, &mut reviews, read)
}

pub(super) fn selection_inventory_with_capture(
    root: &Path,
    config: &Config,
    task_id: &str,
    capture: lk_vault::DiscoveryCapture,
    reviews: &mut lk_vault::ReviewInventory,
    read: &lk_vault::ReadControl,
) -> miette::Result<SelectionInventory> {
    let source = extraction_source_controlled(root, config, task_id, reviews, read)?;
    let Subject::Queue { target, .. } = &source.subject else {
        return Err(miette::miette!(
            "extraction inventory requires a queue subject"
        ));
    };
    let target_owner = lk_vault::resolve_page_owner(root, Path::new(target)).into_diagnostic()?;
    if capture
        .candidates
        .iter()
        .any(|candidate| Path::new(&candidate.owner) == target_owner.as_path())
    {
        return Err(miette::miette!(
            "extraction capture lists its own target as a candidate"
        ));
    }
    Ok(SelectionInventory { capture, source })
}

/// The source an extraction is judged against: its retained input, its target and the
/// target's scoped reviews, without any candidate.
pub(super) fn extraction_source_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    reviews: &mut lk_vault::ReviewInventory,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let task = super::super::queue::select_task_controlled(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
        read,
    )?;
    read.check().into_diagnostic()?;
    let mut source = capture_task_with_audit_controlled(root, config, task, None, read)?;
    if source.context["purpose"]["task_kind"]
        != serde_json::to_value(lk_queue::TaskKind::ExtractConcepts).into_diagnostic()?
    {
        return Err(miette::miette!(
            "discovery recipes require concept extraction"
        ));
    }
    let (target, subjects) = review_subjects(root, &source, &BTreeSet::new())?;
    let mut reviews =
        lk_graph::audit::review_dependencies_with_inventory(root, &subjects, reviews, read)
            .into_diagnostic()?;
    let target_reviews =
        BTreeMap::from([(target.clone(), reviews.remove(&target).unwrap_or_default())]);
    let mut captured = Capture::resume(root, &source);
    capture_reviews(&mut captured, &mut source.context, &target_reviews)?;
    source.context["purpose"]["discovery"] = json!({
        "queries":[],
        "match_count":0,
        "match_revision":lk_core::concept::citation_digest(&[]),
    });
    source.context["purpose"]["extraction_scope"] = json!({"policy":"source-proposals-v1"});
    Ok(captured.finish(source.subject, source.context, config))
}

/// The target and the selected owners as review subjects, parsed from captured bytes.
fn review_subjects(
    root: &Path,
    prepared: &Prepared,
    selected: &BTreeSet<String>,
) -> miette::Result<(String, BTreeMap<String, lk_vault::VaultPage>)> {
    let Subject::Queue { target, .. } = &prepared.subject else {
        return Err(miette::miette!(
            "extraction evidence requires a queue subject"
        ));
    };
    let mut subjects = BTreeMap::new();
    for address in std::iter::once(target).chain(selected) {
        let owner = lk_vault::resolve_page_owner(root, Path::new(address)).into_diagnostic()?;
        let raw = prepared
            .pages
            .get(&owner)
            .ok_or_else(|| miette::miette!("captured extraction page is missing: {address}"))?;
        subjects.insert(
            address.clone(),
            lk_vault::parse_page(raw).map_err(|error| miette::miette!("{address}: {error}"))?,
        );
    }
    Ok((target.clone(), subjects))
}

/// The evidence a screened extraction reads: its source, in full or through its completed
/// observations, plus each selected owner and the scoped reviews of those owners, captured
/// from current bytes. Its context names exactly that, so the Work it identifies is
/// unchanged by candidates that were not selected: those stay bound to their judged cards.
pub(super) fn task_with_selected(
    root: &Path,
    config: &Config,
    source: &Prepared,
    selected: &BTreeSet<String>,
    source_observations: Option<&Value>,
    reviews: &mut lk_vault::ReviewInventory,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let mut captured = Capture::resume(root, source);
    for owner in selected {
        let exists = lk_vault::page_path(root, Path::new(owner))
            .into_diagnostic()?
            .try_exists()
            .into_diagnostic()?;
        if !exists
            || lk_vault::resolve_page_owner(root, Path::new(owner)).into_diagnostic()?
                != Path::new(owner)
        {
            return Err(super::publication_rejection(
                super::RejectionKind::EvidenceScopeChanged,
                "a selected candidate owner is missing or was retargeted; rescreen and regenerate the extraction result",
            ));
        }
        captured.page(owner, None, None, SemanticAuthority::Context)?;
    }
    let mut prepared = captured.finish(source.subject.clone(), source.context.clone(), config);
    let (_, subjects) = review_subjects(root, &prepared, selected)?;
    let reviews =
        lk_graph::audit::review_dependencies_with_inventory(root, &subjects, reviews, read)
            .into_diagnostic()?;
    let mut captured = Capture::resume(root, &prepared);
    capture_reviews(&mut captured, &mut prepared.context, &reviews)?;
    let purpose = prepared.context["purpose"]
        .as_object_mut()
        .ok_or_else(|| miette::miette!("extraction context has no purpose"))?;
    purpose.remove("discovery");
    purpose.insert(
        "extraction_scope".into(),
        json!({"policy":super::selection::POLICY, "selected":selected}),
    );
    if let Some(observations) = source_observations {
        purpose.insert("source_observations".into(), observations.clone());
    }
    let mut prepared = captured.finish(prepared.subject, prepared.context, config);
    prepared.selected_owners = Some(selected.clone());
    Ok(prepared)
}

/// Rebuild screened evidence from what its Work context names, without consulting the
/// candidate roster.
fn screened_evidence_controlled(
    root: &Path,
    config: &Config,
    config_revision: &str,
    task_id: &str,
    context: &Value,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let selected: BTreeSet<String> =
        serde_json::from_value(context["purpose"]["extraction_scope"]["selected"].clone())
            .into_diagnostic()?;
    let mut reviews = lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
        .into_diagnostic()?;
    let mut source = extraction_source_controlled(root, config, task_id, &mut reviews, read)?;
    let observations = if context["purpose"]["source_observations"].is_null() {
        None
    } else {
        let Some((observation, guards)) =
            super::completed_source_observation(root, config, config_revision, &source)?
        else {
            return Err(super::publication_rejection(
                super::RejectionKind::SemanticContextChanged,
                "source observations are no longer complete; replan and reread changed evidence",
            ));
        };
        source.pages.extend(guards);
        Some(observation)
    };
    task_with_selected(
        root,
        config,
        &source,
        &selected,
        observations.as_ref(),
        &mut reviews,
        read,
    )
}

pub(super) fn with_discovery_controlled(
    root: &Path,
    config: &Config,
    mut prepared: Prepared,
    queries: &[lk_core::discovery::DiscoveryQuery],
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    if prepared.context["purpose"]["task_kind"]
        != serde_json::to_value(lk_queue::TaskKind::ExtractConcepts).into_diagnostic()?
    {
        return Err(miette::miette!(
            "discovery recipes require concept extraction"
        ));
    }
    let queries = super::discovery::validate_queries(queries)?;
    let matches = lk_vault::discover_queries_controlled(root, &config.vault.dirs, &queries, read)
        .into_diagnostic()?;
    let target_address = match &prepared.subject {
        Subject::Queue { target, .. } => target,
        _ => return Err(miette::miette!("discovery requires a queue subject")),
    }
    .clone();
    let target = lk_vault::page_path(root, Path::new(&target_address))
        .into_diagnostic()?
        .canonicalize()
        .into_diagnostic()?;
    let mut selected = Vec::new();
    for citation in matches.into_values().flatten() {
        if lk_vault::page_path(root, Path::new(&citation.path))
            .into_diagnostic()?
            .canonicalize()
            .into_diagnostic()?
            != target
        {
            selected.push(citation);
        }
    }
    selected.sort();
    selected.dedup();
    let mut captured = Capture::resume(root, &prepared);
    let mut review_subjects = BTreeMap::from([(
        target_address,
        lk_vault::parse_page(
            captured
                .pages
                .get(target.strip_prefix(root).into_diagnostic()?)
                .ok_or_else(|| miette::miette!("captured extraction target is missing"))?,
        )
        .map_err(|error| miette::miette!("extraction target: {error}"))?,
    )]);
    for candidate in &selected {
        let raw = captured.page(
            &candidate.path,
            None,
            Some(&candidate.revision),
            SemanticAuthority::Context,
        )?;
        review_subjects.insert(
            candidate.path.clone(),
            lk_vault::parse_page(&raw)
                .map_err(|error| miette::miette!("{}: {error}", candidate.path))?,
        );
    }
    let reviews = lk_graph::audit::review_dependencies_controlled(
        root,
        &config.vault.dirs,
        &review_subjects,
        read,
    )
    .into_diagnostic()?;
    capture_reviews(&mut captured, &mut prepared.context, &reviews)?;
    prepared.context["purpose"]["discovery"] = json!({
        "queries":queries,
        "match_count":selected.len(),
        "match_revision":lk_core::concept::citation_digest(&selected),
    });
    Ok(captured.finish(prepared.subject, prepared.context, config))
}

pub(super) fn task_for_publication_controlled(
    root: &Path,
    config: &Config,
    task_id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let task = super::super::queue::select_task_controlled(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
        read,
    )?;
    capture_task_with_audit_controlled(root, config, task, None, read)
}

fn capture_task_with_audit_controlled(
    root: &Path,
    config: &Config,
    task: lk_queue::QueueTask,
    audit_input: Option<&lk_graph::audit::AuditCandidate>,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    if audit_input.is_some_and(|candidate| {
        task.kind != lk_queue::TaskKind::SynthesizeConcept
            || candidate.kind != lk_graph::audit::AuditSubjectKind::Concept
            || candidate.path != task.target.vault_path
    }) {
        return Err(miette::miette!(
            "synthesis audit input belongs to another target"
        ));
    }
    let mut captured = Capture::new(root, read);
    let mut graph_citations = None;
    let target = captured.page(
        &task.target.vault_path,
        None,
        None,
        SemanticAuthority::Subject,
    )?;
    if task.kind != lk_queue::TaskKind::SynthesizeConcept {
        captured.resources.clear();
    }
    let subject = Subject::Queue {
        task_id: task.task_id.clone(),
        cache_hash: task.cache_hash.clone(),
        target: task.target.vault_path.clone(),
        target_revision: file_revision(&target),
    };
    let mut purpose = task.input.clone();
    if let Some(text) = purpose.remove("text") {
        let text = text
            .as_str()
            .ok_or_else(|| miette::miette!("retained task text must be a string"))?;
        let address = format!(
            "retained-input://{}/{}",
            task.target.vault_path,
            task.target.kind.llm_inputs_key()
        );
        captured.resources.insert(
            address.clone(),
            SemanticResource {
                address,
                revision: file_revision(text),
                authority: SemanticAuthority::RetainedInput,
                text: text.to_owned(),
            },
        );
    }
    if let Some(citations) = purpose.remove("citations") {
        for citation in serde_json::from_value::<Vec<Citation>>(citations).into_diagnostic()? {
            captured.citation(&task.target.vault_path, &citation)?;
        }
    }
    if let Some(manifest) = task.input.get("rss_original") {
        let manifest: lk_core::rss_original::RssOriginalManifest =
            serde_json::from_value(manifest.clone()).into_diagnostic()?;
        let mut normalized = BTreeMap::new();
        for event in &manifest.events {
            let path = format!(
                "{}/events/{}.json",
                lk_core::rss_original::RSS_ORIGINAL_STORE,
                event.revision
            );
            let raw = captured.page(
                &path,
                Some(&event.revision),
                None,
                SemanticAuthority::RssEvent,
            )?;
            lk_core::rss_original::verify_payload(&manifest, event, raw.as_bytes())
                .map_err(|e| miette::miette!("{e}"))?;
            normalized.insert(event.event_id.clone(), event.revision.clone());
        }
        purpose.remove("rss_original");
        purpose.insert("rss_evidence".into(), json!({"manifest_revision": manifest.reference().map_err(|e| miette::miette!("{e}"))?, "events": normalized.len()}));
    }
    if task.kind == lk_queue::TaskKind::SynthesizeConcept {
        let loaded;
        let candidate = match audit_input {
            Some(candidate) => candidate,
            None => {
                loaded =
                    load_synthesis_input_controlled(root, config, &task.target.vault_path, read)?;
                &loaded
            }
        };
        graph_citations = Some(lk_core::concept::citation_digest(&candidate.evidence));
        let scoped = knowledge_candidate_controlled(root, config, candidate, read)?;
        captured.strict_reads.extend(scoped.strict_reads);
        for resource in scoped.resources {
            captured
                .resources
                .entry(resource.address.clone())
                .or_insert(resource);
        }
        for (path, raw) in scoped.pages {
            if captured.pages.get(&path).is_some_and(|prior| prior != &raw) {
                return Err(miette::miette!(
                    "synthesis dependency changed during capture: {}",
                    path.display()
                ));
            }
            captured.pages.insert(path, raw);
        }
        let scoped_reviews: Vec<String> =
            serde_json::from_value(scoped.context["purpose"]["review_dependencies"].clone())
                .into_diagnostic()?;
        let gaps = super::gap::open_reviews(
            root,
            &task.target.vault_path,
            &scoped_reviews,
            &captured.pages,
        )?;
        purpose.insert("knowledge_scope".into(), scoped.context["purpose"].clone());
        if !gaps.is_empty() {
            purpose.insert("evidence_gaps".into(), json!(gaps));
        }
    }
    let context = json!({
        "subject": subject,
        "purpose": {"operation":"queue", "task_kind":task.kind, "target":task.target, "parameters":purpose},
        "authority": "Retained input is the task source; the current target is publication context. Source/page/RSS evidence retains its declared authority. A read or part receipt is not factual verification.",
    });
    let mut prepared = captured.finish(subject, context, config);
    prepared.graph_citations = graph_citations;
    if task.kind == lk_queue::TaskKind::SynthesizeConcept {
        let snapshot =
            super::snapshot::ContextSnapshot::load(root, read, &task.task_id, &task.cache_hash)?;
        prepared = prepared.with_snapshot(snapshot)?;
    }
    Ok(prepared)
}

pub(super) fn audit(root: &Path, config: &Config, path: &str) -> miette::Result<Prepared> {
    audit_controlled(root, config, path, &lk_vault::ReadControl::default())
}

pub(super) fn audit_controlled(
    root: &Path,
    config: &Config,
    path: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let candidate = load_audit_candidate_controlled(root, config, path, read)?;
    audit_candidate_controlled(root, config, &candidate, read)
}

/// Digest of a concept's current graph citations: the set a synthesis request for it must
/// answer (`lk_core::concept::citation_digest`, as backlinks records it).
pub(in crate::commands) fn graph_citation_digest(
    root: &Path,
    config: &Config,
    concept: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<String> {
    let candidate = load_synthesis_input_controlled(root, config, concept, read)?;
    Ok(lk_core::concept::citation_digest(&candidate.evidence))
}

fn load_synthesis_input_controlled(
    root: &Path,
    config: &Config,
    concept: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<lk_graph::audit::AuditCandidate> {
    let pages = lk_graph::scan::scan_vault_controlled(root, config.graph.scope.follow_links, read)
        .into_diagnostic()?;
    let mut inputs = lk_graph::audit::load_synthesis_inputs_controlled(
        root,
        &config.vault.dirs,
        &pages,
        &[concept.to_owned()],
        read,
    )
    .into_diagnostic()?;
    if let Some(reason) = inputs.unavailable.remove(concept) {
        return Err(miette::Report::new(super::super::queue::SynthesisHeld {
            concept: concept.to_owned(),
            reason,
        }));
    }
    inputs
        .candidates
        .pop()
        .ok_or_else(|| miette::miette!("synthesis concept not found: {concept}"))
}

fn load_audit_candidate_controlled(
    root: &Path,
    config: &Config,
    path: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<lk_graph::audit::AuditCandidate> {
    let pages = lk_graph::scan::scan_vault_controlled(root, config.graph.scope.follow_links, read)
        .into_diagnostic()?;
    let plan = lk_graph::audit::plan_selected_controlled(
        root,
        &config.vault.dirs,
        lk_graph::audit::AuditCorpus::from(&config.graph.audit),
        &pages,
        1,
        &[path.to_owned()],
        u64::MAX,
        read,
    )
    .into_diagnostic()?;
    let candidate = plan
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| miette::miette!("audit subject not found: {path}"))?;
    Ok(candidate)
}

/// A synthesis reads its citations and direct links in full, and of its discovery matches
/// the ranked roster `lk_graph::audit::SYNTHESIS_CANDIDATES_PER_QUERY` bounds.
fn knowledge_candidate_controlled(
    root: &Path,
    config: &Config,
    candidate: &lk_graph::audit::AuditCandidate,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let mut related = candidate.linked_pages.clone();
    if let Some(discovery) = &candidate.discovery {
        let ranked = discovery.ranked.as_ref().ok_or_else(|| {
            miette::miette!("synthesis discovery was loaded without its ranked roster")
        })?;
        related.extend(
            ranked
                .iter()
                .flat_map(|(_, roster)| roster)
                .map(|matched| matched.citation.clone()),
        );
    }
    related.sort();
    related.dedup();
    capture_audit_scope_controlled(root, config, candidate, &related, None, read)
}

/// Preliminary capture from the controller's selected plan; publication rederives scope.
#[cfg(test)]
pub(super) fn audit_candidate(
    root: &Path,
    config: &Config,
    candidate: &lk_graph::audit::AuditCandidate,
) -> miette::Result<Prepared> {
    audit_candidate_controlled(root, config, candidate, &lk_vault::ReadControl::default())
}

pub(super) fn audit_candidate_controlled(
    root: &Path,
    config: &Config,
    candidate: &lk_graph::audit::AuditCandidate,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let selection = lk_graph::audit::selection::state_reference(candidate)
        .map_err(|error| miette::miette!("{error}"))?;
    capture_audit_scope_controlled(
        root,
        config,
        candidate,
        &candidate.related_pages,
        selection,
        read,
    )
}

fn capture_audit_scope_controlled(
    root: &Path,
    config: &Config,
    candidate: &lk_graph::audit::AuditCandidate,
    related: &[Citation],
    selection: Option<lk_graph::audit::selection::StateReference>,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let path = candidate.path.as_str();
    let mut captured = Capture::new(root, read);
    captured.page(
        path,
        Some(&candidate.file_revision),
        None,
        SemanticAuthority::Subject,
    )?;
    for citation in &candidate.evidence {
        captured.citation(path, citation)?;
    }
    for related in related {
        captured.page(
            &related.path,
            None,
            Some(&related.revision),
            SemanticAuthority::Context,
        )?;
    }
    for review in &candidate.reviews {
        captured.review(review)?;
    }
    let subject = Subject::Audit {
        path: path.to_owned(),
        file_revision: candidate.file_revision.clone(),
        input_revision: candidate.input_revision.clone(),
    };
    let mut context = json!({
        "subject": subject,
        "purpose": {"operation":"audit", "subject_kind":candidate.kind, "path":path, "title":candidate.title, "locale":config.vault.locale,
            "discovery_queries":candidate.discovery.as_ref().map(|discovery| &discovery.queries),
            "review_dependencies":candidate.reviews.iter().map(|review| &review.path).collect::<Vec<_>>()},
        "authority": "The selected page is the audit subject. Concept citations are evidence; a document is its own subject without a self-citation. Related pages are context, not automatically support. Reviews and historical archives are assessments/history, not independent corroboration. Join source-declared scope, time and attribution before judging contradictions; unknown context stays unknown.",
    });
    if let Some(reference) = selection {
        context["purpose"]["audit_selection"] =
            serde_json::to_value(reference).into_diagnostic()?;
    }
    let mut prepared = captured.finish(subject, context, config);
    prepared.audit_reviews = Some(candidate.reviews.clone());
    Ok(prepared)
}

#[cfg(test)]
pub(super) fn refresh(
    root: &Path,
    config: &Config,
    config_revision: &str,
    context: &Value,
) -> miette::Result<Prepared> {
    refresh_controlled(
        root,
        config,
        config_revision,
        context,
        &lk_vault::ReadControl::default(),
    )
}

pub(super) fn refresh_controlled(
    root: &Path,
    config: &Config,
    config_revision: &str,
    context: &Value,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    read.check().into_diagnostic()?;
    let subject: Subject = serde_json::from_value(
        context
            .get("subject")
            .cloned()
            .ok_or_else(|| miette::miette!("semantic work has no subject"))?,
    )
    .into_diagnostic()?;
    let prepared = match &subject {
        Subject::Queue { task_id, .. } => {
            if context["purpose"]["extraction_scope"]["policy"] == "source-proposals-v1" {
                source_task_controlled(root, config, task_id, read)?
            } else if is_screened(context) {
                screened_evidence_controlled(root, config, config_revision, task_id, context, read)?
            } else if context["purpose"]["extraction_scope"]
                .get("selection")
                .is_some()
            {
                return Err(super::publication_rejection(
                    super::RejectionKind::SemanticContextChanged,
                    "this Work was planned under the retired packet selection; replan it from current screening decisions",
                ));
            } else if let Some(discovery) = context["purpose"].get("discovery") {
                let queries: Vec<lk_core::discovery::DiscoveryQuery> =
                    serde_json::from_value(discovery["queries"].clone()).into_diagnostic()?;
                task_with_discovery_controlled(root, config, task_id, &queries, read)?
            } else {
                task_controlled(root, config, task_id, read)?
            }
        }
        Subject::Audit {
            path,
            file_revision,
            input_revision,
        } => {
            let target = lk_vault::page_path(root, Path::new(path)).into_diagnostic()?;
            match std::fs::symlink_metadata(target) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(super::publication_rejection(
                        super::RejectionKind::SemanticContextChanged,
                        "audit subject no longer exists; replan and reread its current scope",
                    ));
                }
                Err(error) => return Err(error).into_diagnostic(),
                Ok(_) => {}
            }
            let candidate = load_audit_candidate_controlled(root, config, path, read)?;
            if candidate.file_revision != *file_revision
                || candidate.input_revision != *input_revision
            {
                return Err(super::publication_rejection(
                    super::RejectionKind::SemanticContextChanged,
                    "audit subject inputs changed; replan and reread its current scope",
                ));
            }
            audit_candidate_controlled(root, config, &candidate, read)?
        }
    };
    read.check().into_diagnostic()?;
    prepared.validate_context(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, raw: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, raw).unwrap();
    }

    fn assert_same_prepared(expected: &Prepared, actual: &Prepared) {
        let encoded = |prepared: &Prepared| {
            serde_json::to_vec(&(
                &prepared.subject,
                &prepared.context,
                &prepared.resources,
                &prepared.pages,
            ))
            .unwrap()
        };
        assert_eq!(encoded(expected), encoded(actual));
    }

    #[test]
    fn selected_evidence_reads_and_guards_only_the_source_and_selected_owners() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(&config_path, "vault:\n  root: .\n  locale: en\nidentity: {name: Test, email: test@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n    concept_policy: supported-new\n").unwrap();
        let config = Config::load(&config_path).unwrap();
        let target = "wiki/documents/source.md";
        let selected = "wiki/concepts/selected.md";
        let excluded = "wiki/documents/excluded.md";
        let client = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue"));
        let task = client.build_task(
            "ext",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                rss_original: None,
                text: "Candidate has directly attributed source support.".into(),
                source_id: "notes".into(),
                source_type: lk_core::config::SourceType::Manual,
                date: jiff::civil::date(2026, 10, 22),
                focus: None,
                locale: lk_core::i18n::Locale::En,
                categories: vec![],
                target: lk_queue::TaskTarget {
                    vault_path: target.into(),
                    kind: lk_queue::TargetKind::DocumentConcepts,
                    anchor: "## Related Concepts".into(),
                },
            },
        );
        write(
            &root,
            target,
            &format!(
                "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nCandidate is source material.\n\n## Related Concepts\n",
                task.cache_hash
            ),
        );
        write(
            &root,
            selected,
            "---\ntype: concept\ntitle: Candidate owner\n---\n## Synthesis\nCandidate has a declared scope.\n",
        );
        let excluded_raw = "---\ntype: document\n---\nCandidate has different context.\n";
        write(&root, excluded, excluded_raw);
        write(
            &root,
            ".lorekeeper/queue/fixture.jsonl",
            &serde_json::to_string(&task).unwrap(),
        );
        let mut review_paths = Vec::new();
        let mut evidence_paths = Vec::new();
        for (name, subject) in [
            ("source", target),
            ("selected", selected),
            ("excluded", excluded),
        ] {
            let evidence = format!("wiki/documents/{name}-evidence.md");
            let raw = "---\ntype: document\n---\nDirect scoped evidence.\n";
            write(&root, &evidence, raw);
            let revision = source_revision(&lk_vault::parse_page(raw).unwrap());
            let review = format!("wiki/explorations/{name}-review.md");
            write(
                &root,
                &review,
                &format!(
                    "---\ntype: exploration\nreview:\n  status: open\n  subjects: [{subject}]\n  evidence: [{{path: {evidence}, revision: {revision}}}]\n---\nA scoped assessment.\n"
                ),
            );
            review_paths.push(review);
            evidence_paths.push(evidence);
        }
        let queries = vec![lk_core::discovery::DiscoveryQuery {
            query: "Candidate".into(),
            phrase: true,
            spacing: false,
        }];
        let has = |prepared: &Prepared, path: &str| {
            prepared
                .resources
                .iter()
                .any(|resource| resource.address == path)
        };
        let source = source_task(&root, &config, &task.task_id).unwrap();
        assert_eq!(
            source.context["purpose"]["extraction_scope"]["policy"],
            "source-proposals-v1"
        );
        for path in [&review_paths[0], &evidence_paths[0]] {
            assert!(has(&source, path));
        }
        for path in [selected, excluded, &review_paths[1], &review_paths[2]] {
            assert!(!has(&source, path));
        }
        let inventory = selection_inventory(&root, &config, &task.task_id, &queries).unwrap();
        assert_same_prepared(&source, &inventory.source);
        assert_eq!(
            inventory
                .capture
                .candidates
                .iter()
                .map(|candidate| candidate.owner.as_str())
                .collect::<Vec<_>>(),
            [selected, excluded],
            "the target is never its own candidate"
        );
        let admit = |owners: &[&str]| {
            let read = lk_vault::ReadControl::default();
            let mut reviews =
                lk_vault::ReviewInventory::load_controlled(&root, &config.vault.dirs, &read)
                    .into_diagnostic()?;
            task_with_selected(
                &root,
                &config,
                &source_task(&root, &config, &task.task_id)?,
                &owners.iter().map(|owner| (*owner).to_owned()).collect(),
                None,
                &mut reviews,
                &read,
            )
        };
        let admitted = admit(&[selected]).unwrap();
        let owner_raw = std::fs::read_to_string(root.join(selected)).unwrap();
        let bookkept = owner_raw.replacen(
            "title: Candidate owner\n",
            "title: Candidate owner\nsource_count: 3\nupdated: 2026-11-21\nllm_inputs:\n  synthesis: \"a\"\n",
            1,
        );
        write(&root, selected, &bookkept);
        let cited = admit(&[selected]).unwrap();
        assert_eq!(
            cited.revision("config").unwrap(),
            admitted.revision("config").unwrap(),
            "a citation or task-state update to a context page leaves the work that read it current"
        );
        assert!(cited.is_reuse_owner(selected, &file_revision(&bookkept)));
        assert!(
            !cited.is_reuse_owner(selected, &file_revision(&owner_raw)),
            "reuse binds the exact file it will write"
        );
        write(
            &root,
            selected,
            &bookkept.replace("declared scope", "narrower scope"),
        );
        assert_ne!(
            admit(&[selected]).unwrap().revision("config").unwrap(),
            admitted.revision("config").unwrap()
        );
        write(
            &root,
            selected,
            &owner_raw.replacen(
                "title: Candidate owner\n",
                "title: Candidate owner\nsource_count: &cited 3\npriority: *cited\n",
                1,
            ),
        );
        let anchored = admit(&[selected]).unwrap();
        let reading = &anchored
            .resources
            .iter()
            .find(|resource| resource.address == selected)
            .unwrap()
            .text;
        let reread = lk_vault::parse_page(reading).unwrap();
        assert_eq!(
            reread.frontmatter.fields["priority"],
            json!(3),
            "a context page that anchors a value on its bookkeeping reads as valid YAML"
        );
        assert!(!reread.frontmatter.fields.contains_key("source_count"));
        write(&root, selected, &owner_raw);
        #[cfg(unix)]
        {
            let roster = |prepared: &Prepared| {
                super::super::generation::Owners::build(&config, prepared)
                    .unwrap()
                    .presentation()
            };
            let owners = roster(&admitted);
            assert_eq!(owners.as_array().unwrap().len(), 1);
            assert_eq!(owners[0]["path"], selected);
            let mut other_authority = admitted.clone();
            other_authority
                .resources
                .iter_mut()
                .find(|resource| resource.address == selected)
                .unwrap()
                .authority = SemanticAuthority::Page;
            assert_eq!(roster(&other_authority), owners);
            let revision = admitted.revision("config").unwrap();
            let mut empty = admitted.clone();
            empty.selected_owners = Some(BTreeSet::new());
            assert_eq!(roster(&empty), json!([]));
            assert_eq!(empty.revision("config").unwrap(), revision);
            let mut extra = admitted.clone();
            extra.resources.push(SemanticResource {
                address: "wiki/concepts/unselected-review-evidence.md".into(),
                revision: "a".repeat(64),
                authority: SemanticAuthority::Context,
                text: "---\ntype: concept\n---\nUnselected review evidence.\n".into(),
            });
            assert_eq!(roster(&extra), owners);
        }
        for path in [
            target,
            selected,
            &review_paths[0],
            &evidence_paths[0],
            &review_paths[1],
            &evidence_paths[1],
        ] {
            assert!(has(&admitted, path) || path == target);
            assert!(admitted.pages.contains_key(Path::new(path)));
        }
        for path in [excluded, &review_paths[2], &evidence_paths[2]] {
            assert!(!has(&admitted, path));
            assert!(
                !admitted.pages.contains_key(Path::new(path)),
                "a candidate that was not selected is bound by its judged card, not its page"
            );
        }
        assert!(admitted.resources.iter().any(|resource| {
            resource.address == selected && resource.authority == SemanticAuthority::Context
        }));
        assert!(has(
            &task_with_discovery(&root, &config, &task.task_id, &queries).unwrap(),
            excluded
        ));

        let revision = admitted.revision("config").unwrap();
        assert!(admitted.context["purpose"].get("discovery").is_none());
        assert_eq!(
            admitted.context["purpose"]["extraction_scope"],
            json!({"policy":"independent-screening-v1","selected":[selected]}),
            "the context names what the Work reads, not the roster it was admitted from"
        );
        let refresh = || {
            refresh_controlled(
                &root,
                &config,
                "config",
                &admitted.context,
                &lk_vault::ReadControl::default(),
            )
        };
        let refreshed = refresh().unwrap();
        assert_same_prepared(&admitted, &refreshed);
        assert!(refreshed.roster.is_none());
        for path in [excluded, &review_paths[2], &evidence_paths[2]] {
            let raw = std::fs::read_to_string(root.join(path)).unwrap();
            write(&root, path, &format!("{raw}\nExternally edited.\n"));
            assert_eq!(
                admit(&[selected]).unwrap().revision("config").unwrap(),
                revision
            );
        }
        write(&root, "wiki/documents/new.md", excluded_raw);
        assert_eq!(
            refresh().unwrap().revision("config").unwrap(),
            revision,
            "unread candidates leave the Work's identity unchanged"
        );
        std::fs::remove_file(root.join("wiki/documents/new.md")).unwrap();
        for path in [selected, &review_paths[1], &evidence_paths[1]] {
            let raw = std::fs::read_to_string(root.join(path)).unwrap();
            write(&root, path, &format!("{raw}\nExternally edited.\n"));
            assert_ne!(
                admit(&[selected]).unwrap().revision("config").unwrap(),
                revision
            );
            assert_ne!(
                refresh()
                    .ok()
                    .map(|current| current.revision("config").unwrap()),
                Some(revision.clone())
            );
            write(&root, path, &raw);
        }
        let mut retired = admitted.context.clone();
        retired["purpose"]["extraction_scope"] =
            json!({"policy":"progressive-evidence-v1","selection":{}});
        assert!(
            refresh_controlled(
                &root,
                &config,
                "config",
                &retired,
                &lk_vault::ReadControl::default(),
            )
            .err()
            .unwrap()
            .to_string()
            .contains("retired packet selection")
        );
        assert_eq!(
            admit(&[selected]).unwrap().revision("config").unwrap(),
            revision
        );
        let missing = admit(&[selected, "wiki/concepts/retired.md"])
            .err()
            .unwrap();
        assert_eq!(
            missing
                .downcast_ref::<crate::commands::queue::application::PublicationRejection>()
                .map(|rejection| rejection.kind),
            Some(super::super::RejectionKind::EvidenceScopeChanged)
        );

        let selected_raw = std::fs::read_to_string(root.join(selected)).unwrap();
        let selected_revision = source_revision(&lk_vault::parse_page(&selected_raw).unwrap());
        write(
            &root,
            &review_paths[0],
            &format!(
                "---\ntype: exploration\nreview:\n  status: open\n  subjects: [{target}, {selected}]\n  evidence: [{{path: {selected}, revision: {selected_revision}}}]\n---\nA shared source assessment.\n"
            ),
        );
        let invalid_review = std::fs::read_to_string(root.join(&review_paths[1])).unwrap();
        write(
            &root,
            &review_paths[1],
            &invalid_review.replace("status: open", "status: unresolved"),
        );
        let archive = lk_vault::archive_evidence(&root, &[selected.into()]).unwrap();
        write(
            &root,
            selected,
            &format!("{selected_raw}\nChanged evidence.\n"),
        );
        let shared = selection_inventory(&root, &config, &task.task_id, &queries).unwrap();
        assert_same_prepared(
            &source_task(&root, &config, &task.task_id).unwrap(),
            &shared.source,
        );
        let card = &shared
            .capture
            .candidates
            .iter()
            .find(|candidate| candidate.owner == selected)
            .unwrap()
            .card;
        assert_eq!(card.reviews.invalid, 1);
        assert_eq!(card.reviews.needs_reassessment, 1);
        let archived = lk_vault::evidence_archive_path(&archive.citations[0].revision).unwrap();
        assert!(shared.source.resources.iter().any(|resource| {
            resource.address == lk_core::vault_path::address(&archived)
                && resource.authority == SemanticAuthority::Archive
        }));
        assert!(
            admit(&[selected])
                .unwrap()
                .resources
                .iter()
                .any(|resource| {
                    resource.address == selected && resource.authority == SemanticAuthority::Page
                }),
            "a selected owner that is also mandatory source evidence keeps its page authority"
        );
        let empty = selection_inventory(&root, &config, &task.task_id, &[]).unwrap();
        assert!(empty.capture.candidates.is_empty());
        assert_same_prepared(&shared.source, &empty.source);
    }

    #[test]
    fn audit_result_dependencies_are_validated_before_semantic_review_holds() {
        let path = "wiki/documents/subject.md";
        let review = "wiki/explorations/review.md";
        let prepared = Prepared {
            read: lk_vault::ReadControl::default(),
            reading: ContextReading::Semantic,
            semantic_config_revision: None,
            subject: Subject::Audit {
                path: path.into(),
                file_revision: "file".into(),
                input_revision: "input".into(),
            },
            context: json!({"purpose":{"review_dependencies":[review]}}),
            resources: Vec::new(),
            pages: BTreeMap::new(),
            strict_reads: BTreeSet::new(),
            snapshot: None,
            roster: None,
            graph_citations: None,
            audit_reviews: Some(vec![lk_graph::audit::AuditReviewDependency {
                path: review.into(),
                file_revision: None,
                source_revision: None,
                state: lk_graph::audit::AuditReviewState::NeedsReassessment,
                errors: Vec::new(),
                evidence: Vec::new(),
            }]),
            selected_owners: None,
        };
        let mut result = json!({"assessments":[{
            "path":path, "file_revision":"file", "input_revision":"input",
            "disposition":"reviewed", "reason":"Captured scoped assessment.", "reviews":[]
        }]});
        let error = prepared.validate_audit_result(&result).err().unwrap();
        assert!(error.downcast_ref::<lk_graph::GraphError>().is_none());
        assert!(
            error
                .to_string()
                .contains("must list all exact current scoped review dependencies")
        );
        result["assessments"][0]["reviews"] = json!([review]);
        let error = prepared.validate_audit_result(&result).err().unwrap();
        assert!(matches!(
            error.downcast_ref::<lk_graph::GraphError>(),
            Some(lk_graph::GraphError::AuditAssessment(
                lk_graph::audit::AuditAssessmentError::ReviewNotCurrent {
                    state: lk_graph::audit::AuditReviewState::NeedsReassessment,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn selected_audit_capture_matches_live_input_and_refuses_dependency_drift() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let documents = root.join("wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        std::fs::write(
            documents.join("a.md"),
            "---\ntype: document\ntitle: A\n---\nSubject. [Context](b.md)\n",
        )
        .unwrap();
        std::fs::write(
            documents.join("b.md"),
            "---\ntype: document\ntitle: B\n---\nContext.\n",
        )
        .unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(
            &config_path,
            "vault:\n  root: .\nidentity:\n  name: Test\n  email: test@example.com\nsources:\n  notes:\n    type: manual\n    enabled: false\n    params: {inbox_dir: inbox}\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        let pages = lk_graph::scan::scan_vault(&root, false).unwrap();
        let plan = lk_graph::audit::plan_selected(
            &root,
            &config.vault.dirs,
            lk_graph::audit::AuditCorpus::from(&config.graph.audit),
            &pages,
            1,
            &["wiki/documents/a.md".into()],
            u64::MAX,
        )
        .unwrap();
        let candidate = &plan.candidates[0];
        let live = audit(&root, &config, &candidate.path).unwrap();
        let selected = audit_candidate(&root, &config, candidate).unwrap();
        assert_eq!(live.subject, selected.subject);
        assert_eq!(live.context, selected.context);
        assert_eq!(live.resources, selected.resources);
        assert_eq!(live.pages, selected.pages);
        std::fs::write(documents.join("unrelated.md"), "---\ninvalid: [\n---\n").unwrap();
        assert!(audit_candidate(&root, &config, candidate).is_ok());
        std::fs::write(
            documents.join("b.md"),
            "---\ntype: document\ntitle: B\n---\nChanged context.\n",
        )
        .unwrap();
        assert!(audit_candidate(&root, &config, candidate).is_err());
    }

    fn synthesis_batch_fixture() -> (
        tempfile::TempDir,
        Config,
        Vec<lk_queue::QueueTask>,
        Vec<lk_graph::audit::AuditCandidate>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(&config_path, "vault:\n  root: .\n  locale: en\nidentity: {name: Test, email: test@example.com}\nsources:\n  notes:\n    type: manual\n    enabled: false\n    params: {inbox_dir: inbox}\n").unwrap();
        let config = Config::load(&config_path).unwrap();
        let source = "daily/source.md";
        let raw = "---\ntype: daily\ntitle: Source\n---\n## Summary\nOne scoped source statement.\n\n## Related Concepts\n[A](../wiki/concepts/a.md) and [B](../wiki/concepts/b.md).\n";
        write(&root, source, raw);
        write(
            &root,
            "wiki/concepts/context.md",
            "---\ntype: concept\ntitle: Context\n---\n## Synthesis\nContext with a distinct authority.\n",
        );
        let citation =
            lk_core::concept::Citation::new(Path::new(source), &lk_vault::parse_page(raw).unwrap());
        let digest = lk_core::concept::citation_digest(std::slice::from_ref(&citation));
        let client = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue"));
        let mut tasks = Vec::new();
        for name in ["a", "b"] {
            let path = format!("wiki/concepts/{name}.md");
            write(
                &root,
                &path,
                &format!(
                    "---\ntype: concept\ntitle: {name}\nsource_count: 1\nllm_inputs: {{synthesis: {digest}}}\n---\n## Synthesis\nAn earlier scoped statement.\n\n## Sources\n\n## Related Concepts\n[Context](context.md)\n"
                ),
            );
            tasks.push(client.build_task(
                &digest,
                lk_queue::TaskKind::SynthesizeConcept,
                lk_queue::ConceptSynthesisRequest {
                    citations: vec![citation.clone()],
                    locale: lk_core::i18n::Locale::En,
                    related_anchor: Some("## Related Concepts".into()),
                    target: lk_queue::TaskTarget {
                        vault_path: path,
                        kind: lk_queue::TargetKind::ConceptSynthesis,
                        anchor: "## Synthesis".into(),
                    },
                },
            ));
        }
        write(
            &root,
            ".lorekeeper/queue/tasks.jsonl",
            &tasks
                .iter()
                .map(|task| serde_json::to_string(task).unwrap() + "\n")
                .collect::<String>(),
        );
        write(
            &root,
            "wiki/explorations/review.md",
            &format!(
                "---\ntype: exploration\ntitle: Shared assessment\nreview:\n  status: open\n  subjects: [wiki/concepts/a.md, wiki/concepts/b.md]\n  evidence: [{{path: {source}, revision: {}}}]\n---\n## Question\nDoes the source support these conditions?\n",
                citation.revision
            ),
        );
        let pages = lk_graph::scan::scan_vault(&root, false).unwrap();
        let candidates = lk_graph::audit::load_synthesis_inputs_controlled(
            &root,
            &config.vault.dirs,
            &pages,
            &tasks
                .iter()
                .map(|task| task.target.vault_path.clone())
                .collect::<Vec<_>>(),
            &lk_vault::ReadControl::default(),
        )
        .unwrap()
        .candidates;
        (directory, config, tasks, candidates)
    }

    #[test]
    fn batched_synthesis_capture_matches_standalone_scope_and_rejects_wrong_audit_subjects() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (directory, config, tasks, candidates) = synthesis_batch_fixture();
        let root = directory.path().canonicalize().unwrap();
        assert_eq!(candidates.len(), 2);
        for (task, candidate) in tasks.iter().zip(&candidates) {
            assert_eq!(candidate.path, task.target.vault_path);
            assert_eq!(candidate.evidence.len(), 1);
            assert_eq!(candidate.evidence[0].path, "daily/source.md");
            assert_eq!(candidate.reviews.len(), 1);
            let standalone = task_with_audit(&root, &config, &task.task_id, None).unwrap();
            let batched = task_with_audit(&root, &config, &task.task_id, Some(candidate)).unwrap();
            assert_same_prepared(&standalone, &batched);
            let cancelled = Arc::new(AtomicBool::new(false));
            let read = lk_vault::ReadControl::new(cancelled.clone());
            let controlled =
                task_with_audit_controlled(&root, &config, &task.task_id, Some(candidate), &read)
                    .unwrap();
            assert_same_prepared(&batched, &controlled);
            assert_eq!(
                batched.revision("config-revision").unwrap(),
                controlled.revision("config-revision").unwrap()
            );
            cancelled.store(true, Ordering::Relaxed);
            assert!(controlled.clone().read_control().is_cancelled());
            assert!(task_controlled(&root, &config, &task.task_id, &read).is_err());
            assert!(section_task_controlled(&root, &config, &task.task_id, &read).is_err());
            assert!(source_task_controlled(&root, &config, &task.task_id, &read).is_err());
            assert!(audit_candidate_controlled(&root, &config, candidate, &read).is_err());
            assert!(
                refresh_controlled(
                    &root,
                    &config,
                    "config-revision",
                    &controlled.context,
                    &read
                )
                .is_err()
            );
            assert_eq!(
                standalone.revision("config-revision").unwrap(),
                batched.revision("config-revision").unwrap()
            );
            assert!(batched.resources.iter().any(|resource| {
                resource.address == "wiki/concepts/context.md"
                    && resource.authority == SemanticAuthority::Context
            }));
            #[cfg(unix)]
            assert_eq!(
                super::super::generation::Owners::build(&config, &batched)
                    .unwrap()
                    .presentation(),
                json!([])
            );
        }
        assert!(task_with_audit(&root, &config, &tasks[0].task_id, Some(&candidates[1])).is_err());
        let mut wrong_kind = candidates[0].clone();
        wrong_kind.kind = lk_graph::audit::AuditSubjectKind::Document;
        assert!(task_with_audit(&root, &config, &tasks[0].task_id, Some(&wrong_kind)).is_err());
    }

    #[test]
    fn batched_synthesis_capture_refuses_drift_and_posthost_refresh_rederives_membership() {
        let (directory, config, tasks, candidates) = synthesis_batch_fixture();
        let root = directory.path().canonicalize().unwrap();
        let task = &tasks[0];
        let candidate = &candidates[0];
        let prepared = task_with_audit(&root, &config, &task.task_id, Some(candidate)).unwrap();
        assert_same_prepared(
            &prepared,
            &refresh(&root, &config, "config-revision", &prepared.context).unwrap(),
        );
        let assert_invalidated = || {
            if let Ok(current) = refresh(&root, &config, "config-revision", &prepared.context) {
                assert_ne!(
                    current.revision("config-revision").unwrap(),
                    prepared.revision("config-revision").unwrap(),
                    "fresh capture must invalidate the stored work input"
                );
            }
        };
        for path in [
            task.target.vault_path.as_str(),
            "daily/source.md",
            "wiki/concepts/context.md",
            "wiki/explorations/review.md",
        ] {
            let before = std::fs::read_to_string(root.join(path)).unwrap();
            write(&root, path, &format!("{before}\nChanged interpretation.\n"));
            assert!(
                task_with_audit(&root, &config, &task.task_id, Some(candidate)).is_err(),
                "old batch must refuse changed {path}"
            );
            assert_invalidated();
            write(&root, path, &before);
        }
        write(
            &root,
            "daily/added.md",
            "---\ntype: daily\ntitle: Added evidence\n---\nAnother scoped source. [A](../wiki/concepts/a.md)\n",
        );
        assert_invalidated();
        std::fs::remove_file(root.join("daily/added.md")).unwrap();
        write(
            &root,
            "wiki/explorations/added.md",
            &format!(
                "---\ntype: exploration\ntitle: Newly scoped review\nreview:\n  status: open\n  subjects: [wiki/concepts/a.md]\n  evidence: [{{path: daily/source.md, revision: {}}}]\n---\n## Question\nA separate assessment.\n",
                candidate.evidence[0].revision
            ),
        );
        assert_invalidated();
        std::fs::remove_file(root.join("wiki/explorations/added.md")).unwrap();
        assert_same_prepared(
            &prepared,
            &refresh(&root, &config, "config-revision", &prepared.context).unwrap(),
        );
    }
    #[test]
    fn a_synthesis_request_reads_context_from_its_snapshot_and_everything_else_current() {
        let (directory, config, tasks, _) = synthesis_batch_fixture();
        let root = directory.path().canonicalize().unwrap();
        let (context, source, revision) = ("wiki/concepts/context.md", "daily/source.md", "config");
        let persist = |task: &lk_queue::QueueTask| {
            let mut prepared = task_with_audit(&root, &config, &task.task_id, None).unwrap();
            let session = lk_vault::MutationSession::acquire(&root).unwrap();
            super::super::persist_prepared(&session, &config, revision, &mut prepared).unwrap()
        };
        let resumed =
            |task: &lk_queue::QueueTask| -> miette::Result<(super::super::Work, Prepared)> {
                let mut prepared = task_with_audit(&root, &config, &task.task_id, None)?;
                let work = super::super::expected(&root, &mut prepared, &config, revision)?;
                Ok((work, prepared))
            };
        let work = persist(&tasks[0]);
        let original = std::fs::read_to_string(root.join(context)).unwrap();
        let resynthesized = format!("{original}\nA resynthesized neighbor body.\n");
        write(&root, context, &resynthesized);

        let (continued, prepared) = resumed(&tasks[0]).unwrap();
        assert_eq!(
            continued, work,
            "a neighbor's resynthesis keeps the request's Work"
        );
        assert_eq!(
            prepared.captured_page(context),
            Some(resynthesized.as_str()),
            "publication still guards the live page"
        );
        let (unsnapshotted, _) = resumed(&tasks[1]).unwrap();
        assert!(
            !unsnapshotted.is_planned(&root).unwrap(),
            "a request without a snapshot reads the page as it is now"
        );

        let cited = std::fs::read_to_string(root.join(source)).unwrap();
        write(
            &root,
            source,
            &format!("{cited}\nA changed source claim.\n"),
        );
        assert!(
            !resumed(&tasks[0]).is_ok_and(|(current, _)| current == work),
            "a changed citation is a current read"
        );
        write(&root, source, &cited);
        assert_eq!(resumed(&tasks[0]).unwrap().0, work);

        let citation = lk_core::concept::Citation::new(
            Path::new(context),
            &lk_vault::parse_page(&resynthesized).unwrap(),
        );
        write(
            &root,
            "wiki/explorations/neighbor.md",
            &format!(
                "---\ntype: exploration\ntitle: Neighbor assessment\nreview:\n  status: open\n  subjects: [wiki/concepts/a.md]\n  evidence: [{{path: {context}, revision: {}}}]\n---\n## Question\nDoes the neighbor hold?\n",
                citation.revision
            ),
        );
        let (strict, prepared) = resumed(&tasks[0]).unwrap();
        assert_ne!(
            strict, work,
            "review evidence is a current read even where it is context"
        );
        assert!(prepared.resources.iter().any(|resource| {
            resource.address == context && resource.text.contains("A resynthesized neighbor body.")
        }));
        std::fs::remove_file(root.join("wiki/explorations/neighbor.md")).unwrap();

        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        let pending = |task: &lk_queue::QueueTask| {
            BTreeSet::from([(task.task_id.clone(), task.cache_hash.clone())])
        };
        let mut plan = lk_vault::MutationPlan::default();
        let kept =
            super::super::snapshot::plan_prune(&root, &pending(&tasks[0]), &mut plan).unwrap();
        assert_eq!(kept, 0);
        let removed =
            super::super::snapshot::plan_prune(&root, &pending(&tasks[1]), &mut plan).unwrap();
        assert_eq!(removed, 1);
        session.apply(plan).unwrap();
        drop(session);
        assert!(
            !resumed(&tasks[0]).unwrap().0.is_planned(&root).unwrap(),
            "without its snapshot the request reads the neighbor as it is now"
        );
    }

    #[test]
    fn synthesis_reads_a_fixed_ranked_roster_and_only_its_own_audit_records() {
        let (directory, config, tasks, _) = synthesis_batch_fixture();
        let root = directory.path().canonicalize().unwrap();
        let task = &tasks[0];
        let path = task.target.vault_path.as_str();
        let limit = lk_graph::audit::SYNTHESIS_CANDIDATES_PER_QUERY;
        let discovered = (0..limit + 2)
            .map(|index| format!("wiki/documents/match-{index:03}.md"))
            .collect::<Vec<_>>();
        for (index, address) in discovered.iter().enumerate() {
            write(
                &root,
                address,
                &format!(
                    "---\ntype: document\ntitle: Match {index}\n---\nScreen candidate {index}.\n"
                ),
            );
        }
        let pages = lk_graph::scan::scan_vault(&root, false).unwrap();
        let raw = std::fs::read_to_string(root.join(path)).unwrap();
        lk_graph::audit::record_discovery(
            &lk_vault::MutationSession::acquire(&root).unwrap(),
            &config.vault.dirs,
            &pages,
            lk_graph::audit::AuditDiscoveryRequest {
                path: path.into(),
                file_revision: file_revision(&raw),
                queries: vec![lk_core::discovery::DiscoveryQuery {
                    query: "screen candidate".into(),
                    phrase: true,
                    spacing: false,
                }],
            },
        )
        .unwrap();
        let prepared = task_with_audit(&root, &config, &task.task_id, None).unwrap();
        let read = discovered
            .iter()
            .filter(|address| {
                prepared
                    .resources
                    .iter()
                    .any(|resource| &resource.address == *address)
            })
            .collect::<Vec<_>>();
        assert_eq!(read, discovered[..limit].iter().collect::<Vec<_>>());

        let config_path = root.join("config.yaml");
        let yaml = std::fs::read_to_string(&config_path).unwrap();
        std::fs::write(
            &config_path,
            format!("{yaml}execution:\n  work: {{max_query_candidates: 1}}\n"),
        )
        .unwrap();
        let paced = Config::load(&config_path).unwrap();
        assert_same_prepared(
            &prepared,
            &task_with_audit(&root, &paced, &task.task_id, None).unwrap(),
        );

        let store = root.join(".lorekeeper/knowledge-audit.json");
        let mut audit: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        audit["entries"]["wiki/concepts/unrelated.md"] = json!({"unreadable": true});
        std::fs::write(&store, audit.to_string()).unwrap();
        assert!(
            lk_graph::audit::load_selected_inputs(
                &root,
                &config.vault.dirs,
                &pages,
                &[path.into()]
            )
            .is_err(),
            "the audit still reports its unreadable record"
        );
        assert_same_prepared(
            &prepared,
            &task_with_audit(&root, &config, &task.task_id, None).unwrap(),
        );

        let recipe = audit["discovery"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        recipe["queries"] = json!([]);
        std::fs::write(&store, audit.to_string()).unwrap();
        let inputs = lk_graph::audit::load_synthesis_inputs_controlled(
            &root,
            &config.vault.dirs,
            &pages,
            &[path.to_owned()],
            &lk_vault::ReadControl::default(),
        )
        .unwrap();
        assert!(inputs.candidates.is_empty());
        assert!(inputs.unavailable[path].contains("discovery recipe"));
        assert!(
            graph_citation_digest(&root, &config, path, &lk_vault::ReadControl::default())
                .unwrap_err()
                .downcast_ref::<super::super::super::queue::SynthesisHeld>()
                .is_some()
        );

        let recipe = audit["discovery"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        recipe["subject_revision"] = json!("an earlier revision");
        recipe["queries"] = json!("unreadable");
        std::fs::write(&store, audit.to_string()).unwrap();
        let inputs = lk_graph::audit::load_synthesis_inputs_controlled(
            &root,
            &config.vault.dirs,
            &pages,
            &[path.to_owned()],
            &lk_vault::ReadControl::default(),
        )
        .unwrap();
        assert!(
            inputs.unavailable.is_empty(),
            "a recipe for an earlier revision is not read"
        );
        assert_eq!(inputs.candidates.len(), 1);

        let raw = std::fs::read_to_string(&store).unwrap();
        std::fs::write(&store, raw.replacen("{", "{\"pending\":[],", 1)).unwrap();
        assert!(
            lk_graph::audit::load_synthesis_inputs_controlled(
                &root,
                &config.vault.dirs,
                &pages,
                &[path.to_owned()],
                &lk_vault::ReadControl::default(),
            )
            .is_err(),
            "a duplicate field leaves the store ambiguous"
        );
    }

    #[test]
    fn synthesis_reads_discovery_context_without_audit_screening_state() {
        let (directory, config, tasks, _) = synthesis_batch_fixture();
        let root = directory.path().canonicalize().unwrap();
        let task = &tasks[0];
        let path = task.target.vault_path.as_str();
        let discovered = "wiki/documents/discovered.md";
        let discovered_raw = "---\ntype: document\ntitle: Discovered scope\n---\nScreen candidate has a distinct condition.\n";
        write(&root, discovered, discovered_raw);
        let pages = || lk_graph::scan::scan_vault(&root, false).unwrap();
        let candidate = || {
            lk_graph::audit::load_selected_inputs(
                &root,
                &config.vault.dirs,
                &pages(),
                &[path.into()],
            )
            .unwrap()
            .remove(0)
        };
        let synthesis_input = || {
            lk_graph::audit::load_synthesis_inputs_controlled(
                &root,
                &config.vault.dirs,
                &pages(),
                &[path.into()],
                &lk_vault::ReadControl::default(),
            )
            .unwrap()
            .candidates
            .remove(0)
        };
        let raw = std::fs::read_to_string(root.join(path)).unwrap();
        lk_graph::audit::record_discovery(
            &lk_vault::MutationSession::acquire(&root).unwrap(),
            &config.vault.dirs,
            &pages(),
            lk_graph::audit::AuditDiscoveryRequest {
                path: path.into(),
                file_revision: file_revision(&raw),
                queries: vec![lk_core::discovery::DiscoveryQuery {
                    query: "screen candidate".into(),
                    phrase: true,
                    spacing: false,
                }],
            },
        )
        .unwrap();
        let pending = candidate();
        assert_eq!(
            pending
                .discovery
                .as_ref()
                .unwrap()
                .selection
                .as_ref()
                .unwrap()
                .unknown,
            1
        );
        assert!(audit_candidate(&root, &config, &pending).is_err());
        let mut readiness = crate::commands::audit::InputAvailability::load_controlled(
            &root,
            &lk_vault::ReadControl::default(),
        )
        .unwrap();
        assert!(
            !readiness.check(&pending).unwrap(),
            "the synthesis prerequisite blocks audit"
        );
        let before =
            task_with_audit(&root, &config, &task.task_id, Some(&synthesis_input())).unwrap();
        assert_same_prepared(
            &before,
            &task_with_audit(&root, &config, &task.task_id, None).unwrap(),
        );
        assert!(
            before
                .resources
                .iter()
                .any(|resource| resource.address == discovered)
        );
        assert_eq!(before.pages[Path::new(discovered)], discovered_raw);
        assert!(
            before.context["purpose"]["parameters"]["knowledge_scope"]
                .get("audit_selection")
                .is_none()
        );
        let limits = lk_graph::audit::selection::ScreeningLimits {
            max_input_bytes: 65536,
            max_result_bytes: 32768,
            max_candidates: 20,
            max_batches: None,
        };
        let packet = lk_graph::audit::selection::candidates(
            &root,
            &config.vault.dirs,
            &pages(),
            path,
            limits,
            false,
            &lk_vault::ReadControl::default(),
        )
        .unwrap();
        let request = packet.request.unwrap();
        assert_eq!(request.candidates.len(), 1);
        lk_graph::audit::selection::record(
            &lk_vault::MutationSession::acquire(&root).unwrap(),
            &config.vault.dirs,
            &pages(),
            lk_graph::audit::selection::ScreeningOutput::Record {
                path: path.into(),
                purpose_revision: request.purpose_revision,
                packet_revision: packet.request_revision.unwrap(),
                reassess: false,
                decisions: request
                    .candidates
                    .iter()
                    .map(|card| lk_graph::audit::selection::CandidateDecision {
                        candidate_key: card.candidate_key.clone(),
                        decision: lk_core::discovery::ScreeningDecision::NotSelected,
                        reason: "The declared condition is outside this audit.".into(),
                    })
                    .collect(),
            },
            limits,
            &lk_vault::ReadControl::default(),
        )
        .unwrap();
        let ready = candidate();
        let audit = audit_candidate(&root, &config, &ready).unwrap();
        assert!(
            !audit
                .resources
                .iter()
                .any(|resource| resource.address == discovered)
        );
        assert_same_prepared(
            &before,
            &task_with_audit(&root, &config, &task.task_id, Some(&synthesis_input())).unwrap(),
        );
        let state = root.join(lk_graph::audit::selection::DIRECTORY);
        let parked = root.join("parked-audit-selection");
        std::fs::rename(&state, &parked).unwrap();
        assert!(
            lk_graph::audit::load_selected_inputs(
                &root,
                &config.vault.dirs,
                &pages(),
                &[path.into()]
            )
            .is_err(),
            "missing screening state stays the audit's error"
        );
        assert_same_prepared(
            &before,
            &task_with_audit(&root, &config, &task.task_id, None).unwrap(),
        );
        std::fs::rename(&parked, &state).unwrap();
        write(
            &root,
            discovered,
            &(discovered_raw.to_owned() + "\nA changed condition.\n"),
        );
        let changed = task_with_audit(&root, &config, &task.task_id, None).unwrap();
        assert_ne!(
            before.revision("policy").unwrap(),
            changed.revision("policy").unwrap()
        );
        let error = refresh_controlled(
            &root,
            &config,
            "policy",
            &audit.context,
            &lk_vault::ReadControl::default(),
        )
        .err()
        .unwrap();
        assert!(
            matches!(error.downcast_ref::<crate::commands::queue::application::PublicationRejection>(),
            Some(rejection) if rejection.kind == crate::commands::queue::application::RejectionKind::SemanticContextChanged)
        );
    }
}
