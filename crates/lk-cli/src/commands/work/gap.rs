//! The evidence-gap callout a concept synthesis keeps while an open evidence-gap review of the
//! concept is in its scope. A synthesis result carries the defining claims that review names as
//! structured `gap_claims`; publication renders the callout beneath the synthesis from those
//! claims and the reviews its own capture found, so the marker and every review reference are
//! Rust's, never prose a writer must reproduce.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::i18n::Locale;
use serde::{Deserialize, Serialize};

/// An open evidence-gap review of the synthesized concept, by address and title.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EvidenceGap {
    pub path: String,
    pub title: String,
}

/// The open evidence-gap reviews of `concept` among a synthesis capture's scoped `reviews`. Each
/// review is read from the bytes that capture stored under its verified owner, and names the
/// concept when one of its subjects has the concept's owner. A review of another kind, resolved,
/// unreadable or about other subjects is not one.
pub(super) fn open_reviews(
    root: &Path,
    concept: &str,
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
            });
        }
    }
    Ok(gaps)
}

/// Check the claims a result carries against the gaps its scope holds: a list exactly when an
/// open evidence-gap review is in scope, each claim one nonblank line. An empty list answers that
/// the citations now support every claim those reviews name; the reviews await reassessment.
pub(super) fn check_claims(gaps: &[EvidenceGap], claims: Option<&[String]>) -> Result<(), String> {
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
        let gaps = open_reviews(&root, concept, &scoped, &pages).unwrap();
        assert_eq!(
            gaps,
            [EvidenceGap {
                path: "wiki/explorations/gap.md".into(),
                title: "Definition gap".into(),
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
        assert_eq!(
            render(Locale::En, concept, &gaps, &claims),
            "> [!gap] These defining claims await supporting evidence.\n> - A protocol is a mutual agreement.\n>\n> Evidence gap review: [Definition gap](../explorations/gap.md)\n"
        );
    }
}
