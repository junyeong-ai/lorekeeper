//! A native synthesis finalizer's review proposal. The finalizer runs alone on its concept, so it
//! takes the coordinator's part the synthesis contract assigns for a material disagreement or a
//! material evidence gap: it proposes one durable review, and Rust publishes it as an open review
//! of that concept through the guarded curation plan. Models never write the vault themselves.
//!
//! One open review of a kind per concept carries that kind of question: while an open
//! `conflict` or `evidence-gap` review names the concept, by any author and whether or not this
//! Work read it, a proposal of the same kind is refused, naming that review, and the finalizer
//! references it and states any distinct question in the synthesis until it is resolved. A new
//! review would make the concept owed a resynthesis, so a rephrased question could otherwise
//! open review after review. A proposal repeating a scoped review exactly (kind, title,
//! judgment and evidence) is that review: it publishes nothing and holds the task while its
//! inputs stay current. Either refusal goes through the review refusal's one reconsideration.
//! The run bounds the rest: a target publishes at most one review per run, and a further
//! proposal of another kind waits unrecorded for a later run, whose request then scopes the
//! reviews published. The same-kind refusal comes first, so that bound never postpones it.

use std::path::{Path, PathBuf};

use lk_core::semantic::SemanticAuthority;
use lk_queue::{QueueTask, TaskKind};
use lk_vault::{MutationPlan, MutationSession};
use miette::IntoDiagnostic;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Prepared;

/// One proposed open review of the synthesized concept. `evidence` names inspected resources of
/// the finalized Work by their roster index.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct ReviewProposal {
    pub kind: lk_vault::ReviewKind,
    pub title: String,
    pub body: String,
    pub evidence: Vec<usize>,
}

const MAX_TITLE_CHARACTERS: usize = 200;
/// Review file stems stay well inside common filename byte limits, whatever the title's script.
const MAX_STEM_BYTES: usize = 120;

/// A proposal checked against the finalized Work, with the curation plan that publishes it.
pub(in crate::commands) struct AdmittedReview {
    path: String,
    curation: MutationPlan,
}

/// Check a proposal against the Work it answers and plan its publication. `Ok(Err(reason))` is a
/// refusal that holds only this task while its inputs stay current; `Err` is a fault reading the
/// vault.
pub(in crate::commands) fn admit(
    root: &Path,
    config: &lk_core::config::Config,
    task: &QueueTask,
    prepared: &Prepared,
    proposal: ReviewProposal,
) -> miette::Result<Result<AdmittedReview, String>> {
    let title = proposal.title.trim();
    if title.is_empty() || title.contains('\n') || title.chars().count() > MAX_TITLE_CHARACTERS {
        return Ok(Err(format!(
            "a review needs a nonblank single-line title of at most {MAX_TITLE_CHARACTERS} characters"
        )));
    }
    if proposal.body.trim().is_empty() {
        return Ok(Err("a review needs a judgment body".into()));
    }
    let Some(path) = review_path(
        root,
        &config.vault.dirs,
        title,
        proposal.kind,
        &prepared.absent,
    )?
    else {
        return Ok(Err(format!(
            "every review address for this title is taken in {}",
            lk_core::vault_path::explorations_dir(&config.vault.dirs).display()
        )));
    };
    let concepts = lk_core::vault_path::concepts_dir(&config.vault.dirs);
    for destination in lk_core::link::extract_dests(&proposal.body) {
        if let Some(linked) = lk_core::link::resolve_dest(Path::new(&path), &destination)
            && reaches(root, &linked, &concepts)?
        {
            return Ok(Err(
                "a review body linking a concept would become that concept's citation".into(),
            ));
        }
    }
    let mut evidence = proposal.evidence.clone();
    evidence.sort_unstable();
    evidence.dedup();
    if evidence.is_empty() || evidence.len() != proposal.evidence.len() {
        return Ok(Err(
            "a review needs distinct evidence resource indices".into()
        ));
    }
    let cited: Vec<lk_core::concept::Citation> = match task
        .input
        .get("citations")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
    {
        Ok(cited) => cited.unwrap_or_default(),
        Err(error) => return Ok(Err(format!("synthesis citations: {error}"))),
    };
    let mut citations = Vec::new();
    let mut guards = std::collections::BTreeMap::new();
    for index in &evidence {
        let admitted = match prepared.resources.get(*index) {
            Some(resource) if resource.authority == SemanticAuthority::RssEvent => {
                rss_evidence(root, &cited, &prepared.pages, &resource.address)?
            }
            Some(resource) if resource.authority == SemanticAuthority::Page => {
                page_evidence(prepared, &resource.address, &concepts)
            }
            _ => Err(format!(
                "review evidence {index} is not a source page or RSS event this Work read"
            )),
        };
        let (citation, guard) = match admitted {
            Ok(admitted) => admitted,
            Err(refusal) => return Ok(Err(refusal)),
        };
        citations.push(citation);
        guards.insert(guard.path.clone(), guard);
    }
    let citations = without_cited_origins(citations);
    if let Some(recorded) = recorded(
        root,
        prepared,
        &task.target.vault_path,
        title,
        proposal.kind,
        &proposal.body,
        &citations,
    )? {
        return Ok(Err(format!(
            "the proposal repeats the scoped review {recorded}; complete the synthesis with that review in scope"
        )));
    }
    if let Some(refusal) = same_kind_refusal(root, config, task, proposal.kind)? {
        return Ok(Err(refusal));
    }
    let metadata = json!({"status":"open","kind":proposal.kind,"subjects":[task.target.vault_path],
        "evidence":citations,"resolution":null,"judged_citation_digest":task.cache_hash});
    let content = format!(
        "---\ntype: exploration\ntitle: {}\nreview: {metadata}\n---\n{}\n",
        serde_json::to_string(title).into_diagnostic()?,
        proposal.body.trim_end(),
    );
    let planned = lk_vault::plan_curation(
        root,
        &config.vault.dirs,
        &config.concepts.categories,
        lk_vault::CurationPlan {
            evidence: guards.into_values().collect(),
            concepts: Vec::new(),
            reviews: vec![lk_vault::ReviewCuration {
                path: path.clone(),
                revision: None,
                content,
            }],
            pages: Vec::new(),
        },
    );
    match planned {
        Ok((curation, _)) => Ok(Ok(AdmittedReview { path, curation })),
        Err(lk_vault::VaultError::Io(error)) => Err(error).into_diagnostic(),
        Err(error) => Ok(Err(format!("the review cannot be published: {error}"))),
    }
}

/// The refusal of a proposal of `kind` while an open review of that kind names the task's
/// concept. It holds whatever else the run allows, so a target that already published a
/// review this run still receives the refusal's reconsideration.
pub(in crate::commands) fn same_kind_refusal(
    root: &Path,
    config: &lk_core::config::Config,
    task: &QueueTask,
    kind: lk_vault::ReviewKind,
) -> miette::Result<Option<String>> {
    let open = open_of_kind(root, config, &task.target.vault_path, kind)?;
    if open.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "the open {} review {} already carries this concept's question of that kind; reference it in the synthesis and state any distinct question there until that review is resolved",
        serde_json::to_value(kind)
            .into_diagnostic()?
            .as_str()
            .unwrap_or_default(),
        open.join(", ")
    )))
}

/// The scoped review of `concept` this Work read that records the same question: the same
/// kind, title, judgment and evidence set, whatever its status and evidence order, and
/// whatever authority the capture first read its page under (a discovery match can read a
/// review as context).
fn recorded(
    root: &Path,
    prepared: &Prepared,
    concept: &str,
    title: &str,
    kind: lk_vault::ReviewKind,
    body: &str,
    evidence: &[Value],
) -> miette::Result<Option<String>> {
    let kind = serde_json::to_value(kind).into_diagnostic()?;
    let evidence_set = |values: &[Value]| {
        let mut set = values.iter().map(Value::to_string).collect::<Vec<_>>();
        set.sort_unstable();
        set
    };
    let evidence = evidence_set(evidence);
    let owner = |address: &str| lk_vault::resolve_page_owner(root, Path::new(address)).ok();
    let concept_owner = owner(concept);
    let scoped =
        prepared.context["purpose"]["parameters"]["knowledge_scope"]["review_dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned);
    let addresses = prepared
        .resources
        .iter()
        .filter(|resource| resource.authority == SemanticAuthority::Review)
        .map(|resource| resource.address.clone())
        .chain(scoped)
        .collect::<std::collections::BTreeSet<_>>();
    for address in &addresses {
        let Some(page) = prepared
            .captured_page(address)
            .and_then(|raw| lk_vault::parse_page(raw).ok())
        else {
            continue;
        };
        let fields = &page.frontmatter.fields;
        let review = fields.get("review");
        let names_concept = review
            .and_then(|review| review.get("subjects"))
            .and_then(Value::as_array)
            .is_some_and(|subjects| {
                subjects
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|subject| owner(subject) == concept_owner)
            });
        if fields.get("title").and_then(Value::as_str) == Some(title)
            && review.and_then(|review| review.get("kind")) == Some(&kind)
            && review
                .and_then(|review| review.get("evidence"))
                .and_then(Value::as_array)
                .is_some_and(|recorded| evidence_set(recorded) == evidence)
            && names_concept
            && judgment(&page.body) == judgment(&body.replace("\r\n", "\n"))
        {
            return Ok(Some(address.clone()));
        }
    }
    Ok(None)
}

/// The open reviews of `kind` that name `concept` under its verified owner, among every review
/// in the vault: the shared inventory, not only the reviews this Work read.
fn open_of_kind(
    root: &Path,
    config: &lk_core::config::Config,
    concept: &str,
    kind: lk_vault::ReviewKind,
) -> miette::Result<Vec<String>> {
    let owner = |address: &str| lk_vault::resolve_page_owner(root, Path::new(address)).ok();
    let Some(concept_owner) = owner(concept) else {
        return Ok(Vec::new());
    };
    let mut inventory =
        lk_vault::ReviewInventory::load(root, &config.vault.dirs).into_diagnostic()?;
    let naming = inventory
        .paths()
        .filter(|path| {
            inventory.subjects(path).is_some_and(|subjects| {
                subjects
                    .iter()
                    .any(|subject| owner(subject).as_ref() == Some(&concept_owner))
            })
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    Ok(naming
        .into_iter()
        .filter(|path| {
            inventory
                .load_report(path)
                .and_then(|report| report.review)
                .is_some_and(|review| {
                    review.status == lk_vault::ReviewStatus::Open && review.kind == Some(kind)
                })
        })
        .collect())
}

/// A review judgment as compared for an exact repeat: without the blank lines framing it,
/// keeping the leading indentation that can make it a code block.
fn judgment(body: &str) -> &str {
    body.trim_start_matches(['\n', '\r']).trim_end()
}

/// `citations` without a page citation of an origin whose RSS events are also cited. A review
/// cites an origin either as a page or by its events (`lk_vault` review evidence); the events
/// are the narrower evidence the judgment selected, and they guard the same origin bytes.
fn without_cited_origins(citations: Vec<Value>) -> Vec<Value> {
    let origins = citations
        .iter()
        .filter(|citation| {
            citation
                .get("rss_event")
                .is_some_and(|event| !event.is_null())
        })
        .filter_map(|citation| citation["path"].as_str().map(str::to_owned))
        .collect::<std::collections::BTreeSet<_>>();
    citations
        .into_iter()
        .filter(|citation| {
            citation
                .get("rss_event")
                .is_some_and(|event| !event.is_null())
                || !citation["path"]
                    .as_str()
                    .is_some_and(|path| origins.contains(path))
        })
        .collect()
}

/// A read source page as page evidence: its source revision, guarded by its exact bytes.
fn page_evidence(
    prepared: &Prepared,
    address: &str,
    concepts: &Path,
) -> Result<(Value, lk_vault::PageRevision), String> {
    let source = PathBuf::from(address);
    let raw = prepared
        .pages
        .get(&source)
        .filter(|_| !source.starts_with(concepts))
        .ok_or_else(|| format!("review evidence {address} is not a source page this Work read"))?;
    let page = lk_core::frontmatter::parse_page(raw)
        .map_err(|error| format!("review evidence {address} is unreadable: {error}"))?;
    Ok((
        json!({"path":address,"revision":lk_core::concept::source_revision(&page)}),
        lk_vault::PageRevision {
            path: address.to_owned(),
            revision: lk_core::concept::file_revision(raw),
        },
    ))
}

/// A read RSS event payload as the synthesis's own RSS citation of it, guarded by the exact
/// bytes of the origin page that binds it, which the capture stored under its owner. The
/// payload's file name is its revision.
fn rss_evidence(
    root: &Path,
    cited: &[lk_core::concept::Citation],
    pages: &std::collections::BTreeMap<PathBuf, String>,
    address: &str,
) -> miette::Result<Result<(Value, lk_vault::PageRevision), String>> {
    let Some(citation) = Path::new(address)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|revision| {
            cited
                .iter()
                .find(|citation| citation.rss_event.is_some() && citation.revision == revision)
        })
    else {
        return Ok(Err(format!(
            "review evidence {address} is not an RSS event this synthesis cites"
        )));
    };
    let owner = lk_vault::find_page_owner(root, Path::new(&citation.path)).into_diagnostic()?;
    let Some((owner, origin)) = owner.and_then(|owner| pages.get_key_value(&owner)) else {
        return Ok(Err(format!(
            "the origin page of {address} is not one this Work read"
        )));
    };
    let guard = lk_vault::PageRevision {
        path: lk_core::vault_path::address(owner),
        revision: lk_core::concept::file_revision(origin),
    };
    Ok(Ok((
        serde_json::to_value(citation).into_diagnostic()?,
        guard,
    )))
}

/// Whether a link destination names the concepts collection, by its address or through the
/// filesystem, so neither a symlinked concept address nor a case or symlink alias of the
/// collection passes for another directory. A destination that cannot resolve is judged by its
/// address alone.
fn reaches(root: &Path, linked: &Path, concepts: &Path) -> miette::Result<bool> {
    if linked.starts_with(concepts) {
        return Ok(true);
    }
    let collection = match std::fs::canonicalize(root.join(concepts)) {
        Ok(collection) => collection,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).into_diagnostic(),
    };
    let mut probe = root.join(linked);
    loop {
        match std::fs::canonicalize(&probe) {
            Ok(resolved) => return Ok(resolved.starts_with(&collection)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !probe.pop() || !probe.starts_with(root) {
                    return Ok(false);
                }
            }
            Err(_) => return Ok(false),
        }
    }
}

/// Publish an admitted review in the same journal as `plan`, which carries the finalized Work's
/// read guards. Returns the review's vault path.
pub(in crate::commands::work) fn record(
    session: &MutationSession,
    task: &QueueTask,
    prepared: &Prepared,
    review: AdmittedReview,
    mut plan: MutationPlan,
) -> miette::Result<String> {
    if task.kind != TaskKind::SynthesizeConcept {
        return Err(miette::miette!("only a synthesis proposes a review"));
    }
    prepared.guard_pages(&mut plan);
    plan.extend(review.curation);
    session.apply(plan).into_diagnostic()?;
    Ok(review.path)
}

/// An absent explorations address for the review: its title's slug, suffixed when taken, within a
/// filename byte bound. `None` when every candidate is taken.
/// The first vacant review address for `title`. An address the preparation guards as a
/// removed scoped review is not vacant: creating there would contradict that guard.
fn review_path(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    title: &str,
    kind: lk_vault::ReviewKind,
    guarded_absent: &std::collections::BTreeSet<PathBuf>,
) -> miette::Result<Option<String>> {
    let directory = lk_core::vault_path::explorations_dir(dirs);
    let mut stem = lk_core::concept::slugify(title).unwrap_or_else(|| match kind {
        lk_vault::ReviewKind::Conflict => "conflict-review".into(),
        lk_vault::ReviewKind::EvidenceGap => "evidence-gap-review".into(),
    });
    let bound = MAX_STEM_BYTES - "-64".len();
    if stem.len() > bound {
        let mut end = bound;
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        stem.truncate(end);
        stem = stem.trim_end_matches('-').to_owned();
    }
    for attempt in 1..=64 {
        let name = if attempt == 1 {
            format!("{stem}.md")
        } else {
            format!("{stem}-{attempt}.md")
        };
        let path = directory.join(name);
        let address = lk_core::vault_path::address(&path);
        // A guarded absence is one file with any spelling a filesystem folds onto it.
        if guarded_absent.iter().any(|absent| {
            lk_core::fs::names_fold_together(&lk_core::vault_path::address(absent), &address)
        }) {
            continue;
        }
        match std::fs::symlink_metadata(root.join(&path)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(address));
            }
            Err(error) => return Err(error).into_diagnostic(),
            Ok(_) => {}
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_rss_payload_maps_to_the_synthesis_citation_that_selects_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("imports")).unwrap();
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::fs::write(root.path().join("imports/rss.md"), "origin bytes").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../imports/rss.md", root.path().join("daily/rss.md")).unwrap();
        let (cited, owner) = if cfg!(unix) {
            ("daily/rss.md", "imports/rss.md")
        } else {
            ("imports/rss.md", "imports/rss.md")
        };
        let root = root.path().canonicalize().unwrap();
        let revision = "a".repeat(64);
        let citation = lk_core::concept::Citation {
            path: cited.into(),
            revision: revision.clone(),
            rss_event: Some(lk_core::rss_evidence::RssEventSelector {
                schema: lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA.into(),
                event_id: "rss:2026-10-18:0000000000000000".into(),
            }),
        };
        let page = lk_core::concept::Citation::at_revision(Path::new(cited), "b".repeat(32));
        let payload = lk_core::rss_original::event_payload_path(&revision);
        let pages =
            std::collections::BTreeMap::from([(PathBuf::from(owner), "origin bytes".to_owned())]);
        let (mapped, guard) =
            rss_evidence(&root, &[page.clone(), citation.clone()], &pages, &payload)
                .unwrap()
                .unwrap();
        assert_eq!(mapped, serde_json::to_value(&citation).unwrap());
        assert_eq!(guard.path, owner, "the origin is read under its owner");
        assert_eq!(
            guard.revision,
            lk_core::concept::file_revision("origin bytes")
        );
        assert!(
            rss_evidence(&root, &[page], &pages, &payload)
                .unwrap()
                .is_err(),
            "an uncited event"
        );
        assert!(
            rss_evidence(&root, &[citation], &Default::default(), &payload)
                .unwrap()
                .is_err(),
            "an origin this Work did not read"
        );
    }

    #[test]
    fn a_long_title_in_any_script_keeps_the_review_filename_within_its_byte_bound() {
        let root = tempfile::tempdir().unwrap();
        let path = review_path(
            root.path(),
            &lk_core::config::VaultDirs::default(),
            &"검토".repeat(100),
            lk_vault::ReviewKind::Conflict,
            &Default::default(),
        )
        .unwrap()
        .unwrap();
        let name = Path::new(&path).file_name().unwrap().to_str().unwrap();
        assert!(
            name.len() + "-64".len() <= MAX_STEM_BYTES + ".md".len(),
            "{name}"
        );
        assert!(name.ends_with(".md") && !name.contains("-.md"));
    }

    #[test]
    fn an_origin_cited_by_its_events_drops_its_page_citation() {
        let event = |id: &str| json!({"path":"daily/news/2026-11-03.md","revision":id,"rss_event":{"event_id":id}});
        let page = json!({"path":"daily/news/2026-11-03.md","revision":"page"});
        let other = json!({"path":"wiki/documents/spec.md","revision":"spec"});
        assert_eq!(
            without_cited_origins(vec![page.clone(), event("a"), other.clone(), event("b")]),
            [event("a"), other.clone(), event("b")]
        );
        assert_eq!(
            without_cited_origins(vec![page.clone(), other.clone()]),
            [page, other],
            "a page cited alone stays"
        );
    }

    #[test]
    fn a_review_never_takes_a_spelling_of_a_guarded_absence() {
        let root = tempfile::tempdir().unwrap();
        let dirs = lk_core::config::VaultDirs::default();
        let name = |absent: &std::collections::BTreeSet<PathBuf>| {
            review_path(
                root.path(),
                &dirs,
                "Gap",
                lk_vault::ReviewKind::Conflict,
                absent,
            )
            .unwrap()
            .unwrap()
        };
        let first = name(&Default::default());
        let file = Path::new(&first).file_name().unwrap().to_str().unwrap();
        let folded = Path::new(&first).with_file_name(file.to_uppercase().replace(".MD", ".md"));
        assert_ne!(name(&[folded].into()), first);
    }
}
