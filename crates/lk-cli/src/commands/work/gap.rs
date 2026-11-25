//! The evidence-gap callout a concept synthesis keeps while an open evidence-gap review of the
//! concept is in its scope. A synthesis result carries the defining claims that review names as
//! structured `gap_claims`; publication renders the callout beneath the synthesis from those
//! claims and the reviews its own capture found, so the marker and every review reference are
//! Rust's, never prose a writer must reproduce.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::i18n::Locale;
use serde::{Deserialize, Serialize};

/// An open evidence-gap review of the synthesized concept, by address and title, and whether
/// the concept's citations changed since the review judged them. A review that records no
/// judged citations counts as changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EvidenceGap {
    pub path: String,
    pub title: String,
    pub citations_changed: bool,
}

/// The open evidence-gap reviews of `concept` among a synthesis capture's scoped `reviews`, for a
/// request answering the citations `citation_digest` names. Each review is read from the bytes
/// that capture stored under its verified owner, and names the concept when one of its subjects
/// has the concept's owner. A review of another kind, resolved, unreadable or about other
/// subjects is not one.
pub(super) fn open_reviews(
    root: &Path,
    concept: &str,
    citation_digest: &str,
    reviews: &[String],
    pages: &BTreeMap<PathBuf, String>,
) -> miette::Result<Vec<EvidenceGap>> {
    let owner = |address: &str| lk_vault::resolve_page_owner(root, Path::new(address)).ok();
    let Some(concept_owner) = owner(concept) else {
        return Ok(Vec::new());
    };
    let mut gaps = Vec::new();
    for path in reviews {
        let Some(page) = owner(path)
            .and_then(|stored| pages.get(&stored))
            .and_then(|raw| lk_core::frontmatter::parse_page(raw).ok())
        else {
            continue;
        };
        let Some(review) = page
            .frontmatter
            .get("review")
            .cloned()
            .and_then(|value| serde_json::from_value::<lk_vault::KnowledgeReview>(value).ok())
        else {
            continue;
        };
        if review.status == lk_vault::ReviewStatus::Open
            && review.kind == Some(lk_vault::ReviewKind::EvidenceGap)
            && review
                .subjects
                .iter()
                .any(|subject| owner(subject).as_ref() == Some(&concept_owner))
        {
            let title = page
                .frontmatter
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| path.clone());
            gaps.push(EvidenceGap {
                path: path.clone(),
                title,
                citations_changed: review.judged_citation_digest.as_deref()
                    != Some(citation_digest),
            });
        }
    }
    Ok(gaps)
}

/// Check the claims a result carries against the gaps its scope holds: a list exactly when an
/// open evidence-gap review is in scope, each claim one nonblank line. An empty list answers that
/// the citations now support every claim those reviews name, so it needs citations that changed
/// since every one of them was judged; the reviews then await reassessment.
pub(super) fn check_claims(gaps: &[EvidenceGap], claims: Option<&[String]>) -> Result<(), String> {
    if claims.is_some_and(<[String]>::is_empty)
        && let Some(unchanged) = gaps.iter().find(|gap| !gap.citations_changed)
    {
        return Err(format!(
            "the citations are those the evidence-gap review {} judged; no new evidence answers it, so return the defining claims it names in gap_claims",
            unchanged.path
        ));
    }
    match (gaps.is_empty(), claims) {
        (true, None) => Ok(()),
        (true, Some(_)) => {
            Err("gap_claims need an open evidence-gap review of this concept in scope".into())
        }
        (false, None) => Err(
            "an open evidence-gap review of this concept is in scope: return the defining claims it names that remain unsupported in gap_claims"
                .into(),
        ),
        (false, Some(claims)) => claims
            .iter()
            .all(|claim| !claim.trim().is_empty() && !claim.contains(['\n', '\r']))
            .then_some(())
            .ok_or_else(|| "each gap claim is one nonblank line".into()),
    }
}

/// The callout: its marker and lead, the defining claims awaiting evidence, and a reference to
/// every open evidence-gap review, each in its own paragraph so no inline span in a claim or
/// title reaches another reference.
pub(super) fn render(
    locale: Locale,
    concept: &str,
    gaps: &[EvidenceGap],
    claims: &[String],
) -> String {
    let strings = locale.strings();
    let mut callout = format!("> [!gap] {}\n", strings.evidence_gap_lead);
    for claim in claims {
        callout.push_str(&format!("> - {}\n", claim.trim()));
    }
    for gap in gaps {
        let destination = lk_core::link::relative_dest(Path::new(concept), Path::new(&gap.path));
        callout.push_str(&format!(
            ">\n> {}: {}\n",
            strings.evidence_gap_review,
            lk_core::link::md_link(&gap.title, &destination)
        ));
    }
    callout
}

/// The reviews a published callout in `body` references, as vault addresses resolved from
/// `concept`. Only the reference lines `render` writes count, never a link inside a claim.
fn callout_references(concept: &str, body: &str) -> Vec<PathBuf> {
    let labels = Locale::ALL
        .iter()
        .map(|locale| format!("> {}: ", locale.strings().evidence_gap_review))
        .collect::<Vec<_>>();
    let mut references = Vec::new();
    let mut inside = false;
    for line in lk_core::markdown::lines_with_endings(body) {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.starts_with("> [!gap] ") {
            inside = true;
            continue;
        }
        inside &= line.starts_with('>');
        let Some(reference) = labels
            .iter()
            .filter(|_| inside)
            .find_map(|label| line.strip_prefix(label.as_str()))
            .and_then(lk_core::link::parse_standalone_page_link)
            .and_then(|link| lk_core::link::resolve_dest(Path::new(concept), &link.dest))
        else {
            continue;
        };
        references.push(reference);
    }
    references
}

/// Whether the callout in the synthesis section `synthesis` of `concept` references a review
/// that is no longer an open evidence-gap review of the concept: resolved, of another kind or
/// about other subjects. That callout no longer matches its reviews, so the synthesis is owed
/// again. A review that is removed, unreadable or not a review is left to review health: a
/// synthesis whose scoped reviews cannot be read cannot be prepared either.
pub(in crate::commands) fn references_closed_review(
    root: &Path,
    concept: &str,
    synthesis: &str,
) -> miette::Result<bool> {
    let owner = |address: &Path| lk_vault::resolve_page_owner(root, address).ok();
    let concept_owner = owner(Path::new(concept));
    for reference in callout_references(concept, synthesis) {
        let raw = match std::fs::read_to_string(root.join(&reference)) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(miette::miette!("read {}: {error}", reference.display())),
        };
        let Some(review) = lk_core::frontmatter::parse_page(&raw)
            .ok()
            .and_then(|page| page.frontmatter.get("review").cloned())
            .and_then(|value| serde_json::from_value::<lk_vault::KnowledgeReview>(value).ok())
        else {
            continue;
        };
        let names_concept = review
            .subjects
            .iter()
            .any(|subject| owner(Path::new(subject)) == concept_owner);
        if review.status != lk_vault::ReviewStatus::Open
            || review.kind != Some(lk_vault::ReviewKind::EvidenceGap)
            || !names_concept
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The open evidence-gap reviews a synthesis context captured.
pub(super) fn from_context(context: &serde_json::Value) -> miette::Result<Vec<EvidenceGap>> {
    match context["purpose"]["parameters"].get("evidence_gaps") {
        Some(gaps) => serde_json::from_value(gaps.clone())
            .map_err(|error| miette::miette!("synthesis evidence gaps: {error}")),
        None => Ok(Vec::new()),
    }
}

/// The locale a synthesis context publishes in.
pub(super) fn locale(context: &serde_json::Value) -> miette::Result<Locale> {
    context["purpose"]["parameters"]["locale"]
        .as_str()
        .and_then(Locale::try_from_tag)
        .ok_or_else(|| miette::miette!("synthesis context has no supported locale"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(kind: &str, status: &str, subject: &str) -> String {
        format!(
            "---\ntype: exploration\ntitle: |-\n  Definition\n  gap\nreview:\n  status: {status}\n  kind: {kind}\n  subjects: [{subject}]\n  evidence: []\n  resolution: null\n---\nThe definition lacks support.\n"
        )
    }

    #[test]
    fn a_callout_follows_only_its_reference_lines_to_each_review_state() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.join("wiki/explorations")).unwrap();
        let concept = "wiki/concepts/topic.md";
        std::fs::write(root.join(concept), "---\ntype: concept\n---\n").unwrap();
        let gap = EvidenceGap {
            path: "wiki/explorations/gap.md".into(),
            title: "Gap".into(),
            citations_changed: false,
        };
        let claims = ["See [the old question](../explorations/closed.md).".to_owned()];
        let page = format!("Body.\n\n{}", render(Locale::Ko, concept, &[gap], &claims));
        std::fs::write(
            root.join("wiki/explorations/closed.md"),
            review("evidence-gap", "resolved", concept),
        )
        .unwrap();
        let write = |status: &str, kind: &str| {
            std::fs::write(
                root.join("wiki/explorations/gap.md"),
                review(kind, status, concept),
            )
            .unwrap()
        };
        write("open", "evidence-gap");
        assert!(
            !references_closed_review(root, concept, &page).unwrap(),
            "an open gap keeps its callout, and a link inside a claim is no reference"
        );
        write("resolved", "evidence-gap");
        assert!(references_closed_review(root, concept, &page).unwrap());
        write("open", "conflict");
        assert!(references_closed_review(root, concept, &page).unwrap());
        std::fs::remove_file(root.join("wiki/explorations/gap.md")).unwrap();
        assert!(
            !references_closed_review(root, concept, &page).unwrap(),
            "a removed review is left to review health"
        );
        assert!(!references_closed_review(root, concept, "No callout.\n").unwrap());
    }

    #[test]
    fn only_an_open_evidence_gap_review_naming_the_concept_owner_obliges_claims() {
        let root = tempfile::tempdir().unwrap();
        let concepts = root.path().join("wiki/concepts");
        std::fs::create_dir_all(&concepts).unwrap();
        std::fs::create_dir_all(root.path().join("wiki/explorations")).unwrap();
        std::fs::write(concepts.join("topic.md"), "---\ntype: concept\n---\n").unwrap();
        std::fs::write(concepts.join("other.md"), "---\ntype: concept\n---\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("topic.md", concepts.join("topic-alias.md")).unwrap();
        let concept = "wiki/concepts/topic.md";
        let alias = if cfg!(unix) {
            "wiki/concepts/topic-alias.md"
        } else {
            concept
        };
        let reviews = [
            ("gap", review("evidence-gap", "open", alias)),
            ("conflict", review("conflict", "open", concept)),
            ("resolved", review("evidence-gap", "resolved", concept)),
            (
                "other",
                review("evidence-gap", "open", "wiki/concepts/other.md"),
            ),
        ];
        let mut pages = BTreeMap::new();
        let mut scoped = Vec::new();
        for (name, raw) in reviews {
            let path = format!("wiki/explorations/{name}.md");
            std::fs::write(root.path().join(&path), &raw).unwrap();
            pages.insert(PathBuf::from(&path), raw);
            scoped.push(path);
        }
        let root = root.path().canonicalize().unwrap();
        let digest = "0123456789abcdef0123456789abcdef";
        let gaps = open_reviews(&root, concept, digest, &scoped, &pages).unwrap();
        assert_eq!(
            gaps,
            [EvidenceGap {
                path: "wiki/explorations/gap.md".into(),
                title: "Definition gap".into(),
                citations_changed: true,
            }],
            "a subject alias names the concept's owner, and the title renders on one line"
        );
        assert!(check_claims(&gaps, None).is_err());
        assert!(
            check_claims(&gaps, Some(&[])).is_ok(),
            "recovered evidence leaves no claim"
        );
        assert!(check_claims(&gaps, Some(&["two\nlines".to_owned()])).is_err());
        assert!(check_claims(&[], Some(&["claim".to_owned()])).is_err());
        let claims = ["A protocol is a mutual agreement.".to_owned()];
        assert!(check_claims(&gaps, Some(&claims)).is_ok());
        let judged = |citations: &str| {
            let raw = review("evidence-gap", "open", concept).replace(
                "  resolution: null\n",
                &format!("  resolution: null\n  judged_citation_digest: {citations}\n"),
            );
            let path = PathBuf::from("wiki/explorations/gap.md");
            std::fs::write(root.join(&path), &raw).unwrap();
            let pages = BTreeMap::from([(path.clone(), raw)]);
            open_reviews(
                &root,
                concept,
                digest,
                &[path.to_string_lossy().into_owned()],
                &pages,
            )
            .unwrap()
        };
        let unchanged = judged(digest);
        assert!(!unchanged[0].citations_changed);
        assert!(
            check_claims(&unchanged, Some(&[])).is_err(),
            "the citations the gap was judged against cannot answer it"
        );
        assert!(check_claims(&unchanged, Some(&claims)).is_ok());
        let changed = judged("fedcba9876543210fedcba9876543210");
        assert!(changed[0].citations_changed);
        assert!(check_claims(&changed, Some(&[])).is_ok());
        assert_eq!(
            render(Locale::En, concept, &gaps, &claims),
            "> [!gap] These defining claims await supporting evidence.\n> - A protocol is a mutual agreement.\n>\n> Evidence gap review: [Definition gap](../explorations/gap.md)\n"
        );
    }
}
