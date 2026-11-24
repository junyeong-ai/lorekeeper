//! A native synthesis finalizer's review proposal. The finalizer runs alone on its concept, so it
//! takes the coordinator's part the synthesis contract assigns for a material disagreement or a
//! material evidence gap: it proposes one durable review, and Rust publishes it as an open review
//! of that concept through the guarded curation plan. Models never write the vault themselves.
//!
//! Whether a question is already recorded is the finalizer's judgment over every scoped review
//! it reads, so an open review never blocks a distinct question about the same concept. The run
//! bounds the outcome instead: a target publishes at most one review per run, and a further
//! proposal waits unrecorded for a later run, whose request then scopes the reviews published.

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
    let Some(path) = review_path(root, &config.vault.dirs, title, proposal.kind)? else {
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
    let mut guards = Vec::new();
    for index in &evidence {
        let admitted = match prepared.resources.get(*index) {
            Some(resource) if resource.authority == SemanticAuthority::RssEvent => {
                rss_evidence(&cited, &prepared.pages, &resource.address)
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
        guards.push(guard);
    }
    let metadata = json!({"status":"open","kind":proposal.kind,"subjects":[task.target.vault_path],
        "evidence":citations,"resolution":null});
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
            evidence: guards,
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
/// bytes of the origin page that binds it. The payload's file name is its revision.
fn rss_evidence(
    cited: &[lk_core::concept::Citation],
    pages: &std::collections::BTreeMap<PathBuf, String>,
    address: &str,
) -> Result<(Value, lk_vault::PageRevision), String> {
    let revision = Path::new(address)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("review evidence {address} is not an RSS event payload"))?;
    let citation = cited
        .iter()
        .find(|citation| citation.rss_event.is_some() && citation.revision == revision)
        .ok_or_else(|| {
            format!("review evidence {address} is not an RSS event this synthesis cites")
        })?;
    let origin = pages
        .get(Path::new(&citation.path))
        .ok_or_else(|| format!("the origin page of {address} is not one this Work read"))?;
    let guard = lk_vault::PageRevision {
        path: citation.path.clone(),
        revision: lk_core::concept::file_revision(origin),
    };
    Ok((
        serde_json::to_value(citation).map_err(|error| error.to_string())?,
        guard,
    ))
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
fn review_path(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    title: &str,
    kind: lk_vault::ReviewKind,
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
        match std::fs::symlink_metadata(root.join(&path)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(path.to_string_lossy().into_owned()));
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
        let revision = "a".repeat(64);
        let citation = lk_core::concept::Citation {
            path: "daily/rss.md".into(),
            revision: revision.clone(),
            rss_event: Some(lk_core::rss_evidence::RssEventSelector {
                schema: lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA.into(),
                event_id: "rss:2026-10-18:0000000000000000".into(),
            }),
        };
        let page =
            lk_core::concept::Citation::at_revision(Path::new("daily/rss.md"), "b".repeat(32));
        let payload = format!(".lorekeeper/evidence/rss/events/{revision}.json");
        let pages = std::collections::BTreeMap::from([(
            PathBuf::from("daily/rss.md"),
            "origin bytes".to_owned(),
        )]);
        let (mapped, guard) =
            rss_evidence(&[page.clone(), citation.clone()], &pages, &payload).unwrap();
        assert_eq!(mapped, serde_json::to_value(&citation).unwrap());
        assert_eq!(guard.path, "daily/rss.md");
        assert_eq!(
            guard.revision,
            lk_core::concept::file_revision("origin bytes")
        );
        assert!(
            rss_evidence(&[page], &pages, &payload).is_err(),
            "an uncited event"
        );
        assert!(
            rss_evidence(&[citation], &Default::default(), &payload).is_err(),
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
}
