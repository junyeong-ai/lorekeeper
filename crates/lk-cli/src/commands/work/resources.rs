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

#[derive(Clone)]
pub(super) struct Prepared {
    pub subject: Subject,
    pub context: Value,
    pub resources: Vec<SemanticResource>,
    pub pages: BTreeMap<PathBuf, String>,
}

pub(super) struct SelectionInventory {
    pub revision: String,
    pub capture: lk_vault::DiscoveryCapture,
    pub pages: BTreeMap<PathBuf, String>,
    pub source: Prepared,
    resources: BTreeMap<String, SemanticResource>,
    review_resources: BTreeMap<String, BTreeSet<String>>,
    reviews: BTreeMap<String, Vec<lk_graph::audit::AuditReviewDependency>>,
}

impl Prepared {
    pub fn revision(&self, config_revision: &str) -> miette::Result<String> {
        let resources = self
            .resources
            .iter()
            .map(|resource| (&resource.address, &resource.revision, resource.authority))
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&(config_revision, &self.subject, &self.context, resources))
            .into_diagnostic()?;
        Ok(lk_core::rss_original::digest(&bytes))
    }

    pub fn guard_pages(&self, plan: &mut lk_vault::MutationPlan) {
        for (path, content) in &self.pages {
            plan.require_unchanged(path, content.clone());
        }
    }
}

struct Capture<'a> {
    root: &'a Path,
    pages: BTreeMap<PathBuf, String>,
    resources: BTreeMap<String, SemanticResource>,
}

impl<'a> Capture<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            pages: BTreeMap::new(),
            resources: BTreeMap::new(),
        }
    }

    fn page(
        &mut self,
        path: &str,
        expected_file: Option<&str>,
        expected_authored: Option<&str>,
        authority: SemanticAuthority,
    ) -> miette::Result<String> {
        let absolute = lk_vault::page_path(self.root, Path::new(path)).into_diagnostic()?;
        let canonical = absolute.canonicalize().into_diagnostic()?;
        let relative = canonical
            .strip_prefix(self.root)
            .into_diagnostic()?
            .to_path_buf();
        let raw = std::fs::read_to_string(&absolute).into_diagnostic()?;
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
        if self.pages.get(&relative).is_some_and(|prior| prior != &raw) {
            return Err(miette::miette!(
                "semantic source changed during capture: {path}"
            ));
        }
        let address = relative.to_string_lossy().into_owned();
        self.pages.insert(relative, raw.clone());
        self.resources
            .entry(address.clone())
            .or_insert_with(|| SemanticResource {
                address,
                revision: exact,
                authority,
                text: raw.clone(),
            });
        Ok(raw)
    }

    fn citation(&mut self, subject: &str, citation: &Citation) -> miette::Result<()> {
        if citation.rss_event.is_some() {
            lk_vault::validate_rss_citation(self.root, Path::new(subject), citation)
                .into_diagnostic()?;
            self.page(&citation.path, None, None, SemanticAuthority::Page)?;
            let path = format!(
                "{}/events/{}.json",
                lk_core::rss_original::RSS_ORIGINAL_STORE,
                citation.revision
            );
            let raw = self.page(
                &path,
                Some(&citation.revision),
                None,
                SemanticAuthority::RssEvent,
            )?;
            let payload: lk_core::rss_original::RssEventPayload =
                serde_json::from_str(&raw).into_diagnostic()?;
            if citation
                .rss_event
                .as_ref()
                .is_none_or(|event| event.event_id != payload.event_id)
            {
                return Err(miette::miette!("semantic RSS event identity changed"));
            }
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
                let authority = if evidence
                    .archived
                    .as_ref()
                    .is_some_and(|archive| archive.path == current.path)
                {
                    SemanticAuthority::Archive
                } else {
                    SemanticAuthority::Page
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

    fn finish(self, subject: Subject, context: Value) -> Prepared {
        Prepared {
            subject,
            context,
            resources: self.resources.into_values().collect(),
            pages: self.pages,
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

pub(super) fn task_with_audit(
    root: &Path,
    config: &Config,
    task_id: &str,
    audit: Option<&lk_graph::audit::AuditCandidate>,
) -> miette::Result<Prepared> {
    let task = super::super::queue::select_task(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    let extraction = task.kind == lk_queue::TaskKind::ExtractConcepts;
    let queries = if extraction {
        super::discovery::current(root, &task)?.unwrap_or_default()
    } else {
        Vec::new()
    };
    let prepared = capture_task_with_audit(root, config, task, audit)?;
    if extraction {
        with_discovery(root, config, prepared, &queries)
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
    let task = super::super::queue::select_task(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    with_discovery(root, config, capture_task(root, config, task)?, queries)
}

pub(super) fn source_task(root: &Path, config: &Config, task_id: &str) -> miette::Result<Prepared> {
    let task = super::super::queue::select_task(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    let mut prepared = with_discovery(root, config, capture_task(root, config, task)?, &[])?;
    prepared.context["purpose"]["extraction_scope"] = json!({"policy":"source-proposals-v1"});
    Ok(prepared)
}

pub(super) fn selection_inventory(
    root: &Path,
    config: &Config,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
) -> miette::Result<SelectionInventory> {
    let reports = lk_vault::list_reviews(root, &config.vault.dirs).into_diagnostic()?;
    let capture =
        lk_vault::capture_discovery_with_reviews(root, &config.vault.dirs, queries, &reports)
            .into_diagnostic()?;
    selection_inventory_with_capture(root, config, task_id, capture, reports)
}

pub(super) fn selection_inventory_with_capture(
    root: &Path,
    config: &Config,
    task_id: &str,
    mut capture: lk_vault::DiscoveryCapture,
    reports: Vec<lk_vault::ReviewReport>,
) -> miette::Result<SelectionInventory> {
    let task = super::super::queue::select_task(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    let mut source = capture_task(root, config, task)?;
    if source.context["purpose"]["task_kind"]
        != serde_json::to_value(lk_queue::TaskKind::ExtractConcepts).into_diagnostic()?
    {
        return Err(miette::miette!(
            "discovery recipes require concept extraction"
        ));
    }
    let target = match &source.subject {
        Subject::Queue { target, .. } => target.clone(),
        _ => {
            return Err(miette::miette!(
                "extraction inventory requires a queue subject"
            ));
        }
    };
    let target_owner = lk_vault::resolve_page_owner(root, Path::new(&target)).into_diagnostic()?;
    capture
        .candidates
        .retain(|candidate| Path::new(&candidate.owner) != target_owner.as_path());
    let target_raw = source
        .pages
        .get(&target_owner)
        .ok_or_else(|| miette::miette!("captured extraction target is missing"))?;
    let mut subjects = BTreeMap::from([(
        target.clone(),
        lk_vault::parse_page(target_raw).map_err(|error| miette::miette!("{target}: {error}"))?,
    )]);
    for candidate in &capture.candidates {
        let raw = capture
            .guarded_pages()
            .get(Path::new(&candidate.owner))
            .ok_or_else(|| miette::miette!("candidate owner is outside the exact capture"))?;
        if file_revision(raw) != candidate.file_revision {
            return Err(miette::miette!(
                "candidate file revision changed during capture"
            ));
        }
        let page = lk_vault::parse_page(raw)
            .map_err(|error| miette::miette!("{}: {error}", candidate.owner))?;
        if source_revision(&page) != candidate.source_revision {
            return Err(miette::miette!(
                "candidate source revision changed during capture"
            ));
        }
        subjects.insert(candidate.owner.clone(), page);
    }
    let reviews = lk_graph::audit::review_dependencies_with_reports(root, &subjects, reports)
        .into_diagnostic()?;
    let target_reviews = BTreeMap::from([(
        target.clone(),
        reviews.get(&target).cloned().unwrap_or_default(),
    )]);
    let mut source_capture = Capture {
        root,
        pages: std::mem::take(&mut source.pages),
        resources: std::mem::take(&mut source.resources)
            .into_iter()
            .map(|resource| (resource.address.clone(), resource))
            .collect(),
    };
    capture_reviews(&mut source_capture, &mut source.context, &target_reviews)?;
    source.context["purpose"]["discovery"] = json!({
        "queries":[],
        "match_count":0,
        "match_revision":lk_core::concept::citation_digest(&[]),
    });
    source.context["purpose"]["extraction_scope"] = json!({"policy":"source-proposals-v1"});
    source = source_capture.finish(source.subject, source.context);
    let mut guarded = Capture {
        root,
        pages: source.pages.clone(),
        resources: source
            .resources
            .iter()
            .cloned()
            .map(|resource| (resource.address.clone(), resource))
            .collect(),
    };
    for (path, raw) in capture.guarded_pages() {
        guarded.page(
            path.to_str()
                .ok_or_else(|| miette::miette!("candidate owner is not UTF-8"))?,
            Some(&file_revision(raw)),
            None,
            SemanticAuthority::Context,
        )?;
    }
    let mut review_resources = BTreeMap::new();
    let mut seen_reviews = BTreeSet::new();
    for (subject, dependencies) in &reviews {
        let mut owners = BTreeSet::new();
        for review in dependencies {
            if seen_reviews.insert(review.path.clone()) {
                guarded.review(review)?;
            }
            for path in std::iter::once(review.path.as_str()).chain(
                review.evidence.iter().flat_map(|evidence| {
                    evidence
                        .current
                        .iter()
                        .chain(&evidence.archived)
                        .map(|current| current.path.as_str())
                }),
            ) {
                owners.insert(
                    lk_vault::resolve_page_owner(root, Path::new(path))
                        .into_diagnostic()?
                        .to_str()
                        .ok_or_else(|| miette::miette!("review owner is not UTF-8"))?
                        .to_owned(),
                );
            }
        }
        review_resources.insert(subject.clone(), owners);
    }
    let guards = guarded
        .pages
        .iter()
        .map(|(path, raw)| (path, file_revision(raw)))
        .collect::<Vec<_>>();
    let revision = file_revision(
        &serde_json::to_string(&(
            "progressive-evidence-inventory-v1",
            &capture.revision,
            &target_owner,
            guards,
            &reviews,
        ))
        .into_diagnostic()?,
    );
    Ok(SelectionInventory {
        revision,
        capture,
        pages: guarded.pages,
        source,
        resources: guarded.resources,
        review_resources,
        reviews,
    })
}

pub(super) fn task_with_selection(
    inventory: &SelectionInventory,
    selection: &lk_queue::ExtractionSelection,
    selected: &BTreeSet<String>,
) -> miette::Result<Prepared> {
    if selection.inventory_revision != inventory.revision {
        return Err(super::publication_rejection(
            super::RejectionKind::SelectionInventoryChanged,
            "extraction candidate inventory changed; reselect evidence",
        ));
    }
    let candidates = inventory
        .capture
        .candidates
        .iter()
        .map(|candidate| (candidate.owner.as_str(), candidate))
        .collect::<BTreeMap<_, _>>();
    let mut resources = inventory
        .source
        .resources
        .iter()
        .cloned()
        .map(|resource| (resource.address.clone(), resource))
        .collect::<BTreeMap<_, _>>();
    for owner in selected {
        let candidate = candidates.get(owner.as_str()).ok_or_else(|| {
            super::publication_rejection(
                super::RejectionKind::EvidenceScopeChanged,
                "selected owner is outside the complete candidate inventory",
            )
        })?;
        let mut resource = inventory
            .resources
            .get(owner)
            .ok_or_else(|| miette::miette!("selected candidate input is missing"))?
            .clone();
        if resource.revision != candidate.file_revision {
            return Err(super::publication_rejection(
                super::RejectionKind::EvidenceScopeChanged,
                "selected candidate input revision changed",
            ));
        }
        resource.authority = SemanticAuthority::Context;
        resources.entry(owner.clone()).or_insert(resource);
        if let Some(dependencies) = inventory.review_resources.get(owner) {
            for address in dependencies {
                let resource = inventory
                    .resources
                    .get(address)
                    .ok_or_else(|| miette::miette!("selected review input is missing"))?;
                resources
                    .entry(address.clone())
                    .or_insert_with(|| resource.clone());
            }
        }
    }
    let mut prepared = inventory.source.clone();
    prepared.pages.clone_from(&inventory.pages);
    prepared.resources = resources.into_values().collect();
    prepared.context["purpose"]["discovery"] = json!({
        "queries":inventory.capture.queries,
        "match_count":inventory.capture.candidates.len(),
        "inventory_revision":inventory.revision,
    });
    let target = match &prepared.subject {
        Subject::Queue { target, .. } => target,
        _ => {
            return Err(miette::miette!(
                "extraction selection requires a queue subject"
            ));
        }
    };
    let reviews = inventory
        .reviews
        .iter()
        .filter(|(subject, _)| *subject == target || selected.contains(*subject))
        .collect::<BTreeMap<_, _>>();
    let review_count = reviews
        .values()
        .flat_map(|reviews| reviews.iter())
        .map(|review| &review.path)
        .collect::<BTreeSet<_>>()
        .len();
    prepared.context["purpose"]["review_dependencies"] = json!({
        "subject_count":reviews.len(),
        "review_count":review_count,
        "revision":file_revision(&serde_json::to_string(&reviews).into_diagnostic()?),
    });
    prepared.context["purpose"]["extraction_scope"] = json!({
        "policy":selection.policy,
        "selection":selection,
        "coverage":{
            "inventory_complete":true,
            "candidate_count":inventory.capture.candidates.len(),
            "full_candidate_inputs":selected.len(),
            "not_full_inputs":inventory.capture.candidates.len().saturating_sub(selected.len()),
        },
    });
    Ok(prepared)
}

pub(super) fn with_discovery(
    root: &Path,
    config: &Config,
    mut prepared: Prepared,
    queries: &[lk_core::discovery::DiscoveryQuery],
) -> miette::Result<Prepared> {
    if prepared.context["purpose"]["task_kind"]
        != serde_json::to_value(lk_queue::TaskKind::ExtractConcepts).into_diagnostic()?
    {
        return Err(miette::miette!(
            "discovery recipes require concept extraction"
        ));
    }
    let queries = super::discovery::validate_queries(queries)?;
    let matches =
        lk_vault::discover_queries(root, &config.vault.dirs, &queries).into_diagnostic()?;
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
    let mut captured = Capture {
        root,
        pages: std::mem::take(&mut prepared.pages),
        resources: std::mem::take(&mut prepared.resources)
            .into_iter()
            .map(|resource| (resource.address.clone(), resource))
            .collect(),
    };
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
    let reviews = lk_graph::audit::review_dependencies(root, &config.vault.dirs, &review_subjects)
        .into_diagnostic()?;
    capture_reviews(&mut captured, &mut prepared.context, &reviews)?;
    prepared.context["purpose"]["discovery"] = json!({
        "queries":queries,
        "match_count":selected.len(),
        "match_revision":lk_core::concept::citation_digest(&selected),
    });
    Ok(captured.finish(prepared.subject, prepared.context))
}

pub(super) fn task_for_publication(
    root: &Path,
    config: &Config,
    task_id: &str,
) -> miette::Result<Prepared> {
    let task = super::super::queue::select_task(
        root,
        task_id,
        super::super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    capture_task(root, config, task)
}

fn capture_task(
    root: &Path,
    config: &Config,
    task: lk_queue::QueueTask,
) -> miette::Result<Prepared> {
    capture_task_with_audit(root, config, task, None)
}

fn capture_task_with_audit(
    root: &Path,
    config: &Config,
    task: lk_queue::QueueTask,
    audit_input: Option<&lk_graph::audit::AuditCandidate>,
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
    let mut captured = Capture::new(root);
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
        let scoped = match audit_input {
            Some(candidate) => audit_candidate(root, config, candidate)?,
            None => audit(root, config, &task.target.vault_path)?,
        };
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
        purpose.insert("knowledge_scope".into(), scoped.context["purpose"].clone());
    }
    let context = json!({
        "subject": subject,
        "purpose": {"operation":"queue", "task_kind":task.kind, "target":task.target, "parameters":purpose},
        "authority": "Retained input is the task source; the current target is publication context. Source/page/RSS evidence retains its declared authority. A read or part receipt is not factual verification.",
    });
    Ok(captured.finish(subject, context))
}

pub(super) fn audit(root: &Path, config: &Config, path: &str) -> miette::Result<Prepared> {
    let pages =
        lk_graph::scan::scan_vault(root, config.graph.scope.follow_links).into_diagnostic()?;
    let plan = lk_graph::audit::plan_selected(
        root,
        &config.vault.dirs,
        &pages,
        1,
        &[path.to_owned()],
        u64::MAX,
    )
    .into_diagnostic()?;
    let candidate = plan
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| miette::miette!("audit subject not found: {path}"))?;
    audit_candidate(root, config, &candidate)
}

/// Preliminary capture from the controller's selected plan; publication rederives scope.
pub(super) fn audit_candidate(
    root: &Path,
    config: &Config,
    candidate: &lk_graph::audit::AuditCandidate,
) -> miette::Result<Prepared> {
    let path = candidate.path.as_str();
    let mut captured = Capture::new(root);
    captured.page(
        path,
        Some(&candidate.file_revision),
        None,
        SemanticAuthority::Subject,
    )?;
    for citation in &candidate.evidence {
        captured.citation(path, citation)?;
    }
    for related in &candidate.related_pages {
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
    let context = json!({
        "subject": subject,
        "purpose": {"operation":"audit", "subject_kind":candidate.kind, "path":path, "title":candidate.title, "locale":config.vault.locale,
            "discovery_queries":candidate.discovery.as_ref().map(|discovery| &discovery.queries),
            "review_dependencies":candidate.reviews.iter().map(|review| &review.path).collect::<Vec<_>>()},
        "authority": "The selected page is the audit subject. Concept citations are evidence; a document is its own subject without a self-citation. Related pages are context, not automatically support. Reviews and historical archives are assessments/history, not independent corroboration. Join source-declared scope, time and attribution before judging contradictions; unknown context stays unknown.",
    });
    Ok(captured.finish(subject, context))
}

pub(super) fn refresh(
    root: &Path,
    config: &Config,
    config_revision: &str,
    context: &Value,
) -> miette::Result<Prepared> {
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
                source_task(root, config, task_id)?
            } else if context["purpose"]["extraction_scope"]["policy"] == "progressive-evidence-v1"
            {
                let queries: Vec<lk_core::discovery::DiscoveryQuery> =
                    serde_json::from_value(context["purpose"]["discovery"]["queries"].clone())
                        .into_diagnostic()?;
                let selection: lk_queue::ExtractionSelection = serde_json::from_value(
                    context["purpose"]["extraction_scope"]["selection"].clone(),
                )
                .into_diagnostic()?;
                super::selected_task(
                    root,
                    config,
                    config_revision,
                    task_id,
                    &queries,
                    &selection,
                    &mut Default::default(),
                )?
                .0
            } else if let Some(discovery) = context["purpose"].get("discovery") {
                let queries: Vec<lk_core::discovery::DiscoveryQuery> =
                    serde_json::from_value(discovery["queries"].clone()).into_diagnostic()?;
                task_with_discovery(root, config, task_id, &queries)?
            } else {
                task(root, config, task_id)?
            }
        }
        Subject::Audit { path, .. } => audit(root, config, path)?,
    };
    if prepared.subject != subject || prepared.context != *context {
        return Err(super::publication_rejection(
            super::RejectionKind::SemanticContextChanged,
            "semantic work inputs changed; replan and reread changed evidence",
        ));
    }
    Ok(prepared)
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
    fn progressive_capture_keeps_all_guards_and_only_admitted_semantic_inputs() {
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
        let source = source_task(&root, &config, &task.task_id).unwrap();
        assert_eq!(
            source.context["purpose"]["extraction_scope"]["policy"],
            "source-proposals-v1"
        );
        for path in [&review_paths[0], &evidence_paths[0]] {
            assert!(
                source
                    .resources
                    .iter()
                    .any(|resource| &resource.address == path)
            );
        }
        for path in [selected, excluded, &review_paths[1], &review_paths[2]] {
            assert!(
                !source
                    .resources
                    .iter()
                    .any(|resource| resource.address == path)
            );
        }
        let inventory = selection_inventory(&root, &config, &task.task_id, &queries).unwrap();
        assert_same_prepared(&source, &inventory.source);
        assert_eq!(inventory.capture.candidates.len(), 2);
        assert!(
            !inventory
                .capture
                .candidates
                .iter()
                .any(|candidate| candidate.owner == target)
        );
        let selection = lk_queue::ExtractionSelection {
            policy: lk_queue::ExtractionSelectionPolicy::ProgressiveV1,
            inventory_revision: inventory.revision.clone(),
            decisions_revision: "reviewed-decisions".into(),
        };
        let admitted =
            task_with_selection(&inventory, &selection, &BTreeSet::from([selected.into()]))
                .unwrap();
        for path in [target, selected, excluded] {
            assert!(admitted.pages.contains_key(Path::new(path)));
        }
        for path in review_paths.iter().chain(&evidence_paths) {
            assert!(admitted.pages.contains_key(Path::new(path)));
        }
        for path in [
            selected,
            &review_paths[0],
            &evidence_paths[0],
            &review_paths[1],
            &evidence_paths[1],
        ] {
            assert!(
                admitted
                    .resources
                    .iter()
                    .any(|resource| resource.address == path)
            );
        }
        for path in [excluded, &review_paths[2], &evidence_paths[2]] {
            assert!(
                !admitted
                    .resources
                    .iter()
                    .any(|resource| resource.address == path)
            );
        }
        assert!(
            admitted
                .resources
                .iter()
                .any(|resource| resource.address == selected
                    && resource.authority == SemanticAuthority::Context)
        );
        assert_eq!(
            admitted.context["purpose"]["extraction_scope"]["coverage"]["not_full_inputs"],
            1
        );
        let legacy = task_with_discovery(&root, &config, &task.task_id, &queries).unwrap();
        assert!(
            legacy
                .resources
                .iter()
                .any(|resource| resource.address == excluded)
        );
        assert!(
            task_with_selection(
                &inventory,
                &selection,
                &BTreeSet::from([evidence_paths[0].clone()])
            )
            .is_err()
        );

        for path in [excluded, &review_paths[2], &evidence_paths[2]] {
            let raw = std::fs::read_to_string(root.join(path)).unwrap();
            write(&root, path, &format!("{raw}\nExternally edited.\n"));
            let changed = selection_inventory(&root, &config, &task.task_id, &queries).unwrap();
            assert_ne!(inventory.revision, changed.revision);
            assert!(task_with_selection(&changed, &selection, &BTreeSet::new()).is_err());
            write(&root, path, &raw);
        }
        write(&root, "wiki/documents/new.md", excluded_raw);
        let added = selection_inventory(&root, &config, &task.task_id, &queries).unwrap();
        assert_ne!(inventory.revision, added.revision);
        assert!(task_with_selection(&added, &selection, &BTreeSet::new()).is_err());

        let selected_raw = std::fs::read_to_string(root.join(selected)).unwrap();
        let selected_revision = source_revision(&lk_vault::parse_page(&selected_raw).unwrap());
        write(
            &root,
            &review_paths[0],
            &format!(
                "---\ntype: exploration\nreview:\n  status: open\n  subjects: [{target}]\n  evidence: [{{path: {selected}, revision: {selected_revision}}}]\n---\nA mandatory source assessment.\n"
            ),
        );
        let mandatory = selection_inventory(&root, &config, &task.task_id, &queries).unwrap();
        assert_same_prepared(
            &source_task(&root, &config, &task.task_id).unwrap(),
            &mandatory.source,
        );
        let selection = lk_queue::ExtractionSelection {
            inventory_revision: mandatory.revision.clone(),
            ..selection
        };
        let admitted =
            task_with_selection(&mandatory, &selection, &BTreeSet::from([selected.into()]))
                .unwrap();
        assert!(
            admitted
                .resources
                .iter()
                .any(|resource| resource.address == selected
                    && resource.authority == SemanticAuthority::Page)
        );

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
        assert_eq!(shared.reviews[selected].len(), 2);
        let card = &shared
            .capture
            .candidates
            .iter()
            .find(|candidate| candidate.owner == selected)
            .unwrap()
            .card;
        assert_eq!(card.reviews.invalid, 1);
        assert_eq!(card.reviews.needs_reassessment, 1);
        assert!(shared.reviews[selected].iter().any(|review| {
            review.path == review_paths[1]
                && review.state == lk_graph::audit::AuditReviewState::Invalid
        }));
        let archived = lk_vault::evidence_archive_path(&archive.citations[0].revision).unwrap();
        assert!(shared.source.resources.iter().any(|resource| {
            resource.address == archived.to_string_lossy()
                && resource.authority == SemanticAuthority::Archive
        }));
        let binding = lk_queue::ExtractionSelection {
            inventory_revision: shared.revision.clone(),
            ..selection
        };
        let admitted =
            task_with_selection(&shared, &binding, &BTreeSet::from([selected.into()])).unwrap();
        assert!(admitted.resources.iter().any(|resource| {
            resource.address == selected && resource.authority == SemanticAuthority::Page
        }));
        let empty = selection_inventory(&root, &config, &task.task_id, &[]).unwrap();
        assert!(empty.capture.candidates.is_empty());
        assert_same_prepared(&shared.source, &empty.source);
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
        let candidates = lk_graph::audit::load_selected_inputs(
            &root,
            &config.vault.dirs,
            &pages,
            &tasks
                .iter()
                .map(|task| task.target.vault_path.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        (directory, config, tasks, candidates)
    }

    #[test]
    fn batched_synthesis_capture_matches_standalone_scope_and_rejects_wrong_audit_subjects() {
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
            assert_eq!(
                standalone.revision("config-revision").unwrap(),
                batched.revision("config-revision").unwrap()
            );
            assert!(batched.resources.iter().any(|resource| {
                resource.address == "wiki/concepts/context.md"
                    && resource.authority == SemanticAuthority::Context
            }));
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
}
