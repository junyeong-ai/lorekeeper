//! The open reviews a concept synthesis answers to. A synthesis capture lists every open
//! review naming the concept; the result names, in `review_refs`, those whose question it
//! leaves open, and for an evidence-gap review the defining claims still unsupported in
//! `gap_claims`. Publication renders the callouts beneath the synthesis from those references
//! and the capture, and records every review it read, with its authored revision and whether
//! it is referenced, on the concept (`llm_inputs.synthesis_reviews`, JSON text like every
//! `llm_inputs` value, which a concept rerender carries with the synthesis it describes), so
//! the marker and every reference are Rust's, and the synthesis follows the reviews naming it
//! from that record, never from prose whose author Rust cannot establish.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::i18n::Locale;
use lk_vault::ReviewKind;
use serde::{Deserialize, Serialize};

/// An open review naming the synthesized concept: its address, title, kind and authored
/// revision (`lk_core::concept::source_revision`), and whether the concept's citations changed
/// since the review judged them. A review that records no judged citations counts as changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct OpenReview {
    pub path: String,
    pub title: String,
    pub kind: Option<ReviewKind>,
    pub revision: String,
    pub citations_changed: bool,
}

/// An open review as the published synthesis recorded it: its kind and authored revision, and
/// whether the synthesis referenced it. Records written before unreferenced reviews were kept
/// hold only referenced ones.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recorded {
    kind: Option<ReviewKind>,
    revision: String,
    #[serde(default = "referenced")]
    referenced: bool,
}

fn referenced() -> bool {
    true
}

impl OpenReview {
    fn is_gap(&self) -> bool {
        self.kind == Some(ReviewKind::EvidenceGap)
    }
}

/// The open reviews of `concept` among a synthesis capture's scoped `reviews`, for a request
/// answering the citations `citation_digest` names, each once under its verified owner's
/// address and read from the bytes that capture stored there. A review names the concept when
/// one of its subjects has the concept's owner. A resolved, unreadable or other-subject review is not one.
pub(in crate::commands) fn open_reviews(
    root: &Path,
    concept: &str,
    citation_digest: &str,
    reviews: &[String],
    pages: &BTreeMap<PathBuf, String>,
) -> miette::Result<Vec<OpenReview>> {
    let owner = |address: &str| lk_vault::resolve_page_owner(root, Path::new(address)).ok();
    let Some(concept_owner) = owner(concept) else {
        return Ok(Vec::new());
    };
    let mut open = Vec::new();
    let mut owners = std::collections::BTreeSet::new();
    for path in reviews {
        let Some(stored) = owner(path).filter(|stored| owners.insert(stored.clone())) else {
            continue;
        };
        let address = lk_core::vault_path::address(&stored);
        let Some(page) = pages
            .get(&stored)
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
                .unwrap_or_else(|| address.clone());
            open.push(OpenReview {
                path: address,
                title,
                kind: review.kind,
                revision: lk_core::concept::source_revision(&page),
                citations_changed: review.judged_citation_digest.as_deref()
                    != Some(citation_digest),
            });
        }
    }
    Ok(open)
}

/// Check a result's references against the open reviews its scope holds. With reviews in
/// scope, `review_refs` lists those whose question the synthesis leaves open, each once; an
/// empty list answers that the citations now settle every one. A review judged against these
/// exact citations stays referenced, since no new evidence answers it. `gap_claims` carries
/// the defining claims, one nonblank line each, exactly when an evidence-gap review is
/// referenced.
pub(super) fn check_reply(
    reviews: &[OpenReview],
    refs: Option<&[String]>,
    claims: Option<&[String]>,
) -> Result<(), String> {
    let refs = match (reviews.is_empty(), refs) {
        (true, None) => &[][..],
        (true, Some(_)) => {
            return Err("review_refs need an open review of this concept in scope".into());
        }
        (false, None) => {
            return Err(
                "open reviews of this concept are in scope: list in review_refs each one whose question the synthesis leaves open, or [] when the citations now settle every one"
                    .into(),
            );
        }
        (false, Some(refs)) => refs,
    };
    let mut seen = Vec::new();
    for reference in refs {
        if !reviews.iter().any(|review| &review.path == reference) || seen.contains(&reference) {
            return Err(format!(
                "review_refs entry {reference} is not one distinct open review in scope"
            ));
        }
        seen.push(reference);
    }
    if let Some(unchanged) = reviews
        .iter()
        .find(|review| !review.citations_changed && !refs.contains(&review.path))
    {
        return Err(format!(
            "the citations are those the review {} judged; no new evidence answers it, so keep it in review_refs",
            unchanged.path
        ));
    }
    let gap = reviews
        .iter()
        .any(|review| review.is_gap() && refs.contains(&review.path));
    match (gap, claims) {
        (false, None) => Ok(()),
        (false, Some(_)) => {
            Err("gap_claims need an evidence-gap review listed in review_refs".into())
        }
        (true, Some(claims))
            if !claims.is_empty()
                && claims
                    .iter()
                    .all(|claim| !claim.trim().is_empty() && !claim.contains(['\n', '\r'])) =>
        {
            Ok(())
        }
        (true, _) => Err(
            "an evidence-gap review listed in review_refs needs the defining claims it names that remain unsupported in gap_claims, each one nonblank line"
                .into(),
        ),
    }
}

/// The callouts for the referenced reviews: the evidence-gap callout with its claims and a
/// reference to each gap review, then one callout referencing every other review. Each
/// reference is its own paragraph so no inline span in a claim or title reaches another.
pub(super) fn render(
    locale: Locale,
    concept: &str,
    reviews: &[OpenReview],
    refs: &[String],
    claims: &[String],
) -> Option<String> {
    let strings = locale.strings();
    let referenced = reviews
        .iter()
        .filter(|review| refs.contains(&review.path))
        .collect::<Vec<_>>();
    let reference = |label: &str, review: &OpenReview| {
        let destination = lk_core::link::relative_dest(Path::new(concept), Path::new(&review.path));
        format!(
            ">\n> {label}: {}\n",
            lk_core::link::md_link(&review.title, &destination)
        )
    };
    let mut callouts = Vec::new();
    let gaps = referenced.iter().filter(|review| review.is_gap());
    if gaps.clone().next().is_some() {
        let mut callout = format!("> [!gap] {}\n", strings.evidence_gap_lead);
        for claim in claims {
            callout.push_str(&format!("> - {}\n", claim.trim()));
        }
        for review in gaps {
            callout.push_str(&reference(strings.evidence_gap_review, review));
        }
        callouts.push(callout);
    }
    let others = referenced.iter().filter(|review| !review.is_gap());
    if others.clone().next().is_some() {
        let mut callout = format!("> [!question] {}\n", strings.open_review_lead);
        for review in others {
            callout.push_str(&reference(strings.open_review, review));
        }
        callouts.push(callout);
    }
    (!callouts.is_empty()).then(|| callouts.join("\n"))
}

/// What a synthesis publication adds for its referenced reviews: the callouts beneath its body,
/// absent when it references none, and the `llm_inputs.synthesis_reviews` record, which every
/// synthesis publication writes and no other result does.
#[derive(Debug, Default)]
pub(in crate::commands) struct PublishedReviews {
    pub callout: Option<String>,
    pub record: Option<String>,
}

/// The `llm_inputs.synthesis_reviews` value recording every open review the synthesis read,
/// with its kind, authored revision and whether the synthesis referenced it; `{}` when its
/// scope held none.
pub(super) fn record(reviews: &[OpenReview], refs: &[String]) -> String {
    let recorded = reviews
        .iter()
        .map(|review| {
            (
                review.path.clone(),
                Recorded {
                    kind: review.kind,
                    revision: review.revision.clone(),
                    referenced: refs.contains(&review.path),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    lk_core::concept::yaml_scalar(&recorded)
}

/// The open reviews a synthesis capture listed.
pub(super) fn from_context(context: &serde_json::Value) -> miette::Result<Vec<OpenReview>> {
    match context["purpose"]["parameters"].get("open_reviews") {
        Some(reviews) => serde_json::from_value(reviews.clone())
            .map_err(|error| miette::miette!("synthesis open reviews: {error}")),
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

/// What a concept's published synthesis owes the open reviews naming it.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::commands) enum PublishedState {
    /// No `llm_inputs.synthesis_reviews` record: published before references were recorded,
    /// or never synthesized. Nothing establishes that a callout on the page is Rust's.
    Unrecorded,
    /// A record Rust cannot read. Nothing establishes what the synthesis answered, so it is
    /// reported for repair and its synthesis is kept.
    Invalid,
    /// The open reviews naming the concept are those the synthesis read, unchanged.
    Current,
    /// A review opened, reopened, retyped or revised since, or a referenced review was removed,
    /// resolved or no longer names the concept: the synthesis is owed again.
    Owed,
}

/// The state of a concept's published synthesis (page `raw`) against `open`, the open reviews
/// naming it now as a synthesis capture of it would list them (`open_reviews`).
pub(in crate::commands) fn published_state(raw: &str, open: &[OpenReview]) -> PublishedState {
    let Some(recorded) = lk_core::frontmatter::parse_page(raw).ok().and_then(|page| {
        page.frontmatter
            .get(lk_core::frontmatter::field::LLM_INPUTS)
            .and_then(|inputs| inputs.get(lk_core::frontmatter::field::SYNTHESIS_REVIEWS))
            .cloned()
    }) else {
        return PublishedState::Unrecorded;
    };
    let Some(Ok(recorded)) = recorded
        .as_str()
        .map(serde_json::from_str::<BTreeMap<String, Recorded>>)
    else {
        return PublishedState::Invalid;
    };
    let read = open.iter().all(|review| {
        recorded.get(&review.path).is_some_and(|recorded| {
            recorded.kind == review.kind && recorded.revision == review.revision
        })
    });
    let referenced_open = recorded
        .iter()
        .filter(|(_, recorded)| recorded.referenced)
        .all(|(path, _)| open.iter().any(|review| &review.path == path));
    if read && referenced_open {
        PublishedState::Current
    } else {
        PublishedState::Owed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(kind: &str, status: &str, subject: &str) -> String {
        format!(
            "---\ntype: exploration\ntitle: |-\n  Definition\n  gap\nreview:\n  status: {status}\n  kind: {kind}\n  subjects: [{subject}]\n  evidence: []\n  resolution: null\n---\nThe definition lacks support.\n"
        )
    }

    fn open(path: &str, kind: ReviewKind, citations_changed: bool) -> OpenReview {
        OpenReview {
            path: path.into(),
            title: "Title".into(),
            kind: Some(kind),
            revision: format!("{path}-revision"),
            citations_changed,
        }
    }

    #[test]
    fn a_published_record_follows_every_open_review_and_prose_is_never_one() {
        let page = |record: &str| {
            format!(
                "---\ntype: concept\nllm_inputs:\n  synthesis_reviews: {}\n---\n## Synthesis\nBody.\n",
                lk_core::concept::yaml_scalar(&record)
            )
        };
        let conflict = open("wiki/explorations/conflict.md", ReviewKind::Conflict, false);
        let gap = open("wiki/explorations/gap.md", ReviewKind::EvidenceGap, false);
        let published = page(&record(
            &[conflict.clone(), gap.clone()],
            std::slice::from_ref(&conflict.path),
        ));
        let state = |open: &[OpenReview]| published_state(&published, open);
        assert_eq!(
            state(&[conflict.clone(), gap.clone()]),
            PublishedState::Current
        );
        assert_eq!(
            state(std::slice::from_ref(&conflict)),
            PublishedState::Current,
            "an unreferenced review closing changes nothing the synthesis says"
        );
        assert_eq!(
            state(std::slice::from_ref(&gap)),
            PublishedState::Owed,
            "a referenced review closed, was removed or no longer names the concept"
        );
        let revised = OpenReview {
            revision: "changed".into(),
            ..gap.clone()
        };
        assert_eq!(state(&[conflict.clone(), revised]), PublishedState::Owed);
        let retyped = OpenReview {
            kind: Some(ReviewKind::Conflict),
            ..gap.clone()
        };
        assert_eq!(state(&[conflict.clone(), retyped]), PublishedState::Owed);
        let opened = open("wiki/explorations/new.md", ReviewKind::Conflict, true);
        assert_eq!(
            state(&[conflict.clone(), gap.clone(), opened]),
            PublishedState::Owed,
            "a review opened after publication is owed an answer"
        );

        let legacy = page(&format!(
            r#"{{"{}":{{"kind":"conflict","revision":"{}"}}}}"#,
            conflict.path, conflict.revision
        ));
        assert_eq!(
            published_state(&legacy, std::slice::from_ref(&conflict)),
            PublishedState::Current,
            "a record listing only referenced reviews reads them as referenced"
        );
        assert_eq!(published_state(&legacy, &[]), PublishedState::Owed);
        assert_eq!(published_state(&page("{}"), &[]), PublishedState::Current);
        assert_eq!(
            published_state(&page("{\"wiki/explorations/gap.md\":"), &[]),
            PublishedState::Invalid,
            "an unreadable record is reported, never a reason to rewrite"
        );
        let quoted = "---\ntype: concept\n---\n## Synthesis\nBody.\n\n> [!gap] These defining claims await supporting evidence.\n>\n> Evidence gap review: [Gap](../explorations/gap.md)\n";
        assert_eq!(
            published_state(quoted, std::slice::from_ref(&gap)),
            PublishedState::Unrecorded,
            "a quote nothing establishes as Lore's never owes a rewrite"
        );
    }

    #[test]
    fn only_an_open_review_naming_the_concept_owner_is_in_scope() {
        let root = tempfile::tempdir().unwrap();
        let concepts = root.path().join("wiki/concepts");
        std::fs::create_dir_all(&concepts).unwrap();
        std::fs::create_dir_all(root.path().join("wiki/explorations")).unwrap();
        std::fs::write(concepts.join("topic.md"), "---\ntype: concept\n---\n").unwrap();
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
            ("conflict-alias", String::new()),
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
            if name == "conflict-alias" {
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink("conflict.md", root.path().join(&path)).unwrap();
                    scoped.push(path);
                }
                continue;
            }
            std::fs::write(root.path().join(&path), &raw).unwrap();
            pages.insert(PathBuf::from(&path), raw);
            scoped.push(path);
        }
        let root = root.path().canonicalize().unwrap();
        let digest = "0123456789abcdef0123456789abcdef";
        let open = open_reviews(&root, concept, digest, &scoped, &pages).unwrap();
        assert_eq!(
            open.iter()
                .map(|review| review.path.as_str())
                .collect::<Vec<_>>(),
            ["wiki/explorations/gap.md", "wiki/explorations/conflict.md"],
            "a subject alias names the concept's owner, and a review reached through two \
             addresses is listed once"
        );
        assert_eq!(
            open[0].title, "Definition gap",
            "the title renders on one line"
        );
        let judged = review("evidence-gap", "open", concept).replace(
            "  resolution: null\n",
            &format!("  resolution: null\n  judged_citation_digest: {digest}\n"),
        );
        let path = PathBuf::from("wiki/explorations/gap.md");
        std::fs::write(root.join(&path), &judged).unwrap();
        let unchanged = open_reviews(
            &root,
            concept,
            digest,
            &[path.to_string_lossy().into_owned()],
            &BTreeMap::from([(path, judged)]),
        )
        .unwrap();
        assert!(!unchanged[0].citations_changed);
    }

    #[test]
    fn references_name_open_reviews_and_gap_claims_follow_a_referenced_gap() {
        let gap = open("wiki/explorations/gap.md", ReviewKind::EvidenceGap, true);
        let conflict = open("wiki/explorations/conflict.md", ReviewKind::Conflict, true);
        let reviews = [gap.clone(), conflict.clone()];
        let claims = ["A protocol is a mutual agreement.".to_owned()];
        let refs = |paths: &[&str]| {
            paths
                .iter()
                .map(|path| path.to_string())
                .collect::<Vec<_>>()
        };
        assert!(check_reply(&[], None, None).is_ok());
        assert!(check_reply(&[], Some(&[]), None).is_err());
        assert!(check_reply(&reviews, None, None).is_err());
        assert!(
            check_reply(&reviews, Some(&[]), None).is_ok(),
            "recovered evidence settles both"
        );
        assert!(check_reply(&reviews, Some(&refs(&[&conflict.path])), None).is_ok());
        assert!(check_reply(&reviews, Some(&refs(&[&gap.path])), None).is_err());
        assert!(check_reply(&reviews, Some(&refs(&[&gap.path])), Some(&claims)).is_ok());
        assert!(check_reply(&reviews, Some(&refs(&[&conflict.path])), Some(&claims)).is_err());
        assert!(
            check_reply(
                &reviews,
                Some(&refs(&[&gap.path, &gap.path])),
                Some(&claims)
            )
            .is_err()
        );
        assert!(check_reply(&reviews, Some(&refs(&["wiki/explorations/other.md"])), None).is_err());
        assert!(
            check_reply(
                &reviews,
                Some(&refs(&[&gap.path])),
                Some(&["two\nlines".to_owned()])
            )
            .is_err()
        );
        let judged = [open(&conflict.path, ReviewKind::Conflict, false)];
        assert!(
            check_reply(&judged, Some(&[]), None).is_err(),
            "the citations a review was judged against cannot settle it"
        );

        let concept = "wiki/concepts/topic.md";
        let both = refs(&[&gap.path, &conflict.path]);
        assert_eq!(
            render(Locale::En, concept, &reviews, &both, &claims).unwrap(),
            "> [!gap] These defining claims await supporting evidence.\n> - A protocol is a mutual agreement.\n>\n> Evidence gap review: [Title](../explorations/gap.md)\n\n> [!question] These reviews keep a question about this concept open.\n>\n> Open review: [Title](../explorations/conflict.md)\n"
        );
        assert_eq!(render(Locale::En, concept, &reviews, &[], &[]), None);
        assert_eq!(
            record(&reviews, &refs(&[&gap.path])),
            r#"{"wiki/explorations/conflict.md":{"kind":"conflict","revision":"wiki/explorations/conflict.md-revision","referenced":false},"wiki/explorations/gap.md":{"kind":"evidence-gap","revision":"wiki/explorations/gap.md-revision","referenced":true}}"#,
            "every open review read is recorded, referenced or not"
        );
        assert_eq!(record(&[], &[]), "{}");
    }
}
