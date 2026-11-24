//! A native synthesis finalizer's review proposal. The finalizer runs alone on its concept, so it
//! takes the coordinator's part the synthesis contract assigns for a material disagreement or a
//! material evidence gap: it proposes one durable review, and Rust publishes it as an open review
//! of that concept through the guarded curation plan. Models never write the vault themselves.

use std::path::{Path, PathBuf};

use lk_core::semantic::SemanticAuthority;
use lk_queue::{QueueTask, TaskKind};
use lk_vault::{MutationPlan, MutationSession};
use miette::IntoDiagnostic;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Prepared;

/// What a proposed review records.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(in crate::commands) enum ReviewKind {
    /// Cited sources disagree on a claim under the same subject, scope and time.
    Conflict,
    /// Cited sources no longer support claims the concept depends on, such as its definition.
    EvidenceGap,
}

/// One proposed open review of the synthesized concept. `evidence` names inspected resources of
/// the finalized Work by their roster index.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct ReviewProposal {
    pub kind: ReviewKind,
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
    if let Some(open) = open_scoped_review(task, prepared) {
        return Ok(Err(format!(
            "the open review {open} already assesses this concept; draft the synthesis against it, and a new question waits until that review is resolved"
        )));
    }
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
    let mut citations = Vec::new();
    let mut guards = Vec::new();
    for index in &evidence {
        let source = prepared
            .resources
            .get(*index)
            .filter(|resource| resource.authority == SemanticAuthority::Page)
            .map(|resource| PathBuf::from(&resource.address))
            .filter(|path| !path.starts_with(&concepts))
            .and_then(|path| prepared.pages.get(&path).map(|raw| (path, raw)));
        let Some((source, raw)) = source else {
            return Ok(Err(format!(
                "review evidence {index} is not a source page this Work read"
            )));
        };
        let page = match lk_core::frontmatter::parse_page(raw) {
            Ok(page) => page,
            Err(error) => {
                return Ok(Err(format!(
                    "review evidence {} is unreadable: {error}",
                    source.display()
                )));
            }
        };
        let address = source.to_string_lossy().into_owned();
        citations.push(json!({"path":address,"revision":lk_core::concept::source_revision(&page)}));
        guards.push(lk_vault::PageRevision {
            path: address,
            revision: lk_core::concept::file_revision(raw),
        });
    }
    let metadata = json!({"status":"open","subjects":[task.target.vault_path],
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

/// The first scoped review that still assesses this concept and is open: its page names the
/// concept in `subjects` and records `status: open`. A review retargeted elsewhere, resolved or
/// unreadable leaves the concept free to propose; the audit reports an unreadable one.
fn open_scoped_review(task: &QueueTask, prepared: &Prepared) -> Option<String> {
    let scoped =
        prepared.context["purpose"]["parameters"]["knowledge_scope"]["review_dependencies"]
            .as_array()?;
    scoped
        .iter()
        .filter_map(Value::as_str)
        .find(|path| {
            let review = prepared
                .pages
                .get(Path::new(path))
                .and_then(|raw| lk_core::frontmatter::parse_page(raw).ok())
                .and_then(|page| page.frontmatter.fields.get("review").cloned());
            review.is_some_and(|review| {
                review["status"] == "open"
                    && review["subjects"].as_array().is_some_and(|subjects| {
                        subjects
                            .iter()
                            .any(|subject| subject == task.target.vault_path.as_str())
                    })
            })
        })
        .map(str::to_owned)
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
    kind: ReviewKind,
) -> miette::Result<Option<String>> {
    let directory = lk_core::vault_path::explorations_dir(dirs);
    let mut stem = lk_core::concept::slugify(title).unwrap_or_else(|| match kind {
        ReviewKind::Conflict => "conflict-review".into(),
        ReviewKind::EvidenceGap => "evidence-gap-review".into(),
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
    fn a_long_title_in_any_script_keeps_the_review_filename_within_its_byte_bound() {
        let root = tempfile::tempdir().unwrap();
        let path = review_path(
            root.path(),
            &lk_core::config::VaultDirs::default(),
            &"검토".repeat(100),
            ReviewKind::Conflict,
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
